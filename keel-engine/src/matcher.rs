//! BarMatcher 保守撮合层（docs/BASELINE.md §6.1）。
//!
//! 独立于订单簿（不建价格档位），单 Bar 级判定。全部规则写死如下（变更须走 ADR）：
//!
//! 1. **OHLC 保守路径**：多单 O→L→H→C，空单 O→H→L→C（判定只用 L/H，不假设顺序收益）；
//! 2. **限价买**：`Low <= limit` 才可能成交，成交价 `min(open, limit)`（开盘即满足按开盘价）；
//!    限价卖对称：`High >= limit`，成交价 `max(open, limit)`；
//! 3. **市价**：开盘价 ± 固定定点滑点（§12），不利方向（买加卖减）；
//! 4. **止损成交**：GuardEngine 触发输出 `Triggered.at`（跳空=开盘价、触及=触发价）作为
//!    市价基准，再按规则 3 加滑点——触发价不参与成交定价（§8.2）；
//! 5. **不可成交**：一字板（O=H=L=C）全拒；候选成交价越涨跌停界（可配置）拒；
//!    最小交易单位（手数）不满足拒；数量为零拒；
//! 6. **部分成交**：v0 简化为全成/全不成（§6.1 允许），Bar 成交量不参与截断；
//! 7. **同 Bar 多订单**：由引擎按 `(ts, phase, seq)` 全序逐单独立判定，互不串价。
//!
//! 订单在 Bar 内不可达（限价未触及）返回 `Rejected::OutOfReach`，由引擎层决定挂起
//! 重试语义（下一根 Bar 重新提交，确定性同序）。

use keel_core::{Price, Qty};
use keel_data::Bar;
use keel_pnl::Side;

/// 撮合配置（全部定点，§12 市场规则最小集）。
#[derive(Clone, Copy, Debug)]
pub struct MatcherConfig {
    /// 市价单/止损成交固定滑点（价格 raw 偏移，不利方向）。
    pub slippage_raw: i64,
    /// 涨停价（None = 不启用涨跌停判定）。
    pub limit_up: Option<Price>,
    /// 跌停价。
    pub limit_down: Option<Price>,
    /// 最小交易单位（Qty raw；0 = 不启用）。
    pub min_lot_raw: i64,
}

/// 订单意图（策略信号 / Guard 触发统一形态）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderKind {
    Limit { limit: Price },
    Market,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderIntent {
    pub side: Side,
    pub kind: OrderKind,
    pub qty: Qty,
}

/// 成交结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fill {
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
}

/// 拒绝原因（§6.1：不可成交判定必须记录原因）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotTraded {
    /// 数量为零。
    ZeroQty,
    /// 数量不是最小交易单位整数倍。
    MinLot,
    /// 一字板（O=H=L=C）：无价格路径，v0 全拒。
    OnePriceBoard,
    /// 限价单在 Bar 内不可达。
    OutOfReach,
    /// 买方候选成交价越涨停。
    LimitUp,
    /// 卖方候选成交价越跌停。
    LimitDown,
    /// 候选价定点溢出。
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchOutcome {
    Filled(Fill),
    Rejected(NotTraded),
}

/// Bar 撮合器（无状态：全部判定数据来自 Bar + 配置 + 单笔订单）。
pub struct BarMatcher {
    cfg: MatcherConfig,
}

impl BarMatcher {
    pub fn new(cfg: MatcherConfig) -> Self {
        BarMatcher { cfg }
    }

