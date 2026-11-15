//! Trade Ledger：成交事实层（docs/BASELINE.md §9，Truth First）。
//!
//! 原则：Ledger 是**唯一事实记录**，PnlState 是它的派生缓存。
//! 追加式、固定容量、不可变（append 后只能读）；每笔事实带费用与资金变动。
//! 热路径零分配：容量耗尽返回结构化错误，绝不扩容。

use keel_core::hash::Xxh3;
use keel_core::{Money, Price, Qty};

/// 成交方向（keel-pnl 自有定义：只消费 FillEvent，不依赖 keel-book，§9）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// 买入（现金流出）。
    Buy,
    /// 卖出（现金流入）。
    Sell,
}

/// 费用模型（§12 费用最小集；全部定点 Rate 1e-8，舍入按 §4.3 ceil）。
#[derive(Clone, Copy, Debug)]
pub struct FeeModel {
    /// 手续费率（按成交金额比例，ceil）。
    pub commission: keel_core::Rate,
    /// 印花税率（仅卖出收取，ceil）；v0 A 股场景，0 表示关闭。
    pub stamp_duty: keel_core::Rate,
    /// 过户费率（双边收取，ceil）；0 表示关闭。
    pub transfer_fee: keel_core::Rate,
}

impl FeeModel {
    pub const ZERO: FeeModel = FeeModel {
        commission: keel_core::Rate::from_raw(0),
        stamp_duty: keel_core::Rate::from_raw(0),
        transfer_fee: keel_core::Rate::from_raw(0),
    };

