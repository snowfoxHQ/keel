//! 回测驱动：策略 trait + §5.1 流水线闭环（docs/BASELINE.md）。
//!
//! 每 Bar 固定阶段次序（v0 内联推进，不经过堆队列；全序键语义由固定次序保证，
//! 确定性由黄金回归覆盖）：
//! 1. **MarketData**：GuardEngine.on_bar —— 先判定触发（跳空/触及），后吸收极值（§6.1）；
//!    触发单立即按 BarMatcher 判定并入账（触发判定先于策略单，次序固定）；
//! 2. **StrategyOrder**：Strategy.on_bar 产出订单 → BarMatcher 判定；`OutOfReach` 订单
//!    进入挂起队列，下一根 Bar 同相重试（保持提交序，确定性）；
//! 3. **Fill**：成交逐笔入 TradeLedger（费用按 §12 模型）→ PnlAccount 就地更新；
//! 4. **CancelAck**：GuardEngine.on_fill 重算绑定数量 / 归零撤销；
//! 5. **Golden**：逐事件累计哈希（§13.1）+ 每 N 事件状态快照（§13.2）。

use keel_core::{hash::Xxh3, Price, Qty};
use keel_data::Bar;
use keel_guard::{GuardConfig, GuardEngine, TriggerSide};
use keel_pnl::{FifoAccount, FifoCost, LedgerEntry, Side, TradeLedger};

use crate::matcher::{BarMatcher, Fill, MatchOutcome, MatcherConfig, NotTraded, OrderIntent};

/// 回测配置（全部定点）。
#[derive(Clone, Copy, Debug)]
pub struct BacktestConfig {
    pub fees: keel_pnl::FeeModel,
    pub matcher: MatcherConfig,
    pub guard: GuardConfig,
    /// 初始资金（Money raw；不计入 Ledger 事实，报告与 ctx 中并入现金）。
    pub initial_cash_raw: i128,
    /// 每 N 个事件（fill/reject/cancel）一个状态快照锚点（§13.2；0 = 不快照）。
    pub snapshot_every: usize,
}

/// 策略上下文（只读视图）。
#[derive(Clone, Copy, Debug)]
pub struct StrategyCtx {
    pub ts: i64,
    pub net_qty: i128,
    pub cash_raw: i128,
    pub realized_raw: i128,
    pub held_cost_raw: i128,
}

/// 策略 trait：Bar 驱动产出订单意图。
pub trait Strategy {
    fn on_bar(&mut self, bar: &Bar, ctx: &StrategyCtx) -> Vec<OrderIntent>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// Ledger 容量耗尽（§3.1）。
    LedgerFull,
    /// 核算溢出。
    Overflow,
}

/// 回测报告（全部精确定点值 + golden 哈希，§13）。
#[derive(Clone, Debug)]
pub struct BacktestReport {
    /// §13.1 事件流累计哈希（fill/trigger/reject/cancel 事件逐一累计 XXH3）。
    pub event_hash: u64,
    /// §13.2 快照锚点：(bar_ts, 状态哈希)。
    pub snapshots: Vec<(i64, u64)>,
    /// §13.4 最终核算（精确值）。
    pub fills: usize,
    pub rejects: usize,
    pub cancels: usize,
    pub final_net_qty: i128,
    pub final_cash_raw: i128,
    pub final_realized_raw: i128,
    pub final_held_cost_raw: i128,
    pub final_fees_raw: i128,
    /// Ledger XXH3 事实链摘要（§9，逐条 chain 累计）。
    pub chain_hash: u64,
}

pub struct Backtester {
    cfg: BacktestConfig,
    matcher: BarMatcher,
    guard: GuardEngine,
    ledger: TradeLedger,
    account: FifoAccount,
    pending: Vec<OrderIntent>,
    /// 事件流累计哈希状态（§13.1）。
    hasher: Xxh3,
    snapshots: Vec<(i64, u64)>,
    fills: usize,
    rejects: usize,
    cancels: usize,
    event_count: usize,
}

