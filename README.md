# Keel / 衡石

单机确定性持仓盈亏与止盈止损内核。热路径零分配、零锁、**逐位可复现**：相同输入必然产生相同输出（golden 回归验收），模拟成交尽可能符合真实市场规则（经济合理性验收，`docs/BASELINE.md` §2 双验收）。

核心特性：全链路 i64/i128 定点运算零浮点（价格 1e-4、数量 1e-8）；`(ts, phase, seq)` 全序键 + BarMatcher 保守不利路径撮合；TradeLedger 事实链（XXH3 逐笔累计）；golden 五项回归（事件哈希/快照锚点/精确核算/跨运行一致/差异定位）；分层基准 L0 派发 0.9ns/event 到 L3 全链路 2158 万 Bar/s。

## 快速开始

前置：Rustup（按 `rust-toolchain.toml` 自动安装锁定的 1.100.0 工具链，无需手动装）。

```bash
git clone <repo> && cd keel

cargo test --workspace              # 全部测试（单元/差分/确定性/golden 回归）
cargo run --release -p keel-bench   # 分层基准报告（七项验收 + 字段齐全）
```

`cargo test --workspace` 退出码 0 = 全部黄金结果逐位重现。任何哈希失配意味着代码或数据集与固化版本不一致（测试输出给出失配项与两侧哈希）。

## 仓库结构

| crate | 职责 |
|-------|------|
| `keel-core` | 定点类型（Price/Qty/Money/Rate）、XXH3 哈希、容量契约 |
| `keel-book` | 订单簿撮合载体（价格档位 + 分层位图 + O(1) 撤单，Tick 模型预留） |
| `keel-pnl` | TradeLedger 事实层 + FIFO 账户核算（环形批次队列）+ 费用模型 |
| `keel-guard` | 止盈止损规则引擎（固定/追踪，桶位图 O(1) 触发） |
| `keel-data` | 黄金 CSV 解析（零浮点、精度拒绝、OHLC 自洽校验） |
| `keel-engine` | BarMatcher 保守撮合 + 回测闭环 + golden 回归框架 |
| `keel-bench` | 分层基准（L0–L3）+ 验收判定 |
| `keel-ingest` | 真实行情源适配器（不属于内核；脏数据清洗为黄金 CSV） |

文档：`docs/BASELINE.md`（设计基线，唯一权威）、`docs/ADR/`（关键决策）、`docs/A2-复现包.md`（复现交付流程）。

## 数据接入

内核只吃**黄金 CSV**（`keel-data` 严格校验）：`ts,open,high,low,close,volume` 六列，首行 `ts,` 开头按表头跳过。规则：价格小数 ≤4 位、数量 ≤8 位（超限拒绝，不静默截断）；纯整数解析零浮点；`high ≥ max(open,close)`、`low ≤ min(open,close)`；ts 单调（数据游标只前进）；价格超 2^44 输入边界拒绝（成交量仅统计用途，豁免该边界）。

真实行情数据走 `keel-ingest` 适配器（源格式：tushare 风格日线 `ts_code,trade_date,open,high,low,close,vol,amount`，vol 单位手，trade_date 为 YYYYMMDD）：

```bash
cargo run -p keel-ingest -- src.csv golden.csv          # strict：小数超 2 位拒绝该行
cargo run -p keel-ingest -- src.csv golden.csv --snap   # snap：四舍五入归位 0.01 网格
```

清洗规则（写死、逐行独立、零浮点）：日期 → ts；手 → 股（×100）；复权价浮点污染在 strict 下拒绝、snap 下归位并逐行记日志；停牌日期缺口不补 Bar、缺口照实进报告；非法行 SKIP 不影响其余行；输出强制通过 keel-data 终检。清洗报告（每行处置 + 缺口清单）写 stderr。

已入库的真实数据回归集：`keel-engine/tests/golden/bars_real.csv`（sh600519 前复权日线 2025H1，117 根，snap 清洗全过），对应引擎回归测试 `real_data_regression`。

## 写一个回测

依赖（keel-engine 已聚合 core/data/guard/pnl）：

```toml
[dependencies]
keel-data   = { path = "../keel-data" }
keel-engine = { path = "../keel-engine" }
keel-guard  = { path = "../keel-guard" }
keel-pnl    = { path = "../keel-pnl" }
```

最小可跑示例：