    /// 某笔成交的总费用（ceil 累加各项；side 决定印花税是否收取）。
    pub fn total_fee(&self, amount: Money, side: Side) -> Result<Money, LedgerError> {
        let fee = amount
            .fee_ceil(self.commission)
            .map_err(|_| LedgerError::Overflow)?;
        let fee = if side == Side::Sell {
            fee.checked_add(amount.fee_ceil(self.stamp_duty).map_err(|_| LedgerError::Overflow)?)
        } else {
            Ok(fee)
        }
        .map_err(|_| LedgerError::Overflow)?;
        fee.checked_add(amount.fee_ceil(self.transfer_fee).map_err(|_| LedgerError::Overflow)?)
            .map_err(|_| LedgerError::Overflow)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerError {
    /// 容量耗尽（§3.1：满载返回结构化错误，不扩容）。
    Full,
    /// 数量非法（≤ 0）。
    InvalidQty,
    /// 费用或金额溢出。
    Overflow,
}

/// 一条不可变交易事实（§9：成交事实——价格、数量、方向、费用、资金变动）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedgerEntry {
    /// 全局单调序号（入账顺序，与 EventKey.seq 同源递增）。
    pub seq: u64,
    /// 事件时间戳（原样记录，不参与费用计算）。
    pub ts: i64,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    /// 本笔总费用（各项 ceil 之和，保守不利，§4.3）。
    pub fee: Money,
    /// 本笔资金变动：卖出 = floor(成交金额) − fee；买入 = −ceil(成交金额) − fee（§4.3）。
    pub cash_delta: Money,
}

impl LedgerEntry {
    /// 成交金额（买 ceil / 卖 floor，§4.3 舍入方向表）。
    pub fn gross_amount(&self) -> Result<Money, LedgerError> {
        Money::price_qty(
            self.price,
            self.qty,
            if self.side == Side::Buy {
                keel_core::fixed::Round::Up
            } else {
                keel_core::fixed::Round::Down
            },
        )
        .map_err(|_| LedgerError::Overflow)
    }
}

/// 追加式交易账本（固定容量，XXH3 事实链）。
/// 容量构造期预分配，`record` 只在预分配容量内 push（零重分配）。
pub struct TradeLedger {
    entries: Vec<LedgerEntry>,
    next_seq: u64,
    /// 事实链摘要：逐条累计 XXH3（§13 事件流累计哈希在核算侧的对应物）。
    chain: u64,
}

impl TradeLedger {
    /// 构造（构造期分配，非热路径）。
    pub fn with_capacity(cap: usize) -> Self {
        TradeLedger {
            entries: Vec::with_capacity(cap),
            next_seq: 0,
            chain: 0,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    /// 事实链摘要（追加式累计，任何历史事实不可变——改变即断链）。
    #[inline]
    pub fn chain_hash(&self) -> u64 {
        self.chain
    }

    /// 记录一笔成交事实（含费用与资金变动计算）。满载返回 [`LedgerError::Full`]。
    ///
    /// 计算（§4.3 舍入方向表）：
    /// - 成交金额：买方 ceil、卖方 floor；
    /// - 手续费/印花税（仅卖出）/过户费：各自 ceil 后求和；
    /// - cash_delta：卖 = floor(金额) − fee；买 = −ceil(金额) − fee。
    pub fn record(
        &mut self,
        ts: i64,
        side: Side,
        price: Price,
        qty: Qty,
        fees: &FeeModel,
    ) -> Result<&LedgerEntry, LedgerError> {
        if qty.raw() <= 0 {
            return Err(LedgerError::InvalidQty);
        }
        if self.entries.len() >= self.entries.capacity() {
            return Err(LedgerError::Full);
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1); // 溢出绕回：事实链本身保证可检测

        let gross = {
            let round = if side == Side::Buy {
                keel_core::fixed::Round::Up
            } else {
                keel_core::fixed::Round::Down
            };
            Money::price_qty(price, qty, round).map_err(|_| LedgerError::Overflow)?
        };
        let fee = fees.total_fee(gross, side)?;
        let cash_delta = match side {
            Side::Sell => gross.checked_sub(fee).map_err(|_| LedgerError::Overflow)?,
            // 买入为现金流出，取负
            Side::Buy => gross
                .checked_add(fee)
                .map_err(|_| LedgerError::Overflow)?
                .neg(),
        };

        let e = LedgerEntry {
            seq,
            ts,
            side,
            price,
            qty,
            fee,
            cash_delta,
        };

        // 事实链：chain = XXH3(旧 chain ‖ 本条固定编码)
        let mut buf = [0u8; 72];
        encode_entry(&e, &mut buf);
        let mut h = Xxh3::new();
        h.update(&self.chain.to_le_bytes());
        h.update(&buf);
        self.chain = h.digest();

        self.entries.push(e);
        Ok(&self.entries[self.entries.len() - 1])
    }

    /// 只读访问第 i 条事实。
    pub fn get(&self, i: usize) -> Option<&LedgerEntry> {
        self.entries.get(i)
    }

    /// 已记录事实的迭代器。
    pub fn iter(&self) -> impl Iterator<Item = &LedgerEntry> {
        self.entries.iter()
    }

    /// 现金变动合计（Σ cash_delta；与逐条 cash_delta 之和必须精确相等——Reconciliation 用）。
    pub fn total_cash_delta(&self) -> Result<Money, LedgerError> {
        let mut acc = Money::ZERO;
        for e in self.iter() {
            acc = acc
                .checked_add(e.cash_delta)
                .map_err(|_| LedgerError::Overflow)?;
        }
        Ok(acc)
    }

    /// 费用合计。
    pub fn total_fee(&self) -> Result<Money, LedgerError> {
        let mut acc = Money::ZERO;
        for e in self.iter() {
            acc = acc.checked_add(e.fee).map_err(|_| LedgerError::Overflow)?;
        }
        Ok(acc)
    }
}

/// 固定小端编码（§13：哈希前序列化字段顺序与宽度固定；改编码必须升版本）。
/// 布局：seq(8) ts(8) side(1) price(8) qty(8) fee(16) cash(16) = 65 字节 + 7 字节版本哨兵 = 72。
fn encode_entry(e: &LedgerEntry, buf: &mut [u8; 72]) {
    let mut off = 0usize;
    let put = |bytes: &[u8], buf: &mut [u8; 72], off: &mut usize| {
        buf[*off..*off + bytes.len()].copy_from_slice(bytes);
        *off += bytes.len();
    };
    put(&e.seq.to_le_bytes(), buf, &mut off); // 8
    put(&e.ts.to_le_bytes(), buf, &mut off); // 8
    put(&(e.side as u8).to_le_bytes(), buf, &mut off); // 1
    put(&e.price.raw().to_le_bytes(), buf, &mut off); // 8
    put(&e.qty.raw().to_le_bytes(), buf, &mut off); // 8
    put(&e.fee.raw_i128().to_le_bytes(), buf, &mut off); // 16
    put(&e.cash_delta.raw_i128().to_le_bytes(), buf, &mut off); // 16
    // 尾部 7 字节版本哨兵，防尾随噪声
    buf[65..72].copy_from_slice(&[0x4B, 0x45, 0x45, 0x4C, 0x56, 0x30, 0x31]);
}

/// 从事实流重建账本链摘要（Reconciliation：重放核对用）。
pub fn replay_chain_hash(entries: &[LedgerEntry]) -> u64 {
    let mut chain = 0u64;
    for e in entries {
        let mut buf = [0u8; 72];
        encode_entry(e, &mut buf);
        let mut h = Xxh3::new();
        h.update(&chain.to_le_bytes());
        h.update(&buf);
        chain = h.digest();
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::fixed::Round;

    fn p(raw: i64) -> Price {
        Price::from_raw(raw)
    }
    fn q(raw: i64) -> Qty {
        Qty::from_raw(raw)
    }

    const FEE: FeeModel = FeeModel {
        commission: keel_core::Rate::from_raw(300),     // 3e-6 ≈ 万3
        stamp_duty: keel_core::Rate::from_raw(1_000_000), // 1e-2（仅卖出）
        transfer_fee: keel_core::Rate::from_raw(10_000), // 1e-4
    };

    #[test]
    fn buy_cash_delta_is_ceil_plus_fees() {
        let mut l = TradeLedger::with_capacity(8);
        // 买 10.00 × 100 股 = 1000.00 元，无余数
        let e = l
            .record(0, Side::Buy, p(100_000), q(10_000_000_000), &FEE)
            .unwrap();
        // 成交金额 = 10.00 × 100 股 = 1000.00 元 @1e-8 = 100_000_000_000
        // 手续费 ceil(1000 × 3e-6) = 0.003 @1e-8 = 300000
        // 过户费 1000 × 1e-4 = 0.1 @1e-8 = 10000000
        assert_eq!(e.fee.raw_i128(), 300_000 + 10_000_000);
        // 买入：−(1000.00 + 0.103) 元
        assert_eq!(e.cash_delta.raw_i128(), -(100_000_000_000 + 10_300_000));
    }

    #[test]
    fn sell_gross_floors_and_stamp_duty_applies() {
        let mut l = TradeLedger::with_capacity(8);
        // 卖 10.005 × 3 股 = 30.015 元：floor @1e-8 = 3_001_500_000
        let e = l
            .record(0, Side::Sell, p(100_050), q(300_000_000), &FEE)
            .unwrap();
        let gross = e.gross_amount().unwrap();
        assert_eq!(gross.raw_i128(), 3_001_500_000);
        // 印花税 ceil(30.015 × 1e-2) + 手续费 + 过户费
        let expect_fee = Money::price_qty(p(100_050), q(300_000_000), Round::Up)
            .unwrap()
            .fee_ceil(FEE.commission)
            .unwrap()
            .checked_add(Money::price_qty(p(100_050), q(300_000_000), Round::Up).unwrap().fee_ceil(FEE.stamp_duty).unwrap())
            .unwrap()
            .checked_add(Money::price_qty(p(100_050), q(300_000_000), Round::Up).unwrap().fee_ceil(FEE.transfer_fee).unwrap())
            .unwrap();
        assert_eq!(e.fee, expect_fee);
        // cash = floor(gross) − fee
        assert_eq!(e.cash_delta, gross.checked_sub(e.fee).unwrap());
    }

    #[test]
    fn full_rejects_no_grow() {
        let mut l = TradeLedger::with_capacity(2);
        l.record(0, Side::Buy, p(100_000), q(1_000_000_000), &FEE).unwrap();
        l.record(1, Side::Buy, p(100_000), q(1_000_000_000), &FEE).unwrap();
        assert_eq!(
            l.record(2, Side::Buy, p(100_000), q(1_000_000_000), &FEE),
            Err(LedgerError::Full)
        );
        assert_eq!(l.capacity(), 2);
    }

    #[test]
    fn chain_hash_is_order_sensitive() {
        let mut a = TradeLedger::with_capacity(8);
        let mut b = TradeLedger::with_capacity(8);
        a.record(0, Side::Buy, p(100_000), q(1_000_000_000), &FEE).unwrap();
        a.record(1, Side::Sell, p(101_000), q(1_000_000_000), &FEE).unwrap();
        b.record(0, Side::Sell, p(101_000), q(1_000_000_000), &FEE).unwrap();
        b.record(1, Side::Buy, p(100_000), q(1_000_000_000), &FEE).unwrap();
        assert_ne!(a.chain_hash(), b.chain_hash());

        // 事实相同顺序相同 → 链一致；replay 重建与追加累计一致
        let entries: Vec<LedgerEntry> = a.iter().copied().collect();
        assert_eq!(replay_chain_hash(&entries), a.chain_hash());
    }
}
