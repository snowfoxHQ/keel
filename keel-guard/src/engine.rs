//! GuardEngine：条件单生命周期（docs/BASELINE.md §8）。
//!
//! 语义要点（§8.1）：
//! - 触发价与成交价分离：`Triggered.at` 只是触发时的市场参考价（跳空 = 开盘价、
//!   Bar 触及 = 触发价、Tick = 当前价），实际成交由撮合层按普通市价单决定；
//! - 跳空穿越按开盘价（不利原则），触发价不参与成交定价；
//! - 跟踪止盈极值随 MarketDataEvent 就地更新，定点存储；
//! - Bar 保守路径（§6.1）：SellStop 先判 low（多单逆势在前）再吸收 high 更新极值；
//!   BuyStop 先判 high 再吸收 low；
//! - 触发输出按注册 (ts, seq) 全序排序（同持仓多规则）；
//! - 绑定数量每次 FillEvent 重算，归零即撤销；持仓归零全量自动撤销。

use super::index::ThresholdIndex;
use super::rule::{
    CancelRecord, CancelReason, GuardError, Rule, RuleHandle, RuleKind, RulePool, RuleSpec,
    TriggerSide, Triggered,
};
use keel_core::{Price, Qty};

/// Guard 引擎配置。
#[derive(Clone, Copy, Debug)]
pub struct GuardConfig {
    /// 索引基准价（raw）。
    pub base_price_raw: i64,
    /// 桶宽（raw）。
    pub bucket_span_raw: i64,
    /// 桶数。
    pub buckets: usize,
    /// 单桶规则数上限（满载拒绝，不扩容）。
    pub bucket_cap: usize,
    /// 规则池容量。
    pub max_rules: usize,
}

pub struct GuardEngine {
    cfg: GuardConfig,
    pool: RulePool,
    index: ThresholdIndex,
    next_seq: u64,
    /// 复用输出缓冲（每次调用 reset）。
    fired: Vec<Triggered>,
    cancels: Vec<CancelRecord>,
    /// 桶扫描候选暂存（复用）。
    cand: Vec<u32>,
    /// 池遍历暂存（复用；热路径禁止分配）。
    tmp: Vec<u32>,
}

impl GuardEngine {
    pub fn new(cfg: GuardConfig) -> Result<Self, GuardError> {
        Ok(GuardEngine {
            pool: RulePool::with_capacity(cfg.max_rules),
            index: ThresholdIndex::new(
                cfg.base_price_raw,
                cfg.bucket_span_raw,
                cfg.buckets,
                cfg.bucket_cap,
            )?,
            cfg,
            next_seq: 0,
            fired: Vec::with_capacity(cfg.max_rules),
            cancels: Vec::with_capacity(cfg.max_rules),
            cand: Vec::with_capacity(cfg.bucket_cap.max(64)),
            tmp: Vec::with_capacity(cfg.max_rules),
        })
    }

    #[inline]
    pub fn live_rules(&self) -> u32 {
        self.pool.live()
    }

    #[inline]
    pub fn cfg(&self) -> &GuardConfig {
        &self.cfg
    }

