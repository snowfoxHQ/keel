//! 事件模型与全序键（docs/BASELINE.md §5.2）+ 固定容量事件队列（§3.2 约束 1）。
//!
//! 全序键 = `(ts, phase, seq)`：
//! - 同 `ts` 事件按 phase 次序派发；
//! - `seq` 为入队时分配的全局单调唯一计数器，**同键不允许出现**；
//! - 不依赖容器内部顺序（`BinaryHeap` 同键次序不可依赖，靠 seq 唯一性规避）。
//!
//! 队列构造时一次性预分配；push 在容量耗尽时返回 [`QueueError::Full`]，禁止隐式扩容。

use core::cmp::Ordering;
use core::fmt;
use std::collections::BinaryHeap;
use std::cmp::Reverse;

/// 流水线阶段，声明顺序即派发次序（§5.2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Phase {
    MarketData = 0,
    Matching = 1,
    Fill = 2,
    CancelAck = 3,
    StrategyOrder = 4,
}

/// 全序键 `(ts, phase, seq)`。derive 顺序即比较优先级。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventKey {
    /// 事件时间戳（热路径只使用事件时间，不取系统时钟，§3.2 约束 4）。
    pub ts: i64,
    pub phase: Phase,
    /// 全局单调唯一计数器。
    pub seq: u64,
}

impl EventKey {
    /// 固定编码长度：ts(8) + phase(1) + seq(8)，小端（§14 序列化契约）。
    pub const ENCODED_LEN: usize = 17;

    /// 固定字段顺序、固定小端编码写入 `out`（长度须 ≥ [`EventKey::ENCODED_LEN`]）。
    /// 供确定性哈希与 golden 回归使用；编码变更须走 ADR。
    pub fn write_le(&self, out: &mut [u8]) {
        assert!(out.len() >= Self::ENCODED_LEN);
        out[0..8].copy_from_slice(&self.ts.to_le_bytes());
        out[8] = self.phase as u8;
        out[9..17].copy_from_slice(&self.seq.to_le_bytes());
    }
}

/// 带全序键的事件。次序仅由 `EventKey` 决定，与 payload 内容无关。
#[derive(Clone, Copy, Debug)]
pub struct Event<T> {
    key: EventKey,
    payload: T,
}

impl<T> Event<T> {
    #[inline]
    pub const fn key(&self) -> EventKey {
        self.key
    }

    #[inline]
    pub const fn payload(&self) -> &T {
        &self.payload
    }

    #[inline]
    pub fn into_parts(self) -> (EventKey, T) {
        (self.key, self.payload)
    }
}

// 次序仅由 key 决定（§5.2：全序键定义事件次序）
impl<T> PartialEq for Event<T> {
    fn eq(&self, o: &Self) -> bool {
        self.key == o.key
    }
}
impl<T> Eq for Event<T> {}
impl<T> PartialOrd for Event<T> {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl<T> Ord for Event<T> {
    fn cmp(&self, o: &Self) -> Ordering {
        self.key.cmp(&o.key)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// 队列满：拒绝入队，不扩容（§3.2 约束 1）。
    Full,
    /// seq 计数器耗尽（u64 上限，实际不可达，防御性保留）。
    SeqExhausted,
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueError::Full => write!(f, "事件队列已满"),
            QueueError::SeqExhausted => write!(f, "seq 计数器耗尽"),
        }
    }
}

impl std::error::Error for QueueError {}

/// 固定容量最小堆事件队列，按 `(ts, phase, seq)` 全序派发。
pub struct EventQueue<T> {
    heap: BinaryHeap<Reverse<Event<T>>>,
    capacity: usize,
    next_seq: u64,
}

impl<T> EventQueue<T> {
    /// 构造（构造期分配，非热路径）。
    pub fn with_capacity(capacity: usize) -> Self {
        EventQueue {
            heap: BinaryHeap::with_capacity(capacity),
            capacity,
            next_seq: 0,
        }
    }

    /// 入队：分配 seq 后按全序键入堆。容量耗尽返回 [`QueueError::Full`]，不扩容。
    pub fn push(&mut self, ts: i64, phase: Phase, payload: T) -> Result<EventKey, QueueError> {
        if self.heap.len() >= self.capacity {
            return Err(QueueError::Full);
        }
        let seq = self.next_seq;
        self.next_seq = seq.checked_add(1).ok_or(QueueError::SeqExhausted)?;
        let key = EventKey { ts, phase, seq };
        self.heap.push(Reverse(Event { key, payload }));
        Ok(key)
    }

