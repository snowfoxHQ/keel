//! P4 验收：条件触发 / 跳空 / 归零撤销 / 注册全序 / 跟踪止盈 / 与 keel-pnl 持仓联动
//! （docs/BASELINE.md §16 P4、§8 全部语义）。

use keel_core::{Price, Qty};
use keel_guard::{
    CancelReason, GuardConfig, GuardEngine, RuleKind, RuleSpec, TriggerSide,
};

fn p(raw: i64) -> Price {
    Price::from_raw(raw)
}
fn q(shares: i64) -> Qty {
    Qty::from_raw(shares * 100_000_000)
}

fn engine() -> GuardEngine {
    GuardEngine::new(GuardConfig {
        base_price_raw: 1_000_000, // 100.00
        bucket_span_raw: 1_000,    // 0.10
        buckets: 1024,
        bucket_cap: 8,
        max_rules: 64,
    })
    .unwrap()
}

fn fixed_sell(id: u64, trigger: i64, qty: i64) -> RuleSpec {
    RuleSpec {
        id,
        ts: id as i64,
        side: TriggerSide::SellStop,
        kind: RuleKind::Fixed,
        trigger: p(trigger),
        extreme: p(0),
        trail: q(0),
        bound_qty: q(qty),
    }
}

/// §8.1 条件触发：Tick 路径精确整数比较，触发价不参与成交定价（at = 当前价）。
#[test]
fn tick_trigger_fixed_sell() {
    let mut g = engine();
    g.register(fixed_sell(1, 950_000, 100)).unwrap(); // 触发价 95.00

    // 未到触发价：不触发
    let out = g.on_price(p(951_000)).unwrap();
    assert!(out.is_empty());
    assert_eq!(g.live_rules(), 1);

    // 恰好触及：触发，at = 当前价
    let out = g.on_price(p(950_000)).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, 1);
    assert_eq!(out[0].at.raw(), 950_000);
    assert_eq!(out[0].side, TriggerSide::SellStop);
    assert_eq!(g.live_rules(), 0);

    // 触发后不再重复触发
    let out = g.on_price(p(900_000)).unwrap();
    assert!(out.is_empty());
}

/// §8.1 跳空穿越：开盘价劣于触发价 → 按开盘价成交（不利原则）；Bar 触及 → at = 触发价。
#[test]
fn bar_gap_crossing() {
    let mut g = engine();
    g.register(fixed_sell(1, 950_000, 100)).unwrap(); // 卖止损 @95.00

    // 跳空：开盘 90.00 直接劣于触发价 → at = 开盘价（不是触发价！）
    let out = g.on_bar(p(900_000), p(910_000), p(895_000)).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].at.raw(), 900_000, "跳空按开盘价（不利原则）");

    // Bar 内触及：开盘 96 > 触发 95，low 94 ≤ 95 → at = 触发价（保守穿越点）
    let mut g2 = engine();
    g2.register(fixed_sell(2, 950_000, 100)).unwrap();
    let out = g2.on_bar(p(960_000), p(965_000), p(940_000)).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].at.raw(), 950_000);

    // 未触及：open 96 / low 95.01 > 95 → 不触发
    let mut g3 = engine();
    g3.register(fixed_sell(3, 950_000, 100)).unwrap();
    let out = g3.on_bar(p(960_000), p(970_000), p(950_100)).unwrap();
    assert!(out.is_empty());
}

/// BuyStop 对称语义（保护空头）。
#[test]
fn buy_stop_symmetric() {
    let mut g = engine();
    g.register(RuleSpec {
        id: 10,
        ts: 0,
        side: TriggerSide::BuyStop,
        kind: RuleKind::Fixed,
        trigger: p(1_050_000), // 105.00 买入止损
        extreme: p(0),
        trail: q(0),
        bound_qty: q(100),
    })
    .unwrap();

    let out = g.on_bar(p(1_040_000), p(1_045_000), p(1_035_000)).unwrap();
    assert!(out.is_empty());

    // 跳空高开：open 108 ≥ 触发 105 → at = 开盘价
    let out = g.on_bar(p(1_080_000), p(1_090_000), p(1_070_000)).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].at.raw(), 1_080_000);

    // Bar 内触及：at = 触发价
    let mut g2 = engine();
    g2.register(RuleSpec {
        id: 11,
        ts: 0,
        side: TriggerSide::BuyStop,
        kind: RuleKind::Fixed,
        trigger: p(1_050_000),
        extreme: p(0),
        trail: q(0),
        bound_qty: q(100),
    })
    .unwrap();
    let out = g2.on_bar(p(1_040_000), p(1_055_000), p(1_035_000)).unwrap();
    assert_eq!(out[0].at.raw(), 1_050_000);
}

