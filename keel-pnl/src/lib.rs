//! keel-pnl：Trade Ledger、持仓状态机、FIFO/均价、Reconciliation。
//!
//! 见 docs/BASELINE.md §9（Truth First：事实记录与派生状态分离）、§4.3（舍入方向表）、§12（费用最小集）。

pub mod ledger;
pub mod position;
pub mod reconcile;

pub use ledger::{FeeModel, LedgerEntry, LedgerError, Side, TradeLedger};
pub use position::{
    ApplyOutcome, AverageAccount, AverageCost, CostMethod, Direction, FifoAccount, FifoCost,
    PnlAccount, PnlError, PositionEvent,
};
pub use reconcile::{reconcile, ReconcileReport};
