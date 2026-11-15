//! keel-book 撮合与回收契约验收（docs/BASELINE.md §16 P2）。

use keel_book::{Book, BookConfig, BookError, FillBuf, Side};
use keel_core::{Price, Qty};

fn cfg(levels: usize, queue_cap: usize, order_cap: u32) -> BookConfig {
    BookConfig {
        base_price_raw: 960_000, // 96.0000（区间中点，容纳双向测试价）
        tick_size_raw: 100,      // 0.0100
        levels,
        queue_capacity: queue_cap,
        order_capacity: order_cap,
    }
}

fn book() -> Book {
    Book::new(cfg(1024, 8, 64)).unwrap()
}

fn p(raw: i64) -> Price {
    Price::from_raw(raw)
}

fn q(raw: i64) -> Qty {
    Qty::from_raw(raw)
}

/// 撮合基础：跨档位吃单按 maker 价格成交，剩余挂入。
#[test]
fn crossing_fills_at_maker_price_fifo() {
    let mut b = book();
    let mut fills = FillBuf::with_capacity(8);

    // 卖方三档：100.00 x2单(3,2)，101.00 x1单(5)
    let m1 = b.submit(1, Side::Ask, p(1_000_000), q(300_000_000), &mut fills).unwrap();
    let m2 = b.submit(2, Side::Ask, p(1_000_000), q(200_000_000), &mut fills).unwrap();
    let m3 = b.submit(3, Side::Ask, p(1_010_000), q(500_000_000), &mut fills).unwrap();
    assert_eq!(b.best_ask(), Some(p(1_000_000)));

    fills.reset();
    // 买 10 @101 → 吃 m1(3)@100, m2(2)@100, m3(5)@101，剩余 0
    let out = b.submit(10, Side::Bid, p(1_010_000), q(1_000_000_000), &mut fills).unwrap();
    assert_eq!(out.filled, q(1_000_000_000));
    assert!(out.handle.is_none());
    let fs: Vec<(u64, i64, i64)> = fills
        .iter()
        .map(|f| (f.maker_id, f.price.raw(), f.qty.raw()))
        .collect();
    assert_eq!(
        fs,
        vec![
            (1, 1_000_000, 300_000_000),
            (2, 1_000_000, 200_000_000),
            (3, 1_010_000, 500_000_000),
        ]
    );
    // 吃空后 BBO：bid 剩余无，ask 空
    assert_eq!(b.best_ask(), None);
    assert_eq!(b.best_bid(), None);
    assert_eq!(b.live_orders(), 0);
    let _ = (m1, m2, m3);
}

/// 档位内价格-时间优先：同价 maker 按提交顺序成交。
#[test]
fn price_time_priority_within_level() {
    let mut b = book();
    let mut fills = FillBuf::with_capacity(8);
    b.submit(1, Side::Ask, p(1_000_000), q(100_000_000), &mut fills).unwrap();
    b.submit(2, Side::Ask, p(1_000_000), q(100_000_000), &mut fills).unwrap();
    b.submit(3, Side::Ask, p(1_000_000), q(100_000_000), &mut fills).unwrap();

    fills.reset();
    b.submit(9, Side::Bid, p(1_000_000), q(250_000_000), &mut fills).unwrap();
    // 三笔成交：m1(100) m2(100) m3(50 部分成交)
    let ids: Vec<u64> = fills.iter().map(|f| f.maker_id).collect();
    assert_eq!(ids, vec![1, 2, 3]);
    let qtys: Vec<i64> = fills.iter().map(|f| f.qty.raw()).collect();
    assert_eq!(qtys, vec![100_000_000, 100_000_000, 50_000_000]);
    // 第三个 maker 部分成交保留且仍在队列中
    assert_eq!(b.best_ask(), Some(p(1_000_000)));
    assert_eq!(b.live_at(Side::Ask, p(1_000_000)).unwrap(), 1);
}

