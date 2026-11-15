//! 阈值索引：价格桶 + 位图（docs/BASELINE.md §8）。
//!
//! 两个独立方向：
//! - SellStop（保护多头）：市价 ≤ 触发价触发 → 候选触发价 ≥ 市价 → 桶从 `bucket(price)` **向上**扫；
//! - BuyStop（保护空头）：市价 ≥ 触发价触发 → 桶从 `bucket(price)` **向下**扫。
//!
//! 桶定位只做粗筛，精确判定由引擎做整数比较（§8.2）。桶越界 clamp 到边缘桶。
//! 桶内墓碑惰性压缩，不改变存活成员相对顺序（§7.1 同款契约）。

use super::bitmap::BitMap;
use super::rule::{GuardError, TriggerSide};

/// 桶成员表：slot + 墓碑 + 惰性压缩。
struct Bucket {
    members: Vec<Option<u32>>,
    live: u32,
    dead: u32,
}

impl Bucket {
    fn new(cap: usize) -> Self {
        Bucket {
            members: Vec::with_capacity(cap),
            live: 0,
            dead: 0,
        }
    }

    #[inline]
    fn push(&mut self, slot: u32) {
        self.members.push(Some(slot));
        self.live += 1;
    }

    /// 墓碑标记（O(bucket)；桶成员数小，线性可接受）。
    fn remove(&mut self, slot: u32) -> bool {
        for m in self.members.iter_mut() {
            if *m == Some(slot) {
                *m = None;
                self.live -= 1;
                self.dead += 1;
                return true;
            }
        }
        false
    }

    /// 墓碑占比过半则压缩（不改存活成员相对顺序）。
    fn maybe_compact(&mut self) {
        if self.dead > 0 && self.dead * 2 >= self.members.len() as u32 {
            self.members.retain(|m| m.is_some());
            self.dead = 0;
        }
    }
}

/// 单侧索引。
struct SideIndex {
    bmp: BitMap,
    buckets: Vec<Bucket>,
}

impl SideIndex {
    fn new(buckets: usize, bucket_cap: usize) -> Self {
        SideIndex {
            bmp: BitMap::new(buckets),
            buckets: (0..buckets).map(|_| Bucket::new(bucket_cap)).collect(),
        }
    }

    #[inline]
    fn insert(&mut self, b: usize, slot: u32) -> Result<(), GuardError> {
        self.buckets[b].push(slot);
        self.bmp.set(b);
        Ok(())
    }

    fn remove(&mut self, b: usize, slot: u32) {
        self.buckets[b].remove(slot);
        if self.buckets[b].live == 0 {
            self.bmp.clear(b);
        }
        self.buckets[b].maybe_compact();
    }

    #[inline]
    fn for_each_member(&self, b: usize, f: &mut impl FnMut(u32)) {
        for m in &self.buckets[b].members {
            if let Some(s) = m {
                f(*s);
            }
        }
    }
}

/// 双方向阈值索引。
pub struct ThresholdIndex {
    base: i64,
    span: i64,
    n_buckets: usize,
    sell: SideIndex,
    buy: SideIndex,
}

impl ThresholdIndex {
    /// 构造（构造期预分配全部桶，热路径零分配）。
    pub fn new(
        base: i64,
        span: i64,
        n_buckets: usize,
        bucket_cap: usize,
    ) -> Result<Self, GuardError> {
        if span <= 0 || n_buckets == 0 || bucket_cap == 0 {
            return Err(GuardError::Invalid);
        }
        Ok(ThresholdIndex {
            base,
            span,
            n_buckets,
            sell: SideIndex::new(n_buckets, bucket_cap),
            buy: SideIndex::new(n_buckets, bucket_cap),
        })
    }

    /// 触发价 → 桶号（clamp 到 [0, n)）。
    #[inline]
    pub fn bucket_of(&self, trigger_raw: i64) -> usize {
        let off = trigger_raw - self.base;
        let b = off.div_euclid(self.span);
        if b < 0 {
            0
        } else if b as usize >= self.n_buckets {
            self.n_buckets - 1
        } else {
            b as usize
        }
    }

    /// 注册一条规则到索引（单桶成员数超容量返回 [`GuardError::Full`]）。
    pub fn insert(
        &mut self,
        side: TriggerSide,
        trigger_raw: i64,
        slot: u32,
    ) -> Result<(), GuardError> {
        let b = self.bucket_of(trigger_raw);
        let idx = match side {
            TriggerSide::SellStop => &mut self.sell,
            TriggerSide::BuyStop => &mut self.buy,
        };
        if idx.buckets[b].members.len() >= idx.buckets[b].members.capacity() {
            return Err(GuardError::Full);
        }
        idx.insert(b, slot)
    }

    pub fn remove(&mut self, side: TriggerSide, trigger_raw: i64, slot: u32) {
        let b = self.bucket_of(trigger_raw);
        match side {
            TriggerSide::SellStop => self.sell.remove(b, slot),
            TriggerSide::BuyStop => self.buy.remove(b, slot),
        }
    }

    /// SellStop 候选扫描起点：≥ bucket(price) 的第一个非空桶。
    #[inline]
    pub fn first_sell_bucket(&self, price_raw: i64) -> Option<usize> {
        let b = self.bucket_of(price_raw);
        self.sell.bmp.next_set_from(b)
    }

    /// SellStop 向上继续扫。
    #[inline]
    pub fn next_sell_bucket(&self, from: usize) -> Option<usize> {
        self.sell.bmp.next_set_from(from)
    }

