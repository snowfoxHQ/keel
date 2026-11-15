//! 定点数类型（docs/BASELINE.md §4 数值契约）。
//!
//! 单位表：
//! - `Price` `i64`，1e-4
//! - `Qty`   `i64`，1e-8
//! - `Money` `i128`，1e-8
//! - `Rate`  `i64`，1e-8
//!
//! 热路径规则：所有跨类型乘除先升 i128、显式 `checked_*`，溢出/除零返回错误，
//! 不 panic、不浮点。舍入方向按 §4.3 表逐项固定。

use core::fmt;

/// 输入边界：|price_raw|、|qty_raw| < 2^44 时 price×qty < 2^88，i128 安全（§4.2）。
pub const RAW_BOUND: i64 = 1 << 44;

const PRICE_SCALE: i128 = 10_000; // 1e4
const QTY_SCALE: i128 = 100_000_000; // 1e8
const RATE_SCALE: i128 = 100_000_000; // 1e8

/// 定点运算错误。热路径返回错误，不 panic。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixedError {
    Overflow,
    DivideByZero,
    OutOfBounds,
    NonIntegralInput,
}

impl fmt::Display for FixedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FixedError::Overflow => write!(f, "定点运算溢出"),
            FixedError::DivideByZero => write!(f, "定点除零"),
            FixedError::OutOfBounds => write!(f, "超出 RAW_BOUND=2^44 输入边界"),
            FixedError::NonIntegralInput => write!(f, "浮点输入无法无损转定点"),
        }
    }
}

impl std::error::Error for FixedError {}

/// 舍入方向（§4.3）。方向由调用场景决定，不允许默认值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Round {
    /// 向 -∞ 取整（floor）。
    Down,
    /// 向 +∞ 取整（ceil）。
    Up,
    /// 向零取整（trunc，仅展示层）。
    ToZero,
}

/// 有符号整数除法并按 `Round` 方向取整。
fn div_round(v: i128, d: i128, round: Round) -> Result<i128, FixedError> {
    if d == 0 {
        return Err(FixedError::DivideByZero);
    }
    let q = v / d;
    let r = v % d;
    if r == 0 {
        return Ok(q);
    }
    Ok(match round {
        Round::ToZero => q,
        Round::Down => {
            if (r < 0) == (d < 0) || r == 0 {
                q
            } else {
                q - 1
            }
        }
        Round::Up => {
            if (r < 0) == (d < 0) {
                q + 1
            } else {
                q
            }
        }
    })
}

macro_rules! define_fixed {
    ($name:ident, $raw:ty, $scale:expr, $bound_abs:expr, $doc:expr) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(transparent)]
        pub struct $name($raw);

        impl $name {
            /// 定点原始值构造。热路径用；debug 构建下校验边界。
            #[inline]
            pub const fn from_raw(raw: $raw) -> Self {
                debug_assert!(raw.abs() <= $bound_abs);
                Self(raw)
            }

            /// 带边界校验的构造（数据接入层用）。
            #[inline]
            pub fn checked_from_raw(raw: $raw) -> Result<Self, FixedError> {
                if raw.abs() >= $bound_abs {
                    return Err(FixedError::OutOfBounds);
                }
                Ok(Self(raw))
            }

            /// 浮点边界转换：×scale 四舍五入后立即转定点（§3.2 约束 3：浮点仅允许出现在解析边界）。
            pub fn from_f64(v: f64) -> Result<Self, FixedError> {
                let scaled = v * ($scale as f64);
                if !scaled.is_finite() {
                    return Err(FixedError::NonIntegralInput);
                }
                let r = scaled.round();
                if r < (RAW_BOUND as f64) && r > -(RAW_BOUND as f64) {
                    Ok(Self(r as $raw))
                } else {
                    Err(FixedError::OutOfBounds)
                }
            }

            #[inline]
            pub const fn raw(self) -> $raw {
                self.0
            }

            #[inline]
            pub const fn scale() -> i128 {
                $scale
            }

            #[inline]
            pub fn checked_add(self, o: Self) -> Result<Self, FixedError> {
                match (self.0 as i128).checked_add(o.0 as i128) {
                    Some(v) => Self::checked_from_i128(v),
                    None => Err(FixedError::Overflow),
                }
            }

            #[inline]
            pub fn checked_sub(self, o: Self) -> Result<Self, FixedError> {
                match (self.0 as i128).checked_sub(o.0 as i128) {
                    Some(v) => Self::checked_from_i128(v),
                    None => Err(FixedError::Overflow),
                }
            }

            fn checked_from_i128(v: i128) -> Result<Self, FixedError> {
                if v < (<$raw>::MIN as i128) || v > (<$raw>::MAX as i128) {
                    return Err(FixedError::Overflow);
                }
                Ok(Self(v as $raw))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // 展示层格式化，非热路径
                let scale = $scale;
                let v = self.0 as i128;
                let int_part = v / scale;
                let frac = (v % scale).abs();
                let digits = scale.trailing_zeros() as usize;
                if v < 0 && frac != 0 {
                    write!(f, "-{}.{:0width$}", int_part.abs(), frac, width = digits)
                } else {
                    write!(f, "{}.{:0width$}", int_part, frac, width = digits)
                }
            }
        }
    };
}

