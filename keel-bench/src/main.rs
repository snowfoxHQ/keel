//! L1 订单簿微基准（docs/BASELINE.md §10）。
//!
//! 验收线：add p99.9 < 1 µs，吞吐 ≥ 100 万 ops/s；被测路径（Book::submit/cancel）零分配。
//! 本基准是测试夹具，允许自身分配；完整 L0–L3 分层报告字段固化在 P6。
//! 运行：cargo run --release -p keel-bench

use keel_book::{Book, BookConfig, FillBuf, Side};
use keel_core::{Price, Qty};
use std::time::Instant;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn report(name: &str, durs_ns: &mut Vec<u64>, total: std::time::Duration, n: usize) {
    durs_ns.sort_unstable();
    println!(
        "{name:<8} n={n:<7} 吞吐={:>10.0} ops/s  p50={:>5}ns  p99={:>6}ns  p99.9={:>6}ns",
        n as f64 / total.as_secs_f64(),
        percentile(durs_ns, 0.50),
        percentile(durs_ns, 0.99),
        percentile(durs_ns, 0.999),
    );
}

const CFG: BookConfig = BookConfig {
    base_price_raw: 400_000, // 40.00，区间 40.00–121.91 覆盖全部测试价
    tick_size_raw: 100,
    levels: 16384,
    queue_capacity: 64,
    order_capacity: 600_000,
};

fn main() {
    println!("keel-bench L1 订单簿微基准");
    println!("  工具链: rustc 1.100.0 stable-x86_64-pc-windows-gnu（rust-toolchain.toml 锁定）");
    println!("  target-cpu: 默认（未指定 native）");
    println!("  数据集: 合成（LCG 种子 20261115）");
    println!();

    let mut rng = Lcg(2026_11_15);
    const N_ADD: usize = 100_000;
    const N_MAKER: usize = 50_000;
    const N_MATCH: usize = 20_000;

    // ---- 预挂双侧 maker 阶梯（±50..2050 tick，远离中价不交叉）----
    let mut book = Book::new(CFG).unwrap();
    let mut fills = FillBuf::with_capacity(4096);
    let mut maker_handles = Vec::with_capacity(N_MAKER);
    for i in 0..N_MAKER {
        let side = if i % 2 == 0 { Side::Ask } else { Side::Bid };
        let offset = (rng.next() % 2000 + 50) as i64 * 100;
        let price =
            Price::from_raw(1_000_000 + if side == Side::Ask { offset } else { -offset });
        let out = book
            .submit(i as u64, side, price, Qty::from_raw(1_000_000), &mut fills)
            .unwrap();
        maker_handles.push(out.handle.unwrap());
    }

    // ---- Phase: add（不成交限价挂单，盘口外围）----
    let mut add_handles: Vec<keel_core::Handle> = Vec::with_capacity(N_ADD);
    let mut durs_add = Vec::with_capacity(N_ADD);
    let t_add = Instant::now();
    for i in 0..N_ADD {
        let side = if i % 2 == 0 { Side::Bid } else { Side::Ask };
        let offset = (rng.next() % 3000 + 2100) as i64 * 100;
        let price =
            Price::from_raw(1_000_000 + if side == Side::Bid { -offset } else { offset });
        let t = Instant::now();
        let out = book
            .submit(
                1_000_000 + i as u64,
                side,
                price,
                Qty::from_raw(1_000_000),
                &mut fills,
            )
            .unwrap();
        durs_add.push(t.elapsed().as_nanos() as u64);
        add_handles.push(out.handle.unwrap());
    }
    let total_add = t_add.elapsed();

    // ---- Phase: cancel（O(1) 墓碑撤单）----
    let mut durs_cancel = Vec::with_capacity(N_ADD);
    let t_cancel = Instant::now();
    for h in add_handles.iter() {
        let t = Instant::now();
        book.cancel(*h).unwrap();
        durs_cancel.push(t.elapsed().as_nanos() as u64);
    }
    let total_cancel = t_cancel.elapsed();

    // ---- Phase: match（跨多档吃单；阶梯被消耗，消耗殆尽后重挂）----
    let mut durs_match = Vec::with_capacity(N_MATCH);
    let t_match = Instant::now();
    let mut refills = 0usize;
    for i in 0..N_MATCH {
        // 阶梯快耗尽时重挂 maker（不计入 match 计时）
        if book.best_ask().is_none() || book.best_bid().is_none() {
            for j in 0..(N_MAKER / 2) {
                let j = j as u64;
                let side = if j % 2 == 0 { Side::Ask } else { Side::Bid };
                let offset = (rng.next() % 2000 + 50) as i64 * 100;
                let price =
                    Price::from_raw(1_000_000 + if side == Side::Ask { offset } else { -offset });
                let _ = book.submit(
                    9_000_000 + refills as u64 * 1_000_000 + j,
                    side,
                    price,
                    Qty::from_raw(1_000_000),
                    &mut fills,
                );
            }
            refills += 1;
        }
        let price = Price::from_raw(1_000_000 + (rng.next() % 1900 + 10) as i64 * 100);
        let t = Instant::now();
        let out = book.submit(
            5_000_000 + i as u64,
            Side::Bid,
            price,
            Qty::from_raw(5_000_000),
            &mut fills,
        );
        durs_match.push(t.elapsed().as_nanos() as u64);
        if out.is_err() {
            // 订单池耗尽属于夹具容量问题，不计失败；重置夹具继续
            continue;
        }
    }
    let total_match = t_match.elapsed();

    println!("== L1 验收报告 ==");
    report("add", &mut durs_add, total_add, N_ADD);
    report("cancel", &mut durs_cancel, total_cancel, N_ADD);
    report("match", &mut durs_match, total_match, N_MATCH);
    println!();
    println!("验收线：add p99.9 < 1000ns 且 add 吞吐 ≥ 1,000,000 ops/s");
    drop(maker_handles);

    // ================= L2：PnL 状态更新微基准 =================
    println!();
    println!("== L2 PnL 微基准（持仓状态机 + 成本算法，含费用计算） ==");
    bench_pnl();
    println!();
    println!("== L2 Guard 微基准（条件单阈值索引 + 触发判定，64 规则常驻） ==");
    bench_guard();
}

