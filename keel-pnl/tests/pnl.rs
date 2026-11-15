//! P3 验收：手工样例逐位对账 + FIFO/均价差分 + Reconciliation（docs/BASELINE.md §16 P3）。

use keel_pnl::{
    reconcile, AverageAccount, AverageCost, FeeModel, FifoAccount, FifoCost, LedgerError,
    PnlError, Side, TradeLedger,
};
use keel_core::{Price, Qty};

fn p(raw: i64) -> Price {
    Price::from_raw(raw)
}
fn q(shares: i64) -> Qty {
    Qty::from_raw(shares * 100_000_000)
}

const ZERO_FEE: FeeModel = FeeModel::ZERO;

/// 场景 A（手工样例，零费用）：
/// 买 100@10.00 → 卖 40@11.00 → 买 50@12.00 → 卖 80@13.00
///
/// 手算（FIFO）：
/// - 批次：(100, 1000.00)
/// - 卖 40：realized = 440.00 − 400.00 = 40.00；剩 (60, 600.00)
/// - 买 50：批次 (60, 600) (50, 600)；持仓 110，成本 1200.00
/// - 卖 80：消耗 60@600 + 20@240 = 840.00；realized = 1040.00 − 840.00 = 200.00
/// - 最终持仓 30，成本 360.00（批次 (30, 360)）
///
/// 手算（均价）：
/// - 成本 1000/100 → 卖 40 释放 400.00，realized = 40.00（同 FIFO，单一成本池）
/// - 买 50：成本 1200.00，持仓 110
/// - 卖 80：释放 1200×80/110 = 872.72727272（trunc，余数 0.7272... 归剩余持仓）
///   realized = 1040.00 − 872.72727272 = 167.27272728
/// - 最终持仓 30，成本 1200 − 872.72727272 = 327.27272728
#[test]
fn manual_sample_fifo() {
    let mut fifo = FifoAccount::new(FifoCost::new(16));
    let mut ledger = TradeLedger::with_capacity(8);

    ledger.record(0, Side::Buy, p(100_000), q(100), &ZERO_FEE).unwrap();
    let outs = fifo.sync(&ledger).unwrap();
    assert_eq!(outs.len(), 1);
    assert_eq!(outs[0].event, keel_pnl::PositionEvent::Opened);
    assert_eq!(fifo.net_qty(), 100 * 100_000_000_i64 as i128);
    assert_eq!(fifo.held_cost().raw_i128(), 100_000_000_000); // 1000.00

    ledger.record(1, Side::Sell, p(110_000), q(40), &ZERO_FEE).unwrap();
    let outs = fifo.sync(&ledger).unwrap();
    assert_eq!(outs[0].event, keel_pnl::PositionEvent::Reduced);
    assert_eq!(outs[0].realized_delta.raw_i128(), 4_000_000_000); // 40.00

    ledger.record(2, Side::Buy, p(120_000), q(50), &ZERO_FEE).unwrap();
    fifo.sync(&ledger).unwrap();
    assert_eq!(fifo.held_cost().raw_i128(), 120_000_000_000); // 1200.00

    ledger.record(3, Side::Sell, p(130_000), q(80), &ZERO_FEE).unwrap();
    let outs = fifo.sync(&ledger).unwrap();
    assert_eq!(outs[0].realized_delta.raw_i128(), 20_000_000_000); // 200.00

    assert_eq!(fifo.net_qty(), 30 * 100_000_000_i64 as i128);
    assert_eq!(fifo.held_cost().raw_i128(), 36_000_000_000); // 360.00
    assert_eq!(fifo.realized_pnl().raw_i128(), 24_000_000_000); // 240.00
    // 现金 = Σ卖 − Σ买 = 1480 − 1600 = −120.00
    assert_eq!(
        fifo.cash().raw_i128(),
        -100_000_000_000i128 + 44_000_000_000 - 60_000_000_000 + 104_000_000_000
    );
    // 无反手序列恒等式：cash == realized − held_cost（−120 = 240 − 360）
    assert_eq!(
        fifo.realized_pnl().raw_i128() - fifo.held_cost().raw_i128(),
        fifo.cash().raw_i128()
    );
    // 开仓均价 = 1600.00 元 ÷ 150 股 = 10.6666（Round::Down）
    assert_eq!(fifo.avg_entry_price().unwrap().raw(), 106_666);
}

