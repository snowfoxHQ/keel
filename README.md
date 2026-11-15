# Keel / 衡石

**一句话：这是一个"诚实的时光机"——拿真实的历史行情，重新演练你的买卖策略，告诉你如果当时真的这么做会赚还是会亏。**

玩过模拟炒股或者见过别人说"我这策略历史回测年化 30%"的话，你就已经理解回测是什么了。Keel 就是一个做这件事的引擎，但它把一件大多数回测工具没做好的事当作生命线：

> **同样的数据跑一万遍，结果一个字节都不会差。**

听起来理所当然？其实现实中很多回测程序今天跑和明天跑结果不一样——小数计算有舍入误差、成交撮合全凭想当然、多线程带来随机性。结果是：你看到的"赚 30%"里，可能有一部分只是计算误差的随机噪声。Keel 从数学底层就杜绝了这种事：全程不用小数（用整数精确表示每一分钱），成交规则按"对你最不利"的方向保守处理，每一步操作都留下可校验的指纹。你拿到的每一个数字，都能追溯到它的来龙去脉。

## 图形界面：点按钮就能跑回测（推荐新手从这里开始）

不想碰命令行？引擎自带一个本地网页版工作台：

```bash
cargo run -p keel-ui
```

启动后终端会提示打开 **http://127.0.0.1:8787**（复制到浏览器即可）。页面上三步：选行情数据（内置了贵州茅台 2025 上半年的真实日线，可以直接跑；上传自己的 CSV 也行——行情软件 / tushare 风格导出自动清洗归位，不需要懂"黄金 CSV"格式）→ 挑一个策略、设好资金和费用 → 点"开始回测"。结果页会给出总盈亏、收益率、资金曲线图（标注每笔买卖点）、成交明细表和过程指纹。

- 所有计算都在你自己的电脑上完成，数据不会上传到任何地方
- 内置三种策略：周期买卖、一次全仓买入持有、均线交叉；支持可选的固定止损
- 关闭那个终端窗口（或 Ctrl+C），服务就停了

## 一分钟上手

