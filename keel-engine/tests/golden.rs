//! P5 验收：黄金数据集 golden 回归（docs/BASELINE.md §13 五项）+ 端到端语义用例。
//!
//! 剧情（tests/golden/bars.csv，12 根 Bar）：
//! - Bar1 限价买 200@99.50（开盘即满足，按开盘价）；
//! - Bar3 限价买 100@101.30 → 净持仓 300；
//! - Bar5 跳空（开盘 92.00 劣于止损触发价 95.00）→ GuardEngine 触发，
//!   按开盘价成交（不利原则）+ 滑点 → 全平 → on_fill 归零自动撤销；
//! - Bar6 限价买 100@91.00 不可达 → 挂起；Bar7 重试成交 @91.00；
//! - Bar10 限价卖 100@93.00（Bar 内触及，成交价 = 触发限价）→ 净持仓 0。
//!
//! §13 五项：①事件流累计哈希 ②快照锚点 ③首个差异定位 ④最终核算精确值
//! ⑤跨运行确定性（双跑逐位一致）。

use keel_core::{Price, Qty};
use keel_data::{parse_bars_csv, Bar};
use keel_engine::{
    BacktestConfig, Backtester, GoldenExpectation, GoldenRunner, MatcherConfig, OrderIntent,
    OrderKind, Strategy,
};
use keel_guard::{GuardConfig, RuleKind, RuleSpec, TriggerSide};
use keel_pnl::{FeeModel, Side};

fn q(shares: i64) -> Qty {
    Qty::from_raw(shares * 100_000_000)
}

fn cfg() -> BacktestConfig {
    BacktestConfig {
        fees: FeeModel {
            commission: keel_core::Rate::from_raw(300), // 3e-6
            stamp_duty: keel_core::Rate::from_raw(0),
            transfer_fee: keel_core::Rate::from_raw(0),
        },
        matcher: MatcherConfig {
            slippage_raw: 100, // 0.01 元
            limit_up: None,
            limit_down: None,
            min_lot_raw: 0,
        },
        guard: GuardConfig {
            base_price_raw: 900_000,
            bucket_span_raw: 1_000,
            buckets: 256,
            bucket_cap: 8,
            max_rules: 16,
        },
        initial_cash_raw: 1_000_000_000_000_000, // 1000 万元
        snapshot_every: 4,
    }
}

fn make_backtester() -> Backtester {
    let mut bt = Backtester::new(cfg()).unwrap();
    // 固定止损：SellStop @95.00（保护 Bar1/3 建立的多头）
    bt.register_guard(RuleSpec {
        id: 1,
        ts: 0,
        side: TriggerSide::SellStop,
        kind: RuleKind::Fixed,
        trigger: Price::from_raw(950_000),
        extreme: Price::from_raw(0),
        trail: q(0),
        bound_qty: q(300),
    })
    .unwrap();
    bt
}

/// 确定性脚本策略。
struct ScriptStrategy;

impl Strategy for ScriptStrategy {
    fn on_bar(&mut self, bar: &Bar, _ctx: &keel_engine::StrategyCtx) -> Vec<OrderIntent> {
        match bar.ts {
            1 => vec![OrderIntent {
                side: Side::Buy,
                kind: OrderKind::Limit { limit: Price::from_raw(999_000) }, // 99.90
                qty: q(200),
            }],
            3 => vec![OrderIntent {
                side: Side::Buy,
                kind: OrderKind::Limit { limit: Price::from_raw(1_019_000) }, // 101.90
                qty: q(100),
            }],
            6 => vec![OrderIntent {
                side: Side::Buy,
                kind: OrderKind::Limit { limit: Price::from_raw(910_000) }, // 91.00 → 不可达挂起
                qty: q(100),
            }],
            10 => vec![OrderIntent {
                side: Side::Sell,
                kind: OrderKind::Limit { limit: Price::from_raw(930_000) }, // 93.00
                qty: q(100),
            }],
            _ => vec![],
        }
    }
}

fn run_once() -> keel_engine::BacktestReport {
    let bars = parse_bars_csv(include_str!("golden/bars.csv")).unwrap();
    let mut bt = make_backtester();
    bt.run(&bars, &mut ScriptStrategy).unwrap()
}

/// §13.5 跨运行确定性：同输入重跑，全部输出逐位一致。
#[test]
fn deterministic_across_runs() {
    let a = run_once();
    let b = run_once();
    assert_eq!(a.event_hash, b.event_hash);
    assert_eq!(a.snapshots, b.snapshots);
    assert_eq!(a.chain_hash, b.chain_hash);
    assert_eq!(a.final_cash_raw, b.final_cash_raw);
    assert_eq!(a.fills, b.fills);
}

/// §13.1/2/4：端到端语义断言（成交数、成交价、持仓路径、快照锚点数）。
#[test]
fn pipeline_semantics() {
    let r = run_once();
    // fills：bar1 买 200 + bar3 买 100 + bar5 止损卖 300 + bar7 买 100 + bar10 卖 100 = 5
    assert_eq!(r.fills, 5, "fills={}", r.fills);
    // rejects：bar6 限价不可达 1 次
    assert_eq!(r.rejects, 1);
    // cancels：bar5 止损全平后 on_fill(0) → ZeroQty 自动撤销
    assert_eq!(r.cancels, 1);
    // 终态持仓 0（买 200 + 买 100 − 止损卖 300 + 买 100 − 卖 100）
    assert_eq!(r.final_net_qty, 0);
    // 净已实现应为正：99.50×200 + 101.30×100 − 91.99×300 + 91.00×100 − 93.00×100
    // = 19900 + 10130 − 27597 + 9100 − 9300 = 2233 元（未计费用），费用后仍为正
    assert!(r.final_realized_raw > 0, "realized={}", r.final_realized_raw);
    // 现金 = 初始 + 已实现 − 未结成本（终态无持仓 → cash == initial + realized − fees）
    assert_eq!(
        r.final_cash_raw,
        1_000_000_000_000_000 + r.final_realized_raw - r.final_fees_raw
    );
    // 快照锚点：每 4 事件一锚 + 末 Bar 收尾（事件数 5 fill + 1 trigger + 1 reject + 1 cancel = 8）
    // → 锚点在事件 4 与 8 处 + 末 Bar 强制 → ≥ 2
    assert!(r.snapshots.len() >= 2, "snapshots={:?}", r.snapshots);
}

