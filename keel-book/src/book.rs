//! 扁平订单簿与撮合（docs/BASELINE.md §6 Order Book Model、§7、§7.1 资源回收契约）。

use crate::bitmap::LevelBitmap;
use keel_core::{Handle, Pool, Price, Qty};

/// 订单方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    /// 对手方。
    #[inline]
    pub const fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

/// 订单簿配置（§7：base/range/tick_size 配置，越界返回错误）。
#[derive(Clone, Copy, Debug)]
pub struct BookConfig {
    /// 档位 0 的价格（raw，单位 1e-4）。
    pub base_price_raw: i64,
    /// 最小报价单位（raw，> 0）。
    pub tick_size_raw: i64,
    /// 档位数量（≥ 1）。
    pub levels: usize,
    /// 每档位 FIFO 队列容量。
    pub queue_capacity: usize,
    /// 订单池容量。
    pub order_capacity: u32,
}

impl BookConfig {
    pub fn validate(&self) -> Result<(), BookError> {
        if self.tick_size_raw <= 0 {
            return Err(BookError::ConfigInvalid("tick_size_raw 必须 > 0"));
        }
        if self.levels == 0 {
            return Err(BookError::ConfigInvalid("levels 必须 >= 1"));
        }
        if self.queue_capacity == 0 {
            return Err(BookError::ConfigInvalid("queue_capacity 必须 >= 1"));
        }
        if self.order_capacity == 0 {
            return Err(BookError::ConfigInvalid("order_capacity 必须 >= 1"));
        }
        let span = (self.levels as i128 - 1) * self.tick_size_raw as i128;
        let top = self.base_price_raw as i128 + span;
        if (self.base_price_raw as i128).abs() >= keel_core::RAW_BOUND as i128
            || top.abs() >= keel_core::RAW_BOUND as i128
        {
            return Err(BookError::ConfigInvalid("价格区间超出 RAW_BOUND"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BookError {
    ConfigInvalid(&'static str),
    /// 价格超出档位索引范围。
    PriceOutOfRange,
    /// 价格不在 tick 网格上。
    PriceOffGrid,
    /// 数量非法（≤ 0）。
    InvalidQty,
    /// 目标档位队列满（§7.1：拒绝新订单，不扩容）。
    QueueFull { side: Side, price: Price },
    /// 订单池满（§7.1）。
    PoolFull,
    /// 成交缓冲容量不足。preflight 阶段拒绝时状态零变更；撮合中途触发属防御路径。
    FillBufferFull,
    /// 撤单拒绝：句柄失效或订单已完全成交（§5.3：不回滚成交）。
    OrderGone,
    /// 内部不变量破坏（防御性，正常流程不可达）。
    Inconsistent,
}

impl core::fmt::Display for BookError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BookError::ConfigInvalid(m) => write!(f, "订单簿配置非法：{m}"),
            BookError::PriceOutOfRange => write!(f, "价格超出档位范围"),
            BookError::PriceOffGrid => write!(f, "价格不在 tick 网格上"),
            BookError::InvalidQty => write!(f, "数量非法"),
            BookError::QueueFull { side, price } => write!(f, "档位队列已满：{side:?}@{price}"),
            BookError::PoolFull => write!(f, "订单池已满"),
            BookError::FillBufferFull => write!(f, "成交缓冲容量不足"),
            BookError::OrderGone => write!(f, "订单已不存在（撤单拒绝）"),
            BookError::Inconsistent => write!(f, "订单簿内部状态不一致"),
        }
    }
}

impl std::error::Error for BookError {}

/// 一笔成交（maker 视角：价格与订单为被动方）。
#[derive(Clone, Copy, Debug)]
pub struct Fill {
    pub maker: Handle,
    pub maker_id: u64,
    pub price: Price,
    pub qty: Qty,
}

/// 固定容量成交缓冲（热路径零分配：submit 前由 preflight 保证容量充足）。
pub struct FillBuf {
    buf: Box<[Option<Fill>]>,
    len: usize,
}

impl FillBuf {
    pub fn with_capacity(cap: usize) -> Self {
        FillBuf {
            buf: (0..cap).map(|_| None).collect(),
            len: 0,
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn reset(&mut self) {
        self.len = 0;
    }

    /// 迭代本轮成交。
    pub fn iter(&self) -> impl Iterator<Item = &Fill> {
        self.buf[..self.len]
            .iter()
            .map(|o| o.as_ref().expect("len 内必为 Some"))
    }

    #[inline]
    fn push(&mut self, f: Fill) -> Result<(), BookError> {
        if self.len >= self.buf.len() {
            return Err(BookError::FillBufferFull);
        }
        self.buf[self.len] = Some(f);
        self.len += 1;
        Ok(())
    }
}

/// 订单信息视图（测试/对账用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderView {
    pub id: u64,
    pub side: Side,
    pub price: Price,
    pub remaining: Qty,
}

struct OrderEntry {
    id: u64,
    side: Side,
    price: Price,
    remaining: Qty,
    /// 所属档位下标。
    level: usize,
    /// 档位队列中的物理下标（压缩时由 compact 维护）。
    pos: usize,
}

struct Level {
    /// FIFO 数组化队列；None = 墓碑（O(1) 撤单，§7）。
    queue: Vec<Option<Handle>>,
    /// 逻辑队首（物理下标）。
    head: usize,
    live: u32,
    dead: u32,
}

/// 惰性压缩阈值（§7.1）：墓碑数达到该值即压缩该档位。
const COMPACT_DEAD_THRESHOLD: u32 = 4;
/// 队首空洞达到该值且占比过半即压缩。
const COMPACT_HEAD_THRESHOLD: usize = 8;

impl Level {
    fn new(capacity: usize) -> Self {
        Level {
            queue: Vec::with_capacity(capacity),
            head: 0,
            live: 0,
            dead: 0,
        }
    }

    #[inline]
    fn logical_len(&self) -> usize {
        self.queue.len() - self.head
    }

    #[inline]
    fn has_space(&self, capacity: usize) -> bool {
        self.logical_len() < capacity
    }

    /// 窥视队首存活订单（跳过并消费墓碑；**不**移除存活订单）。
    fn peek_live(&mut self) -> Option<Handle> {
        while self.head < self.queue.len() {
            match self.queue[self.head] {
                Some(h) => return Some(h),
                None => {
                    self.head += 1;
                    self.dead -= 1;
                }
            }
        }
        None
    }

    /// 消费队首存活订单（仅在 maker 完全成交时调用）。
    fn consume_head(&mut self) {
        debug_assert!(self.head < self.queue.len());
        self.queue[self.head] = None;
        self.head += 1;
        self.live -= 1;
    }
}

/// 提交结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubmitOutcome {
    /// 剩余部分挂单句柄；完全成交则为 None。
    pub handle: Option<Handle>,
    /// 本轮成交总量。
    pub filled: Qty,
    /// 订单所在（或曾试图挂入的）档位下标。
    pub level_idx: usize,
}

/// 扁平订单簿。
pub struct Book {
    cfg: BookConfig,
    bids: Box<[Level]>,
    asks: Box<[Level]>,
    bid_bmp: LevelBitmap,
    ask_bmp: LevelBitmap,
    pool: Pool<OrderEntry>,
    /// BBO 缓存：档位下标。
    best_bid: Option<usize>,
    best_ask: Option<usize>,
}

impl Book {
    pub fn new(cfg: BookConfig) -> Result<Self, BookError> {
        cfg.validate()?;
        let c = cfg;
        Ok(Book {
            cfg: c,
            bids: (0..cfg.levels).map(|_| Level::new(cfg.queue_capacity)).collect(),
            asks: (0..cfg.levels).map(|_| Level::new(cfg.queue_capacity)).collect(),
            bid_bmp: LevelBitmap::new(cfg.levels),
            ask_bmp: LevelBitmap::new(cfg.levels),
            pool: Pool::new(cfg.order_capacity),
            best_bid: None,
            best_ask: None,
        })
    }

    /// 档位下标的价格。
    #[inline]
    fn price_at(&self, idx: usize) -> Price {
        Price::from_raw(self.cfg.base_price_raw + (idx as i64) * self.cfg.tick_size_raw)
    }

    /// 价格 → 档位下标（越界/脱格返回错误）。
    fn level_index(&self, price: Price) -> Result<usize, BookError> {
        let diff = (price.raw() as i128) - (self.cfg.base_price_raw as i128);
        let span = (self.cfg.levels as i128 - 1) * self.cfg.tick_size_raw as i128;
        if diff < 0 || diff > span {
            return Err(BookError::PriceOutOfRange);
        }
        if diff % self.cfg.tick_size_raw as i128 != 0 {
            return Err(BookError::PriceOffGrid);
        }
        Ok((diff / self.cfg.tick_size_raw as i128) as usize)
    }

    /// 最优买价（最高买档位）。
    pub fn best_bid(&self) -> Option<Price> {
        self.best_bid.map(|i| self.price_at(i))
    }

    /// 最优卖价（最低卖档位）。
    pub fn best_ask(&self) -> Option<Price> {
        self.best_ask.map(|i| self.price_at(i))
    }

    pub fn order(&self, h: Handle) -> Option<OrderView> {
        self.pool.get(h).map(|e| OrderView {
            id: e.id,
            side: e.side,
            price: e.price,
            remaining: e.remaining,
        })
    }

    pub fn live_orders(&self) -> u32 {
        self.pool.live()
    }

    /// 指定档位的存活订单数（测试/对账用）。
    pub fn live_at(&self, side: Side, price: Price) -> Result<usize, BookError> {
        let idx = self.level_index(price)?;
        Ok(self.levels(side)[idx].live as usize)
    }

    /// 限价单提交（GTC）：先吃对手方队列（价格-时间优先），剩余挂入本方档位。
    ///
    /// preflight：池满 / 队列满 / 成交缓冲不足均在**任何状态变更前**返回结构化错误。
    pub fn submit(
        &mut self,
        id: u64,
        side: Side,
        price: Price,
        qty: Qty,
        fills: &mut FillBuf,
    ) -> Result<SubmitOutcome, BookError> {
        if qty.raw() <= 0 {
            return Err(BookError::InvalidQty);
        }
        let idx = self.level_index(price)?;

        // preflight 1：池必须有空位（保守：即使可能完全成交也要求）
        if self.pool.free_slots() == 0 {
            return Err(BookError::PoolFull);
        }
        // preflight 2：成交缓冲容量 ≥ 可吃到的 maker 数（越界即拒绝，状态零变更）
        self.check_fill_capacity(side, price, fills.capacity())?;
        // preflight 3：目标档位队列必须有空间；不足时先尝试惰性压缩回收（§7.1）
        {
            let levels = match side {
                Side::Bid => &mut self.bids[..],
                Side::Ask => &mut self.asks[..],
            };
            let level = &mut levels[idx];
            if !level.has_space(self.cfg.queue_capacity)
                && level.head == 0
                && level.dead == 0
            {
                return Err(BookError::QueueFull { side, price });
            }
        }
        {
            let levels = match side {
                Side::Bid => &mut self.bids[..],
                Side::Ask => &mut self.asks[..],
            };
            if !levels[idx].has_space(self.cfg.queue_capacity) {
                self.compact_level(side, idx);
                let levels = match side {
                    Side::Bid => &mut self.bids[..],
                    Side::Ask => &mut self.asks[..],
                };
                if !levels[idx].has_space(self.cfg.queue_capacity) {
                    return Err(BookError::QueueFull { side, price });
                }
            }
        }

        // ---- 撮合：吃对手方 ----
        let mut remaining = qty;
        let opp = side.opposite();
        loop {
            let best = match opp {
                Side::Bid => self.best_bid,
                Side::Ask => self.best_ask,
            };
            let Some(bidx) = best else { break };
            let maker_price = self.price_at(bidx);
            let crosses = match side {
                Side::Bid => maker_price.raw() <= price.raw(),
                Side::Ask => maker_price.raw() >= price.raw(),
            };
            if !crosses {
                break;
            }

            // 吃该档位 FIFO
            loop {
                let peeked = {
                    let levels = match opp {
                        Side::Bid => &mut self.bids[..],
                        Side::Ask => &mut self.asks[..],
                    };
                    levels[bidx].peek_live()
                };
                let Some(h) = peeked else {
                    // 档位耗尽：清位图，推进 BBO 缓存
                    match opp {
                        Side::Bid => {
                            self.bid_bmp.clear(bidx);
                            self.best_bid = self.bid_bmp.prev_set_before(bidx);
                        }
                        Side::Ask => {
                            self.ask_bmp.clear(bidx);
                            self.best_ask = self.ask_bmp.next_set_from(bidx + 1);
                        }
                    }
                    break;
                };
                let maker_empty = {
                    let (maker_id, maker_remaining) = {
                        let e = self.pool.get(h).ok_or(BookError::Inconsistent)?;
                        (e.id, e.remaining)
                    };
                    let fill_qty = if maker_remaining.raw() < remaining.raw() {
                        maker_remaining
                    } else {
                        remaining
                    };
                    fills.push(Fill {
                        maker: h,
                        maker_id,
                        price: maker_price,
                        qty: fill_qty,
                    })?;
                    remaining = remaining
                        .checked_sub(fill_qty)
                        .map_err(|_| BookError::Inconsistent)?;
                    let maker_empty = maker_remaining.raw() == fill_qty.raw();
                    {
                        let entry = self.pool.get_mut(h).ok_or(BookError::Inconsistent)?;
                        entry.remaining = if maker_empty {
                            Qty::from_raw(0)
                        } else {
                            entry
                                .remaining
                                .checked_sub(fill_qty)
                                .map_err(|_| BookError::Inconsistent)?
                        };
                    }
                    maker_empty
                };
                if maker_empty {
                    // 完全成交：移出订单池并消费队列头（成交事实不可变，撤单语义见 §5.3）
                    self.pool.remove(h).map_err(|_| BookError::Inconsistent)?;
                    let levels = match opp {
                        Side::Bid => &mut self.bids[..],
                        Side::Ask => &mut self.asks[..],
                    };
                    levels[bidx].consume_head();
                }
                if remaining.raw() == 0 || !maker_empty {
                    // maker 部分成交保留（此时 taker 必然已满）
                    break;
                }
            }
            // taker 恰好吃空本档位（未走"档位耗尽"分支）时，同步位图与 BBO
            let drained = {
                let levels = match opp {
                    Side::Bid => &self.bids[..],
                    Side::Ask => &self.asks[..],
                };
                levels[bidx].live == 0
            };
            if drained {
                match opp {
                    Side::Bid => {
                        self.bid_bmp.clear(bidx);
                        if self.best_bid == Some(bidx) {
                            self.best_bid = self.bid_bmp.prev_set_before(bidx);
                        }
                    }
                    Side::Ask => {
                        self.ask_bmp.clear(bidx);
                        if self.best_ask == Some(bidx) {
                            self.best_ask = self.ask_bmp.next_set_from(bidx + 1);
                        }
                    }
                }
            }
            if remaining.raw() == 0 {
                break;
            }
        }

        // ---- 剩余挂入本方档位 ----
        let mut handle = None;
        if remaining.raw() > 0 {
            let h = self
                .pool
                .insert(OrderEntry {
                    id,
                    side,
                    price,
                    remaining,
                    level: idx,
                    pos: 0,
                })
                .map_err(|_| BookError::PoolFull)?;
            {
                let (levels, pool) = match side {
                    Side::Bid => (&mut self.bids[..], &mut self.pool),
                    Side::Ask => (&mut self.asks[..], &mut self.pool),
                };
                let level = &mut levels[idx];
                if !level.has_space(self.cfg.queue_capacity) {
                    // preflight 已保证，防御性回滚
                    pool.remove(h).map_err(|_| BookError::Inconsistent)?;
                    return Err(BookError::QueueFull { side, price });
                }
                let pos = level.queue.len();
                level.queue.push(Some(h));
                level.live += 1;
                if let Some(e) = pool.get_mut(h) {
                    e.pos = pos;
                }
            }
            self.bmp_set_best(side, idx);
            handle = Some(h);
        }

        Ok(SubmitOutcome {
            handle,
            filled: qty
                .checked_sub(remaining)
                .map_err(|_| BookError::Inconsistent)?,
            level_idx: idx,
        })
    }

    /// O(1) 撤单：墓碑标记 + 订单池回收（§7.1）。
    /// 返回撤单时的剩余数量。订单已成交/句柄失效 → [`BookError::OrderGone`]。
    pub fn cancel(&mut self, h: Handle) -> Result<Qty, BookError> {
        let (side, level_idx, pos, remaining) = {
            let e = self.pool.get(h).ok_or(BookError::OrderGone)?;
            (e.side, e.level, e.pos, e.remaining)
        };
        {
            let (levels, pool) = match side {
                Side::Bid => (&mut self.bids[..], &mut self.pool),
                Side::Ask => (&mut self.asks[..], &mut self.pool),
            };
            let level = &mut levels[level_idx];
            if level.queue.get(pos) != Some(&Some(h)) {
                return Err(BookError::OrderGone);
            }
            level.queue[pos] = None;
            level.live -= 1;
            level.dead += 1;
            pool.remove(h).map_err(|_| BookError::OrderGone)?;
        }
        if self.levels(side)[level_idx].live == 0 {
            match side {
                Side::Bid => {
                    self.bid_bmp.clear(level_idx);
                    if self.best_bid == Some(level_idx) {
                        self.best_bid = self.bid_bmp.prev_set_before(level_idx);
                    }
                }
                Side::Ask => {
                    self.ask_bmp.clear(level_idx);
                    if self.best_ask == Some(level_idx) {
                        self.best_ask = self.ask_bmp.next_set_from(level_idx + 1);
                    }
                }
            }
        }
        self.maybe_compact(side, level_idx);
        Ok(remaining)
    }

    #[inline]
    fn bmp_set_best(&mut self, side: Side, idx: usize) {
        match side {
            Side::Bid => {
                self.bid_bmp.set(idx);
                if self.best_bid.is_none_or(|b| idx > b) {
                    self.best_bid = Some(idx);
                }
            }
            Side::Ask => {
                self.ask_bmp.set(idx);
                if self.best_ask.is_none_or(|a| idx < a) {
                    self.best_ask = Some(idx);
                }
            }
        }
    }

    /// 惰性压缩（§7.1）：墓碑超阈值、或队首空洞过大、或全空时执行。
    /// 压缩保留存活订单相对顺序（FIFO 不变性，确定性回归覆盖）。
    fn maybe_compact(&mut self, side: Side, idx: usize) {
        let need = {
            let levels = match side {
                Side::Bid => &self.bids[..],
                Side::Ask => &self.asks[..],
            };
            let level = &levels[idx];
            level.dead >= COMPACT_DEAD_THRESHOLD
                || (level.head >= COMPACT_HEAD_THRESHOLD && level.head * 2 >= level.queue.len())
                || (level.head > 0 && level.live == 0)
        };
        if need {
            self.compact_level(side, idx);
        }
    }

    /// 档位级压缩：保留 Some 项相对顺序，回收墓碑与队首空洞，并同步订单 pos。
    /// 两遍扫描，无临时分配。
    fn compact_level(&mut self, side: Side, idx: usize) {
        {
            let levels = match side {
                Side::Bid => &mut self.bids[..],
                Side::Ask => &mut self.asks[..],
            };
            let level = &mut levels[idx];
            let mut w = 0usize;
            for r in 0..level.queue.len() {
                if let Some(h) = level.queue[r] {
                    if w != r {
                        level.queue[w] = Some(h);
                        level.queue[r] = None;
                    }
                    w += 1;
                }
            }
            level.queue.truncate(w);
            level.head = 0;
            level.dead = 0;
        }
        // 同步存活订单的 pos
        let levels = match side {
            Side::Bid => &mut self.bids[..],
            Side::Ask => &mut self.asks[..],
        };
        let level = &mut levels[idx];
        for r in 0..level.queue.len() {
            let h = level.queue[r].expect("压缩后队列前段必为存活订单");
            if let Some(e) = self.pool.get_mut(h) {
                e.pos = r;
            }
        }
    }

    /// 统计限价单可吃到的 maker 数上限（只读，preflight 用）。
    /// 超出 `fill_cap` 立即返回错误，避免为长队列做无谓遍历。
    fn check_fill_capacity(&self, side: Side, price: Price, fill_cap: usize) -> Result<(), BookError> {
        let opp = side.opposite();
        let mut count = 0usize;
        let mut cur = match opp {
            Side::Ask => 0usize,
            Side::Bid => self.bid_bmp.total_bits(),
        };
        loop {
            let next = match opp {
                Side::Ask => self.ask_bmp.next_set_from(cur),
                Side::Bid => self.bid_bmp.prev_set_before(cur),
            };
            let Some(bidx) = next else { break };
            let maker_price = self.price_at(bidx);
            let crosses = match side {
                Side::Bid => maker_price.raw() <= price.raw(),
                Side::Ask => maker_price.raw() >= price.raw(),
            };
            if !crosses {
                break;
            }
            count += self.levels(opp)[bidx].live as usize;
            if count > fill_cap {
                return Err(BookError::FillBufferFull);
            }
            cur = match opp {
                Side::Ask => bidx + 1,
                Side::Bid => bidx,
            };
        }
        Ok(())
    }

    #[inline]
    fn levels(&self, side: Side) -> &[Level] {
        match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        }
    }
}
