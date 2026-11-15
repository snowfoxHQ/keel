//! keel-book：扁平订单簿与撮合（事件级，docs/BASELINE.md §6/§7）。
//!
//! 设计要点：
//! - 价格档位固定步长直接索引，base/range/tick_size 配置校验，越界/脱格返回错误；
//! - 分层位图（[`bitmap::LevelBitmap`]）定位非空档位，BBO 缓存 O(1) 访问；
//! - 订单存于 keel-core 的固定容量对象池，句柄 `(slot, generation)`；
//! - 档位内 FIFO 数组化队列，墓碑 O(1) 撤单，惰性压缩不改 FIFO 顺序（§7.1）；
//! - preflight 校验：池满/队列满/成交缓冲不足在**任何状态变更之前**返回结构化错误；
//! - 热路径零分配、零锁、零 panic。

pub mod bitmap;
pub mod book;

pub use bitmap::LevelBitmap;
pub use book::{
    Book, BookConfig, BookError, Fill, FillBuf, OrderView, Side, SubmitOutcome,
};