/// 部分成交：maker 保留，taker 剩余挂入。
#[test]
fn partial_fill_and_remainder_queues() {
    let mut b = book();
    let mut fills = FillBuf::with_capacity(8);
    let maker = b.submit(1, Side::Ask, p(1_000_000), q(600_000_000), &mut fills).unwrap();

    fills.reset();
    let taker = b.submit(2, Side::Bid, p(1_000_000), q(400_000_000), &mut fills).unwrap();
    assert_eq!(fills.len(), 1);
    assert_eq!(fills.iter().next().unwrap().qty, q(400_000_000));
    assert_eq!(b.order(maker.handle.unwrap()).unwrap().remaining, q(200_000_000));
    // taker 剩余 0（400 全部成交）→ handle None
    assert!(taker.handle.is_none());
    assert_eq!(b.best_bid(), None); // taker 全成，无剩余买单
    assert_eq!(b.best_ask(), Some(p(1_000_000)));

    // 反向：卖单剩余挂入
    fills.reset();
    let t2 = b.submit(3, Side::Bid, p(1_000_000), q(500_000_000), &mut fills).unwrap();
    assert_eq!(fills.iter().next().unwrap().qty, q(200_000_000));
    let th = t2.handle.unwrap();
    assert_eq!(b.order(th).unwrap().remaining, q(300_000_000));
    assert_eq!(b.best_bid(), Some(p(1_000_000)));
}

/// O(1) 撤单：墓碑 + BBO 更新 + 失效句柄拒绝。
#[test]
fn cancel_tombstone_and_stale_handle() {
    let mut b = book();
    let mut fills = FillBuf::with_capacity(8);
    let o1 = b.submit(1, Side::Bid, p(1_000_000), q(100_000_000), &mut fills).unwrap().handle.unwrap();
    let o2 = b.submit(2, Side::Bid, p(990_000), q(100_000_000), &mut fills).unwrap().handle.unwrap();
    assert_eq!(b.best_bid(), Some(p(1_000_000)));

    // 撤最优档
    let rem = b.cancel(o1).unwrap();
    assert_eq!(rem, q(100_000_000));
    assert_eq!(b.best_bid(), Some(p(990_000)));
    // 旧句柄失效
    assert_eq!(b.cancel(o1), Err(BookError::OrderGone));
    assert_eq!(b.order(o1), None);
    assert_eq!(b.cancel(o2).unwrap(), q(100_000_000));
    assert_eq!(b.best_bid(), None);
}

/// 撤单拒绝：完全成交后的句柄 → OrderGone（§5.3，不回滚成交）。
#[test]
fn cancel_after_full_fill_rejected() {
    let mut b = book();
    let mut fills = FillBuf::with_capacity(8);
    let maker = b.submit(1, Side::Ask, p(1_000_000), q(100_000_000), &mut fills).unwrap().handle.unwrap();
    fills.reset();
    b.submit(2, Side::Bid, p(1_000_000), q(100_000_000), &mut fills).unwrap();
    // maker 已完全成交并被移出池
    assert_eq!(b.cancel(maker), Err(BookError::OrderGone));
}

/// 满载拒绝：队列满（§7.1），错误路径不改变任何状态。
#[test]
fn queue_full_rejected_no_mutation() {
    let mut b = Book::new(cfg(1024, 2, 64)).unwrap();
    let mut fills = FillBuf::with_capacity(8);
    b.submit(1, Side::Ask, p(1_000_000), q(1_000_000), &mut fills).unwrap();
    b.submit(2, Side::Ask, p(1_000_000), q(1_000_000), &mut fills).unwrap();
    let before_live = b.live_orders();
    let err = b.submit(3, Side::Ask, p(1_000_000), q(1_000_000), &mut fills);
    assert!(matches!(err, Err(BookError::QueueFull { .. })));
    assert_eq!(b.live_orders(), before_live);
    assert_eq!(b.best_ask(), Some(p(1_000_000)));
}

