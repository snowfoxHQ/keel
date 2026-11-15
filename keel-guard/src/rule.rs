//! 条件单规则与槽位池（docs/BASELINE.md §8.1）。

use keel_core::{Price, Qty};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardError {
    /// 规则池或桶容量耗尽（不扩容，§3.1）。
    Full,
    /// 规则不存在或已被触发/撤销（句柄失效）。
    RuleGone,
    /// 触发价超出索引桶范围。
    OutOfRange,
    /// 定点溢出。
    Overflow,
    /// 非法参数（如 trail 为负）。
    Invalid,
}

/// 触发方向：SellStop 保护多头（价格跌破触发价时卖出）；BuyStop 保护空头。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerSide {
    SellStop,
    BuyStop,
}

/// 规则类型：固定触发价 / 跟踪止盈（极值随行情就地更新，§8.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleKind {
    Fixed,
    Trailing,
}

/// 一条条件单规则。
#[derive(Clone, Copy, Debug)]
pub struct Rule {
    /// 策略侧规则 ID（原样透传）。
    pub id: u64,
    /// 注册全序号（引擎分配，单调递增；§8.1 同持仓多规则按 (ts, seq) 排序）。
    pub seq: u64,
    pub ts: i64,
    pub side: TriggerSide,
    pub kind: RuleKind,
    /// 当前触发价（raw；Trailing 时随极值移动）。
    pub trigger_raw: i64,
    /// 跟踪极值（raw；SellStop 记最高价、BuyStop 记最低价）。
    pub extreme_raw: i64,
    /// 跟踪步长（raw 定点偏移；§8.2 全定点）。
    pub trail_raw: i64,
    /// 绑定的未平仓数量（raw；§8.1 每次 FillEvent 后重算）。
    pub bound_qty_raw: i128,
}

/// 注册参数。
#[derive(Clone, Copy, Debug)]
pub struct RuleSpec {
    pub id: u64,
    pub ts: i64,
    pub side: TriggerSide,
    pub kind: RuleKind,
    /// 初始触发价（Fixed）；Trailing 忽略（由 extreme − trail 推导）。
    pub trigger: Price,
    /// 跟踪初始极值（Trailing 必填）。
    pub extreme: Price,
    /// 跟踪步长（Trailing 必填）。
    pub trail: Qty,
    /// 绑定未平仓数量。
    pub bound_qty: Qty,
}

/// 撤销原因（§8.1 CancelAck 语义）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    Manual,
    /// 持仓归零自动撤销。
    PositionClosed,
    /// FillEvent 后重算数量为 0。
    ZeroQty,
}

/// 撤销记录（§8.1：生成撤销记录）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CancelRecord {
    pub id: u64,
    pub seq: u64,
    pub reason: CancelReason,
}

/// 触发输出（§8.1：触发价与成交价分离——`at` 是触发时的市场参考价，
/// 实际成交由撮合层按普通市价单 + §6.1 规则决定，触发价不参与成交定价）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Triggered {
    pub id: u64,
    pub seq: u64,
    pub side: TriggerSide,
    /// 触发时的市场参考价：跳空 = 开盘价；Bar 触及 = 触发价；Tick = 当前价。
    pub at: Price,
}

/// 规则槽位池（固定容量 + free-list；热路径零分配）。
pub struct RulePool {
    slots: Vec<Option<Rule>>,
    free: Vec<u32>,
    live: u32,
}

/// 规则槽位句柄。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuleHandle(pub u32);

impl RulePool {
    pub fn with_capacity(cap: usize) -> Self {
        let mut free = Vec::with_capacity(cap);
        for i in (0..cap).rev() {
            free.push(i as u32);
        }
        RulePool {
            slots: vec![None; cap],
            free,
            live: 0,
        }
    }

    #[inline]
    pub fn live(&self) -> u32 {
        self.live
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub fn alloc(&mut self, rule: Rule) -> Result<RuleHandle, GuardError> {
        match self.free.pop() {
            Some(s) => {
                self.slots[s as usize] = Some(rule);
                self.live += 1;
                Ok(RuleHandle(s))
            }
            None => Err(GuardError::Full),
        }
    }

    #[inline]
    pub fn get(&self, h: RuleHandle) -> Option<&Rule> {
        self.slots.get(h.0 as usize).and_then(|s| s.as_ref())
    }

    #[inline]
    pub fn get_mut(&mut self, h: RuleHandle) -> Option<&mut Rule> {
        self.slots.get_mut(h.0 as usize).and_then(|s| s.as_mut())
    }

    /// 释放槽位（触发/撤销后调用；句柄只在单次注册生命周期内有效）。
    pub fn free_slot(&mut self, h: RuleHandle) -> Result<(), GuardError> {
        let s = h.0 as usize;
        if s >= self.slots.len() || self.slots[s].is_none() {
            return Err(GuardError::RuleGone);
        }
        self.slots[s] = None;
        self.free.push(h.0);
        self.live -= 1;
        Ok(())
    }

    /// 存活槽位迭代（确定性：槽位序）。
    pub fn iter(&self) -> impl Iterator<Item = (RuleHandle, &Rule)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|r| (RuleHandle(i as u32), r)))
    }
}
