//! keel-engine：BarMatcher、策略 trait、回测驱动、golden 回归（组合层）。
//!
//! 见 docs/BASELINE.md §5.1（流水线）、§6.1（保守撮合）、§13（回归协议）。
//! 依赖注记：§11 依赖图标注 `keel-core + keel-data ← keel-engine`，组合层闭环
//! （Ledger/PnL/Guard 联动）额外依赖 keel-pnl + keel-guard，v0 实现即如此，待评审修订。

pub mod backtest;
pub mod golden;
pub mod matcher;

pub use backtest::{BacktestConfig, BacktestReport, Strategy};
pub use golden::{GoldenReport, GoldenRunner};
pub use matcher::{BarMatcher, Fill, MatchOutcome, MatcherConfig, NotTraded, OrderIntent, OrderKind};