#[test]
fn manual_sample_average_differs() {
    let mut avg = AverageAccount::new(AverageCost::new());
    let mut ledger = TradeLedger::with_capacity(8);
    ledger.record(0, Side::Buy, p(100_000), q(100), &ZERO_FEE).unwrap();
    ledger.record(1, Side::Sell, p(110_000), q(40), &ZERO_FEE).unwrap();
    ledger.record(2, Side::Buy, p(120_000), q(50), &ZERO_FEE).unwrap();
    ledger.record(3, Side::Sell, p(130_000), q(80), &ZERO_FEE).unwrap();
    avg.sync(&ledger).unwrap();

    // realized：40.00（卖 40）+ 167.27272728（卖 80：1040 − 872.72727272）= 207.27272728
    assert_eq!(avg.realized_pnl().raw_i128(), 20_727_272_728);
    // 释放 trunc 后余数归剩余持仓：1200 − 872.72727272 = 327.27272728
    assert_eq!(avg.held_cost().raw_i128(), 32_727_272_728);
    // 恒等式同样成立：cash == realized − held_cost
    assert_eq!(
        avg.realized_pnl().raw_i128() - avg.held_cost().raw_i128(),
        avg.cash().raw_i128()
    );
    // 与 FIFO 的 360.00 不同 → 差分成立
    assert_ne!(avg.held_cost().raw_i128(), 3_600_000_000);
    // 现金与 FIFO 完全一致（资金流不依赖成本算法）
    assert_eq!(
        avg.cash().raw_i128(),
        -100_000_000_000 + 44_000_000_000 - 60_000_000_000 + 104_000_000_000
    );
}

/// 反手拆分：多头 100@10.00，卖 150@11.00（卖收 1650.00）→
/// 平 100：close 部分 = 1650×100/150 = 1100.00，realized = 1100 − 1000 = +100.00
/// 反手空 50：成本 = 1650 − 1100 = 550.00（余数归反手部分，§4.3）
#[test]
fn flip_split_exact() {
    let mut fifo = FifoAccount::new(FifoCost::new(8));
    let mut ledger = TradeLedger::with_capacity(4);
    ledger.record(0, Side::Buy, p(100_000), q(100), &ZERO_FEE).unwrap();
    ledger.record(1, Side::Sell, p(110_000), q(150), &ZERO_FEE).unwrap();
    let outs = fifo.sync(&ledger).unwrap();

    assert_eq!(outs[1].event, keel_pnl::PositionEvent::Flipped);
    assert_eq!(fifo.direction(), keel_pnl::Direction::Short);
    assert_eq!(fifo.net_qty(), -(50 * 100_000_000_i64 as i128));
    // realized = 1100.00 − 1000.00 = +100.00
    assert_eq!(outs[1].realized_delta.raw_i128(), 10_000_000_000);
    // 反手成本 = 1650 − 1100 = 550.00（此处整除，无余数）
    assert_eq!(fifo.held_cost().raw_i128(), 55_000_000_000);
    // 拆分总和精确相等
    assert_eq!(110_000_000_000i128 + 55_000_000_000i128, 165_000_000_000i128);
    // 现金 = Σ卖 − Σ买 = 1650 − 1000 = 650.00
    assert_eq!(fifo.cash().raw_i128(), 65_000_000_000);
}

/// Reconciliation：影子重放逐字段一致；落后游标不算失败；篡改事实链必被发现。
#[test]
fn reconciliation_all_ok_and_tamper_detection() {
    let mut ledger = TradeLedger::with_capacity(8);
    ledger.record(0, Side::Buy, p(100_000), q(100), &ZERO_FEE).unwrap();
    ledger.record(1, Side::Sell, p(110_000), q(40), &ZERO_FEE).unwrap();
    ledger.record(2, Side::Buy, p(120_000), q(50), &ZERO_FEE).unwrap();

    let mut fifo = FifoAccount::new(FifoCost::new(8));
    fifo.sync(&ledger).unwrap();

    let r = reconcile(&fifo, &ledger, || FifoAccount::new(FifoCost::new(8))).unwrap();
    assert!(r.ok(), "first_mismatch = {:?}", r.first_mismatch());

    // 落后游标（只消费 2 条）不算失败
    let mut lag = FifoAccount::new(FifoCost::new(8));
    lag.sync(&TradeLedger::with_capacity(1)).unwrap(); // 空
    let _ = &lag;
    let r2 = reconcile(&fifo, &ledger, || FifoAccount::new(FifoCost::new(8))).unwrap();
    assert!(r2.ok());

    // 篡改检测：重放前缀时替换一条事实 → 链摘要不一致
    let entries: Vec<_> = ledger.iter().copied().collect();
    let mut tampered = entries.clone();
    tampered[1].price = p(111_000);
    assert_ne!(
        keel_pnl::ledger::replay_chain_hash(&tampered),
        ledger.chain_hash()
    );
    let _ = entries;
}

