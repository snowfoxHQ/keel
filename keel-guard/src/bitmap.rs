//! 桶位图：keel-guard 阈值索引的定位层（docs/BASELINE.md §8）。
//!
//! 语义与 keel-book 分层位图一致，但这里是单层（桶数固定，word 数组直接覆盖）：
//! set/clear 向上传播 word、next_set_from / prev_set_before 定位下一个非空桶。
//! 确定性：纯整数位运算，无容器内部顺序依赖。

/// 固定容量位图（word = u64，64 桶/word）。
pub struct BitMap {
    words: Vec<u64>,
    /// 非空桶计数。
    pub count: u32,
}

impl BitMap {
    pub fn new(buckets: usize) -> Self {
        BitMap {
            words: vec![0u64; buckets.div_ceil(64)],
            count: 0,
        }
    }

    #[inline]
    pub fn set(&mut self, idx: usize) {
        let (w, b) = (idx / 64, idx % 64);
        if self.words[w] & (1u64 << b) == 0 {
            self.words[w] |= 1u64 << b;
            self.count += 1;
        }
    }

    #[inline]
    pub fn clear(&mut self, idx: usize) {
        let (w, b) = (idx / 64, idx % 64);
        if self.words[w] & (1u64 << b) != 0 {
            self.words[w] &= !(1u64 << b);
            self.count -= 1;
        }
    }

    #[inline]
    pub fn is_set(&self, idx: usize) -> bool {
        self.words[idx / 64] & (1u64 << (idx % 64)) != 0
    }

    /// ≥ from 的最小非空桶。
    pub fn next_set_from(&self, from: usize) -> Option<usize> {
        if from >= self.words.len() * 64 {
            return None;
        }
        let (w, b) = (from / 64, from % 64);
        // 首个 word：抹掉低位
        let masked = self.words[w] & (!0u64 << b);
        if masked != 0 {
            return Some(w * 64 + masked.trailing_zeros() as usize);
        }
        for wi in (w + 1)..self.words.len() {
            if self.words[wi] != 0 {
                return Some(wi * 64 + self.words[wi].trailing_zeros() as usize);
            }
        }
        None
    }

    /// ≤ to 的最大非空桶（**包含 to 本身**——同桶规则必须能被找到）。
    pub fn prev_set_before(&self, to: usize) -> Option<usize> {
        if to >= self.words.len() * 64 {
            return None;
        }
        let (w, b) = (to / 64, to % 64);
        let mask: u64 = if b == 63 { u64::MAX } else { (1u64 << (b + 1)) - 1 };
        let masked = self.words[w] & mask;
        if masked != 0 {
            return Some(w * 64 + 63 - masked.leading_zeros() as usize);
        }
        for wi in (0..w).rev() {
            if self.words[wi] != 0 {
                return Some(wi * 64 + 63 - self.words[wi].leading_zeros() as usize);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_clear_next_prev() {
        let mut m = BitMap::new(256);
        assert_eq!(m.next_set_from(0), None);
        m.set(7);
        m.set(130);
        m.set(255);
        assert_eq!(m.count, 3);
        assert_eq!(m.next_set_from(0), Some(7));
        assert_eq!(m.next_set_from(8), Some(130));
        assert_eq!(m.next_set_from(131), Some(255));
        assert_eq!(m.next_set_from(256), None);
        assert_eq!(m.prev_set_before(255), Some(255)); // 包含语义：≤ to
        assert_eq!(m.prev_set_before(254), Some(130));
        assert_eq!(m.prev_set_before(130), Some(130));
        assert_eq!(m.prev_set_before(129), Some(7));
        assert_eq!(m.prev_set_before(6), None);
        m.clear(130);
        assert_eq!(m.next_set_from(8), Some(255));
        assert_eq!(m.count, 2);
    }

    /// 与朴素 Vec<bool> 模型随机差分。
    #[test]
    fn differential_vs_naive() {
        struct Lcg(u64);
        impl Lcg {
            fn next(&mut self) -> u64 {
                self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                self.0 >> 11
            }
        }
        let mut rng = Lcg(2026_11_15);
        let n = 512;
        let mut m = BitMap::new(n);
        let mut naive = vec![false; n];
        for _ in 0..20_000 {
            let i = (rng.next() % n as u64) as usize;
            if rng.next() & 1 == 0 {
                m.set(i);
                naive[i] = true;
            } else {
                m.clear(i);
                naive[i] = false;
            }
            // 全量比对
            let mut expect: Vec<usize> = (0..n).filter(|&k| naive[k]).collect();
            expect.sort_unstable();
            let got: Vec<usize> = (0..n).filter(|&k| m.is_set(k)).collect();
            assert_eq!(got, expect);
            // 定位函数抽查
            for from in [0usize, 1, 63, 64, 65, 255, 511] {
                let expect_next = expect.iter().find(|&&k| k >= from && k < n).copied();
                assert_eq!(m.next_set_from(from), expect_next, "from={from}");
                let expect_prev = expect.iter().rev().find(|&&k| k <= from).copied();
                assert_eq!(m.prev_set_before(from.min(n - 1)), expect_prev, "to={from}");
            }
        }
    }
}