define_fixed!(
    Price,
    i64,
    PRICE_SCALE,
    RAW_BOUND,
    "价格，i64 定点，单位 1e-4（原始价 ×10⁴）。"
);
define_fixed!(
    Qty,
    i64,
    QTY_SCALE,
    RAW_BOUND,
    "数量，i64 定点，单位 1e-8（原始量 ×10⁸）。"
);

impl Qty {
    /// 统计专用构造：豁免 §4.2 撮合边界（2^44）。
    ///
    /// 仅用于**不参与乘法/撮合/核算**的量（当前唯一场景：Bar 成交量，v0 撮合
    /// 全成全不成、不用 volume 截断）。进撮合的下单数量一律 `from_raw` /
    /// `checked_from_raw`。raw 值域即 i64 自然界（调用方解析层 checked 保证）。
    #[inline]
    pub const fn from_raw_stat(raw: i64) -> Self {
        Self(raw)
    }
}
define_fixed!(
    Rate,
    i64,
    RATE_SCALE,
    RAW_BOUND,
    "比率（费率/税率/滑点），i64 定点，单位 1e-8。"
);
define_fixed!(
    Money,
    i128,
    QTY_SCALE,
    i128::MAX,
    "金额，i128 定点，单位 1e-8。公式：price_raw × qty_raw / 10⁴。"
);

impl Money {
    /// 零值。
    pub const ZERO: Money = Money(0);

    /// 取负（现金流出方向标记用；不溢出——i128 负界比正界宽 1）。
    #[inline]
    pub const fn neg(self) -> Self {
        Money(-self.0)
    }

    /// 价格 × 数量 → 金额，指定舍入方向（§4.3：买方向上、卖方向下）。
    #[inline]
    pub fn price_qty(p: Price, q: Qty, round: Round) -> Result<Money, FixedError> {
        let v = (p.raw() as i128)
            .checked_mul(q.raw() as i128)
            .ok_or(FixedError::Overflow)?;
        // v 单位 1e-12，除 1e4 得 Money(1e-8)
        div_round(v, PRICE_SCALE, round).map(Money)
    }

    /// 比率费用（手续费/印花税等）：amount × rate，方向向上（ceil，§4.3）。
    #[inline]
    pub fn fee_ceil(self, rate: Rate) -> Result<Money, FixedError> {
        let v = self
            .0
            .checked_mul(rate.raw() as i128)
            .ok_or(FixedError::Overflow)?;
        // v 单位 1e-16，除 1e8 得 Money(1e-8)
        div_round(v, RATE_SCALE, Round::Up).map(Money)
    }