    /// 派发全序最小事件。
    pub fn pop(&mut self) -> Option<Event<T>> {
        self.heap.pop().map(|r| r.0)
    }

    /// 窥视全序最小事件（不移除）。
    pub fn peek(&self) -> Option<&Event<T>> {
        self.heap.peek().map(|r| &r.0)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 下一个将分配的 seq（回放重建用）。
    #[inline]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// 清空（容量与 seq 计数器语义保留：清空不重置 seq，保证跨批次唯一）。
    pub fn clear(&mut self) {
        self.heap.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ordering_ts_phase_seq() {
        let a = EventKey { ts: 1, phase: Phase::MarketData, seq: 0 };
        let b = EventKey { ts: 1, phase: Phase::Matching, seq: 1 };
        let c = EventKey { ts: 2, phase: Phase::MarketData, seq: 2 };
        assert!(a < b);
        assert!(b < c);
        // 同 ts 同 phase，seq 决定次序
        let a2 = EventKey { ts: 1, phase: Phase::MarketData, seq: 5 };
        assert!(a < a2);
    }

    #[test]
    fn queue_restores_timestamp_order() {
        let mut q = EventQueue::with_capacity(8);
        // 乱序注入（§16 P1 验收）
        q.push(5, Phase::MarketData, 'e').unwrap();
        q.push(2, Phase::Fill, 'b').unwrap();
        q.push(2, Phase::MarketData, 'a').unwrap();
        q.push(2, Phase::CancelAck, 'd').unwrap();
        q.push(2, Phase::Fill, 'c').unwrap();

        let mut order = Vec::new();
        while let Some(ev) = q.pop() {
            order.push((ev.key(), *ev.payload()));
        }
        let chars: Vec<char> = order.iter().map(|e| e.1).collect();
        assert_eq!(chars, vec!['a', 'b', 'c', 'd', 'e']);
        // 全序键严格递增
        for w in order.windows(2) {
            assert!(w[0].0 < w[1].0);
        }
        // seq 唯一（pop 序不要求整体递增：seq 是入队序号，仅在同 (ts,phase) 内保证 FIFO）
        let seqs: Vec<u64> = order.iter().map(|e| e.0.seq).collect();
        let mut uniq = seqs.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(seqs.len(), uniq.len());
    }

    #[test]
    fn queue_full_no_realloc() {
        let mut q = EventQueue::<u8>::with_capacity(2);
        assert!(q.push(1, Phase::MarketData, 1).is_ok());
        assert!(q.push(2, Phase::MarketData, 2).is_ok());
        assert_eq!(q.push(3, Phase::MarketData, 3), Err(QueueError::Full));
        assert_eq!(q.len(), 2);
        q.pop();
        assert!(q.push(3, Phase::MarketData, 3).is_ok());
    }

    #[test]
    fn seq_monotonic_across_clear() {
        let mut q = EventQueue::with_capacity(4);
        let k1 = q.push(1, Phase::MarketData, ()).unwrap();
        q.clear();
        let k2 = q.push(1, Phase::MarketData, ()).unwrap();
        assert_eq!(k2.seq, k1.seq + 1);
        assert_eq!(q.next_seq(), 2);
    }

    #[test]
    fn fixed_le_encoding() {
        let k = EventKey { ts: -1, phase: Phase::StrategyOrder, seq: 0x0102030405060708 };
        let mut buf = [0u8; EventKey::ENCODED_LEN];
        k.write_le(&mut buf);
        assert_eq!(buf.len(), 17);
        assert_eq!(&buf[0..8], &(-1i64).to_le_bytes());
        assert_eq!(buf[8], 4);
        assert_eq!(&buf[9..17], &0x0102030405060708u64.to_le_bytes());
    }

    #[test]
    fn event_ordering_ignores_payload() {
        let mut q = EventQueue::with_capacity(4);
        q.push(1, Phase::Fill, 999).unwrap();
        q.push(1, Phase::MarketData, -1).unwrap();
        let a = q.pop().unwrap();
        let b = q.pop().unwrap();
        assert_eq!(*a.payload(), -1);
        assert_eq!(*b.payload(), 999);
        assert!(a.key() < b.key());
    }
}