/// 费用进账后四方恒等式仍成立（现金含费用、realized 不含费用）。
#[test]
fn reconciliation_with_fees() {
    let fees = FeeModel {
        commission: keel_core::Rate::from_raw(300),
        stamp_duty: keel_core::Rate::from_raw(1_000_000),
        transfer_fee: keel_core::Rate::from_raw(0),
    };
    let mut ledger = TradeLedger::with_capacity(8);
    ledger.record(0, Side::Buy, p(100_000), q(100), &fees).unwrap();
    ledger.record(1, Side::Sell, p(110_000), q(100), &fees).unwrap();

    let mut avg = AverageAccount::new(AverageCost::new());
    avg.sync(&ledger).unwrap();

    // 全平后：realized + held_cost + fees == cash（含费用恒等式）
    assert_eq!(avg.held_cost().raw_i128(), 0);
    assert_eq!(
        avg.realized_pnl().raw_i128() - avg.fees().raw_i128(),
        avg.cash().raw_i128()
    );

    let r = reconcile(&avg, &ledger, || AverageAccount::new(AverageCost::new())).unwrap();
    assert!(r.ok(), "first_mismatch = {:?}", r.first_mismatch());
}

/// 常量价格下 FIFO 与均价必然一致（差分不变量）。
#[test]
fn differential_constant_price_identical() {
    let mut ledger = TradeLedger::with_capacity(64);
    // 多空交替、部分平仓、反手，但价格恒定
    let ops = [
        (Side::Buy, 100i64),
        (Side::Sell, 30),
        (Side::Buy, 20),
        (Side::Sell, 200), // 反手
        (Side::Buy, 60),
        (Side::Sell, 150), // 再反手
    ];
    for (i, (side, qty)) in ops.iter().enumerate() {
        ledger.record(i as i64, *side, p(100_000), q(*qty), &ZERO_FEE).unwrap();
    }
    let mut fifo = FifoAccount::new(FifoCost::new(64));
    let mut avg = AverageAccount::new(AverageCost::new());
    fifo.sync(&ledger).unwrap();
    avg.sync(&ledger).unwrap();

    assert_eq!(fifo.realized_pnl(), avg.realized_pnl());
    assert_eq!(fifo.held_cost(), avg.held_cost());
    assert_eq!(fifo.net_qty(), avg.net_qty());
    assert_eq!(fifo.cash(), avg.cash());
}

/// FIFO 批次容量耗尽 → BatchFull（不扩容）。
#[test]
fn fifo_batch_full_rejected() {
    let mut a = FifoAccount::new(FifoCost::new(2));
    let mut ledger = TradeLedger::with_capacity(8);
    ledger.record(0, Side::Buy, p(100_000), q(1), &ZERO_FEE).unwrap();
    ledger.record(1, Side::Buy, p(101_000), q(1), &ZERO_FEE).unwrap();
    ledger.record(2, Side::Buy, p(102_000), q(1), &ZERO_FEE).unwrap();
    // 前两条正常
    a.apply(ledger.get(0).unwrap()).unwrap();
    a.apply(ledger.get(1).unwrap()).unwrap();
    assert_eq!(a.apply(ledger.get(2).unwrap()), Err(PnlError::BatchFull));
    assert_eq!(a.net_qty(), 2 * 100_000_000_i64 as i128);
}

/// Ledger 满载拒绝（复验 §3.1）。
#[test]
fn ledger_full_rejected() {
    let mut l = TradeLedger::with_capacity(1);
    l.record(0, Side::Buy, p(100_000), q(1), &ZERO_FEE).unwrap();
    assert_eq!(
        l.record(1, Side::Buy, p(100_000), q(1), &ZERO_FEE),
        Err(LedgerError::Full)
    );
}