    /// 金额 ÷ 数量 → 单价。余数按 §4.3 均价摊分规则由调用方归入末笔，不得静默丢弃。
    /// 返回 (单价, 余数)。余数单位 1e-12 元（numerator 的原生精度：Money raw ×1e4 − price_raw × qty_raw），
    /// 恒满足 |余数| < 数量 × 1e-4 元；金额本身无法表示的 sub-1e-8 部分包含在其中，调用方摊分时一并处理。
    pub fn div_qty(self, q: Qty, round: Round) -> Result<(Price, i128), FixedError> {
        if q.raw() == 0 {
            return Err(FixedError::DivideByZero);
        }
        // price_raw = money_raw × 1e4 / qty_raw；money_raw 由 price×qty 生成时 |m| ≤ 2^88，×1e4 < 2^102 安全
        let num = self.0.checked_mul(PRICE_SCALE).ok_or(FixedError::Overflow)?;
        let price_raw = div_round(num, q.raw() as i128, round)?;
        if price_raw.abs() >= RAW_BOUND as i128 {
            return Err(FixedError::OutOfBounds);
        }
        let rem = num - price_raw * (q.raw() as i128);
        Ok((Price(price_raw as i64), rem))
    }

    /// 精确比较用的原始值（Reconciliation）。
    #[inline]
    pub const fn raw_i128(self) -> i128 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 单位与边界 ----

    #[test]
    fn raw_bounds() {
        assert!(Price::checked_from_raw(RAW_BOUND).is_err());
        assert!(Price::checked_from_raw(RAW_BOUND - 1).is_ok());
        assert!(Price::checked_from_raw(-(RAW_BOUND - 1)).is_ok());
        assert!(Qty::checked_from_raw(RAW_BOUND).is_err());
    }

    #[test]
    fn overflow_errors_not_panics() {
        // Money 溢出路径（i128 上限）
        assert_eq!(
            Money(i128::MAX).checked_add(Money(1)),
            Err(FixedError::Overflow)
        );
        // 边界内的最大 price × qty 不溢出（2^88 < 2^127），证明 RAW_BOUND 设计成立
        let p = Price::checked_from_raw(RAW_BOUND - 1).unwrap();
        let q = Qty::checked_from_raw(RAW_BOUND - 1).unwrap();
        assert!(Money::price_qty(p, q, Round::Up).is_ok());
    }

    // ---- price × qty 公式与舍入方向 ----

    #[test]
    fn price_qty_exact() {
        // 123.4567 × 2.5 → 308.64175
        let p = Price::from_raw(1_234_567);
        let q = Qty::from_raw(250_000_000);
        let m = Money::price_qty(p, q, Round::Up).unwrap();
        assert_eq!(m.raw_i128(), 30_864_175_000); // 308.64175 元 @1e-8
    }

    #[test]
    fn price_qty_rounding_directions() {
        // 0.0001 × 0.00000001 → 1e-12，除 1e4 得 1e-16：无法整除
        let p = Price::from_raw(1);
        let q = Qty::from_raw(1);
        assert_eq!(
            Money::price_qty(p, q, Round::Up).unwrap().raw_i128(),
            1, // ceil: 0.00000001
        );
        assert_eq!(
            Money::price_qty(p, q, Round::Down).unwrap().raw_i128(),
            0, // floor
        );
    }

    #[test]
    fn price_qty_negative_symmetry() {
        // 空头方向：负价格或负数量的符号语义保持 (p*q) 符号
        let p = Price::from_raw(-10_000); // -1.0
        let q = Qty::from_raw(300_000_000); // 3.0
        let m = Money::price_qty(p, q, Round::Up).unwrap();
        assert_eq!(m.raw_i128(), -300_000_000); // -3.0 精确无舍入
    }

    // ---- 费用 ----