/// 满载拒绝：订单池满。
#[test]
fn pool_full_rejected() {
    let mut b = Book::new(cfg(1024, 8, 2)).unwrap();
    let mut fills = FillBuf::with_capacity(8);
    b.submit(1, Side::Ask, p(1_000_000), q(1_000_000), &mut fills).unwrap();
    b.submit(2, Side::Ask, p(1_010_000), q(1_000_000), &mut fills).unwrap();
    let err = b.submit(3, Side::Ask, p(1_020_000), q(1_000_000), &mut fills);
    assert_eq!(err, Err(BookError::PoolFull));
    assert_eq!(b.live_orders(), 2);
}

/// preflight：成交缓冲不足 → 拒绝且状态零变更（§7 preflight 承诺）。
#[test]
fn fill_buffer_full_preflight_no_mutation() {
    let mut b = book();
    let mut fills = FillBuf::with_capacity(2);
    // 三个独立档位的卖单（都会被吃）
    b.submit(1, Side::Ask, p(1_000_000), q(1_000_000), &mut fills).unwrap();
    b.submit(2, Side::Ask, p(1_010_000), q(1_000_000), &mut fills).unwrap();
    b.submit(3, Side::Ask, p(1_020_000), q(1_000_000), &mut fills).unwrap();
    let before = (b.best_ask(), b.live_orders());

    let small = FillBuf::with_capacity(2);
    let mut small = small;
    let err = b.submit(9, Side::Bid, p(1_020_000), q(3_000_000), &mut small);
    assert_eq!(err, Err(BookError::FillBufferFull));
    // 状态零变更
    assert_eq!((b.best_ask(), b.live_orders()), before);
    assert_eq!(b.live_at(Side::Ask, p(1_000_000)).unwrap(), 1);

    // 换大缓冲后成功
    let mut big = FillBuf::with_capacity(8);
    let out = b.submit(9, Side::Bid, p(1_020_000), q(3_000_000), &mut big).unwrap();
    assert_eq!(big.len(), 3);
    assert!(out.handle.is_none());
}

/// 回收契约：墓碑压缩不改 FIFO 顺序（§7.1）。
#[test]
fn compaction_preserves_fifo() {
    // queue_capacity=4；COMPACT_DEAD_THRESHOLD=4
    let mut b = Book::new(cfg(1024, 4, 64)).unwrap();
    let mut fills = FillBuf::with_capacity(16);

    let hs: Vec<_> = (0..4)
        .map(|i| {
            b.submit(i, Side::Ask, p(1_000_000), q(1_000_000), &mut fills)
                .unwrap()
                .handle
                .unwrap()
        })
        .collect();

    // 撤中间两个 → 墓碑 2（< 阈值 4，不触发 maybe_compact）
    b.cancel(hs[1]).unwrap();
    b.cancel(hs[2]).unwrap();
    // 队列满 → submit 触发压缩回收
    let e = b.submit(10, Side::Ask, p(1_000_000), q(1_000_000), &mut fills);
    assert!(e.is_ok(), "压缩应回收出空位");
    let h5 = e.unwrap().handle.unwrap();
    // 压缩后 cancel 仍正常（pos 已同步）
    b.cancel(h5).unwrap();

    // 吃单：FIFO 顺序必须是幸存的 0, 3（提交顺序）
    fills.reset();
    b.submit(20, Side::Bid, p(1_000_000), q(2_000_000), &mut fills).unwrap();
    let ids: Vec<u64> = fills.iter().map(|f| f.maker_id).collect();
    assert_eq!(ids, vec![0, 3]);
}

