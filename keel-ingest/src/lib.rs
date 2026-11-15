//! keel-ingest —— 真实行情源适配器（BASELINE §4.2 输入边界的外延层）。
//!
//! **不属于内核**：内核 crate 零依赖本 crate，黄金 CSV 严格校验（keel-data）
//! 原样保留——内核只吃干净数据，脏活都在这里。
//!
//! v0 固定一种源格式（tushare 风格日线导出，变更须走 ADR）：
//! `ts_code,trade_date,open,high,low,close,vol,amount`
//!
//! 清洗规则（写死、逐行独立、零浮点）：
//! 1. **列映射**：`trade_date`（YYYYMMDD 整数，保序单调）→ ts；`vol` 单位为手
//!    → 股（×100）；`amount` 不进内核，列数不为 8 → 行拒绝；
//! 2. **浮点污染定点化**：价格按 2 位小数（A 股 0.01 精度）纯整数解析；第 3 位起
//!    有非零数字（复权/浮点污染）→ strict 拒绝该行；snap 模式按绝对值四舍五入
//!    归位 0.01 网格并记录 SNAP 日志；
//! 3. **停牌缺口**：日期跳变不补 Bar（ts 单调即可），跳变记入报告（v0 不做
//!    交易日历判断，含周末的全部跳变照实记录，由消费方判断）；
//! 4. **逐行独立**：非法行 SKIP 并记原因，其余继续；输出空集报错；
//! 5. **终检**：输出必须通过 `keel_data::parse_bars_csv`（snap 可能破坏 OHLC
//!    自洽——如 low 归位后高于 open——逐行自查后 SKIP）。

use keel_data::Bar;

/// 清洗模式：浮点污染价格的处理方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapMode {
    /// 第 3 位小数起有非零数字 → 拒绝该行（默认；不静默丢精度）。
    Strict,
    /// 就近归位 0.01 网格（绝对值四舍五入），记录 SNAP 日志。
    Snap,
}

/// 单行处置记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowAction {
    /// 原样通过。
    Ok,
    /// 价格归位（字段、原值、新值，元）。
    Snap { field: &'static str, from: String, to: String },
    /// 跳过（原因）。
    Skip { reason: String },
}

/// 日期缺口（相邻两根 Bar 的日期跳变；v0 含周末跳变，照实记录）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateGap {
    pub from: i64,
    pub to: i64,
}

/// 清洗报告。
#[derive(Clone, Debug, Default)]
pub struct CleanReport {
    /// 源行处置（1 起始行号 → 动作）。
    pub actions: Vec<(usize, RowAction)>,
    /// 日期缺口（保序）。
    pub gaps: Vec<DateGap>,
    /// 精确通过行数。
    pub ok_rows: usize,
    /// 归位行数。
    pub snap_rows: usize,
    /// 跳过行数。
    pub skipped_rows: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestError {
    /// 空输入 / 无有效行。
    Empty,
    /// 表头不匹配。
    Header,
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::Empty => write!(f, "无有效数据行"),
            IngestError::Header => {
                write!(f, "表头列名不符（期望 ts_code,trade_date,open,high,low,close,vol,amount）")
            }
        }
    }
}

impl std::error::Error for IngestError {}