/// §8.1 跟踪止盈：极值随行情就地更新（定点），触发价随之移动。
#[test]
fn trailing_stop_updates_extreme() {
    let mut g = engine();
    // 跟踪止盈：极值 102.00，步长 2.00（价格偏移 raw = 20_000）→ 初始触发 100.00
    g.register(RuleSpec {
        id: 1,
        ts: 0,
        side: TriggerSide::SellStop,
        kind: RuleKind::Trailing,
        trigger: p(0),
        extreme: p(1_020_000),
        trail: Qty::from_raw(20_000),
        bound_qty: q(100),
    })
    .unwrap();

    // 价格上冲 104 → 极值 104，触发价升至 102
    let out = g.on_price(p(1_040_000)).unwrap();
    assert!(out.is_empty());
    // 价格回落到 102.01（高于新触发价）→ 不触发
    let out = g.on_price(p(1_020_100)).unwrap();
    assert!(out.is_empty());
    // 回落触及 102.00 → 触发（若极值未跟踪，老触发价 100 不会触发）
    let out = g.on_price(p(1_020_000)).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].at.raw(), 1_020_000);
}

/// Bar 保守路径（§6.1 + §8.1）：SellStop 先判 low 再吸收 high——
/// 同 Bar 内 high 先抬极值也不能让本 Bar 的 low 用"未来触发价"判定。
#[test]
fn bar_conservative_trailing_order() {
    let mut g = engine();
    // 极值 100.00，步长 2.00（raw 20_000）→ 触发 98.00
    g.register(RuleSpec {
        id: 1,
        ts: 0,
        side: TriggerSide::SellStop,
        kind: RuleKind::Trailing,
        trigger: p(0),
        extreme: p(1_000_000),
        trail: Qty::from_raw(20_000),
        bound_qty: q(100),
    })
    .unwrap();

    // Bar：O=100.00, H=104.00, L=99.00
    // 保守路径 O→L→H：low 99 > 当前触发 98 → 不触发；
    // 判定后才吸收 H=104（触发价升至 102）
    let out = g.on_bar(p(1_000_000), p(1_040_000), p(990_000)).unwrap();
    assert!(out.is_empty());

    // 下一 Bar L=101 < 新触发 102 → 触发，at = 触发价 102.00（保守穿越点；开盘 103 非跳空）
    let out = g.on_bar(p(1_030_000), p(1_035_000), p(1_010_000)).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].at.raw(), 1_020_000);
}

/// §8.1 多 GuardRule：同持仓多条规则按注册 (ts, seq) 全序触发。
#[test]
fn multi_rule_registration_order() {
    let mut g = engine();
    // 同一触发价注册三条（注册序 = seq 序）；故意乱序注册 id 以验证按 seq 而非 id
    g.register(fixed_sell(7, 950_000, 100)).unwrap();
    g.register(fixed_sell(3, 950_000, 100)).unwrap();
    g.register(fixed_sell(5, 950_000, 100)).unwrap();

    let out = g.on_price(p(949_000)).unwrap();
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].id, 7);
    assert_eq!(out[1].id, 3);
    assert_eq!(out[2].id, 5);
    assert_eq!(g.live_rules(), 0);
}