/// 大队列 + 多轮压缩后 FIFO 与撮合仍正确（压力型契约）。
#[test]
fn repeated_cancel_submit_cycle_fifo_stable() {
    let mut b = Book::new(cfg(1024, 16, 128)).unwrap();
    let mut fills = FillBuf::with_capacity(64);

    // 交替提交/撤销 100 单，保留每第 13 单（容量 8，恰好 8 个幸存者）
    for i in 0..100u64 {
        let h = b
            .submit(i, Side::Ask, p(1_000_000), q(1_000_000), &mut fills)
            .unwrap()
            .handle
            .unwrap();
        if i % 13 != 0 {
            b.cancel(h).unwrap();
        }
    }
    // 幸存者：0,13,26,...,91 → 8 单
    assert_eq!(b.live_orders(), 8);

    fills.reset();
    b.submit(999, Side::Bid, p(1_000_000), q(8_000_000), &mut fills).unwrap();
    let ids: Vec<u64> = fills.iter().map(|f| f.maker_id).collect();
    let expect: Vec<u64> = (0..100u64).step_by(13).collect();
    assert_eq!(ids, expect);
}

/// 价格校验：脱格与越界。
#[test]
fn price_validation() {
    let mut b = book();
    let mut fills = FillBuf::with_capacity(8);
    // base=100.0000, tick=0.0100 → 100.0050 脱格
    assert_eq!(
        b.submit(1, Side::Ask, p(1_000_050), q(1_000_000), &mut fills),
        Err(BookError::PriceOffGrid)
    );
    // 超出 1024 档范围
    assert_eq!(
        b.submit(1, Side::Ask, p(1_070_000), q(1_000_000), &mut fills),
        Err(BookError::PriceOutOfRange)
    );
    // 低于基准价
    assert_eq!(
        b.submit(1, Side::Ask, p(959_900), q(1_000_000), &mut fills),
        Err(BookError::PriceOutOfRange)
    );
}

/// 配置校验。
#[test]
fn config_validation() {
    let mut c = cfg(1024, 8, 64);
    c.tick_size_raw = 0;
    assert!(matches!(
        Book::new(c),
        Err(BookError::ConfigInvalid("tick_size_raw 必须 > 0"))
    ));
    let mut c = cfg(1024, 0, 64);
    assert!(Book::new(c).is_err());
    let mut c = cfg(1024, 8, 64);
    c.base_price_raw = i64::MAX - 100;
    assert!(Book::new(c).is_err());
}

/// 确定性冒烟：同一操作序列在两个订单簿上产生完全相同的 BBO 轨迹与成交。
#[test]
fn deterministic_replay_smoke() {
    fn run() -> Vec<(i64, i64, Vec<(u64, i64, i64)>)> {
        let mut b = Book::new(cfg(1024, 8, 64)).unwrap();
        let mut fills = FillBuf::with_capacity(16);
        let mut trace = Vec::new();
        // 固定序列（无随机源，纯确定性）
        let ops: [(u64, Side, i64, i64); 10] = [
            (1, Side::Ask, 1_000_000, 2_000_000),
            (2, Side::Ask, 1_010_000, 2_000_000),
            (3, Side::Bid, 990_000, 1_000_000),
            (4, Side::Bid, 1_000_000, 1_500_000),
            (5, Side::Ask, 1_000_000, 3_000_000),
            (6, Side::Bid, 1_020_000, 4_000_000),
            (7, Side::Ask, 980_000, 2_000_000),
            (8, Side::Bid, 980_000, 2_000_000),
            (9, Side::Ask, 1_000_000, 1_000_000),
            (10, Side::Bid, 1_000_000, 2_000_000),
        ];
        for (id, side, pr, qt) in ops {
            fills.reset();
            let _ = b.submit(id, side, p(pr), q(qt), &mut fills).unwrap();
            trace.push((
                b.best_bid().map(|x| x.raw()).unwrap_or(0),
                b.best_ask().map(|x| x.raw()).unwrap_or(0),
                fills
                    .iter()
                    .map(|f| (f.maker_id, f.price.raw(), f.qty.raw()))
                    .collect(),
            ));
        }
        trace
    }
    assert_eq!(run(), run());
}