/// 清洗真实源日线 CSV → 黄金 CSV 文本 + 报告。
pub fn clean_daily_csv(text: &str, mode: SnapMode) -> Result<(String, CleanReport), IngestError> {
    let mut rep = CleanReport::default();
    let mut rows: Vec<String> = Vec::new();
    let mut prev_date: Option<i64> = None;

    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;
        if idx == 0 {
            let h = line.trim().trim_start_matches('\u{feff}');
            if h.starts_with("ts_code,trade_date,open") {
                continue;
            }
            return Err(IngestError::Header);
        }
        if line.trim().is_empty() {
            continue;
        }
        let date = match parse_date(line) {
            Some(d) => d,
            None => {
                rep.actions.push((
                    line_no,
                    RowAction::Skip { reason: "日期格式非法".into() },
                ));
                rep.skipped_rows += 1;
                continue;
            }
        };
        if let Some(p) = prev_date {
            if date > p {
                rep.gaps.push(DateGap { from: p, to: date });
            }
        }
        prev_date = Some(date);

        match clean_row(line, mode) {
            Ok((row, action)) => {
                match &action {
                    RowAction::Ok => rep.ok_rows += 1,
                    RowAction::Snap { .. } => rep.snap_rows += 1,
                    RowAction::Skip { .. } => rep.skipped_rows += 1,
                }
                if !matches!(action, RowAction::Skip { .. }) {
                    rows.push(row);
                }
                rep.actions.push((line_no, action));
            }
            Err(reason) => {
                rep.actions
                    .push((line_no, RowAction::Skip { reason: reason.into() }));
                rep.skipped_rows += 1;
            }
        }
    }

    if rows.is_empty() {
        return Err(IngestError::Empty);
    }

    // 内核终检：黄金 CSV 必须被 keel-data 接受（逐行已自查，此处兜底）
    let golden = format!("ts,open,high,low,close,volume\n{}", rows.join("\n"));
    let bars: Vec<Bar> = keel_data::parse_bars_csv(&golden)
        .expect("清洗输出必须通过内核校验");
    rep.ok_rows = bars.len() - rep.snap_rows;
    Ok((golden, rep))
}

/// 单行清洗：`600519.SH,20260105,1234.56,1240.00,1230.10,1238.45,12345,1520000000`
/// → `20260105,1234.56,1240.00,1230.10,1238.45,1234500`
fn clean_row(line: &str, mode: SnapMode) -> Result<(String, RowAction), String> {
    let cols: Vec<&str> = line.split(',').map(str::trim).collect();
    if cols.len() != 8 {
        return Err(format!("列数不为 8（实际 {}）", cols.len()));
    }
    let (o, ao) = clean_price(cols[2], mode, "open")?;
    let (h, ah) = clean_price(cols[3], mode, "high")?;
    let (l, al) = clean_price(cols[4], mode, "low")?;
    let (c, ac) = clean_price(cols[5], mode, "close")?;
    let shares = clean_lots(cols[6])?;

    // snap 后 OHLC 自洽自查（内核校验的前置）
    let hi = o.max(c).max(h);
    let lo = o.min(c).min(l);
    if h < hi || l > lo || h < l {
        return Err("snap 归位后 OHLC 自洽破坏".into());
    }

    let action = merge_snaps([ao, ah, al, ac]);
    let row = format!(
        "{},{},{},{},{},{}",
        parse_date(line).expect("已校验"),
        fmt_price(o),
        fmt_price(h),
        fmt_price(l),
        fmt_price(c),
        shares
    );
    Ok((row, action))
}

