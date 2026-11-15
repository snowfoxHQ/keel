//! CSV 数据接入（docs/BASELINE.md §3.1 允许边界分配 / §4.2 输入边界）。
//!
//! 确定性规则（写死，变更须走 ADR）：
//! - 格式：`ts,open,high,low,close,volume`，首行若为 `ts,` 开头按表头跳过；
//! - 价格解析：十进制字符串 → `Price`（1e-4），**纯整数解析、零浮点**；
//!   小数位 > 4 拒绝（禁止静默截断丢精度）；
//! - 数量解析：十进制字符串 → `Qty`（1e-8）；小数位 > 8 拒绝；
//! - 输入边界：`|price_raw|`、`|qty_raw|` ≥ 2^44 拒绝（§4.2，保证 i128 乘法安全）；
//! - Bar 自洽性：`high ≥ max(open,close)`、`low ≤ min(open,close)` 否则拒绝；
//! - 错误带行号（首个差异定位（§13.3）在数据层的前置）。

use keel_core::{FixedError, Price, Qty, RAW_BOUND};

/// 一根 Bar（定点化后）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    pub ts: i64,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    /// 成交量（股）。仅约束统计；v0 撮合按 §6.1 全成/全不成，不用 volume 截断。
    pub volume: Qty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataError {
    /// 空文件 / 无有效行。
    Empty,
    /// 行语法错误（列数、非数字、多点、空字段）。携带 1 起始行号。
    Syntax(usize),
    /// 小数位超精度（价格 >4 位或数量 >8 位）。携带行号。
    Precision(usize),
    /// 超出 2^44 输入边界。携带行号。
    OutOfBounds(usize),
    /// OHLC 自洽性破坏（high/low 不覆盖 open/close）。携带行号。
    InvalidBar(usize),
    /// 定点层错误（容量/构造）。
    Fixed(FixedError),
}

impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataError::Empty => write!(f, "数据为空"),
            DataError::Syntax(l) => write!(f, "第 {l} 行语法错误"),
            DataError::Precision(l) => write!(f, "第 {l} 行小数位超出定点精度"),
            DataError::OutOfBounds(l) => write!(f, "第 {l} 行超出 2^44 输入边界"),
            DataError::InvalidBar(l) => write!(f, "第 {l} 行 OHLC 自洽性破坏"),
            DataError::Fixed(e) => write!(f, "定点错误：{e}"),
        }
    }
}

impl std::error::Error for DataError {}

impl From<FixedError> for DataError {
    fn from(e: FixedError) -> Self {
        DataError::Fixed(e)
    }
}

/// 解析整个 CSV 文本（非热路径，允许分配；v0 用一次性读入，mmap 预读延后）。
pub fn parse_bars_csv(text: &str) -> Result<Vec<Bar>, DataError> {
    let mut bars = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;
        if idx == 0 && line.starts_with("ts,") {
            continue; // 表头
        }
        if line.is_empty() {
            continue;
        }
        bars.push(parse_bar_line(line, line_no)?);
    }
    if bars.is_empty() {
        return Err(DataError::Empty);
    }
    Ok(bars)
}

fn parse_bar_line(line: &str, line_no: usize) -> Result<Bar, DataError> {
    let cols: Vec<&str> = line.split(',').collect();
    if cols.len() != 6 {
        return Err(DataError::Syntax(line_no));
    }
    let ts = parse_i64(cols[0]).ok_or(DataError::Syntax(line_no))?;
    let open = parse_price(cols[1], line_no)?;
    let high = parse_price(cols[2], line_no)?;
    let low = parse_price(cols[3], line_no)?;
    let close = parse_price(cols[4], line_no)?;
    let volume = parse_volume(cols[5], line_no)?;

    // OHLC 自洽性（§6.1 数据前提；异常行在接入层拒绝，不进撮合）
    let hi = open.raw().max(close.raw()).max(high.raw());
    let lo = open.raw().min(close.raw()).min(low.raw());
    if high.raw() < hi || low.raw() > lo || high.raw() < low.raw() {
        return Err(DataError::InvalidBar(line_no));
    }
    Ok(Bar {
        ts,
        open,
        high,
        low,
        close,
        volume,
    })
}

/// 十进制定点解析（价格，1e-4）。零浮点：逐字符整数运算，确定性逐位可复现。
fn parse_price(s: &str, line_no: usize) -> Result<Price, DataError> {
    let raw = parse_scaled(s, 4, line_no)?;
    if raw.abs() >= RAW_BOUND {
        return Err(DataError::OutOfBounds(line_no));
    }
    Ok(Price::from_raw(raw))
}

/// 十进制定点解析（数量，1e-8）。进撮合的数量适用 §4.2 撮合边界（2^44）。
fn parse_qty(s: &str, line_no: usize) -> Result<Qty, DataError> {
    let raw = parse_scaled(s, 8, line_no)?;
    if raw.abs() >= RAW_BOUND {
        return Err(DataError::OutOfBounds(line_no));
    }
    Ok(Qty::from_raw(raw))
}

/// 十进制定点解析（Bar 成交量，1e-8，仅统计）。
///
/// **豁免 §4.2 撮合边界**：volume 不参与任何乘法/撮合/核算（v0 撮合全成全不成，
/// 不用 volume 截断），2^44 边界仅为 price×qty 的 i128 乘法安全服务；对只统计的
/// volume 照搬会拒绝几乎所有真实日线（数万手 = 数百万股 → raw > 2^44）。
/// 约束放宽为 i64 自然界（parse_scaled 内部 checked_mul 保证）。
fn parse_volume(s: &str, line_no: usize) -> Result<Qty, DataError> {
    let raw = parse_scaled(s, 8, line_no)?;
    Ok(Qty::from_raw_stat(raw))
}

