//! golden 回归协议（docs/BASELINE.md §13 五项）。
//!
//! 1. 事件流累计哈希（BacktestReport.event_hash，XXH3-64 固定）；
//! 2. 分阶段状态快照（每 N 事件锚点，snapshots）；
//! 3. 首个差异定位（快照锚点逐个比对，输出首个偏差 (ts, 期望, 实际)）；
//! 4. 最终核算精确值（现金/已实现/持仓/费用/成交笔数逐字段比对）；
//! 5. 跨运行确定性（同输入重跑逐位一致——由集成测试覆盖）。
//!
//! 序列化契约：哈希输入为小端整数、固定 tag、固定字段序（见 backtest.rs）；
//! 黄金值（[`GoldenExpectation`]）小数据集 commit 进仓库（tests/golden/）。

use crate::backtest::BacktestReport;

/// 黄金期望值（入库的基准）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoldenExpectation {
    pub event_hash: u64,
    pub snapshots: Vec<(i64, u64)>,
    pub chain_hash: u64,
    pub fills: usize,
    pub cancels: usize,
    pub final_net_qty: i128,
    pub final_cash_raw: i128,
    pub final_realized_raw: i128,
    pub final_fees_raw: i128,
}

impl GoldenExpectation {
    /// 从一次正确运行生成黄金值。
    pub fn generate(r: &BacktestReport) -> Self {
        GoldenExpectation {
            event_hash: r.event_hash,
            snapshots: r.snapshots.clone(),
            chain_hash: r.chain_hash,
            fills: r.fills,
            cancels: r.cancels,
            final_net_qty: r.final_net_qty,
            final_cash_raw: r.final_cash_raw,
            final_realized_raw: r.final_realized_raw,
            final_fees_raw: r.final_fees_raw,
        }
    }
}

/// 首个差异定位（§13.3）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirstDiff {
    /// 差异类别（快照锚点 / 事件流哈希 / 最终核算字段）。
    pub kind: &'static str,
    /// 锚点 ts（快照差异时）；最终核算差异时为末锚点或 i64::MIN。
    pub ts: i64,
    pub expected: String,
    pub actual: String,
}

/// 回归结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoldenReport {
    pub ok: bool,
    pub first_diff: Option<FirstDiff>,
    /// 全部差异计数（诊断用；定位只输出第一个）。
    pub diff_count: usize,
}

/// golden 比对器。
pub struct GoldenRunner;

impl GoldenRunner {
    /// §13 五项比对：快照锚点 → 事件流哈希 → 最终核算。
    /// 返回首个差异（§13.3）；全一致返回 ok。
    pub fn compare(actual: &BacktestReport, expect: &GoldenExpectation) -> GoldenReport {
        let mut diffs = 0usize;
        let mut first: Option<FirstDiff> = None;

        // 2) 快照锚点数量与逐点比对（首个偏差即 §13.3 定位输出）
        if actual.snapshots.len() != expect.snapshots.len() {
            diffs += 1;
            first = first.or_else(|| {
                Some(FirstDiff {
                    kind: "snapshot-count",
                    ts: i64::MIN,
                    expected: expect.snapshots.len().to_string(),
                    actual: actual.snapshots.len().to_string(),
                })
            });
        }
        for ((ets, ehash), (ats, ahash)) in expect.snapshots.iter().zip(actual.snapshots.iter()) {
            if ets != ats || ehash != ahash {
                diffs += 1;
                first = first.or_else(|| {
                    Some(FirstDiff {
                        kind: "snapshot",
                        ts: *ats,
                        expected: format!("ts={ets} hash={ehash:#018x}"),
                        actual: format!("ts={ats} hash={ahash:#018x}"),
                    })
                });
            }
        }

        // 1) 事件流累计哈希
        if actual.event_hash != expect.event_hash {
            diffs += 1;
            first = first.or_else(|| {
                Some(FirstDiff {
                    kind: "event-hash",
                    ts: actual
                        .snapshots
                        .last()
                        .map(|(ts, _)| *ts)
                        .unwrap_or(i64::MIN),
                    expected: format!("{:#018x}", expect.event_hash),
                    actual: format!("{:#018x}", actual.event_hash),
                })
            });
        }

        // 4) 最终核算精确值
        let tail_ts = actual.snapshots.last().map(|(ts, _)| *ts).unwrap_or(i64::MIN);
        for (kind, e, a) in [
            ("chain-hash", expect.chain_hash as i128, actual.chain_hash as i128),
            ("fills", expect.fills as i128, actual.fills as i128),
            ("cancels", expect.cancels as i128, actual.cancels as i128),
            ("net-qty", expect.final_net_qty, actual.final_net_qty),
            ("cash", expect.final_cash_raw, actual.final_cash_raw),
            ("realized", expect.final_realized_raw, actual.final_realized_raw),
            ("fees", expect.final_fees_raw, actual.final_fees_raw),
        ] {
            if e != a {
                diffs += 1;
                first = first.or_else(|| {
                    Some(FirstDiff {
                        kind,
                        ts: tail_ts,
                        expected: e.to_string(),
                        actual: a.to_string(),
                    })
                });
            }
        }

        GoldenReport {
            ok: diffs == 0,
            first_diff: first,
            diff_count: diffs,
        }
    }
}
