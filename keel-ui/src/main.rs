//! keel-ui —— Keel 回测工作台（本地 Web 界面，不属于内核）。
//!
//! 运行：`cargo run -p keel-ui`，浏览器打开 http://127.0.0.1:8787
//!
//! 架构：std TCP 手写 HTTP（零外部依赖）；前端表单以 URL 编码提交（CSV 含逗号，
//! encodeURIComponent 后安全）；响应 JSON 由服务端手写序列化——金额一律以"分"
//! （1e-2 元）整数传输，前端只做展示除法，全链路不出现浮点。

use keel_core::{Price, Qty, Rate};
use keel_data::{parse_bars_csv, Bar};
use keel_engine::{
    BacktestConfig, Backtester, MatcherConfig, OrderIntent, OrderKind, Strategy, StrategyCtx,
};
use keel_guard::{GuardConfig, RuleKind, RuleSpec, TriggerSide};
use keel_pnl::{FeeModel, Side};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

const SAMPLE: &str = include_str!("../static/builtin_sample.csv");
const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const STYLE_CSS: &str = include_str!("../static/style.css");

fn main() {
    let addr = "127.0.0.1:8787";
    let listener = TcpListener::bind(addr).expect("启动失败：8787 端口被占用（可能已有一个 keel-ui 在跑）");
    println!("==============================================");
    println!("  Keel 回测工作台已启动");
    println!("  请用浏览器打开:  http://{addr}");
    println!("  关闭本窗口（或 Ctrl+C）即停止服务");
    println!("==============================================");
    for stream in listener.incoming() {
        if let Ok(s) = stream {
            handle_conn(s);
        }
    }
}

