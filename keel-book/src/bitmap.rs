//! 分层位图：价格档位的非空标记。
//!
//! - `set`/`clear`：O(log layers) 向上传播；
//! - `next_set_from`：自上而下爬层定位下一个非空档位（最优卖价/最高买价遍历）；
//! - `prev_set_before`：对称的向前定位。
//!
//! 构造期一次性分配（非热路径）；查询/更新全程零分配、零 panic。

/// 单层位图数组 + 向上聚合层。
pub struct LevelBitmap {
    layers: Vec<Box<[u64]>>,
}

impl LevelBitmap {
    /// 构造覆盖 `levels` 个档位的分层位图（构造期分配）。
    pub fn new(levels: usize) -> Self {
        assert!(levels >= 1);
        let mut layers = Vec::new();
        let mut n = levels;
        loop {
            let words = n.div_ceil(64);
            layers.push(vec![0u64; words].into_boxed_slice());
            if words == 1 {
                break;
            }
            n = words;
        }
        LevelBitmap { layers }
    }

    /// 叶层总 bit 数（≥ levels）。
    #[inline]
    pub fn total_bits(&self) -> usize {
        self.layers[0].len() * 64
    }

    #[inline]
    fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// 置位并向所有上层传播。
    pub fn set(&mut self, idx: usize) {
        debug_assert!(idx < self.total_bits());
        let mut idx = idx;
        for layer in &mut self.layers {
            let (w, b) = (idx / 64, idx % 64);
            layer[w] |= 1u64 << b;
            idx /= 64;
        }
    }

    /// 清位：仅当所在 word 变空时向上层传播（否则祖先位仍被其它子位支撑）。
    pub fn clear(&mut self, idx: usize) {
        debug_assert!(idx < self.total_bits());
        let mut idx = idx;
        for layer in &mut self.layers {
            let (w, b) = (idx / 64, idx % 64);
            layer[w] &= !(1u64 << b);
            if layer[w] != 0 {
                break;
            }
            idx /= 64;
        }
    }

    /// 第一个置位且 ≥ `start` 的下标。
    pub fn next_set_from(&self, start: usize) -> Option<usize> {
        let top = self.layer_count();
        let total = self.total_bits();
        if start >= total {
            return None;
        }
        // 叶层 word 内快速路径
        let (w0, b0) = (start / 64, start % 64);
        let word = self.layers[0][w0] & ((!0u64) << b0);
        if word != 0 {
            return Some(w0 * 64 + word.trailing_zeros() as usize);
        }
        // 逐层向上找下一个非空 leaf word；`idx` 为当前层的 bit 下标
        let mut li = 1usize;
        let mut idx = w0 + 1; // leaf word 空间
        loop {
            if li >= top {
                return None;
            }
            let layer = &self.layers[li];
            let nbits = layer.len() * 64;
            if idx >= nbits {
                return None;
            }
            let (w, b) = (idx / 64, idx % 64);
            let word = layer[w] & ((!0u64) << b);
            if word == 0 {
                // 本 word 之后才有候选 → 上层从 w+1 继续
                idx = w + 1;
                li += 1;
                continue;
            }
            let pos = w * 64 + word.trailing_zeros() as usize;
            if li == 1 {
                // pos 即 leaf word 下标
                let lw = self.layers[0][pos];
                debug_assert!(lw != 0);
                return Some(pos * 64 + lw.trailing_zeros() as usize);
            }
            idx = pos * 64;
            li -= 1;
        }
    }