前提：装过 [Rust](https://rustup.rs)（一条命令装好，语言工具链会自动下载对应版本）。

```bash
git clone <repo> && cd keel
cargo test --workspace              # 跑完全部 100 多个自动化测试
cargo run --release -p keel-bench   # 打印一份性能测试报告
```

第一条命令跑完全绿，说明这套引擎在你机器上完全正常工作——它同时验证了"结果可以复现"这个承诺本身（同样的测试在谁机器上跑都是同样的结果）。第二条会打印一份像体检报告一样的性能单：引擎每秒能处理 2000 多万根日线数据，比绝大部分场景需要的快得多。

**上面的命令在哪敲？** 在**终端**里——就是那个能输入文字命令的窗口：Windows 按 `Win` 键、输入"PowerShell"回车打开；Mac 在"启动台"搜"终端"。命令敲完按回车执行。

**代码在哪写？** 用**编辑器**打开文件修改——最简单用 Windows 自带的"记事本"或 Mac 的"文本编辑"，推荐免费的 [VS Code](https://code.visualstudio.com/)（对新手够用且更省心）。

**项目文件夹在哪？** 就是 `git clone` 下载到本地的那份（克隆时会显示保存路径）。找不到的话，在终端里 `cd keel` 后输入 `pwd`（Mac）或 `cd`（Windows）回车，就会打印当前文件夹的完整路径。

## 几个名词，一分钟看完

| 名词 | 人话解释 |
|------|----------|
| 回测 | 拿历史行情数据，让程序按你的规则"假装"买卖，统计赚亏 |
| Bar（K线） | 一天的行情摘要：开盘价、最高价、最低价、收盘价、成交量 |
| 撮合 | 你挂单后，引擎决定这笔单子成没成交、按什么价成交 |
| 滑点 | 真实世界里成交价总比理想价差一点点，引擎故意按更差的价格算（保守） |
| 止损 | 亏到某个价位自动卖出，防止越亏越多 |
| 逐位可复现 | 同样的输入，任何时候、任何机器跑，输出连标点符号都一模一样 |
| 指纹（哈希） | 把全部交易过程压缩成一个短代号，两次结果代号相同 = 过程完全相同 |

## 用它跑一个回测：会发生什么

下面是一个**完整可运行的例子**（我们逐字验证过）。跟着做四步，就能在你自己的电脑上跑起来：

**第 1 步**：打开终端（怎么打开见上一节），进入项目文件夹：

```bash
cd keel
```

**第 2 步**：用编辑器（记事本或 VS Code）打开项目里的 `keel-bench` 文件夹，在其中新建一个名叫 `examples` 的文件夹，再在 `examples` 里新建一个文本文件，命名为 `my_first_backtest.rs`（注意：文件名以 `.rs` 结尾，别存成 `.txt`）。

**第 3 步**：把下面的代码**完整复制**进去，保存文件。

**第 4 步**：回到终端，敲这一条命令并回车：

```bash
cargo run -p keel-bench --example my_first_backtest
```

第一次运行会先编译（几十秒），然后打印：

```text
成交笔数: 2
最终持仓: 0 股
最终资金: 1000037.8847 元
过程指纹: 0x4342b94c5e7cb3fe
```

这个例子里两天的行情让策略赚了 **37.88 元**（100 万起步）。注意最后一行"过程指纹"——**你跑出来的指纹和这里印的应该一模一样**，尽管我们的电脑大概率不同。这就是"逐位可复现"的第一次亲身体验。

策略只有两句话：**空仓就买 100 股；持仓后一旦遇到收阳线的日子（收盘价比开盘价高），就在当天开盘价把持仓全部卖掉。**

```rust
use keel_core::{Qty, Rate};
use keel_data::{parse_bars_csv, Bar};
use keel_engine::{BacktestConfig, Backtester, MatcherConfig, OrderIntent, OrderKind, Strategy, StrategyCtx};
use keel_guard::GuardConfig;
use keel_pnl::{FeeModel, Side};

struct Simple;

impl Strategy for Simple {
    // 每天开盘前，引擎会来问你一次：今天想下什么单？
    fn on_bar(&mut self, bar: &Bar, ctx: &StrategyCtx) -> Vec<OrderIntent> {
        if ctx.net_qty == 0 {
            // 空仓：市价买 100 股。
            // 为什么乘 1 亿？程序内部所有价格、数量、金额都用"放大 1 亿倍的整数"记，
            // 彻底不用小数——没有小数就没有舍入误差，这是"跑一万遍结果完全一样"的根基。
            vec![OrderIntent { side: Side::Buy, kind: OrderKind::Market, qty: Qty::from_raw(100 * 100_000_000) }]
        } else if bar.close.raw() > bar.open.raw() {
            // 持仓且今天收阳线（收盘 > 开盘）：挂卖单，把手里股票全部卖掉
            vec![OrderIntent { side: Side::Sell, kind: OrderKind::Limit { limit: bar.open }, qty: Qty::from_raw(ctx.net_qty as i64) }]
        } else {
            vec![] // 其他情况：什么都不做
        }
    }
}

fn main() {
    // 行情数据：示例里直接写死两根 K 线（一行 = 一天的行情）；
    // 真实使用时换成读 CSV 文件，做法见下文"数据接入"一节。
    let bars = parse_bars_csv(
        "ts,open,high,low,close,volume\n20260105,10.00,10.50,9.80,10.40,10000\n20260106,10.50,11.00,10.20,10.90,12000\n",
    ).unwrap();

    // 回测规则设定：收多少费、滑多大、带多少钱进场
    let cfg = BacktestConfig {
        fees: FeeModel {
            commission: Rate::from_raw(30_000),    // 佣金 0.03%（费率也放大 1 亿倍：0.03% = 0.0003 = 30_000）
            stamp_duty: Rate::from_raw(1_000_000), // 印花税 1%，只在卖出时收
            transfer_fee: Rate::from_raw(0),       // 过户费：填 0 = 不收
        },
        matcher: MatcherConfig {
            slippage_raw: 100,                     // 滑点 0.01 元：买的时候多算一分、卖的时候少算一分
            limit_up: None, limit_down: None, min_lot_raw: 0,
        },
        guard: GuardConfig {                       // 止损引擎的容量参数：新手照抄即可
            base_price_raw: 100_000, bucket_span_raw: 1_000, buckets: 256, bucket_cap: 8, max_rules: 16,
        },
        initial_cash_raw: 100_000_000_000_000,     // 初始资金 100 万元（同样放大 1 亿倍：1 元 = 1 亿）
        snapshot_every: 4,                         // 每隔 4 笔交易动作存一次档，方便事后排查（0 = 不存）
    };

    let mut bt = Backtester::new(cfg).unwrap();
    let report = bt.run(&bars, &mut Simple).unwrap();

    println!("成交笔数: {}", report.fills);
    println!("最终持仓: {} 股", report.final_net_qty);
    println!("最终资金: {} 元", report.final_cash_raw as f64 / 1e8); // 资金内部是放大 1 亿倍的整数，给人看时除回 1 亿
    println!("过程指纹: {:#018x}", report.event_hash);
}
```

运行后引擎做的事，按顺序：读入行情 → 每天问你的策略要单子 → 按保守规则判定成交（买的时候帮你按更贵的价格算，卖的时候按更便宜的算，模拟真实摩擦）→ 自动收取手续费和印花税 → 全程记账 → 最后交给你一份报告：成交了几笔、赚亏多少、整个过程独一无二的指纹。

想加"跌到 9.5 元自动卖出"的止损？把下面的代码插入上面示例里 `let mut bt = ...` 和 `let report = ...` 两行之间（需要 `use keel_core::Price;` 等三行放在文件开头），再跑一次第 4 步的命令：

```rust
use keel_core::Price;
use keel_guard::{RuleKind, RuleSpec, TriggerSide};

bt.register_guard(RuleSpec {
    id: 1, ts: 0,                          // 规则编号和登记时间，编号自己起
    side: TriggerSide::SellStop,           // 卖出止损：往下跌破触发价就卖
    kind: RuleKind::Fixed,                 // 固定价位止损（另一种是"追踪止损"，跟着最高价走）
    trigger: Price::from_raw(95_000),      // 跌到 9.50 元触发（价格放大 1 万倍：1 元 = 10000）
    extreme: Price::from_raw(0),           // 固定止损用不到，填 0
    trail: Qty::from_raw(0),               // 同上，追踪止损才用
    bound_qty: Qty::from_raw(100 * 100_000_000), // 触发后卖多少股（这里是全部 100 股）
}).unwrap();
```

## 仓库地图

| crate | 一句话 | 管什么 |
|-------|--------|--------|
| `keel-core` | 地基 | 精确数字类型（整数定点）、哈希指纹 |
| `keel-data` | 门口的安检 | 只放"干净规整"的行情数据进来，脏数据一律拒收 |
| `keel-ingest` | 洗数据工 | 把真实行情软件导出的原始数据清洗成安检认可的标准格式 |
| `keel-engine` | 干活的 | 回测主流程：撮合 + 记账 + 出报告 |
| `keel-pnl` | 账房 | 每笔成交的盈亏计算、手续费，全程留底 |
| `keel-guard` | 保镖 | 止损止盈，触发就平仓 |
| `keel-book` | 仓库储备 | 订单簿撮合内核，为未来逐笔（Tick）行情预留 |
| `keel-bench` | 体检测速 | 各环节性能基准，保证"快"不是靠吹的 |

---

以下为**开发者进阶**内容（写代码、改数据、交付复现前必读）。设计原则的唯一权威是 `docs/BASELINE.md`，关键决策记录在 `docs/ADR/`。

## 数据接入

内核只吃**黄金 CSV**（`keel-data` 严格校验）：`ts,open,high,low,close,volume` 六列，首行 `ts,` 开头按表头跳过。规则：价格小数 ≤4 位、数量 ≤8 位（超限拒绝，不静默截断）；纯整数解析零浮点；`high ≥ max(open,close)`、`low ≤ min(open,close)`；ts 单调（数据游标只前进）；价格超 2^44 输入边界拒绝（成交量仅统计用途，豁免该边界）。

真实行情数据走 `keel-ingest` 适配器（源格式：tushare 风格日线 `ts_code,trade_date,open,high,low,close,vol,amount`，vol 单位手，trade_date 为 YYYYMMDD）：

```bash
cargo run -p keel-ingest -- src.csv golden.csv          # strict：小数超 2 位拒绝该行
cargo run -p keel-ingest -- src.csv golden.csv --snap   # snap：四舍五入归位 0.01 网格
```

清洗规则（写死、逐行独立、零浮点）：日期 → ts；手 → 股（×100）；复权价浮点污染在 strict 下拒绝、snap 下归位并逐行记日志；停牌日期缺口不补 Bar、缺口照实进报告；非法行 SKIP 不影响其余行；输出强制通过 keel-data 终检。清洗报告（每行处置 + 缺口清单）写 stderr。

已入库的真实数据回归集：`keel-engine/tests/golden/bars_real.csv`（sh600519 前复权日线 2025H1，117 根，snap 清洗全过），对应引擎回归测试 `real_data_regression`。

## 黄金回归与复现

golden 回归位于 `keel-engine/tests/golden.rs`，五项全自动化：①事件流累计哈希 ②快照锚点 ③首个差异定位 ④最终核算精确值 ⑤跨运行逐位一致。数据集哈希清单（`dataset_manifest_matches`）用 XXH3-64 逐字节锚定全部数据集，任何字节级改动（含行尾 CRLF 混入）都会失配。

复现流程（详见 `docs/A2-复现包.md`）：clone → `cargo test --workspace` 绿 → `cargo run --release -p keel-bench`。注意 `.gitattributes` 强制数据/文本文件 LF——复现的字节级前提。锚值变更协议：合法变更（数据集更新、语义修订）须跑测试取新锚、更新常量、commit message 注明"重新生成 golden"。

## 确定性红线（改内核代码前必读）

热路径（撮合/核算/规则触发）禁止：浮点、分配、锁、panic（容量耗尽/溢出一律返回错误）、任何依赖容器内部顺序的遍历（同键事件次序只由 `(ts, phase, seq)` 决定）。跨类型乘除先升 i128 + `checked_*`。舍入方向按 §4.3 表逐项固定，不允许默认值。数据接入层（`keel-data`）之外禁止新的解析路径；真实数据清洗只能走 `keel-ingest`。变更确定性规则须走 ADR。

## 基准

`cargo run --release -p keel-bench` 输出 L0（派发）、L1（订单簿 add/cancel）、L2（pnl/guard）、L3（全链路回测）四级基准与七项验收判定；报告固定字段：硬件型号、工具链版本、target-cpu、数据集哈希、是否含撮合/指标。基准数据为确定性 Lcg 合成，无外部数据依赖。