/// §8.1 FillEvent 重算绑定数量：keel-pnl 净持仓驱动（P5 组合方式的预演）；归零自动撤销。
#[test]
fn fill_rebind_and_position_close() {
    use keel_pnl::{AverageAccount, AverageCost, Side, TradeLedger};

    let mut g = engine();
    g.register(fixed_sell(1, 950_000, 100)).unwrap();
    g.register(fixed_sell(2, 940_000, 100)).unwrap();
    assert_eq!(g.live_rules(), 2);

    // keel-pnl：多头 100 → 平 50 → 净持仓 50；两规则保持存活
    let mut ledger = TradeLedger::with_capacity(4);
    let mut acct = AverageAccount::new(AverageCost::new());
    ledger
        .record(0, Side::Buy, p(1_000_000), q(100), &keel_pnl::FeeModel::ZERO)
        .unwrap();
    ledger
        .record(1, Side::Sell, p(1_010_000), q(50), &keel_pnl::FeeModel::ZERO)
        .unwrap();
    acct.apply(ledger.get(0).unwrap()).unwrap();
    acct.apply(ledger.get(1).unwrap()).unwrap();
    assert_eq!(acct.net_qty(), 50 * 100_000_000_i64 as i128);

    let out = g
        .on_fill(Qty::from_raw(acct.net_qty() as i64))
        .unwrap();
    assert!(out.is_empty(), "仍有持仓，不撤销");
    assert_eq!(g.live_rules(), 2);

    // 清仓 → 净持仓 0 → on_fill(0) 全量自动撤销（CancelAck 语义 + 撤销记录）
    ledger
        .record(2, Side::Sell, p(1_020_000), q(50), &keel_pnl::FeeModel::ZERO)
        .unwrap();
    acct.apply(ledger.get(2).unwrap()).unwrap();
    assert_eq!(acct.net_qty(), 0);

    let out = g.on_fill(Qty::from_raw(acct.net_qty() as i64)).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].id, 1);
    assert_eq!(out[1].id, 2);
    assert_eq!(out[0].reason, CancelReason::ZeroQty);
    assert_eq!(g.live_rules(), 0);

    // 撤销后行情再穿越也不触发
    let out = g.on_price(p(900_000)).unwrap();
    assert!(out.is_empty());
}

/// §8.1 数量为 0 即撤销（on_fill 路径，区别于显式 on_position_closed）。
#[test]
fn zero_qty_auto_cancel() {
    let mut g = engine();
    g.register(fixed_sell(1, 950_000, 100)).unwrap();
    let out = g.on_fill(Qty::from_raw(0)).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].reason, CancelReason::ZeroQty);
    assert_eq!(g.live_rules(), 0);
}

/// 手动撤销 + 句柄失效。
#[test]
fn manual_cancel_and_stale_handle() {
    let mut g = engine();
    let h = g.register(fixed_sell(1, 950_000, 100)).unwrap();
    let rec = g.cancel(h).unwrap();
    assert_eq!(rec.id, 1);
    assert_eq!(rec.reason, CancelReason::Manual);
    // 句柄失效：再撤报 RuleGone
    assert!(g.cancel(h).is_err());
    let out = g.on_price(p(900_000)).unwrap();
    assert!(out.is_empty());
}

/// 满载拒绝（§3.1）：单桶规则数超上限 / 规则池超容量，注册整体回滚。
#[test]
fn capacity_exhaustion() {
    let mut g = GuardEngine::new(GuardConfig {
        base_price_raw: 1_000_000,
        bucket_span_raw: 1_000_000, // 一个桶盖住全部测试触发价
        buckets: 16,
        bucket_cap: 2,
        max_rules: 8,
    })
    .unwrap();
    g.register(fixed_sell(1, 900_000, 100)).unwrap();
    g.register(fixed_sell(2, 890_000, 100)).unwrap();
    // 同桶第三条：拒绝且不留半注册态
    assert!(g.register(fixed_sell(3, 880_000, 100)).is_err());
    assert_eq!(g.live_rules(), 2);

    // 价格 80.00 触发两条（均 ≤ 触发价）→ 全部腾空
    let out = g.on_price(p(800_000)).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(g.live_rules(), 0);
    g.register(fixed_sell(4, 900_000, 100)).unwrap();
    assert_eq!(g.live_rules(), 1);
}