impl Backtester {
    pub fn new(cfg: BacktestConfig) -> Result<Self, EngineError> {
        Ok(Backtester {
            matcher: BarMatcher::new(cfg.matcher),
            guard: GuardEngine::new(cfg.guard).map_err(|_| EngineError::Overflow)?,
            account: FifoAccount::new(FifoCost::new(1 << 14)),
            ledger: TradeLedger::with_capacity(1 << 16),
            cfg,
            pending: Vec::new(),
            hasher: Xxh3::new(),
            snapshots: Vec::new(),
            fills: 0,
            rejects: 0,
            cancels: 0,
            event_count: 0,
        })
    }

    /// 只读访问成交事实账本（诊断/对账用）。
    pub fn ledger(&self) -> &TradeLedger {
        &self.ledger
    }

    /// 回测开始前注册条件单（§8.1 注册全序：按调用次序分配 seq）。
    pub fn register_guard(
        &mut self,
        spec: keel_guard::RuleSpec,
    ) -> Result<keel_guard::RuleHandle, keel_guard::GuardError> {
        self.guard.register(spec)
    }

    /// 运行完整回测（bars 必须已按 ts 升序且互不重复——由 keel-data 保证）。
    pub fn run(
        &mut self,
        bars: &[Bar],
        strategy: &mut dyn Strategy,
    ) -> Result<BacktestReport, EngineError> {
        let last_ts = bars.last().map(|b| b.ts);

        for bar in bars {
            // 1) MarketData：guard 触发（先判定后吸收极值，§6.1 保守路径）
            let triggered: Vec<(u64, TriggerSide, Price)> = self
                .guard
                .on_bar(bar.open, bar.high, bar.low)
                .map_err(|_| EngineError::Overflow)?
                .iter()
                .map(|t| (t.seq, t.side, t.at))
                .collect();
            for (seq, side, at) in &triggered {
                self.hash_trigger(*seq, *side, *at);
                self.event_count += 1;
                // 止损成交：at 为市价基准（跳空已在 at 中体现，§8.2）
                let qty = Qty::from_raw(self.account.net_qty().abs() as i64);
                match self.matcher.match_stop(*side, *at, qty, bar) {
                    MatchOutcome::Filled(f) => {
                        let pnl_side = match side {
                            TriggerSide::SellStop => Side::Sell,
                            TriggerSide::BuyStop => Side::Buy,
                        };
                        self.apply_fill(bar.ts, pnl_side, f)?;
                    }
                    MatchOutcome::Rejected(n) => {
                        self.rejects += 1;
                        self.hash_reject(bar.ts, n);
                        self.event_count += 1;
                    }
                }
            }

            // 2) StrategyOrder：挂起订单先（原序），再本 Bar 新订单
            let mut intents: Vec<OrderIntent> = std::mem::take(&mut self.pending);
            let ctx = self.ctx(bar.ts);
            intents.extend(strategy.on_bar(bar, &ctx));
            for intent in &intents {
                match self.matcher.match_order(intent, bar) {
                    MatchOutcome::Filled(f) => {
                        self.apply_fill(bar.ts, intent.side, f)?;
                    }
                    MatchOutcome::Rejected(NotTraded::OutOfReach) => {
                        // 挂起重试（下一 Bar 同相重判）
                        self.pending.push(*intent);
                    }
                    MatchOutcome::Rejected(n) => {
                        self.rejects += 1;
                        self.hash_reject(bar.ts, n);
                        self.event_count += 1;
                    }
                }
            }

            // 3) CancelAck：Fill 后重算绑定数量（归零撤销，§8.1）
            let net = self.account.net_qty();
            let cancels: Vec<keel_guard::CancelRecord> = self
                .guard
                .on_fill(Qty::from_raw(net as i64))
                .map_err(|_| EngineError::Overflow)?
                .to_vec();
            for c in &cancels {
                self.cancels += 1;
                self.hash_cancel(c.seq, c.reason as u64);
                self.event_count += 1;
            }

            // 4) 快照锚点（§13.2：每 N 事件 + 末根 Bar 强制收尾快照）
            if self.cfg.snapshot_every > 0
                && (self.event_count % self.cfg.snapshot_every == 0 || Some(bar.ts) == last_ts)
            {
                self.snapshots.push((bar.ts, self.state_hash()));
            }
        }

        Ok(BacktestReport {
            event_hash: self.hasher.digest(),
            snapshots: std::mem::take(&mut self.snapshots),
            fills: self.fills,
            rejects: self.rejects,
            cancels: self.cancels,
            final_net_qty: self.account.net_qty(),
            final_cash_raw: self.cfg.initial_cash_raw + self.account.cash().raw_i128(),
            final_realized_raw: self.account.realized_pnl().raw_i128(),
            final_held_cost_raw: self.account.held_cost().raw_i128(),
            final_fees_raw: self.account.fees().raw_i128(),
            chain_hash: self.ledger_chain_hash(),
        })
    }

