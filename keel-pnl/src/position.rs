//! 持仓状态机与成本算法（docs/BASELINE.md §9）。
//!
//! - 持仓方向状态机：开仓 / 加仓 / 减仓 / 清仓 / 反手（flip），净持仓有符号；
//! - 成本核算：FIFO 与均价是**两套独立算法**（`FifoCost` / `AverageCost`，批次归属规则不同），
//!   不是同一算法的参数开关（§9）；
//! - 摊分余数规则（§4.3）：部分消费按 trunc 摊分，余数归入最后一笔
//!   （批次清零 / 持仓清零时吃掉全部剩余成本）→ 批次总和精确相等；
//! - 反手拆分规则：一笔 fill 同时平仓并反手时，成交金额按 trunc 拆分给
//!   "平仓部分 / 反手开仓部分"，余数归反手开仓部分（最后一笔），拆分总和精确相等；
//! - 已实现盈亏口径：成交金额 − 消耗成本，**不含费用**（费用单列，§9.1）；
//! - 热路径零分配：FIFO 批次队列固定容量，耗尽返回 [`PnlError::BatchFull`]。

use crate::ledger::{LedgerEntry, Side};
use keel_core::{Money, Price, Qty};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PnlError {
    /// FIFO 批次队列容量耗尽（不扩容）。
    BatchFull,
    /// 定点溢出（i128 checked）。
    Overflow,
    /// 事实顺序错乱（seq 不连续或重复消费）。
    OutOfOrder,
}

/// 持仓方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Long,
    Short,
    Flat,
}

/// 单笔 fill 应用后的状态迁移。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionEvent {
    Opened,
    Increased,
    Reduced,
    Closed,
    Flipped,
}

/// 应用结果（两套成本算法产出同构结果，便于差分对账）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub event: PositionEvent,
    /// 本笔已实现盈亏变动（不含费用）。
    pub realized_delta: Money,
    /// 应用后的净持仓（有符号 raw：>0 多头，<0 空头）。
    pub net_qty: i128,
    pub fee: Money,
    pub cash_delta: Money,
}

/// 成本核算算法接口。两个实现必须满足同一契约：
/// `release_cost` 的累计回收量 == `add_cost` 的累计投入量（持仓清零后 held_cost == 0）。
pub trait CostMethod {
    /// 增加持仓数量与成本。
    fn add_cost(&mut self, qty_raw: i128, gross: Money) -> Result<(), PnlError>;
    /// 消耗指定数量对应的成本；余数按 §4.3 归入剩余部分，清零笔吃掉全部剩余。
    fn release_cost(&mut self, qty_raw: i128) -> Result<Money, PnlError>;
    /// 当前持仓数量（幅值）。
    fn held_qty(&self) -> i128;
    /// 当前持仓总成本。
    fn held_cost(&self) -> Money;
}

/// FIFO 成本算法：独立批次队列，先开先平。
pub struct FifoCost {
    /// 批次 (数量幅值, 成本)；head 为队首游标。
    batches: Vec<(i128, Money)>,
    head: usize,
    held: i128,
    total: Money,
}

impl FifoCost {
    pub fn new(batch_capacity: usize) -> Self {
        FifoCost {
            batches: Vec::with_capacity(batch_capacity),
            head: 0,
            held: 0,
            total: Money::ZERO,
        }
    }
}

impl CostMethod for FifoCost {
    fn add_cost(&mut self, qty_raw: i128, gross: Money) -> Result<(), PnlError> {
        if qty_raw <= 0 {
            return Ok(());
        }
        // 复用队首已耗尽的槽位（压缩复用，物理容量不变）
        if self.head > 0 {
            self.batches.truncate(self.head);
            self.head = 0;
        }
        if self.batches.len() >= self.batches.capacity() {
            return Err(PnlError::BatchFull);
        }
        self.batches.push((qty_raw, gross));
        self.held += qty_raw;
        self.total = self
            .total
            .checked_add(gross)
            .map_err(|_| PnlError::Overflow)?;
        Ok(())
    }

    fn release_cost(&mut self, qty_raw: i128) -> Result<Money, PnlError> {
        debug_assert!(qty_raw >= 0);
        if qty_raw == 0 {
            return Ok(Money::ZERO);
        }
        let mut need = qty_raw;
        let mut released = Money::ZERO;
        while need > 0 {
            let (bq, bcost) = self.batches[self.head];
            let take = if bq <= need { bq } else { need };
            let part = if take == bq {
                // 清零笔：吃掉该批次全部成本（精确相等）
                bcost
            } else {
                // 部分消费：trunc 摊分，余数留在批次内
                trunc_part(bcost, take, bq)
            };
            released = released.checked_add(part).map_err(|_| PnlError::Overflow)?;
            if take == bq {
                self.head += 1;
            } else {
                self.batches[self.head] = (
                    bq - take,
                    bcost
                        .checked_sub(part)
                        .map_err(|_| PnlError::Overflow)?,
                );
            }
            self.total = self.total.checked_sub(part).map_err(|_| PnlError::Overflow)?;
            need -= take;
        }
        self.held -= qty_raw;
        Ok(released)
    }