/// 引擎级差分：随机 Bar 流 + 随机注册/撤销 vs 朴素逐规则扫描模型（触发数与 at 值全等）。
#[test]
fn differential_vs_naive_scan() {
    use keel_guard::RuleHandle;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 11
        }
    }
    let mut rng = Lcg(2026_11_16);

    let mut g = engine();
    // 朴素模型条目：与 guard 注册一一对应（注册序 = seq 序）。
    struct Naive {
        id: u64,
        side: TriggerSide,
        kind: RuleKind,
        trigger: i64,
        extreme: i64,
        trail: i64,
    }
    let mut naive: Vec<Option<Naive>> = Vec::new();
    let mut handles: Vec<Option<RuleHandle>> = Vec::new();
    let mut next_id = 1u64;

    for bar in 0..4000 {
        match rng.next() % 8 {
            // 注册（朴素同步追加）
            0..=2 => {
                let side = if rng.next() & 1 == 0 {
                    TriggerSide::SellStop
                } else {
                    TriggerSide::BuyStop
                };
                let kind = if rng.next() % 3 == 0 {
                    RuleKind::Trailing
                } else {
                    RuleKind::Fixed
                };
                let trigger = 900_000 + (rng.next() % 200_000) as i64;
                let extreme = 900_000 + (rng.next() % 200_000) as i64;
                let trail = 1000 + (rng.next() % 5000) as i64;
                let id = next_id;
                next_id += 1;
                let h = g
                    .register(RuleSpec {
                        id,
                        ts: bar,
                        side,
                        kind,
                        trigger: p(trigger),
                        extreme: p(extreme),
                        trail: Qty::from_raw(trail),
                        bound_qty: q(100),
                    })
                    .unwrap();
                // guard 的 Trailing 触发价由 extreme ∓ trail 推导，朴素同镜像
                let t0 = match side {
                    TriggerSide::SellStop => extreme - trail,
                    TriggerSide::BuyStop => extreme + trail,
                };
                naive.push(Some(Naive {
                    id,
                    side,
                    kind,
                    trigger: if kind == RuleKind::Fixed { trigger } else { t0 },
                    extreme,
                    trail,
                }));
                handles.push(Some(h));
            }
            // 随机手动撤销（双侧同步）
            3 => {
                let live: Vec<usize> = (0..naive.len()).filter(|&i| naive[i].is_some()).collect();
                if let Some(&i) = live.first() {
                    g.cancel(handles[i].unwrap()).unwrap();
                    naive[i] = None;
                    handles[i] = None;
                }
            }
            _ => {}
        }

        // 随机 Bar（合法 OHLC：l ≤ o ≤ h）
        let o = 900_000 + (rng.next() % 200_000) as i64;
        let h = o + (rng.next() % 5_000) as i64;
        let l = o - (rng.next() % 5_000) as i64;

        // guard 判定
        let out = g.on_bar(p(o), p(h), p(l)).unwrap();

        // 朴素判定：先触发（at 按跳空规则），后吸收极值；触发即消失
        let mut expect: Vec<i64> = Vec::new();
        for e in naive.iter_mut() {
            let Some(n) = e else { continue };
            let hit = match n.side {
                TriggerSide::SellStop => l <= n.trigger,
                TriggerSide::BuyStop => h >= n.trigger,
            };
            if hit {
                let at = match n.side {
                    TriggerSide::SellStop => o.min(n.trigger),
                    TriggerSide::BuyStop => o.max(n.trigger),
                };
                expect.push(at);
                *e = None;
            } else if n.kind == RuleKind::Trailing {
                let (new_extreme, changed) = match n.side {
                    TriggerSide::SellStop => (h.max(n.extreme), h > n.extreme),
                    TriggerSide::BuyStop => (l.min(n.extreme), l < n.extreme),
                };
                if changed {
                    n.extreme = new_extreme;
                    n.trigger = match n.side {
                        TriggerSide::SellStop => new_extreme - n.trail,
                        TriggerSide::BuyStop => new_extreme + n.trail,
                    };
                }
            }
        }

        // 比对：触发顺序 = 注册序（guard 按 seq 排序），at 值全等
        assert_eq!(
            out.len(),
            expect.len(),
            "bar={bar} out={:?} expect={:?}",
            out.iter().map(|t| (t.id, t.at.raw())).collect::<Vec<_>>(),
            expect
        );
        for (go, eat) in out.iter().zip(expect.iter()) {
            assert_eq!(go.at.raw(), *eat, "bar={bar}");
        }
        // 存活数一致
        assert_eq!(
            g.live_rules() as usize,
            naive.iter().filter(|e| e.is_some()).count(),
            "bar={bar}"
        );
    }
}