    fn apply_fill(&mut self, ts: i64, side: Side, f: Fill) -> Result<(), EngineError> {
        // 拷贝事实条目：record 返回的引用持有 ledger 的可变借用，须解除后再 apply
        let e: LedgerEntry = *self
            .ledger
            .record(ts, side, f.price, f.qty, &self.cfg.fees)
            .map_err(|_| EngineError::LedgerFull)?;
        self.account.apply(&e).map_err(|_| EngineError::Overflow)?;
        self.fills += 1;
        self.event_count += 1;
        self.hash_fill(ts, &e);
        Ok(())
    }

    fn ctx(&self, ts: i64) -> StrategyCtx {
        StrategyCtx {
            ts,
            net_qty: self.account.net_qty(),
            cash_raw: self.cfg.initial_cash_raw + self.account.cash().raw_i128(),
            realized_raw: self.account.realized_pnl().raw_i128(),
            held_cost_raw: self.account.held_cost().raw_i128(),
        }
    }

    fn ledger_chain_hash(&self) -> u64 {
        // Ledger 内部 XXH3 事实链摘要（链自洽性由 P3 Reconciliation 保证）
        self.ledger.chain_hash()
    }

    // ---- golden 事件编码（§13.1/§14：固定 tag + 固定字段序 + 小端） ----

    fn hash_fill(&mut self, ts: i64, e: &LedgerEntry) {
        self.hasher.update(&0u64.to_le_bytes()); // tag 0: fill
        self.hasher.update(&ts.to_le_bytes());
        self.hasher.update(&(e.side as u64).to_le_bytes());
        self.hasher.update(&e.price.raw().to_le_bytes());
        self.hasher.update(&e.qty.raw().to_le_bytes());
        self.hasher.update(&e.fee.raw_i128().to_le_bytes());
        self.hasher.update(&e.cash_delta.raw_i128().to_le_bytes());
    }

    fn hash_trigger(&mut self, seq: u64, side: TriggerSide, at: Price) {
        self.hasher.update(&1u64.to_le_bytes()); // tag 1: guard-trigger
        self.hasher.update(&seq.to_le_bytes());
        self.hasher.update(&(side as u64).to_le_bytes());
        self.hasher.update(&at.raw().to_le_bytes());
    }

    fn hash_reject(&mut self, ts: i64, n: NotTraded) {
        self.hasher.update(&3u64.to_le_bytes()); // tag 3: reject
        self.hasher.update(&ts.to_le_bytes());
        self.hasher.update(&(n as u64).to_le_bytes());
    }

    fn hash_cancel(&mut self, seq: u64, reason: u64) {
        self.hasher.update(&2u64.to_le_bytes()); // tag 2: cancel
        self.hasher.update(&seq.to_le_bytes());
        self.hasher.update(&reason.to_le_bytes());
    }

    /// 状态快照哈希（§13.2：持仓/现金/盈亏/费用/计数器/事实链，固定字段序、小端）。
    fn state_hash(&self) -> u64 {
        let mut h = Xxh3::new();
        h.update(&self.account.net_qty().to_le_bytes());
        h.update(&(self.cfg.initial_cash_raw + self.account.cash().raw_i128()).to_le_bytes());
        h.update(&self.account.realized_pnl().raw_i128().to_le_bytes());
        h.update(&self.account.held_cost().raw_i128().to_le_bytes());
        h.update(&self.account.fees().raw_i128().to_le_bytes());
        h.update(&(self.fills as u64).to_le_bytes());
        h.update(&(self.rejects as u64).to_le_bytes());
        h.update(&(self.cancels as u64).to_le_bytes());
        h.update(&self.ledger_chain_hash().to_le_bytes());
        h.digest()
    }
}