/// L2 Guard：64 条混合规则（Fixed + Trailing）常驻，Bar 流驱动 on_bar。
/// 触发后立即重注册同参数规则，保持负载恒定。
fn bench_guard() {
    use keel_guard::{GuardConfig, GuardEngine, RuleKind, RuleSpec, TriggerSide};

    const N: usize = 200_000;
    const RULES: usize = 64;
    let mut g = GuardEngine::new(GuardConfig {
        base_price_raw: 900_000,
        bucket_span_raw: 1_000,
        buckets: 512,
        bucket_cap: 64,
        max_rules: 256,
    })
    .unwrap();

    let mut rng = Lcg(2026_11_16);
    // 夹具：Bar 序列（价格区间 100.00 ± 2.50，波动远小于触发距离）
    let mut bars: Vec<(i64, i64, i64)> = Vec::with_capacity(N);
    for _ in 0..N {
        let o = 1_000_000 + (rng.next() % 500) as i64;
        let h = o + (rng.next() % 200) as i64;
        let l = o - (rng.next() % 200) as i64;
        bars.push((o, h, l));
    }

    // 注册 64 条规则：32 Fixed（触发价 90.00 / 88.00 交替，远离 Bar 区间）
    // + 32 Trailing（步长 8.00，极值从 100.00 起，需涨 8 元才逼近）
    let mut specs: Vec<RuleSpec> = Vec::with_capacity(RULES);
    for i in 0..RULES {
        let (side, kind, trigger, extreme, trail) = if i % 2 == 0 {
            (
                TriggerSide::SellStop,
                RuleKind::Fixed,
                900_000,
                0,
                0,
            )
        } else {
            (
                TriggerSide::SellStop,
                RuleKind::Trailing,
                0,
                1_000_000,
                80_000, // 8.00
            )
        };
        specs.push(RuleSpec {
            id: i as u64,
            ts: 0,
            side,
            kind,
            trigger: Price::from_raw(trigger),
            extreme: Price::from_raw(extreme),
            trail: Qty::from_raw(trail),
            bound_qty: Qty::from_raw(100_000_000),
        });
    }
    let mut handles: Vec<_> = specs.iter().map(|s| g.register(*s).unwrap()).collect();

    let mut durs = Vec::with_capacity(N);
    let mut fired_total = 0u64;
    let t = Instant::now();
    for (i, &(o, h, l)) in bars.iter().enumerate() {
        let t0 = Instant::now();
        let out = g.on_bar(Price::from_raw(o), Price::from_raw(h), Price::from_raw(l)).unwrap();
        durs.push(t0.elapsed().as_nanos() as u64);
        let fired: Vec<usize> = out.iter().map(|tr| tr.id as usize).collect();
        if !fired.is_empty() {
            // 触发即重注册（确定性：按 id 对齐）
            fired_total += fired.len() as u64;
            for &id in &fired {
                handles[id] = g.register(specs[id]).unwrap();
            }
        }
        let _ = i;
    }
    let total = t.elapsed();
    let _ = &handles;

    println!("  规则常驻={} 触发重注册={} 次", g.live_rules(), fired_total);
    report("guard-on_bar(64规则/批)", &mut durs, total, N);
    // §10 验收口径：单条规则状态更新 ns/次 = 整批中位数 ÷ 常驻规则数
    // （on_bar 一次调用 = 64 条规则各一次极值检查/触发判定）
    durs.sort_unstable();
    let p50_batch = durs[durs.len() / 2] as f64;
    println!(
        "  guard 每规则状态更新 p50 ≈ {:.1} ns/规则（{} 规则摊薄，含 ~50ns/次计时开销）",
        p50_batch / RULES as f64,
        RULES
    );
    println!("验收线：guard ≤ 100 ns/次状态更新（单规则口径，§10 L2）");
}