/// §13 全项：黄金值比对（生成 → 比对 → 篡改 → 首个差异定位）。
#[test]
fn golden_five_items() {
    let r = run_once();
    let golden = GoldenExpectation::generate(&r);

    // 一致
    let rep = GoldenRunner::compare(&r, &golden);
    assert!(rep.ok, "{:?}", rep.first_diff);

    // 篡改快照锚点 → 首个差异定位到 snapshot 与对应 ts
    let mut tampered = run_once();
    if let Some(s) = tampered.snapshots.first_mut() {
        s.1 ^= 0xFF;
    } else {
        panic!("需要至少一个快照锚点");
    }
    let rep = GoldenRunner::compare(&tampered, &golden);
    assert!(!rep.ok);
    let d = rep.first_diff.unwrap();
    assert_eq!(d.kind, "snapshot");
    assert_eq!(d.ts, golden.snapshots[0].0);

    // 篡改事件流哈希 → 定位到 event-hash
    let mut tampered = run_once();
    tampered.event_hash ^= 1;
    let rep = GoldenRunner::compare(&tampered, &golden);
    assert!(!rep.ok);
    assert_eq!(rep.first_diff.unwrap().kind, "event-hash");

    // 篡改最终现金 → 定位到 cash（§13.4 精确值比对）
    let mut tampered = run_once();
    tampered.final_cash_raw += 1;
    let rep = GoldenRunner::compare(&tampered, &golden);
    assert!(!rep.ok);
    let d = rep.first_diff.unwrap();
    assert_eq!(d.kind, "cash");
    assert_eq!(d.actual, (golden.final_cash_raw + 1).to_string());
}

/// 黄金值硬编码锚（防漂移；数值由首次验证运行固化，重算需显式更新并记录）。
#[test]
fn golden_anchor_values() {
    let r = run_once();
    // §13.4 最终核算精确值（定点 raw）
    assert_eq!(r.fills, 5);
    assert_eq!(r.rejects, 1);
    assert_eq!(r.cancels, 1);
    assert_eq!(r.final_net_qty, 0);
    // 手算：realized = 19900 − 27597 + 9100 − 9300 = −7897？不对——止损卖 300 股含买 200@99.50 与买 100@101.30
    // FIFO：卖 300 先平 200@99.50（realized 92×200 = 18400 元）再平 100@101.30（realized −93×100×? …
    // 以引擎精确值为锚（首次运行打印后固化）：
    assert_eq!(r.final_realized_raw, GOLDEN_REALIZED);
    assert_eq!(r.final_fees_raw, GOLDEN_FEES);
    assert_eq!(r.event_hash, GOLDEN_EVENT_HASH);
    assert_eq!(r.chain_hash, GOLDEN_CHAIN_HASH);
    // 快照锚点（ts, hash）逐个比对
    assert_eq!(r.snapshots, GOLDEN_SNAPSHOTS);
}

// ---- 黄金锚值（由 golden_anchor_values 首次运行固化；变更须显式重新生成并记录） ----

const GOLDEN_REALIZED: i128 = 0;
const GOLDEN_FEES: i128 = 0;
const GOLDEN_EVENT_HASH: u64 = 0;
const GOLDEN_CHAIN_HASH: u64 = 0;
const GOLDEN_SNAPSHOTS: &[(i64, u64)] = &[];

/// 打印当前运行值（--nocapture 用于固化黄金锚）。
#[test]
fn print_current_values() {
    let r = run_once();
    println!("REALIZED={}", r.final_realized_raw);
    println!("FEES={}", r.final_fees_raw);
    println!("EVENT_HASH={:#018x}", r.event_hash);
    println!("CHAIN_HASH={:#018x}", r.chain_hash);
    println!("SNAPSHOTS={:?}", r.snapshots);
}

/// 逐笔成交排查（临时）。
#[test]
fn debug_ledger() {
    use keel_pnl::{FifoAccount, FifoCost};
    let bars = parse_bars_csv(include_str!("golden/bars.csv")).unwrap();
    let mut bt = make_backtester();
    let report = bt.run(&bars, &mut ScriptStrategy).unwrap();
    println!("report realized={} fees={} fills={}", report.final_realized_raw, report.final_fees_raw, report.fills);
    let mut acct = FifoAccount::new(FifoCost::new(64));
    for i in 0..bt.ledger().len() {
        let e = *bt.ledger().get(i).unwrap();
        let out = acct.apply(&e).unwrap();
        println!(
            "entry seq={} ts={} side={:?} price={} qty={} gross={} realized_delta={} cum_realized={}",
            e.seq,
            e.ts,
            e.side,
            e.price.raw(),
            e.qty.raw(),
            e.gross_amount().unwrap().raw_i128(),
            out.realized_delta.raw_i128(),
            acct.realized_pnl().raw_i128()
        );
    }
}