    /// 注册条件单。seq 由引擎分配（单调递增，§8.1 注册全序）。
    pub fn register(&mut self, spec: RuleSpec) -> Result<RuleHandle, GuardError> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1); // 绕回可由事实链检测
        let (trigger_raw, extreme_raw, trail_raw) = match spec.kind {
            RuleKind::Fixed => (spec.trigger.raw(), 0, 0),
            RuleKind::Trailing => {
                if spec.trail.raw() <= 0 {
                    return Err(GuardError::Invalid);
                }
                let t = spec.extreme.raw() as i128;
                let tr = spec.trail.raw() as i128;
                let trig = match spec.side {
                    TriggerSide::SellStop => t - tr, // 触发价 = 极值(高) − 步长
                    TriggerSide::BuyStop => t + tr,  // 触发价 = 极值(低) + 步长
                };
                (clamp_i64(trig)?, spec.extreme.raw(), spec.trail.raw())
            }
        };
        let rule = Rule {
            id: spec.id,
            seq,
            ts: spec.ts,
            side: spec.side,
            kind: spec.kind,
            trigger_raw,
            extreme_raw,
            trail_raw,
            bound_qty_raw: spec.bound_qty.raw() as i128,
        };
        let h = self.pool.alloc(rule)?;
        // 索引插入失败则回滚槽位（§3.1 满载语义：整体拒绝，不留半注册态）
        if let Err(e) = self.index.insert(spec.side, rule.trigger_raw, h.0) {
            let _ = self.pool.free_slot(h);
            return Err(e);
        }
        Ok(h)
    }

    /// 手动撤销。
    pub fn cancel(&mut self, h: RuleHandle) -> Result<CancelRecord, GuardError> {
        let r = self.pool.get(h).ok_or(GuardError::RuleGone)?;
        let (id, seq, side, trigger) = (r.id, r.seq, r.side, r.trigger_raw);
        self.index.remove(side, trigger, h.0);
        self.pool.free_slot(h)?;
        self.cancels.clear();
        self.cancels.push(CancelRecord {
            id,
            seq,
            reason: CancelReason::Manual,
        });
        Ok(self.cancels[0])
    }

    /// Tick/价格点驱动触发（MarketDataEvent(Tick) 路径）。
    /// 先更新跟踪极值并重排桶，再按位图候选 + 精确整数比较触发。
    pub fn on_price(&mut self, price: Price) -> Result<&[Triggered], GuardError> {
        let p = price.raw();
        self.update_trailing(p)?;
        self.fired.clear();
        self.scan_side(TriggerSide::SellStop, p, |trigger| {
            if p <= trigger {
                Some(p) // Tick 路径：at = 当前价（跳空亦然，不利原则）
            } else {
                None
            }
        });
        self.scan_side(TriggerSide::BuyStop, p, |trigger| {
            if p >= trigger {
                Some(p)
            } else {
                None
            }
        });
        self.sort_fired();
        Ok(&self.fired)
    }

    /// Bar 驱动触发（MarketDataEvent(Bar) 路径，§8.1 跳空 + §6.1 保守路径）。
    ///
    /// 顺序：先用**当前**触发价判定（跳空/触及），再吸收本 Bar 极值更新跟踪触发价——
    /// 多单保守路径 O→L→H→C 中 low 在 high 之前，极值吸收必须在判定之后。
    pub fn on_bar(
        &mut self,
        open: Price,
        high: Price,
        low: Price,
    ) -> Result<&[Triggered], GuardError> {
        self.fired.clear();
        let (o, h, l) = (open.raw(), high.raw(), low.raw());

        // 1) SellStop：low 触及即触发；跳空（开盘劣于触发价）按开盘价（不利原则）
        self.scan_side(TriggerSide::SellStop, l, |trigger| {
            if l <= trigger {
                Some(if o <= trigger { o } else { trigger }) // at = min(open, trigger)
            } else {
                None
            }
        });
        // BuyStop：high 触及即触发；跳空按开盘价
        self.scan_side(TriggerSide::BuyStop, h, |trigger| {
            if h >= trigger {
                Some(if o >= trigger { o } else { trigger }) // at = max(open, trigger)
            } else {
                None
            }
        });
        self.sort_fired();

        // 2) 吸收本 Bar 极值（判定之后，§6.1 保守路径）
        self.update_trailing_extremes(h, l)?;

        Ok(&self.fired)
    }

    /// FillEvent 后重算绑定数量（§8.1：绑定持仓状态机未平仓数量）。
    /// 持仓数量为 0 的规则自动撤销（ZeroQty 记录）。
    pub fn on_fill(&mut self, position_qty: Qty) -> Result<&[CancelRecord], GuardError> {
        let q = position_qty.raw() as i128;
        self.cancels.clear();
        for si in 0..self.pool.capacity() {
            let h = RuleHandle(si as u32);
            let gone = q == 0; // 数量为 0 即撤销（§8.1）
            let Some(r) = self.pool.get(h) else {
                continue;
            };
            let (id, seq, side, trigger) = (r.id, r.seq, r.side, r.trigger_raw);
            self.pool.get_mut(h).unwrap().bound_qty_raw = q;
            if gone {
                self.index.remove(side, trigger, h.0);
                self.pool.free_slot(h)?;
                self.cancels.push(CancelRecord {
                    id,
                    seq,
                    reason: CancelReason::ZeroQty,
                });
            }
        }
        Ok(&self.cancels)
    }

    /// 持仓归零：绑定该持仓的全部 GuardRule 自动撤销（§8.1 CancelAck 语义）。
    pub fn on_position_closed(&mut self) -> Result<&[CancelRecord], GuardError> {
        self.cancels.clear();
        for si in 0..self.pool.capacity() {
            let h = RuleHandle(si as u32);
            let Some(r) = self.pool.get(h) else {
                continue;
            };
            let (id, seq, side, trigger) = (r.id, r.seq, r.side, r.trigger_raw);
            self.index.remove(side, trigger, h.0);
            self.pool.free_slot(h)?;
            self.cancels.push(CancelRecord {
                id,
                seq,
                reason: CancelReason::PositionClosed,
            });
        }
        Ok(&self.cancels)
    }

    // ---- 内部 ----

    /// 跟踪止盈极值更新 + 触发价重排（Tick 路径）。
    /// 按下标遍历槽位（free 只置 None 不移动内存，无需快照、零分配）。
    fn update_trailing(&mut self, p: i64) -> Result<(), GuardError> {
        for si in 0..self.pool.capacity() {
            let h = RuleHandle(si as u32);
            // 读写分离：块内读取，块外变更
            let (old_trigger, t128, side) = {
                let r = match self.pool.get(h) {
                    Some(r) => r,
                    None => continue,
                };
                if r.kind != RuleKind::Trailing {
                    continue;
                }
                let better = match r.side {
                    TriggerSide::SellStop => p > r.extreme_raw,
                    TriggerSide::BuyStop => p < r.extreme_raw,
                };
                if !better {
                    continue;
                }
                let t128 = match r.side {
                    TriggerSide::SellStop => p as i128 - r.trail_raw as i128,
                    TriggerSide::BuyStop => p as i128 + r.trail_raw as i128,
                };
                (r.trigger_raw, t128, r.side)
            };
            let new_trigger = clamp_i64(t128)?;
            {
                let r = self.pool.get_mut(h).unwrap();
                r.extreme_raw = p;
                r.trigger_raw = new_trigger;
            }
            self.rebucket(side, old_trigger, new_trigger, h)?;
        }
        Ok(())
    }

    /// Bar 路径专用：只吸收极值（判定已在前面完成）。
    fn update_trailing_extremes(&mut self, high: i64, low: i64) -> Result<(), GuardError> {
        for si in 0..self.pool.capacity() {
            let h = RuleHandle(si as u32);
            let (old_trigger, t128, side, extreme) = {
                let r = match self.pool.get(h) {
                    Some(r) => r,
                    None => continue,
                };
                if r.kind != RuleKind::Trailing {
                    continue;
                }
                let (better, extreme) = match r.side {
                    TriggerSide::SellStop => (high > r.extreme_raw, high),
                    TriggerSide::BuyStop => (low < r.extreme_raw, low),
                };
                if !better {
                    continue;
                }
                let t128 = match r.side {
                    TriggerSide::SellStop => extreme as i128 - r.trail_raw as i128,
                    TriggerSide::BuyStop => extreme as i128 + r.trail_raw as i128,
                };
                (r.trigger_raw, t128, r.side, extreme)
            };
            let new_trigger = clamp_i64(t128)?;
            {
                let r = self.pool.get_mut(h).unwrap();
                r.extreme_raw = extreme;
                r.trigger_raw = new_trigger;
            }
            self.rebucket(side, old_trigger, new_trigger, h)?;
        }
        Ok(())
    }

    /// 触发价跨桶则重排索引。
    #[inline]
    fn rebucket(
        &mut self,
        side: TriggerSide,
        old_trigger: i64,
        new_trigger: i64,
        h: RuleHandle,
    ) -> Result<(), GuardError> {
        let old_b = self.index.bucket_of(old_trigger);
        let new_b = self.index.bucket_of(new_trigger);
        if old_b != new_b {
            self.index.remove(side, old_trigger, h.0);
            self.index.insert(side, new_trigger, h.0)?;
        }
        Ok(())
    }

    /// 单侧候选收集（位图粗筛）+ 精确整数判定 + 触发。
    /// `at_fn`：由规则触发价决定触发参考价（Tick = 当前价；Bar = min/max(open, trigger)）。
    fn scan_side(&mut self, side: TriggerSide, probe: i64, at_fn: impl Fn(i64) -> Option<i64>) {
        let mut b = match side {
            TriggerSide::SellStop => self.index.first_sell_bucket(probe),
            TriggerSide::BuyStop => self.index.first_buy_bucket(probe),
        };
        while let Some(cb) = b {
            self.cand.clear();
            self.index.for_each_member(side, cb, &mut |s| self.cand.push(s));
            for s in self.cand.iter().copied() {
                let h = RuleHandle(s);
                let Some(r) = self.pool.get(h) else {
                    continue;
                };
                if r.side != side {
                    continue;
                }
                if let Some(at) = at_fn(r.trigger_raw) {
                    self.fired.push(Triggered {
                        id: r.id,
                        seq: r.seq,
                        side: r.side,
                        at: Price::from_raw(at),
                    });
                    self.index.remove(side, r.trigger_raw, s);
                    let _ = self.pool.free_slot(h);
                }
            }
            b = match side {
                TriggerSide::SellStop => self.index.next_sell_bucket(cb + 1),
                TriggerSide::BuyStop => {
                    if cb == 0 {
                        None
                    } else {
                        self.index.prev_buy_bucket(cb - 1)
                    }
                }
            };
        }
    }

    /// 触发输出按注册 seq 排序（§8.1：同持仓多规则按注册全序触发）。
    /// 插入排序：n 小、基本有序、确定性、零分配。
    fn sort_fired(&mut self) {
        let v = &mut self.fired;
        for i in 1..v.len() {
            let key = v[i];
            let mut j = i;
            while j > 0 && v[j - 1].seq > key.seq {
                v[j] = v[j - 1];
                j -= 1;
            }
            v[j] = key;
        }
    }
}

/// i128 → i64（越界返回 Overflow；定点契约：不允许静默截断）。
#[inline]
fn clamp_i64(v: i128) -> Result<i64, GuardError> {
    if v > i64::MAX as i128 || v < i64::MIN as i128 {
        return Err(GuardError::Overflow);
    }
    Ok(v as i64)
}