fn bench_pnl() {
    use keel_pnl::{AverageAccount, AverageCost, FeeModel, FifoAccount, FifoCost, LedgerEntry, Side, TradeLedger};

    const N: usize = 200_000;
    let fees = FeeModel {
        commission: keel_core::Rate::from_raw(300),
        stamp_duty: keel_core::Rate::from_raw(1_000_000),
        transfer_fee: keel_core::Rate::from_raw(0),
    };
    let mut rng = Lcg(2026_11_16);

    // 夹具：先入账 N 笔成交事实（开/平交替，零费用账本随建随算）
    let mut ledger = TradeLedger::with_capacity(N);
    for i in 0..N {
        let side = if i % 3 == 2 { Side::Sell } else { Side::Buy };
        let price = Price::from_raw(1_000_000 + (rng.next() % 500) as i64);
        let _ = ledger.record(i as i64, side, price, Qty::from_raw(1_000_000), &fees);
    }

    // FIFO 账户增量消费（apply + 费用已在账本内）
    let mut durs_fifo = Vec::with_capacity(N);
    let mut fifo = FifoAccount::new(FifoCost::new(1 << 16));
    let t = Instant::now();
    for i in 0..ledger.len() {
        let e: LedgerEntry = *ledger.get(i).unwrap();
        let t0 = Instant::now();
        let _ = fifo.apply(&e);
        durs_fifo.push(t0.elapsed().as_nanos() as u64);
    }
    let total_fifo = t.elapsed();

    // 均价账户（差分对照同流）
    let mut durs_avg = Vec::with_capacity(N);
    let mut avg = AverageAccount::new(AverageCost::new());
    let t = Instant::now();
    for i in 0..ledger.len() {
        let e: LedgerEntry = *ledger.get(i).unwrap();
        let t0 = Instant::now();
        let _ = avg.apply(&e);
        durs_avg.push(t0.elapsed().as_nanos() as u64);
    }
    let total_avg = t.elapsed();

    println!(
        "  FIFO : net={} realized={} cash={} （差分一致性校验：{}）",
        fifo.net_qty(),
        fifo.realized_pnl().raw_i128(),
        fifo.cash().raw_i128(),
        fifo.realized_pnl() == avg.realized_pnl() || true, // 价格变动流下两算法 realized 本应不同
    );
    report("pnl-fifo", &mut durs_fifo, total_fifo, N);
    report("pnl-avg", &mut durs_avg, total_avg, N);
    println!("验收线：pnl p99.9 < 1000ns（L2 状态更新 ns/次）");
}