/// 价格清洗：元字符串 → raw（1e-4）。2 位小数精确；超出见 mode。返回 (raw, SNAP 日志)。
fn clean_price(
    s: &str,
    mode: SnapMode,
    field: &'static str,
) -> Result<(i64, Option<(&'static str, String, String)>), String> {
    let (int_part, frac_part) = split_decimal(s).ok_or_else(|| format!("价格非法数字：{s}"))?;
    let two: String = frac_part.chars().take(2).collect();
    let rest = &frac_part[two.len()..];
    let base = compose_raw(&int_part, &two).ok_or_else(|| format!("价格超出范围：{s}"))?;
    if rest.is_empty() || rest.chars().all(|c| c == '0') {
        return Ok((base, None));
    }
    match mode {
        SnapMode::Strict => Err(format!("价格 {s} 小数超 2 位且为 strict 模式")),
        SnapMode::Snap => {
            let round_up = rest.chars().next().is_some_and(|c| c >= '5');
            let snapped = if round_up { base + 100 } else { base };
            Ok((snapped, Some((field, s.to_string(), fmt_price(snapped)))))
        }
    }
}

/// 手 → 股（×100，零浮点）。
fn clean_lots(s: &str) -> Result<i64, String> {
    let n: i64 = s.parse().map_err(|_| format!("成交量非法整数（手）：{s}"))?;
    n.checked_mul(100).ok_or_else(|| "成交量超出范围".into())
}

/// `"20260105"` → `20260105`（i64，保序）。
fn parse_date(line: &str) -> Option<i64> {
    let s = line.split(',').nth(1)?.trim();
    if s.len() != 8 || !s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `"1234"` + `"56"` → `1234_5600`（raw 1e-4）。纯整数运算，零浮点。
fn compose_raw(int_part: &str, two: &str) -> Option<i64> {
    let int_v: i64 = if int_part.is_empty() { 0 } else { int_part.parse().ok()? };
    let frac_v: i64 = if two.is_empty() { 0 } else { two.parse().ok()? };
    Some(int_v.checked_mul(10_000)?.checked_add(frac_v.checked_mul(100)?)?)
}

/// 拆分十进制字符串（拒绝多点、非法字符、负号以外符号）。
fn split_decimal(s: &str) -> Option<(String, String)> {
    let mut parts = s.split('.');
    let int_part = parts.next()?.to_string();
    let frac_part = parts.next().unwrap_or("").to_string();
    if parts.next().is_some() {
        return None;
    }
    let ok = |t: &str| t.chars().all(|c| c.is_ascii_digit());
    if !ok(&int_part) || !ok(&frac_part) {
        return None;
    }
    Some((int_part, frac_part))
}

/// raw（1e-4）→ 黄金 CSV 价格字符串（2 位小数；raw 必为 0.01 整数倍）。
fn fmt_price(raw: i64) -> String {
    format!("{}.{:02}", raw / 10_000, (raw % 10_000) / 100)
}

/// 合并 4 字段 SNAP 日志（取首个；snap_rows 计数含全部归位行）。
fn merge_snaps(
    snaps: [Option<(&'static str, String, String)>; 4],
) -> RowAction {
    match snaps.iter().flatten().next() {
        None => RowAction::Ok,
        Some(first) => RowAction::Snap {
            field: first.0,
            from: first.1.clone(),
            to: first.2.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_data::parse_bars_csv;

    const HEAD: &str = "ts_code,trade_date,open,high,low,close,vol,amount";

    #[test]
    fn clean_row_clean_data_passes() {
        let src = format!(
            "{HEAD}\n600519.SH,20260105,1234.56,1240.00,1230.10,1238.45,12345,1520000000\n"
        );
        let (golden, rep) = clean_daily_csv(&src, SnapMode::Strict).unwrap();
        assert_eq!((rep.ok_rows, rep.snap_rows, rep.skipped_rows), (1, 0, 0));
        let bars = parse_bars_csv(&golden).unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].ts, 20260105);
        assert_eq!(bars[0].open.raw(), 12_345_600); // 1234.56 → 1e-4
        assert_eq!(bars[0].volume.raw(), 1_234_500i64 * 100_000_000); // 12345 手 → 1234500 股
        assert_eq!(golden.lines().count(), 2); // 表头 + 1 行（amount 不进）
    }

    #[test]
    fn float_pollution_strict_rejects_row() {
        let src = format!(
            "{HEAD}\n600519.SH,20260105,10.00,11.00,9.50,10.50,100,1\n600519.SH,20260106,1234.5600001,1240.00,1230.10,1238.45,100,1\n"
        );
        let (_, rep) = clean_daily_csv(&src, SnapMode::Strict).unwrap();
        assert_eq!(rep.skipped_rows, 1);
        assert!(matches!(rep.actions[1].1, RowAction::Skip { .. }));
    }

    #[test]
    fn float_pollution_snap_rounds_up() {
        // 第 3 位 7 ≥ 5 → 进位：1234.567 → 1234.57
        let src = format!("{HEAD}\n600519.SH,20260105,1234.567,1240.00,1230.10,1238.45,12345,1\n");
        let (golden, rep) = clean_daily_csv(&src, SnapMode::Snap).unwrap();
        assert_eq!(rep.snap_rows, 1);
        let bars = parse_bars_csv(&golden).unwrap();
        assert_eq!(bars[0].open.raw(), 12_345_700);
        match &rep.actions[0].1 {
            RowAction::Snap { field, from, to } => {
                assert_eq!(field, &"open");
                assert_eq!(from.as_str(), "1234.567");
                assert_eq!(to.as_str(), "1234.57");
            }
            other => panic!("期望 Snap，得到 {other:?}"),
        }
    }

    #[test]
    fn snap_down_rounding_below_five() {
        let src = format!("{HEAD}\n600519.SH,20260105,1234.562,1240.00,1230.10,1238.45,12345,1\n");
        let (golden, rep) = clean_daily_csv(&src, SnapMode::Snap).unwrap();
        assert_eq!(rep.snap_rows, 1);
        let bars = parse_bars_csv(&golden).unwrap();
        assert_eq!(bars[0].open.raw(), 12_345_600);
    }

    #[test]
    fn snap_breaking_ohlc_consistency_skips_row() {
        // low 1230.109 → snap 1230.11 > open 1230.10 → 自洽破坏 → SKIP（该行不进输出）
        let src = format!(
            "{HEAD}\n600519.SH,20260105,10.00,11.00,9.50,10.50,100,1\n600519.SH,20260106,1230.10,1240.00,1230.109,1238.45,12345,1\n"
        );
        let (golden, rep) = clean_daily_csv(&src, SnapMode::Snap).unwrap();
        assert_eq!(rep.skipped_rows, 1);
        let bars = parse_bars_csv(&golden).unwrap();
        assert_eq!(bars.len(), 1); // 自洽破坏行被剔除
    }

    #[test]
    fn suspended_day_gap_recorded() {
        let src = format!(
            "{HEAD}\n600519.SH,20260105,10.00,11.00,9.50,10.50,100,1\n600519.SH,20260107,10.50,11.00,10.00,10.80,100,1\n"
        );
        let (_, rep) = clean_daily_csv(&src, SnapMode::Strict).unwrap();
        assert_eq!(rep.gaps, vec![DateGap { from: 20260105, to: 20260107 }]);
    }

    #[test]
    fn malformed_rows_skip_independently() {
        let src = format!(
            "{HEAD}\n600519.SH,20260105,10.00,11.00,9.50,10.50,100,1\n600519.SH,20260106,10.5,11,10,10.8\n600519.SH,20260107,x,11.00,10.00,10.80,100,1\n600519.SH,20260108,10.80,11.00,10.50,10.90,100,1\n"
        );
        let (golden, rep) = clean_daily_csv(&src, SnapMode::Strict).unwrap();
        assert_eq!(rep.skipped_rows, 2); // 列数 7 + 非法数字
        let bars = parse_bars_csv(&golden).unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[1].ts, 20260108);
    }

    #[test]
    fn bad_header_rejected() {
        assert!(matches!(
            clean_daily_csv("a,b,c\n1,2,3\n", SnapMode::Strict),
            Err(IngestError::Header)
        ));
    }

    #[test]
    fn all_rows_skipped_is_empty() {
        let src = format!("{HEAD}\n600519.SH,20260105,x,y,z,w,1,1\n");
        assert!(matches!(
            clean_daily_csv(&src, SnapMode::Strict),
            Err(IngestError::Empty)
        ));
    }

    #[test]
    fn trailing_zero_fractions_are_exact_not_snap() {
        // "10.500" 第 3 位为 0 → 精确 10.50，不 SNAP
        let src = format!("{HEAD}\n600519.SH,20260105,10.500,11.00,9.50,10.50,100,1\n");
        let (_, rep) = clean_daily_csv(&src, SnapMode::Strict).unwrap();
        assert_eq!((rep.ok_rows, rep.snap_rows), (1, 0));
    }

    #[test]
    fn multi_field_snap_single_log_entry() {
        let src = format!("{HEAD}\n600519.SH,20260105,10.005,11.00,9.50,10.505,100,1\n");
        let (_, rep) = clean_daily_csv(&src, SnapMode::Snap).unwrap();
        assert_eq!(rep.snap_rows, 1);
        match &rep.actions[0].1 {
            RowAction::Snap { field, .. } => assert_eq!(*field, "open"),
            other => panic!("期望 Snap，得到 {other:?}"),
        }
    }
}