    /// 最后一个置位且 < `end` 的下标。
    pub fn prev_set_before(&self, end: usize) -> Option<usize> {
        let top = self.layer_count();
        if top == 0 || end == 0 {
            return None;
        }
        let total = self.total_bits();
        let end = end.min(total);
        if end == 0 {
            return None;
        }
        // 叶层 word 内快速路径
        let (w0, b0) = ((end - 1) / 64, (end - 1) % 64);
        let mask = if b0 == 63 { !0u64 } else { (!0u64) >> (63 - b0) };
        let word = self.layers[0][w0] & mask;
        if word != 0 {
            return Some(w0 * 64 + (63 - word.leading_zeros() as usize));
        }
        // 逐层向上找前一个非空 leaf word；`end_idx` 为当前层的排他上界
        let mut li = 1usize;
        let mut end_idx = w0; // leaf word 空间：找 leaf word index < w0
        loop {
            if end_idx == 0 || li >= top {
                return None;
            }
            let layer = &self.layers[li];
            let nbits = layer.len() * 64;
            if end_idx > nbits {
                end_idx = nbits;
            }
            let (w, b) = ((end_idx - 1) / 64, (end_idx - 1) % 64);
            let mask = if b == 63 { !0u64 } else { (!0u64) >> (63 - b) };
            let word = layer[w] & mask;
            if word == 0 {
                if w == 0 {
                    return None;
                }
                // 剩余候选 < w*64 → 上层上界映射为 w
                end_idx = w;
                li += 1;
                continue;
            }
            let pos = w * 64 + (63 - word.leading_zeros() as usize);
            if li == 1 {
                let lw = self.layers[0][pos];
                debug_assert!(lw != 0);
                return Some(pos * 64 + (63 - lw.leading_zeros() as usize));
            }
            end_idx = (pos + 1) * 64;
            li -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性伪随机。
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

    /// 与朴素模型（Vec<bool>）做穷举差分：随机 set/clear 后，
    /// 对每个查询点比较 next/prev 结果。
    #[test]
    fn differential_against_naive_model() {
        const N: usize = 10_000; // 3 层位图（157 words → 3 words → 1 word）
        let mut bmp = LevelBitmap::new(N);
        let mut model = vec![false; N];
        let mut rng = Lcg(42);

        for round in 0..2000 {
            let idx = (rng.next() as usize) % N;
            if rng.next() & 1 == 0 {
                bmp.set(idx);
                model[idx] = true;
            } else {
                bmp.clear(idx);
                model[idx] = false;
            }

            // 抽查若干查询点（确定性步进）
            for q in (round % N..N).step_by(97) {
                let expect_next = model[q..].iter().position(|b| *b).map(|p| q + p);
                assert_eq!(bmp.next_set_from(q), expect_next, "round={round} q={q}");
                let expect_prev = (0..q).rev().find(|i| model[*i]);
                assert_eq!(bmp.prev_set_before(q), expect_prev, "round={round} q={q}");
            }
        }
    }

    #[test]
    fn set_clear_single_bit() {
        let mut bmp = LevelBitmap::new(128);
        assert_eq!(bmp.next_set_from(0), None);
        bmp.set(65);
        assert_eq!(bmp.next_set_from(0), Some(65));
        assert_eq!(bmp.next_set_from(65), Some(65));
        assert_eq!(bmp.next_set_from(66), None);
        assert_eq!(bmp.prev_set_before(66), Some(65));
        assert_eq!(bmp.prev_set_before(65), None);
        bmp.clear(65);
        assert_eq!(bmp.next_set_from(0), None);
        assert_eq!(bmp.prev_set_before(128), None);
    }

    #[test]
    fn multi_layer_navigation() {
        // 跨 word、跨层的连续定位
        let mut bmp = LevelBitmap::new(10_000);
        for idx in [0, 63, 64, 4095, 4096, 9999] {
            bmp.set(idx);
        }
        let mut cur = 0;
        let mut found = Vec::new();
        while let Some(n) = bmp.next_set_from(cur) {
            found.push(n);
            cur = n + 1;
        }
        assert_eq!(found, vec![0, 63, 64, 4095, 4096, 9999]);

        // 反向
        let mut end = 10_000;
        let mut rev = Vec::new();
        while let Some(p) = bmp.prev_set_before(end) {
            rev.push(p);
            end = p;
        }
        assert_eq!(rev, vec![9999, 4096, 4095, 64, 63, 0]);
    }

    #[test]
    fn single_level_book_bitmap() {
        let mut bmp = LevelBitmap::new(1);
        bmp.set(0);
        assert_eq!(bmp.next_set_from(0), Some(0));
        bmp.clear(0);
        assert_eq!(bmp.next_set_from(0), None);
    }
}