    /// 策略/常规订单按 Bar 判定。
    pub fn match_order(&self, intent: &OrderIntent, bar: &Bar) -> MatchOutcome {
        if intent.qty.raw() == 0 {
            return MatchOutcome::Rejected(NotTraded::ZeroQty);
        }
        if self.cfg.min_lot_raw > 0 && intent.qty.raw() % self.cfg.min_lot_raw != 0 {
            return MatchOutcome::Rejected(NotTraded::MinLot);
        }
        // 一字板：无价格路径（保守：v0 全拒，含限价单——规则写死）
        if bar.open.raw() == bar.high.raw()
            && bar.high.raw() == bar.low.raw()
            && bar.low.raw() == bar.close.raw()
        {
            return MatchOutcome::Rejected(NotTraded::OnePriceBoard);
        }
        // 候选成交价（§6.1 规则 2/3）
        let candidate = match (intent.side, intent.kind) {
            (_, OrderKind::Market) => {
                let slip = self.cfg.slippage_raw;
                match intent.side {
                    Side::Buy => Price::checked_from_raw(
                        bar.open.raw().checked_add(slip).unwrap_or(i64::MAX),
                    ),
                    Side::Sell => Price::checked_from_raw(
                        bar.open.raw().checked_sub(slip).unwrap_or(i64::MIN),
                    ),
                }
            }
            (Side::Buy, OrderKind::Limit { limit }) => {
                if bar.low.raw() > limit.raw() {
                    return MatchOutcome::Rejected(NotTraded::OutOfReach);
                }
                Ok(if bar.open.raw() <= limit.raw() {
                    bar.open
                } else {
                    limit
                })
            }
            (Side::Sell, OrderKind::Limit { limit }) => {
                if bar.high.raw() < limit.raw() {
                    return MatchOutcome::Rejected(NotTraded::OutOfReach);
                }
                Ok(if bar.open.raw() >= limit.raw() {
                    bar.open
                } else {
                    limit
                })
            }
        };
        let price = match candidate {
            Ok(p) => p,
            Err(_) => return MatchOutcome::Rejected(NotTraded::Overflow),
        };
        // 涨跌停判定（候选价确定后；§6.1 规则 5）
        if intent.side == Side::Buy {
            if let Some(up) = self.cfg.limit_up {
                if price.raw() > up.raw() {
                    return MatchOutcome::Rejected(NotTraded::LimitUp);
                }
            }
        } else if let Some(down) = self.cfg.limit_down {
            if price.raw() < down.raw() {
                return MatchOutcome::Rejected(NotTraded::LimitDown);
            }
        }
        MatchOutcome::Filled(Fill {
            side: intent.side,
            price,
            qty: intent.qty,
        })
    }