fn handle_conn(mut s: TcpStream) {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        match s.read(&mut tmp) {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = find_header_end(&buf) {
                    break p;
                }
                if buf.len() > (1 << 24) {
                    return; // 请求超 16MB，拒绝
                }
            }
            Err(_) => return,
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let first = head.lines().next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut content_len = 0usize;
    for line in head.lines().skip(1) {
        let l = line.trim().to_ascii_lowercase();
        if let Some(v) = l.strip_prefix("content-length:") {
            content_len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body: Vec<u8> = buf[header_end + 4..].to_vec();
    while body.len() < content_len {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    body.truncate(content_len);

    let (status, ctype, payload) = route(&method, &path, &body);
    let head_line = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let _ = s.write_all(head_line.as_bytes());
    let _ = s.write_all(&payload);
}

fn find_header_end(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n")
}
fn route(method: &str, path: &str, body: &[u8]) -> (&'static str, &'static str, Vec<u8>) {
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => ("200 OK", "text/html", INDEX_HTML.as_bytes().to_vec()),
        ("GET", "/app.js") => ("200 OK", "text/javascript", APP_JS.as_bytes().to_vec()),
        ("GET", "/style.css") => ("200 OK", "text/css", STYLE_CSS.as_bytes().to_vec()),
        ("POST", "/api/backtest") => match run_backtest(&String::from_utf8_lossy(body)) {
            Ok(json) => ("200 OK", "application/json", json.into_bytes()),
            Err(e) => (
                "400 Bad Request",
                "application/json",
                format!("{{\"ok\":false,\"error\":\"{}\"}}", json_escape(&e)).into_bytes(),
            ),
        },
        _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
    }
}

// ---- 表单解析（application/x-www-form-urlencoded） ----

fn parse_form(body: &str) -> HashMap<String, String> {
    body
        .split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((url_decode(k), url_decode(v)))
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hi = (b[i + 1] as char).to_digit(16);
                let lo = (b[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r")
}
// ---- 策略实现（确定性整数运算；非内核热路径，允许分配） ----

/// 买入数量上限：§4.2 数量契约 2^44（raw 1e-8）≈ 17.5 万股。全仓按此截断。
const MAX_QTY_RAW: i64 = (1i64 << 43) - 1;

/// 市价买 `shares` 股（100 股整数手）。
fn market_buy(shares: i64) -> Option<OrderIntent> {
    if shares < 100 {
        return None; // 不足一手，不买
    }
    let raw = shares.checked_mul(100_000_000)?.min(MAX_QTY_RAW);
    Some(OrderIntent { side: Side::Buy, kind: OrderKind::Market, qty: Qty::from_raw(raw) })
}

/// 全仓市价买入（按当前现金 / 开盘价估算可买股数，100 股取整）。
struct BuyHold;
impl Strategy for BuyHold {
    fn on_bar(&mut self, bar: &Bar, ctx: &StrategyCtx) -> Vec<OrderIntent> {
        if ctx.net_qty != 0 {
            return vec![];
        }
        let price_raw = (bar.open.raw() as i128).max(1);
        // cash_raw(1e-8 元) / price_raw(1e-4 元) = 可买股数 × 1e4
        let shares = (ctx.cash_raw / price_raw / 10_000) as i64 / 100 * 100;
        market_buy(shares).into_iter().collect()
    }
}

/// 周期买卖：空仓就市价买 100 股，持有 `hold_days` 天后市价卖出，循环。
struct DayCycle {
    hold_days: i64,
    held: i64,
}
impl Strategy for DayCycle {
    fn on_bar(&mut self, _bar: &Bar, ctx: &StrategyCtx) -> Vec<OrderIntent> {
        if ctx.net_qty > 0 {
            self.held += 1;
            if self.held >= self.hold_days {
                self.held = 0;
                return vec![OrderIntent {
                    side: Side::Sell,
                    kind: OrderKind::Market,
                    qty: Qty::from_raw(ctx.net_qty as i64),
                }];
            }
            vec![]
        } else {
            self.held = 0;
            market_buy(100).into_iter().collect()
        }
    }
}

/// 均线交叉：快线上穿慢线买入 100 股，下穿卖出全部（整数和比较，无除法）。
struct MaCross {
    fast: usize,
    slow: usize,
    closes: Vec<i64>,
}
impl MaCross {
    /// 窗口末 `n` 根收盘价的均值，以 sum×1e? 无除法比较：返回 sum 与 n。
    fn window_sum(&self, n: usize) -> i128 {
        self.closes[self.closes.len() - n..].iter().map(|c| *c as i128).sum()
    }
}
impl Strategy for MaCross {
    fn on_bar(&mut self, bar: &Bar, ctx: &StrategyCtx) -> Vec<OrderIntent> {
        self.closes.push(bar.close.raw());
        if self.closes.len() < self.slow + 1 {
            return vec![]; // 需要"昨天"和"今天"两个完整窗口
        }
        let f = self.window_sum(self.fast);
        let s = self.window_sum(self.slow);
        let pf = self.window_sum_at(self.fast, 1);
        let ps = self.window_sum_at(self.slow, 1);
        // fast_ma > slow_ma ⇔ sum_f × slow_n > sum_s × fast_n（整数无除法）
        let up = pf * (self.slow as i128) <= ps * (self.fast as i128)
            && f * (self.slow as i128) > s * (self.fast as i128);
        let down = pf * (self.slow as i128) >= ps * (self.fast as i128)
            && f * (self.slow as i128) < s * (self.fast as i128);
        if ctx.net_qty == 0 && up {
            market_buy(100).into_iter().collect()
        } else if ctx.net_qty > 0 && down {
            vec![OrderIntent { side: Side::Sell, kind: OrderKind::Market, qty: Qty::from_raw(ctx.net_qty as i64) }]
        } else {
            vec![]
        }
    }
}
impl MaCross {
    fn window_sum_at(&self, n: usize, back: usize) -> i128 {
        let end = self.closes.len() - back;
        self.closes[end - n..end].iter().map(|c| *c as i128).sum()
    }
}
// ---- 回测执行 + JSON 输出 ----

/// 自动识别行情格式：黄金 CSV 直接用；tushare 风格源数据（首行含 ts_code）
/// 走 keel-ingest 清洗（snap 归位 0.01 网格）。用户不需要知道"黄金 CSV"。
fn parse_any_csv(text: &str) -> Result<Vec<Bar>, String> {
    let head = text.trim_start().get(..64).unwrap_or("").to_ascii_lowercase();
    if head.contains("ts_code") || head.contains("date,open") {
        // 源格式：自动清洗（snap 归位 0.01 网格），被跳过的行不进回测
        let (golden, _rep) = keel_ingest::clean_daily_csv(text, keel_ingest::SnapMode::Snap)
            .map_err(|e| format!("数据清洗失败：{e:?}"))?;
        let bars = parse_bars_csv(&golden)
            .map_err(|e| format!("清洗后仍校验失败（数据损坏）：{e}"))?;
        Ok(bars)
    } else {
        parse_bars_csv(text)
            .map_err(|e| format!("行情数据校验失败：{e}（支持黄金 CSV 六列，或行情软件 tushare 风格导出，自动清洗）"))
    }
}

/// 包装策略：每根 Bar 记录 (ts, 净持仓 raw, 现金 raw)，用于资金曲线。
/// 注意时点：ctx 为当根撮合前状态，曲线含义 = "第 t 天开盘时账户净值"。
struct Recorder {
    inner: Box<dyn Strategy>,
    curve: Vec<(i64, i128, i128)>,
}
impl Strategy for Recorder {
    fn on_bar(&mut self, bar: &Bar, ctx: &StrategyCtx) -> Vec<OrderIntent> {
        let orders = self.inner.on_bar(bar, ctx);
        self.curve.push((bar.ts, ctx.net_qty, ctx.cash_raw));
        orders
    }
}

fn run_backtest(body: &str) -> Result<String, String> {
    let f = parse_form(body);
    let csv_text = match f.get("source").map(String::as_str) {
        Some("builtin") => SAMPLE.to_string(),
        _ => f.get("csv").cloned().ok_or("没有行情数据：请上传文件或选择内置样本")?,
    };
    let bars = parse_any_csv(&csv_text)?;

    let get_i64 = |k: &str, dflt: i64| f.get(k).and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(dflt);
    let cash_yuan = get_i64("cash_yuan", 1_000_000).max(1);
    let cash_raw = (cash_yuan as i128) * 100_000_000;
    let commission_bp = get_i64("commission_bp", 3).clamp(0, 10_000);
    let stamp_bp = get_i64("stamp_bp", 100).clamp(0, 10_000);
    let transfer_bp = get_i64("transfer_bp", 1).clamp(0, 10_000);
    let slippage_cents = get_i64("slippage_cents", 1).clamp(0, 1_000);

    // guard 桶位图按数据价格区间自动覆盖（base = 最低价，跨度保证盖住最高价）
    let min_p = bars.iter().map(|b| b.low.raw()).min().unwrap();
    let max_p = bars.iter().map(|b| b.high.raw()).max().unwrap();
    let span = (((max_p - min_p) / 1024) + 1).max(1_000);
    let guard_cfg = GuardConfig {
        base_price_raw: min_p,
        bucket_span_raw: span,
        buckets: 1024,
        bucket_cap: 8,
        max_rules: 16,
    };

    let cfg = BacktestConfig {
        fees: FeeModel {
            commission: Rate::from_raw(commission_bp * 10_000),
            stamp_duty: Rate::from_raw(stamp_bp * 10_000),
            transfer_fee: Rate::from_raw(transfer_bp * 10_000),
        },
        matcher: MatcherConfig {
            slippage_raw: slippage_cents * 100,
            limit_up: None,
            limit_down: None,
            min_lot_raw: 0,
        },
        guard: guard_cfg,
        initial_cash_raw: cash_raw,
        snapshot_every: 0,
    };
    let mut bt = Backtester::new(cfg).map_err(|_| "引擎初始化失败（资金超出范围？）")?;

    // 可选固定止损（两位小数价格字符串，如 "9.50"）
    if let Some(sp) = f.get("stop_yuan").map(String::as_str).filter(|s| !s.trim().is_empty()) {
        let s = sp.trim();
        if !s.chars().all(|c| c.is_ascii_digit() || c == '.') || s.matches('.').count() > 1 {
            return Err("止损价格式不对：请填最多两位小数的数字，如 9.50".into());
        }
        let (i, frac) = match s.split_once('.') { Some((i, fr)) => (i, fr), None => (s, "") };
        if frac.len() > 2 { return Err("止损价最多两位小数".into()); }
        let fr2: String = frac.chars().take(2).chain(std::iter::repeat('0').take(2 - frac.len())).collect();
        let int_v: i64 = i.parse().map_err(|_| "止损价数字太大")?;
        let frac_v: i64 = if fr2.is_empty() { 0 } else { fr2.parse().map_err(|_| "止损价格式不对")? };
        let stop_raw = int_v
            .checked_mul(10_000)
            .and_then(|v| v.checked_add(frac_v.checked_mul(100).unwrap_or(0)))
            .ok_or("止损价超出范围")?;
        // bound_qty = 按"最便宜的行情价"估算的最大可能持仓，截断到数量契约
        let bound = (((cash_raw / 10_000) as i64) / 100 * 100).min(MAX_QTY_RAW);
        bt.register_guard(RuleSpec {
            id: 1,
            ts: 0,
            side: TriggerSide::SellStop,
            kind: RuleKind::Fixed,
            trigger: Price::from_raw(stop_raw),
            extreme: Price::from_raw(0),
            trail: Qty::from_raw(0),
            bound_qty: Qty::from_raw(bound),
        }).map_err(|e| format!("止损规则登记失败：{e:?}"))?;
    }

    let kind = f.get("strategy").map(String::as_str).unwrap_or("day_cycle");
    let inner: Box<dyn Strategy> = match kind {
        "buy_hold" => Box::new(BuyHold),
        "ma_cross" => Box::new(MaCross {
            fast: get_i64("fast", 5).clamp(2, 250) as usize,
            slow: get_i64("slow", 20).clamp(3, 500) as usize,
            closes: Vec::new(),
        }),
        _ => Box::new(DayCycle { hold_days: get_i64("hold_days", 5).clamp(1, 10_000), held: 0 }),
    };
    let mut strat = Recorder { inner, curve: Vec::new() };
    let report = bt.run(&bars, &mut strat).map_err(|e| format!("回测运行失败：{e:?}"))?;
    Ok(render(&bars, &strat.curve, &report, &bt, cash_raw, kind))
}
/// 结果序列化为 JSON（金额统一"分"整数 = raw / 1e6；手写序列化，字段全为数字/数组）。
fn render(
    bars: &[Bar],
    curve: &[(i64, i128, i128)],
    report: &keel_engine::BacktestReport,
    bt: &Backtester,
    cash_raw: i128,
    kind: &str,
) -> String {
    let close_of = |ts: i64| bars.iter().find(|b| b.ts == ts).map(|b| b.close.raw()).unwrap_or(0);

    // 资金曲线：现金 + 持仓市值（raw 1e-8 → 分 = /1e6；乘积上界 i128 安全，见模块注释）
    let mut curve_json = String::new();
    for (ts, net_qty_raw, cash_raw_at) in curve {
        let close = close_of(*ts) as i128;
        let equity_raw = cash_raw_at + (net_qty_raw * close) / 10_000;
        curve_json.push_str(&format!("[{},{}],", ts, equity_raw / 1_000_000));
    }
    curve_json.pop();

    let mut price_json = String::new();
    for b in bars {
        price_json.push_str(&format!("[{},{}],", b.ts, b.close.raw() / 100)); // 收盘价，分
    }
    price_json.pop();

    // 成交明细（Ledger 事实条目）
    let mut trades_json = String::new();
    let mut buys: Vec<i64> = Vec::new();
    let mut sells: Vec<i64> = Vec::new();
    for (i, e) in bt.ledger().iter().enumerate() {
        let (side_cn, marker_arr) = match e.side {
            Side::Buy => ("买", &mut buys),
            Side::Sell => ("卖", &mut sells),
        };
        marker_arr.push(e.ts);
        trades_json.push_str(&format!(
            "[{},{},\"{}\",{},{},{},{}],",
            i + 1,
            e.ts,
            side_cn,
            e.price.raw() / 100,   // 成交价，分
            e.qty.raw() / 100_000_000, // 股数
            e.fee.raw() / 1_000_000,     // 费用，分
            e.cash_delta.raw() / 1_000_000 // 资金变动，分
        ));
    }
    trades_json.pop();

    // 总盈亏（含持仓市值）与收益率（基点 = 0.01%）
    let last_close = bars.last().map(|b| b.close.raw()).unwrap_or(0) as i128;
    let net_qty_raw = report.final_net_qty;
    let total_raw = report.final_cash_raw + (net_qty_raw * last_close) / 10_000 - cash_raw;
    let return_bp = if cash_raw > 0 { total_raw * 10_000 / cash_raw } else { 0 };

    format!(
        "{{\"ok\":true,\"strategy\":\"{}\",\"fills\":{},\"rejects\":{},\"cancels\":{},\
         \"net_shares\":{},\"final_cash_cents\":{},\"realized_cents\":{},\"fees_cents\":{},\
         \"total_cents\":{},\"return_bp\":{},\
         \"event_hash\":\"{:#018x}\",\"chain_hash\":\"{:#018x}\",\
         \"curve\":[{}],\"price\":[{}],\"trades\":[{}],\"buys\":[{}],\"sells\":[{}]}}",
        kind,
        report.fills,
        report.rejects,
        report.cancels,
        report.final_net_qty / 100_000_000,
        report.final_cash_raw / 1_000_000,
        report.final_realized_raw / 1_000_000,
        report.final_fees_raw / 1_000_000,
        total_raw / 1_000_000,
        return_bp,
        report.event_hash,
        report.chain_hash,
        curve_json,
        price_json,
        trades_json,
        buys.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(","),
        sells.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(",")
    )
}