    fn held_qty(&self) -> i128 {
        self.held
    }

    fn held_cost(&self) -> Money {
        self.total
    }
}

/// 均价成本算法：单一成本池，按持仓比例摊分。
pub struct AverageCost {
    held: i128,
    total: Money,
}

impl AverageCost {
    pub fn new() -> Self {
        AverageCost {
            held: 0,
            total: Money::ZERO,
        }
    }
}

impl Default for AverageCost {
    fn default() -> Self {
        Self::new()
    }
}

impl CostMethod for AverageCost {
    fn add_cost(&mut self, qty_raw: i128, gross: Money) -> Result<(), PnlError> {
        if qty_raw <= 0 {
            return Ok(());
        }
        self.held += qty_raw;
        self.total = self
            .total
            .checked_add(gross)
            .map_err(|_| PnlError::Overflow)?;
        Ok(())
    }

    fn release_cost(&mut self, qty_raw: i128) -> Result<Money, PnlError> {
        if qty_raw == 0 {
            return Ok(Money::ZERO);
        }
        let part = if qty_raw == self.held {
            // 清仓：吃掉全部剩余成本（精确相等）
            self.total
        } else {
            // 部分平仓：trunc 摊分，余数归剩余持仓
            trunc_part(self.total, qty_raw, self.held)
        };
        self.total = self.total.checked_sub(part).map_err(|_| PnlError::Overflow)?;
        self.held -= qty_raw;
        Ok(part)
    }

    fn held_qty(&self) -> i128 {
        self.held
    }

    fn held_cost(&self) -> Money {
        self.total
    }
}

/// trunc 摊分：part = trunc(gross × n / total)。n < total，结果恒 ≤ gross，无溢出路径。
#[inline]
fn trunc_part(gross: Money, n: i128, total: i128) -> Money {
    debug_assert!(n > 0 && total > 0 && n <= total);
    Money::from_raw(gross.raw_i128() * n / total)
}

/// PnL 账户：持仓状态机 + 可插拔成本算法（`FifoCost` / `AverageCost` 各自独立实例化）。
pub struct PnlAccount<M: CostMethod> {
    cost: M,
    /// 净持仓（有符号 raw）。
    net: i128,
    open_qty: i128,
    close_qty: i128,
    realized: Money,
    fees: Money,
    cash: Money,
    /// 累计开仓成本（均价计算用）。
    total_open_cost: Money,
    /// 已消费的事实条数（增量 replay 游标）。
    consumed: usize,
}

/// FIFO 账户。
pub type FifoAccount = PnlAccount<FifoCost>;
/// 均价账户。
pub type AverageAccount = PnlAccount<AverageCost>;

impl<M: CostMethod> PnlAccount<M> {
    pub fn new(cost: M) -> Self {
        PnlAccount {
            cost,
            net: 0,
            open_qty: 0,
            close_qty: 0,
            realized: Money::ZERO,
            fees: Money::ZERO,
            cash: Money::ZERO,
            total_open_cost: Money::ZERO,
            consumed: 0,
        }
    }

    // ---- 状态查询（§9.1 PnlState 必备字段）----

    /// 净持仓（有符号）。
    pub fn net_qty(&self) -> i128 {
        self.net
    }

    pub fn direction(&self) -> Direction {
        match self.net.signum() {
            1 => Direction::Long,
            -1 => Direction::Short,
            _ => Direction::Flat,
        }
    }

    /// 累计开仓数量（幅值）。
    pub fn open_qty(&self) -> i128 {
        self.open_qty
    }

    /// 累计平仓数量（幅值）。
    pub fn close_qty(&self) -> i128 {
        self.close_qty
    }

    /// 已实现盈亏（累计，不含费用）。
    pub fn realized_pnl(&self) -> Money {
        self.realized
    }

    /// 手续费合计。
    pub fn fees(&self) -> Money {
        self.fees
    }

    /// 账户现金（Σ ledger.cash_delta）。
    pub fn cash(&self) -> Money {
        self.cash
    }

    /// 当前持仓总成本。
    pub fn held_cost(&self) -> Money {
        self.cost.held_cost()
    }

    /// 开仓均价（= 累计开仓成本 ÷ 累计开仓数量，Round::Down）。无开仓返回 None。
    pub fn avg_entry_price(&self) -> Option<Price> {
        if self.open_qty == 0 {
            return None;
        }
        let q = Qty::from_raw(self.open_qty as i64);
        let (p, _rem) = self
            .total_open_cost
            .div_qty(q, keel_core::fixed::Round::Down)
            .ok()?;
        Some(p)
        // 余数 _rem 归入统计展示层（§4.3 盈亏统计 trunc），不参与状态机
    }

