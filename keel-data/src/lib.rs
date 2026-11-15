//! keel-data：CSV→定点转换边界。P5 实现，见 docs/BASELINE.md §3.1/§4.2。
//!
//! v0 用一次性读入 + 纯整数解析；mmap 预读为后续优化项（不改变解析语义）。

pub mod csv;

pub use csv::{parse_bars_csv, Bar, DataError};
