//! keel-core：确定性内核——事件调度、全序键、定点数、Arena/对象池、固定容量队列。
//!
//! 确定性四约束见 docs/BASELINE.md §3；本 crate 是全部模块的依赖底座。
pub mod fixed;

pub use fixed::{Money, Price, Qty};