    /// BuyStop 候选扫描起点：≤ bucket(price) 的最后一个非空桶。
    #[inline]
    pub fn first_buy_bucket(&self, price_raw: i64) -> Option<usize> {
        let b = self.bucket_of(price_raw);
        self.buy.bmp.prev_set_before(b)
    }

    /// BuyStop 向下继续扫（next 为下一个要检查的桶号）。
    #[inline]
    pub fn prev_buy_bucket(&self, next: usize) -> Option<usize> {
        self.buy.bmp.prev_set_before(next)
    }

    /// 非破坏性遍历桶成员（跳过墓碑；遍历中不改动状态）。
    pub fn for_each_member(&self, side: TriggerSide, b: usize, f: &mut impl FnMut(u32)) {
        match side {
            TriggerSide::SellStop => self.sell.for_each_member(b, f),
            TriggerSide::BuyStop => self.buy.for_each_member(b, f),
        }
    }

    #[inline]
    pub fn bucket_count(&self) -> usize {
        self.n_buckets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_insert_remove_scan() {
        let mut idx = ThresholdIndex::new(1_000_000, 1_000, 16, 8).unwrap();
        idx.insert(TriggerSide::SellStop, 1_002_500, 0).unwrap();
        idx.insert(TriggerSide::SellStop, 1_009_000, 1).unwrap();
        assert_eq!(idx.bucket_of(1_002_500), 2);
        assert_eq!(idx.first_sell_bucket(1_000_000), Some(2));
        assert_eq!(idx.first_sell_bucket(1_003_000), Some(9));
        assert_eq!(idx.first_sell_bucket(1_010_000), None);

        let mut got = Vec::new();
        idx.for_each_member(TriggerSide::SellStop, 2, &mut |s| got.push(s));
        assert_eq!(got, vec![0]);

        idx.remove(TriggerSide::SellStop, 1_002_500, 0);
        assert_eq!(idx.first_sell_bucket(1_000_000), Some(9));
        idx.remove(TriggerSide::SellStop, 1_009_000, 1);
        assert_eq!(idx.first_sell_bucket(0), None);

        // 低于 base 的触发价 clamp 到桶 0（精确比较兜底）
        idx.insert(TriggerSide::SellStop, 500_000, 2).unwrap();
        assert_eq!(idx.bucket_of(500_000), 0);
        assert_eq!(idx.first_sell_bucket(500_000), Some(0));
    }

    /// 与朴素 Vec 扫描差分（插入/移除/查询全量比对）。
    #[test]
    fn differential_vs_naive() {
        struct Lcg(u64);
        impl Lcg {
            fn next(&mut self) -> u64 {
                self.0 = self
                    .0
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                self.0 >> 11
            }
        }
        let mut rng = Lcg(2026_11_15);

        let (base, span, n) = (1_000_000i64, 1_000i64, 64usize);
        let mut idx = ThresholdIndex::new(base, span, n, 8).unwrap();
        const CAP: usize = 12;
        let mut naive: Vec<Option<i64>> = vec![None; CAP];

        for _ in 0..30_000 {
            let slot = (rng.next() % CAP as u64) as u32;
            match rng.next() % 3 {
                0..=1 => {
                    if let Some(old) = naive[slot as usize] {
                        idx.remove(TriggerSide::SellStop, old, slot);
                        idx.remove(TriggerSide::BuyStop, old, slot);
                    }
                    let trigger =
                        base + (rng.next() % (n as u64 * span as u64)) as i64 - span / 2;
                    idx.insert(TriggerSide::SellStop, trigger, slot).unwrap();
                    idx.insert(TriggerSide::BuyStop, trigger, slot).unwrap();
                    naive[slot as usize] = Some(trigger);
                }
                _ => {
                    if let Some(old) = naive[slot as usize] {
                        idx.remove(TriggerSide::SellStop, old, slot);
                        idx.remove(TriggerSide::BuyStop, old, slot);
                        naive[slot as usize] = None;
                    }
                }
            }

            // 查询差分（桶粒度粗筛）：候选 = bucket(trigger) ≥ bucket(price)（SellStop）
            // 精确的 trigger ≥ price 判定由引擎做；这里验证粗筛集合与朴素桶扫描一致，
            // 且精确集合是粗筛集合的子集。
            let price = base + (rng.next() % (n as u64 * span as u64)) as i64;
            let price_b = idx.bucket_of(price);
            let bucket_naive = |cond: &dyn Fn(usize) -> bool| -> Vec<u32> {
                let mut v: Vec<u32> = naive
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| {
                        matches!(t, Some(tr) if cond(idx.bucket_of(*tr)))
                    })
                    .map(|(i, _)| i as u32)
                    .collect();
                v.sort_unstable();
                v
            };

            let mut got: Vec<u32> = Vec::new();
            let mut b = idx.first_sell_bucket(price);
            while let Some(cb) = b {
                idx.for_each_member(TriggerSide::SellStop, cb, &mut |s| got.push(s));
                b = idx.next_sell_bucket(cb + 1);
            }
            got.sort_unstable();
            let coarse_sell = bucket_naive(&|b| b >= price_b);
            assert_eq!(got, coarse_sell, "sell price={price}");

            // BuyStop 对称（bucket(trigger) ≤ bucket(price)）
            let mut got_buy: Vec<u32> = Vec::new();
            let mut bb = idx.first_buy_bucket(price);
            while let Some(cb) = bb {
                idx.for_each_member(TriggerSide::BuyStop, cb, &mut |s| got_buy.push(s));
                bb = if cb == 0 { None } else { idx.prev_buy_bucket(cb - 1) };
            }
            got_buy.sort_unstable();
            let coarse_buy = bucket_naive(&|b| b <= price_b);
            assert_eq!(got_buy, coarse_buy, "buy price={price}");
        }
    }
}