    /// 未实现盈亏（按标记价；多头 = mark×qty − 持仓成本，空头 = 持仓成本 − mark×|qty|）。
    pub fn unrealized_pnl(&self, mark: Price) -> Result<Money, PnlError> {
        let held = self.cost.held_qty();
        if held == 0 {
            return Ok(Money::ZERO);
        }
        let mark_val = Money::price_qty(
            mark,
            Qty::from_raw(held as i64),
            keel_core::fixed::Round::Down,
        )
        .map_err(|_| PnlError::Overflow)?;
        if self.net > 0 {
            mark_val.checked_sub(self.cost.held_cost()).map_err(|_| PnlError::Overflow)
        } else {
            self.cost.held_cost().checked_sub(mark_val).map_err(|_| PnlError::Overflow)
        }
    }

    /// 已消费的事实条数。
    pub fn consumed(&self) -> usize {
        self.consumed
    }

    // ---- 状态推进 ----

    /// 消费账本新增事实（增量；重复/跳序返回 [`PnlError::OutOfOrder`]）。
    pub fn sync(
        &mut self,
        ledger: &crate::ledger::TradeLedger,
    ) -> Result<Vec<ApplyOutcome>, PnlError> {
        let mut outs = Vec::new();
        while self.consumed < ledger.len() {
            let e = ledger.get(self.consumed).ok_or(PnlError::OutOfOrder)?;
            outs.push(self.apply(e)?);
            self.consumed += 1;
        }
        Ok(outs)
    }

    /// 应用单笔成交事实（状态机核心）。
    pub fn apply(&mut self, e: &LedgerEntry) -> Result<ApplyOutcome, PnlError> {
        let gross = e.gross_amount().map_err(|_| PnlError::Overflow)?;
        self.fees = self.fees.checked_add(e.fee).map_err(|_| PnlError::Overflow)?;
        self.cash = self
            .cash
            .checked_add(e.cash_delta)
            .map_err(|_| PnlError::Overflow)?;

        let q = e.qty.raw() as i128;
        let d = if e.side == Side::Buy { q } else { -q };
        let before = self.net;
        let mut realized_delta = Money::ZERO;

        if before == 0 {
            // 开仓：全额计入成本
            self.cost.add_cost(q, gross)?;
            self.total_open_cost = self
                .total_open_cost
                .checked_add(gross)
                .map_err(|_| PnlError::Overflow)?;
            self.open_qty += q;
            self.net = d;
        } else if (before > 0) == (d > 0) {
            // 同向加仓：全额计入成本
            self.cost.add_cost(q, gross)?;
            self.total_open_cost = self
                .total_open_cost
                .checked_add(gross)
                .map_err(|_| PnlError::Overflow)?;
            self.open_qty += q;
            self.net += d;
        } else {
            // 逆向：先平仓（c = min(q, |net|)），余量反手
            let held_abs = before.abs();
            let c = if q < held_abs { q } else { held_abs };
            // 平仓侧成交金额：fill gross 按 trunc 拆分，余数归反手部分（最后一笔，§4.3）
            let close_gross = if c == q { gross } else { trunc_part(gross, c, q) };
            let flip_gross = if c == q {
                Money::ZERO
            } else {
                gross.checked_sub(close_gross).map_err(|_| PnlError::Overflow)?
            };
            let cost_released = self.cost.release_cost(c)?;
            // 多头平仓：卖收 − 成本；空头平仓：释放成本 − 买付
            realized_delta = if before > 0 {
                close_gross
                    .checked_sub(cost_released)
                    .map_err(|_| PnlError::Overflow)?
            } else {
                cost_released
                    .checked_sub(close_gross)
                    .map_err(|_| PnlError::Overflow)?
            };
            self.realized = self
                .realized
                .checked_add(realized_delta)
                .map_err(|_| PnlError::Overflow)?;
            self.close_qty += c;
            self.net += if before > 0 { -(c as i128) } else { c as i128 };

            if q > c {
                // 反手：余量按对手方向开仓（成本 = flip_gross，含拆分余数）
                let r = q - c;
                self.cost.add_cost(r, flip_gross)?;
                self.total_open_cost = self
                    .total_open_cost
                    .checked_add(flip_gross)
                    .map_err(|_| PnlError::Overflow)?;
                self.open_qty += r;
                self.net = if before > 0 { -(r as i128) } else { r as i128 };
            }
        }

        let event = if before == 0 {
            PositionEvent::Opened
        } else if self.net == 0 {
            PositionEvent::Closed
        } else if before.signum() == self.net.signum() {
            if self.net.abs() > before.abs() {
                PositionEvent::Increased
            } else {
                PositionEvent::Reduced
            }
        } else {
            PositionEvent::Flipped
        };

        Ok(ApplyOutcome {
            event,
            realized_delta,
            net_qty: self.net,
            fee: e.fee,
            cash_delta: e.cash_delta,
        })
    }
}
