//! Reconciliation：账本、持仓、现金、盈亏四方核对（docs/BASELINE.md §9）。
//!
//! 方法：用同一成本算法从零重建影子账户，逐字段与在用账户比对；
//! 另核对账本事实链摘要的自洽性与现金/费用合计。
//! 任何不一致即为 bug（Truth First：不"相信内存状态"）。

use crate::ledger::{replay_chain_hash, TradeLedger};
use crate::position::{CostMethod, PnlAccount, PnlError};

/// 核对报告：每项 true = 一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconcileReport {
    /// 账本事实链摘要与独立重放一致（无篡改/编码漂移）。
    pub chain_ok: bool,
    /// 净持仓与影子重放一致。
    pub net_qty_ok: bool,
    /// 持仓成本与影子重放一致。
    pub held_cost_ok: bool,
    /// 现金 = Σ ledger.cash_delta。
    pub cash_ok: bool,
    /// 费用 = Σ ledger.fee。
    pub fees_ok: bool,
    /// 已实现盈亏与影子重放一致。
    pub realized_ok: bool,
    /// 开仓/平仓累计数量与影子重放一致。
    pub open_close_ok: bool,
}

impl ReconcileReport {
    pub const ALL_OK: ReconcileReport = ReconcileReport {
        chain_ok: true,
        net_qty_ok: true,
        held_cost_ok: true,
        cash_ok: true,
        fees_ok: true,
        realized_ok: true,
        open_close_ok: true,
    };

    pub fn ok(&self) -> bool {
        self.chain_ok
            && self.net_qty_ok
            && self.held_cost_ok
            && self.cash_ok
            && self.fees_ok
            && self.realized_ok
            && self.open_close_ok
    }

    /// 首个不一致项（诊断定位用，对应 §13"首个差异定位"）。
    pub fn first_mismatch(&self) -> Option<&'static str> {
        match () {
            () if !self.chain_ok => Some("chain"),
            () if !self.net_qty_ok => Some("net_qty"),
            () if !self.held_cost_ok => Some("held_cost"),
            () if !self.cash_ok => Some("cash"),
            () if !self.fees_ok => Some("fees"),
            () if !self.realized_ok => Some("realized"),
            () if !self.open_close_ok => Some("open_close"),
            _ => None,
        }
    }
}

/// 对账。影子账户从零重放 `acct` 已消费的前缀事实，逐字段比对。
/// acct 消费进度允许落后于账本（增量消费）；完全同步后 cash/fees 才与账本全量合计比对。
pub fn reconcile<M: CostMethod>(
    acct: &PnlAccount<M>,
    ledger: &TradeLedger,
    make_shadow: impl FnOnce() -> PnlAccount<M>,
) -> Result<ReconcileReport, PnlError> {
    let n = acct.consumed();
    let mut shadow = make_shadow();
    for i in 0..n {
        shadow.apply(ledger.get(i).ok_or(PnlError::OutOfOrder)?)?;
    }

    // 事实链自洽：账本累计摘要 == 全量重放摘要
    let all: Vec<_> = ledger.iter().copied().collect();
    let chain_ok = replay_chain_hash(&all) == ledger.chain_hash();

    // acct 已消费完账本时，现金/费用才能与全量合计比对
    let fully_synced = n == ledger.len();

    Ok(ReconcileReport {
        chain_ok,
        net_qty_ok: shadow.net_qty() == acct.net_qty(),
        held_cost_ok: shadow.held_cost() == acct.held_cost(),
        cash_ok: shadow.cash() == acct.cash()
            && (!fully_synced
                || acct.cash() == ledger.total_cash_delta().map_err(|_| PnlError::Overflow)?),
        fees_ok: shadow.fees() == acct.fees()
            && (!fully_synced || acct.fees() == ledger.total_fee().map_err(|_| PnlError::Overflow)?),
        realized_ok: shadow.realized_pnl() == acct.realized_pnl(),
        open_close_ok: shadow.open_qty() == acct.open_qty()
            && shadow.close_qty() == acct.close_qty(),
    })
}