    /// GuardEngine 触发输出 → 止损市价成交（§6.1 规则 4：at 为市价基准，跳空已体现）。
    pub fn match_stop(
        &self,
        side: keel_guard::TriggerSide,
        at: Price,
        qty: Qty,
        bar: &Bar,
    ) -> MatchOutcome {
        if qty.raw() == 0 {
            return MatchOutcome::Rejected(NotTraded::ZeroQty);
        }
        if self.cfg.min_lot_raw > 0 && qty.raw() % self.cfg.min_lot_raw != 0 {
            return MatchOutcome::Rejected(NotTraded::MinLot);
        }
        if bar.open.raw() == bar.high.raw()
            && bar.high.raw() == bar.low.raw()
            && bar.low.raw() == bar.close.raw()
        {
            return MatchOutcome::Rejected(NotTraded::OnePriceBoard);
        }
        let slip = self.cfg.slippage_raw;
        // SellStop 保护多头 → 卖出（不利：价减）；BuyStop 保护空头 → 买入（价加）
        let (pnl_side, candidate) = match side {
            keel_guard::TriggerSide::SellStop => (
                Side::Sell,
                Price::checked_from_raw(at.raw().checked_sub(slip).unwrap_or(i64::MIN)),
            ),
            keel_guard::TriggerSide::BuyStop => (
                Side::Buy,
                Price::checked_from_raw(at.raw().checked_add(slip).unwrap_or(i64::MAX)),
            ),
        };
        let price = match candidate {
            Ok(p) => p,
            Err(_) => return MatchOutcome::Rejected(NotTraded::Overflow),
        };
        match pnl_side {
            Side::Buy => {
                if let Some(up) = self.cfg.limit_up {
                    if price.raw() > up.raw() {
                        return MatchOutcome::Rejected(NotTraded::LimitUp);
                    }
                }
            }
            Side::Sell => {
                if let Some(down) = self.cfg.limit_down {
                    if price.raw() < down.raw() {
                        return MatchOutcome::Rejected(NotTraded::LimitDown);
                    }
                }
            }
        }
        MatchOutcome::Filled(Fill {
            side: pnl_side,
            price,
            qty,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::Qty;

    fn p(raw: i64) -> Price {
        Price::from_raw(raw)
    }
    fn bar(o: i64, h: i64, l: i64, c: i64) -> Bar {
        Bar {
            ts: 1,
            open: p(o),
            high: p(h),
            low: p(l),
            close: p(c),
            volume: Qty::from_raw(100 * 100_000_000),
        }
    }
    fn cfg() -> MatcherConfig {
        MatcherConfig {
            slippage_raw: 100, // 0.01 元
            limit_up: None,
            limit_down: None,
            min_lot_raw: 0,
        }
    }
    fn buy_limit(limit: i64, qty_shares: i64) -> OrderIntent {
        OrderIntent {
            side: Side::Buy,
            kind: OrderKind::Limit { limit: p(limit) },
            qty: Qty::from_raw(qty_shares * 100_000_000),
        }
    }
    fn buy_market(qty_shares: i64) -> OrderIntent {
        OrderIntent {
            side: Side::Buy,
            kind: OrderKind::Market,
            qty: Qty::from_raw(qty_shares * 100_000_000),
        }
    }

    /// 限价买三态（§6.1 规则 2）。
    #[test]
    fn limit_buy_three_states() {
        let m = BarMatcher::new(cfg());
        // 跳空低开：open 9.00 < limit 9.50 → 按开盘价 9.00（不利原则）
        let out = m.match_order(&buy_limit(95_000, 100), &bar(90_000, 92_000, 88_000, 91_000));
        assert_eq!(
            out,
            MatchOutcome::Filled(Fill {
                side: Side::Buy,
                price: p(90_000),
                qty: Qty::from_raw(100 * 100_000_000)
            })
        );
        // Bar 内触及：open 9.80 > limit 9.50，low 9.40 ≤ 9.50 → 成交价 = limit
        let out = m.match_order(&buy_limit(95_000, 100), &bar(98_000, 99_000, 94_000, 97_000));
        assert_eq!(out, MatchOutcome::Filled(Fill { side: Side::Buy, price: p(95_000), qty: Qty::from_raw(100 * 100_000_000) }));
        // 不可达：low 9.60 > limit 9.50
        let out = m.match_order(&buy_limit(95_000, 100), &bar(98_000, 99_000, 96_000, 97_000));
        assert_eq!(out, MatchOutcome::Rejected(NotTraded::OutOfReach));
    }

    /// 限价卖对称（成交价 = max(open, limit)）。
    #[test]
    fn limit_sell_symmetric() {
        let m = BarMatcher::new(cfg());
        let sell = OrderIntent {
            side: Side::Sell,
            kind: OrderKind::Limit { limit: p(105_000) },
            qty: Qty::from_raw(100 * 100_000_000),
        };
        // 跳空高开：open 10.80 > limit 10.50 → 按开盘价
        let out = m.match_order(&sell, &bar(108_000, 110_000, 106_000, 109_000));
        assert_eq!(out, MatchOutcome::Filled(Fill { side: Side::Sell, price: p(108_000), qty: Qty::from_raw(100 * 100_000_000) }));
        // Bar 内触及：open 10.20 < limit 10.50，high 10.60 ≥ 10.50 → 成交价 = limit
        let out = m.match_order(&sell, &bar(102_000, 106_000, 101_000, 104_000));
        assert_eq!(out, MatchOutcome::Filled(Fill { side: Side::Sell, price: p(105_000), qty: Qty::from_raw(100 * 100_000_000) }));
        // 不可达
        let out = m.match_order(&sell, &bar(102_000, 104_000, 101_000, 103_000));
        assert_eq!(out, MatchOutcome::Rejected(NotTraded::OutOfReach));
    }

    /// 市价单滑点：买加卖减（§6.1 规则 3）。
    #[test]
    fn market_slippage_adverse() {
        let m = BarMatcher::new(cfg());
        let out = m.match_order(&buy_market(100), &bar(100_000, 102_000, 99_000, 101_000));
        assert_eq!(out, MatchOutcome::Filled(Fill { side: Side::Buy, price: p(100_100), qty: Qty::from_raw(100 * 100_000_000) }));
        let sell = OrderIntent { side: Side::Sell, kind: OrderKind::Market, qty: Qty::from_raw(100 * 100_000_000) };
        let out = m.match_order(&sell, &bar(100_000, 102_000, 99_000, 101_000));
        assert_eq!(out, MatchOutcome::Filled(Fill { side: Side::Sell, price: p(99_900), qty: Qty::from_raw(100 * 100_000_000) }));
    }

    /// 止损成交：guard at 为基准再加拉点（卖减），跳空 at=开盘价已体现。
    #[test]
    fn stop_fill_uses_guard_reference_price() {
        let m = BarMatcher::new(cfg());
        // guard 判定跳空 at = open 90.00 → 成交 90.00 − 0.01 = 89.99
        let out = m.match_stop(
            keel_guard::TriggerSide::SellStop,
            p(90_000),
            Qty::from_raw(100 * 100_000_000),
            &bar(90_000, 92_000, 88_000, 91_000),
        );
        assert_eq!(out, MatchOutcome::Filled(Fill { side: Side::Sell, price: p(89_900), qty: Qty::from_raw(100 * 100_000_000) }));
        // Bar 内触及 at = 触发价 95.00 → 成交 94.99
        let out = m.match_stop(
            keel_guard::TriggerSide::SellStop,
            p(95_000),
            Qty::from_raw(100 * 100_000_000),
            &bar(98_000, 99_000, 94_000, 97_000),
        );
        assert_eq!(out, MatchOutcome::Filled(Fill { side: Side::Sell, price: p(94_900), qty: Qty::from_raw(100 * 100_000_000) }));
    }

    /// 不可成交判定：一字板全拒、涨跌停、手数、零数量。
    #[test]
    fn rejects() {
        let m = BarMatcher::new(cfg());
        // 一字板
        let out = m.match_order(&buy_market(100), &bar(100_000, 100_000, 100_000, 100_000));
        assert_eq!(out, MatchOutcome::Rejected(NotTraded::OnePriceBoard));
        // 零数量
        let out = m.match_order(&buy_market(0), &bar(100_000, 102_000, 99_000, 101_000));
        assert_eq!(out, MatchOutcome::Rejected(NotTraded::ZeroQty));

        // 涨跌停：买候选 10.01 > 涨停 10.00 → 拒
        let m2 = BarMatcher::new(MatcherConfig {
            limit_up: Some(p(100_000)),
            ..cfg()
        });
        let out = m2.match_order(&buy_market(100), &bar(100_000, 102_000, 99_000, 101_000));
        assert_eq!(out, MatchOutcome::Rejected(NotTraded::LimitUp));
        // 卖候选 9.99 < 跌停 10.00 → 拒
        let m3 = BarMatcher::new(MatcherConfig {
            limit_down: Some(p(100_000)),
            ..cfg()
        });
        let sell = OrderIntent { side: Side::Sell, kind: OrderKind::Market, qty: Qty::from_raw(100 * 100_000_000) };
        let out = m3.match_order(&sell, &bar(100_000, 102_000, 99_000, 101_000));
        assert_eq!(out, MatchOutcome::Rejected(NotTraded::LimitDown));

        // 手数：min_lot = 100 股，qty = 50 股 → 拒
        let m4 = BarMatcher::new(MatcherConfig {
            min_lot_raw: 100 * 100_000_000,
            ..cfg()
        });
        let out = m4.match_order(&buy_market(50), &bar(100_000, 102_000, 99_000, 101_000));
        assert_eq!(out, MatchOutcome::Rejected(NotTraded::MinLot));
    }
}
