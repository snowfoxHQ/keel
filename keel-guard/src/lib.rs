//! keel-guard：条件单注册、阈值索引、触发。
//!
//! 见 docs/BASELINE.md §8（触发价/成交价分离、跳空不利原则、跟踪止盈极值就地更新、
//! FillEvent 重算绑定数量、持仓归零自动撤销、注册全序触发）。

pub mod bitmap;
pub mod engine;
pub mod index;
pub mod rule;

pub use bitmap::BitMap;
pub use engine::{GuardConfig, GuardEngine};
pub use index::ThresholdIndex;
pub use rule::{
    CancelReason, CancelRecord, GuardError, Rule, RuleHandle, RuleKind, RulePool, RuleSpec,
    TriggerSide, Triggered,
};