/// `"12.34"` → `1234 × 10^(scale-2)`；小数位超 scale 返回 None。
fn parse_scaled(s: &str, scale_digits: usize, line_no: usize) -> Result<i64, DataError> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s),
    };
    if body.is_empty() {
        return Err(DataError::Syntax(line_no));
    }
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(DataError::Syntax(line_no));
    }
    if frac_part.len() > scale_digits {
        return Err(DataError::Precision(line_no));
    }
    let mut raw: i64 = 0;
    for c in int_part.chars().chain(frac_part.chars()) {
        let d = match c.to_digit(10) {
            Some(d) => d as i64,
            None => return Err(DataError::Syntax(line_no)),
        };
        raw = raw
            .checked_mul(10)
            .and_then(|v| v.checked_add(d))
            .ok_or(DataError::OutOfBounds(line_no))?;
    }
    // 右侧补零到 scale 位
    for _ in frac_part.len()..scale_digits {
        raw = raw
            .checked_mul(10)
            .ok_or(DataError::OutOfBounds(line_no))?;
    }
    Ok(if neg { -raw } else { raw })
}

fn parse_i64(s: &str) -> Option<i64> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s),
    };
    if body.is_empty() || !body.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut v: i64 = 0;
    for c in body.bytes() {
        v = v.checked_mul(10)?.checked_add((c - b'0') as i64)?;
    }
    Some(if neg { -v } else { v })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CSV: &str = "ts,open,high,low,close,volume\n1,10.00,10.50,9.80,10.20,1000\n2,10.20,10.60,10.10,10.55,800\n";

    #[test]
    fn parse_basic() {
        let bars = parse_bars_csv(CSV).unwrap();
        assert_eq!(bars.len(), 2);
        let b = &bars[0];
        assert_eq!(b.ts, 1);
        assert_eq!(b.open.raw(), 100_000); // 10.00
        assert_eq!(b.high.raw(), 105_000);
        assert_eq!(b.low.raw(), 98_000);
        assert_eq!(b.close.raw(), 102_000);
        assert_eq!(b.volume.raw(), 1000 * 100_000_000);
        assert_eq!(bars[1].close.raw(), 105_500);
    }

    #[test]
    fn no_header_also_ok() {
        let bars = parse_bars_csv("1,1.00,1.00,1.00,1.00,1\n").unwrap();
        assert_eq!(bars.len(), 1);
    }

    #[test]
    fn empty_file_rejected() {
        assert_eq!(parse_bars_csv("ts,open,high,low,close,volume\n"), Err(DataError::Empty));
    }

    #[test]
    fn syntax_error_carries_line_no() {
        // 第 2 行列数错
        let err = parse_bars_csv("ts,open,high,low,close,volume\n1,1.00,1.00,1.00\n").unwrap_err();
        assert_eq!(err, DataError::Syntax(2));
        // 非数字
        let err = parse_bars_csv("1,x.00,1.00,1.00,1.00,1\n").unwrap_err();
        assert_eq!(err, DataError::Syntax(1));
        // 多点
        let err = parse_bars_csv("1,1.0.0,1.00,1.00,1.00,1\n").unwrap_err();
        assert_eq!(err, DataError::Syntax(1));
    }

    #[test]
    fn precision_overflow_rejected() {
        // 价格第 5 位小数：拒绝（不静默截断）
        let err = parse_bars_csv("1,1.00001,1.00,1.00,1.00,1\n").unwrap_err();
        assert_eq!(err, DataError::Precision(1));
        // 数量第 9 位小数：拒绝
        let err = parse_bars_csv("1,1.00,1.00,1.00,1.00,1.000000001\n").unwrap_err();
        assert_eq!(err, DataError::Precision(1));
        // 数量 8 位小数：允许
        assert!(parse_bars_csv("1,1.00,1.00,1.00,1.00,1.12345678\n").is_ok());
    }

    #[test]
    fn raw_bound_rejected() {
        // 2^44 = 17592186044416 → 价格 raw 17592186050000 ≥ 边界，拒绝
        let line = "1,1759218605.00,1759218605.00,1759218605.00,1759218605.00,1\n";
        let err = parse_bars_csv(line).unwrap_err();
        assert_eq!(err, DataError::OutOfBounds(1));
    }

    #[test]
    fn invalid_ohlc_rejected() {
        // high < close
        let err = parse_bars_csv("1,1.00,0.90,0.80,1.00,1\n").unwrap_err();
        assert_eq!(err, DataError::InvalidBar(1));
        // low > open
        let err = parse_bars_csv("1,1.00,1.10,1.05,1.00,1\n").unwrap_err();
        assert_eq!(err, DataError::InvalidBar(1));
    }

    #[test]
    fn negative_price_ok_and_deterministic() {
        let a = parse_bars_csv("1,-1.2345,1.00,-2.00,0.50,1\n").unwrap();
        assert_eq!(a[0].open.raw(), -12_345);
        // 同输入重解析逐位一致（确定性）
        let b = parse_bars_csv("1,-1.2345,1.00,-2.00,0.50,1\n").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn scaled_parse_units() {
        assert_eq!(parse_scaled("100", 4, 1).unwrap(), 1_000_000);
        assert_eq!(parse_scaled("0.0001", 4, 1).unwrap(), 1);
        assert_eq!(parse_scaled("-0.5", 4, 1).unwrap(), -5_000);
        assert_eq!(parse_scaled(".5", 4, 1).unwrap(), 5_000);
        assert_eq!(parse_scaled("1.", 4, 1).unwrap(), 10_000);
    }
}