```rust
use keel_core::{Qty, Rate};
use keel_data::{parse_bars_csv, Bar};
use keel_engine::{BacktestConfig, Backtester, MatcherConfig, OrderIntent, OrderKind, Strategy, StrategyCtx};
use keel_guard::GuardConfig;
use keel_pnl::{FeeModel, Side};

struct BuyDipSellRip;

impl Strategy for BuyDipSellRip {
    fn on_bar(&mut self, bar: &Bar, ctx: &StrategyCtx) -> Vec<OrderIntent> {
        if ctx.net_qty == 0 {
            vec![OrderIntent { side: Side::Buy, kind: OrderKind::Market, qty: Qty::from_raw(100 * 100_000_000) }]
        } else if bar.close.raw() > bar.open.raw() {
            vec![OrderIntent { side: Side::Sell, kind: OrderKind::Limit { limit: bar.open }, qty: Qty::from_raw(ctx.net_qty as i64) }]
        } else {
            vec![]
        }
    }
}

fn main() {
    let bars = parse_bars_csv(
        "ts,open,high,low,close,volume\n20260105,10.00,10.50,9.80,10.40,10000\n20260106,10.50,11.00,10.20,10.90,12000\n",
    ).unwrap();

    let cfg = BacktestConfig {
        fees: FeeModel {                          // Rate 单位 1e-8：30_000 = 0.03%，1_000_000 = 1%
            commission: Rate::from_raw(30_000),
            stamp_duty: Rate::from_raw(1_000_000), // 仅卖出收取
            transfer_fee: Rate::from_raw(0),
        },
        matcher: MatcherConfig {                  // 滑点 100 = 0.01 元（保守不利方向）
            slippage_raw: 100, limit_up: None, limit_down: None, min_lot_raw: 0,
        },
        guard: GuardConfig {                      // 桶位图须覆盖价格区间：base ± buckets×span
            base_price_raw: 100_000, bucket_span_raw: 1_000, buckets: 256, bucket_cap: 8, max_rules: 16,
        },
        initial_cash_raw: 100_000_000_000_000,    // 100 万元（Money 单位 1e-8）
        snapshot_every: 4,                        // 每 4 个事件一个快照锚点（0 = 关闭）
    };

    let mut bt = Backtester::new(cfg).unwrap();
    let report = bt.run(&bars, &mut BuyDipSellRip).unwrap();
    println!("fills={} net={} cash={}", report.fills, report.final_net_qty, report.final_cash_raw);
    println!("event_hash={:#018x} chain_hash={:#018x}", report.event_hash, report.chain_hash);
}
```

策略接口：`Strategy::on_bar(&bar, &ctx) -> Vec<OrderIntent>`；ctx 暴露 ts/net_qty/cash/realized/held_cost。订单即时生效（当根 Bar 撮合），市价按当根开盘价 ± 滑点成交，限价按保守规则撮合（§6.1：跳空劣于触发价按开盘价成交等）。策略只看到当根及之前的数据，API 不暴露任何未来信息。

止盈止损（可选）：在 run 前注册 guard，触发后按开盘价全平 bound_qty 并自动撤销挂单：

```rust
use keel_core::Price;
use keel_guard::{RuleKind, RuleSpec, TriggerSide};

bt.register_guard(RuleSpec {
    id: 1, ts: 0,
    side: TriggerSide::SellStop,
    kind: RuleKind::Fixed,                 // 或追踪止损（extreme/trail 语义见 BASELINE §8）
    trigger: Price::from_raw(95_000),      // 9.50 元触发
    extreme: Price::from_raw(0),
    trail: Qty::from_raw(0),
    bound_qty: Qty::from_raw(100 * 100_000_000),
}).unwrap();
```

报告字段（全部精确定点值 + 哈希）：`fills/rejects/cancels`、`final_net_qty/final_cash_raw/final_realized_raw/final_fees_raw`、`event_hash`（§13.1 事件流累计哈希）、`snapshots`（§13.2 快照锚点）、`chain_hash`（§9 Ledger 事实链摘要）。

## 黄金回归与复现

golden 回归位于 `keel-engine/tests/golden.rs`，五项全自动化：①事件流累计哈希 ②快照锚点 ③首个差异定位 ④最终核算精确值 ⑤跨运行逐位一致。数据集哈希清单（`dataset_manifest_matches`）用 XXH3-64 逐字节锚定全部数据集，任何字节级改动（含行尾 CRLF 混入）都会失配。

复现流程（详见 `docs/A2-复现包.md`）：clone → `cargo test --workspace` 绿 → `cargo run --release -p keel-bench`。注意 `.gitattributes` 强制数据/文本文件 LF——复现的字节级前提。锚值变更协议：合法变更（数据集更新、语义修订）须跑测试取新锚、更新常量、commit message 注明"重新生成 golden"。

## 确定性红线（写代码前必读）

热路径（撮合/核算/规则触发）禁止：浮点、分配、锁、panic（容量耗尽/溢出一律返回错误）、任何依赖容器内部顺序的遍历（同键事件次序只由 `(ts, phase, seq)` 决定）。跨类型乘除先升 i128 + `checked_*`。舍入方向按 §4.3 表逐项固定，不允许默认值。数据接入层（`keel-data`）之外禁止新的解析路径；真实数据清洗只能走 `keel-ingest`。变更确定性规则须走 ADR。

## 基准

`cargo run --release -p keel-bench` 输出 L0（派发）、L1（订单簿 add/cancel）、L2（pnl/guard）、L3（全链路回测）四级基准与七项验收判定；报告固定字段：硬件型号、工具链版本、target-cpu、数据集哈希、是否含撮合/指标。基准数据为确定性 Lcg 合成，无外部数据依赖。