    #[test]
    fn fee_ceil_direction() {
        // 100 元 × 万分之3 手续费 = 0.03 精确
        let amt = Money(10_000_000_000);
        let rate = Rate::from_raw(30_000); // 3e-4
        assert_eq!(amt.fee_ceil(rate).unwrap().raw_i128(), 3_000_000);

        // 不能整除时向上：100 元 × 1e-9
        let rate2 = Rate::from_raw(100); // 1e-6... 用 1e-6: 0.0001
        let r = amt.fee_ceil(rate2).unwrap().raw_i128(); // 100*1e-6 = 1e-4 元 = 10000 @1e-8 精确
        assert_eq!(r, 10_000);
    }

    #[test]
    fn fee_ceil_rounds_up_on_remainder() {
        // 3 元 × 0.33333333 = 0.99999999 → 精确无舍入
        let amt = Money(300_000_000); // 3.0
        let rate = Rate::from_raw(33_333_333); // 0.33333333
        let f = amt.fee_ceil(rate).unwrap().raw_i128();
        assert_eq!(f, 99_999_999);
        // 真正有余数：0.03 元 × 0.33333333 = 0.0099999999 元 = 999999.99 @1e-8 → ceil = 1000000
        let amt2 = Money(3_000_000); // 0.03
        let rate3 = Rate::from_raw(33_333_333);
        let f2 = amt2.fee_ceil(rate3).unwrap().raw_i128();
        assert_eq!(f2, 1_000_000);
    }

    // ---- 均价摊分 ----

    #[test]
    fn div_qty_with_remainder() {
        // 100.01 元 ÷ 3 股 = 33.336666...：floor 单价 33.3366，ceil 单价 33.3367
        let m = Money(10_001_000_000); // 100.01
        let q = Qty::from_raw(300_000_000); // 3
        let (floor_p, rem_floor) = m.div_qty(q, Round::Down).unwrap();
        assert_eq!(floor_p.raw(), 333_366); // 33.3366
        // 余数 = 100.01 − 33.3366×3 = 0.0002 元，@1e-12 单位
        assert_eq!(rem_floor, 200_000_000);
        let (ceil_p, rem_ceil) = m.div_qty(q, Round::Up).unwrap();
        assert_eq!(ceil_p.raw(), 333_367);
        // ceil 后总价 100.0101 超出 100.01，余数 −0.0001 元
        assert_eq!(rem_ceil, -100_000_000);
    }

    #[test]
    fn div_qty_zero_errors() {
        let m = Money(1);
        assert_eq!(m.div_qty(Qty::from_raw(0), Round::Up), Err(FixedError::DivideByZero));
    }

    // ---- 浮点边界 ----

    #[test]
    fn from_f64_boundary() {
        assert_eq!(Price::from_f64(123.4567).unwrap().raw(), 1_234_567);
        assert!(Price::from_f64(f64::NAN).is_err());
        assert!(Price::from_f64(1e14).is_err()); // 超出 2^44/1e4
        assert!(Qty::from_f64(-2.5).is_ok());
    }

    // ---- checked 加减 ----

    #[test]
    fn checked_add_sub_overflow() {
        let a = Money(i128::MAX);
        assert_eq!(
            a.checked_add(Money(1)),
            Err(FixedError::Overflow)
        );
        // 边界内的 Price 加法不溢出（2^45 << i64::MAX）
        let p1 = Price::from_raw(RAW_BOUND - 1);
        let p2 = Price::from_raw(RAW_BOUND - 1);
        assert!(p1.checked_add(p2).is_ok());
    }

    // ---- 展示 ----

    #[test]
    fn display_formatting() {
        assert_eq!(Price::from_raw(1_234_567).to_string(), "123.4567");
        assert_eq!(Price::from_raw(-1_234_567).to_string(), "-123.4567");
        assert_eq!(Money(30_864_175_000).to_string(), "308.64175000");
    }
}
