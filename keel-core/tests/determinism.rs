//! P1 确定性回放验收（docs/BASELINE.md §16 P1、§13）。
//!
//! 验证：同一输入流注入两次，事件队列派发序列逐位一致；
//! 乱序时间戳注入后按 `(ts, phase, seq)` 全序恢复；
//! 事件流累计 XXH3 哈希跨运行稳定（golden 协议的内核层雏形）。

use keel_core::{EventKey, EventQueue, Phase};

/// 事件流累计哈希：键（固定小端编码）+ payload 逐事件累计 XXH3。
/// payload 不参与 key 次序，但参与摘要——任何派发差异都能被 golden 捕获（§13）。
fn stream_digest<T: Copy>(popped: &[(EventKey, T)]) -> u64
where
    u64: From<T>,
{
    use xxhash_rust::xxh3::Xxh3;
    let mut h = Xxh3::new();
    let mut buf = [0u8; EventKey::ENCODED_LEN + 8];
    for (k, v) in popped {
        k.write_le(&mut buf);
        let v64: u64 = (*v).into();
        buf[EventKey::ENCODED_LEN..].copy_from_slice(&v64.to_le_bytes());
        h.update(&buf);
    }
    h.digest()
}

/// 确定性伪随机（LCG，测试内固定种子，不用系统熵）。
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// 生成固定事件输入流：随机 ts（0..1000）、随机 phase、payload = 注入序号。
/// 返回注入列表（乱序）。
fn generate_stream(n: usize) -> Vec<(i64, Phase)> {
    let mut rng = Lcg(2026_11_15);
    (0..n)
        .map(|_| {
            let ts = (rng.next() % 1000) as i64;
            let phase = match rng.next() % 5 {
                0 => Phase::MarketData,
                1 => Phase::Matching,
                2 => Phase::Fill,
                3 => Phase::CancelAck,
                _ => Phase::StrategyOrder,
            };
            (ts, phase)
        })
        .collect()
}

fn feed_and_drain(injection: &[(i64, Phase)]) -> Vec<(EventKey, u64)> {
    let mut q: EventQueue<u64> = EventQueue::with_capacity(injection.len());
    for (i, (ts, phase)) in injection.iter().enumerate() {
        q.push(*ts, *phase, i as u64)
            .expect("队列容量应与注入数一致");
    }
    let mut out = Vec::with_capacity(injection.len());
    while let Some(ev) = q.pop() {
        let (k, v) = ev.into_parts();
        out.push((k, v));
    }
    out
}

#[test]
fn same_input_replay_bit_identical() {
    let injection = generate_stream(10_000);

    let run1 = feed_and_drain(&injection);
    let run2 = feed_and_drain(&injection);

    assert_eq!(run1.len(), run2.len());
    // 逐位一致：键与 payload 完全相同
    for (a, b) in run1.iter().zip(run2.iter()) {
        assert_eq!(a.0, b.0);
        assert_eq!(a.1, b.1);
    }
    // 累计哈希一致且非零
    let d1 = stream_digest(&run1);
    let d2 = stream_digest(&run2);
    assert_eq!(d1, d2);
    assert_ne!(d1, 0);
}

#[test]
fn total_order_restored_from_shuffled_injection() {
    let injection = generate_stream(5_000);
    let popped = feed_and_drain(&injection);

    assert_eq!(popped.len(), injection.len());

    // 全序键严格递增（seq 唯一保证同 (ts,phase) 内也有全序）
    for w in popped.windows(2) {
        assert!(w[0].0 < w[1].0);
        assert!(w[0].0.ts <= w[1].0.ts);
    }
    // seq 唯一
    let mut seqs: Vec<u64> = popped.iter().map(|e| e.0.seq).collect();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), popped.len());
    // payload（注入序号）完整保留
    let mut payloads: Vec<u64> = popped.iter().map(|e| e.1).collect();
    payloads.sort_unstable();
    assert_eq!(payloads, (0..injection.len() as u64).collect::<Vec<_>>());
}

#[test]
fn same_ts_phase_fifo_insertion_order() {
    // 同 (ts, phase) 的事件按入队序派发（seq FIFO，§5.2）
    let injection = vec![
        (7, Phase::Fill),
        (7, Phase::Fill),
        (7, Phase::Fill),
        (7, Phase::Fill),
    ];
    let popped = feed_and_drain(&injection);
    let order: Vec<u64> = popped.iter().map(|e| e.1).collect();
    assert_eq!(order, vec![0, 1, 2, 3]);
    // 全部 MarketData(0) < Fill(2)：四个事件 phase 相同，键按 seq 递增
    for w in popped.windows(2) {
        assert!(w[0].0 < w[1].0);
    }
}

#[test]
fn digest_detects_any_deviation() {
    // golden 摘要的灵敏度：任何键变化（含 payload 编码位）都改变摘要
    let injection = vec![(1, Phase::MarketData), (1, Phase::Fill)];
    let base = stream_digest(&feed_and_drain(&injection));

    let injection_shifted_ts = vec![(2, Phase::MarketData), (1, Phase::Fill)];
    assert_ne!(base, stream_digest(&feed_and_drain(&injection_shifted_ts)));

    let injection_shifted_phase = vec![(1, Phase::Matching), (1, Phase::Fill)];
    assert_ne!(
        base,
        stream_digest(&feed_and_drain(&injection_shifted_phase))
    );
}
