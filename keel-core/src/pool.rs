//! 固定容量内存设施（docs/BASELINE.md §3.2 约束 1、§7.1 资源回收契约）。
//!
//! - [`Pool`]：固定容量对象池，`(slot, generation)` 句柄，O(1) 撤单/回收。
//! - [`Arena`]：bump allocator，容量耗尽返回 `None`，禁止隐式扩容。
//!
//! 两者构造时的分配不属于热路径；热路径内的 insert/remove/alloc 全部零分配。

use core::fmt;

/// 句柄初始 generation（非零，保留 0 作无效值）。
const GENERATION_INIT: u32 = 1;

/// 池句柄：`(slot_index, generation)`，generation 校验防悬空/复用误用（§7.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Handle {
    slot: u32,
    gen: u32,
}

impl Handle {
    #[inline]
    pub const fn slot(self) -> u32 {
        self.slot
    }

    #[inline]
    pub const fn generation(self) -> u32 {
        self.gen
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolError {
    /// 池满：拒绝插入，不扩容（§7.1）。
    Full,
    /// 句柄失效：slot 越界、generation 不匹配或对象已被取回。
    InvalidHandle,
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PoolError::Full => write!(f, "对象池已满"),
            PoolError::InvalidHandle => write!(f, "对象池句柄失效"),
        }
    }
}

impl std::error::Error for PoolError {}

struct Entry<T> {
    gen: u32,
    item: Option<T>,
}

/// 固定容量对象池。free-list 按固定顺序预填充，insert/remove 全程零分配。
pub struct Pool<T> {
    entries: Box<[Entry<T>]>,
    free: Vec<u32>,
    live: u32,
}

impl<T> Pool<T> {
    /// 构造固定容量池（构造期分配，非热路径）。
    pub fn new(capacity: u32) -> Self {
        let mut free = Vec::with_capacity(capacity as usize);
        // 固定顺序（降序）填充，保证分配次序跨回放确定
        for slot in (0..capacity).rev() {
            free.push(slot);
        }
        Pool {
            entries: (0..capacity)
                .map(|_| Entry {
                    gen: GENERATION_INIT,
                    item: None,
                })
                .collect(),
            free,
            live: 0,
        }
    }

    /// 插入对象，返回句柄。池满返回 [`PoolError::Full`]，不扩容。
    pub fn insert(&mut self, item: T) -> Result<Handle, PoolError> {
        let slot = self.free.pop().ok_or(PoolError::Full)?;
        let e = &mut self.entries[slot as usize];
        debug_assert!(e.item.is_none());
        e.item = Some(item);
        self.live += 1;
        Ok(Handle {
            slot,
            gen: e.gen,
        })
    }

    #[inline]
    fn entry_live(&self, h: Handle) -> Option<&Entry<T>> {
        self.entries
            .get(h.slot as usize)
            .filter(|e| e.gen == h.gen && e.item.is_some())
    }

    /// 句柄是否有效（对应对象仍存活）。
    pub fn contains(&self, h: Handle) -> bool {
        self.entry_live(h).is_some()
    }

    pub fn get(&self, h: Handle) -> Option<&T> {
        self.entry_live(h).and_then(|e| e.item.as_ref())
    }

    pub fn get_mut(&mut self, h: Handle) -> Option<&mut T> {
        match self.entries.get_mut(h.slot as usize) {
            Some(e) if e.gen == h.gen => e.item.as_mut(),
            _ => None,
        }
    }

    /// 取回对象并回收 slot（O(1)，墓碑语义：generation 立即失效）。
    ///
    /// generation 溢出（u32::MAX → 0）时该 slot 标记永久不可复用，
    /// 不再进入 free-list——容量永久减一，换取句柄永不复用碰撞（§7.1）。
    pub fn remove(&mut self, h: Handle) -> Result<T, PoolError> {
        let e = self
            .entries
            .get_mut(h.slot as usize)
            .ok_or(PoolError::InvalidHandle)?;
        if e.gen != h.gen {
            return Err(PoolError::InvalidHandle);
        }
        let item = e.item.take().ok_or(PoolError::InvalidHandle)?;
        let next_gen = e.gen.wrapping_add(1);
        if next_gen == 0 {
            // 溢出：永久死槽，不归还 free-list
            e.gen = 0;
        } else {
            e.gen = next_gen;
            self.free.push(h.slot);
        }
        self.live -= 1;
        Ok(item)
    }

    /// 当前存活对象数。
    #[inline]
    pub fn live(&self) -> u32 {
        self.live
    }

    /// 剩余空位。
    #[inline]
    pub fn free_slots(&self) -> u32 {
        self.free.len() as u32
    }

    /// 池总容量。
    #[inline]
    pub fn capacity(&self) -> u32 {
        self.entries.len() as u32
    }

    /// 仅测试：强制写 generation，用于覆盖溢出路径。
    #[cfg(test)]
    fn force_generation(&mut self, slot: u32, gen: u32) {
        self.entries[slot as usize].gen = gen;
    }
}

/// 最大对齐支持：16 字节（覆盖 i128 定点数）。
const ARENA_MAX_ALIGN: usize = 16;

/// 固定容量 bump allocator（§3.2 约束 1）。
///
/// 构造时按 `ARENA_MAX_ALIGN` 一次性分配原始内存（非热路径，唯一 unsafe 点集中于此）；
/// `alloc` 容量耗尽返回 `None`，永不扩容、永不 panic。
/// 返回 `&mut [MaybeUninit<u8>]`，调用方写入后自行初始化。
pub struct Arena {
    ptr: core::ptr::NonNull<u8>,
    cap: usize,
    used: usize,
}

// Arena 的内存是自有单所有权缓冲，跨线程转移是安全的。
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

impl Arena {
    /// 构造（构造期分配，非热路径）。
    pub fn new(capacity: usize) -> Self {
        if capacity == 0 {
            return Arena {
                ptr: core::ptr::NonNull::dangling(),
                cap: 0,
                used: 0,
            };
        }
        let layout = core::alloc::Layout::from_size_align(capacity, ARENA_MAX_ALIGN)
            .expect("Arena 容量与对齐非法");
        let ptr = unsafe { std::alloc::alloc(layout) };
        Arena {
            ptr: core::ptr::NonNull::new(ptr).expect("Arena 分配失败（OOM）"),
            cap: capacity,
            used: 0,
        }
    }

    /// 按对齐分配一段未初始化内存。耗尽或 align > 16 返回 `None`。
    pub fn alloc(&mut self, len: usize, align: usize) -> Option<&mut [core::mem::MaybeUninit<u8>]> {
        debug_assert!(align.is_power_of_two(), "align 必须是 2 的幂");
        if align > ARENA_MAX_ALIGN {
            return None;
        }
        let start = self.used.div_ceil(align) * align;
        let end = start.checked_add(len)?;
        if end > self.cap {
            return None;
        }
        self.used = end;
        // SAFETY: start+len ≤ cap，缓冲按 ARENA_MAX_ALIGN 对齐且独占所有权
        Some(unsafe {
            core::slice::from_raw_parts_mut(self.ptr.as_ptr().add(start) as *mut _, len)
        })
    }

    /// 重置水位（批量场景复用同一 Arena；重置不清零内存，调用方负责初始化）。
    pub fn reset(&mut self) {
        self.used = 0;
    }

    #[inline]
    pub fn used_bytes(&self) -> usize {
        self.used
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        if self.cap != 0 {
            let layout = core::alloc::Layout::from_size_align(self.cap, ARENA_MAX_ALIGN)
                .expect("Arena 布局在 Drop 时不应非法");
            unsafe { std::alloc::dealloc(self.ptr.as_ptr(), layout) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Pool 基本语义 ----

    #[test]
    fn insert_get_remove_roundtrip() {
        let mut pool = Pool::new(4);
        let h = pool.insert(41u32).unwrap();
        assert_eq!(pool.get(h), Some(&41));
        assert_eq!(pool.remove(h), Ok(41));
        assert_eq!(pool.get(h), None);
        assert!(!pool.contains(h));
    }

    #[test]
    fn stale_handle_rejected() {
        let mut pool = Pool::new(2);
        let h1 = pool.insert("a").unwrap();
        pool.remove(h1).unwrap();
        let h2 = pool.insert("b").unwrap();
        // 复用同一 slot，旧句柄 generation 不匹配
        assert_eq!(h1.slot(), h2.slot());
        assert_ne!(h1.generation(), h2.generation());
        assert_eq!(pool.get(h1), None);
        assert_eq!(pool.remove(h1), Err(PoolError::InvalidHandle));
        assert_eq!(pool.get(h2), Some(&"b"));
    }

    #[test]
    fn full_pool_returns_error() {
        let mut pool = Pool::new(2);
        let _a = pool.insert(1u8).unwrap();
        let _b = pool.insert(2u8).unwrap();
        assert_eq!(pool.insert(3u8), Err(PoolError::Full));
        pool.remove(_a).unwrap();
        assert!(pool.insert(3u8).is_ok());
    }

    #[test]
    fn invalid_slot_handle() {
        let pool: Pool<u32> = Pool::new(1);
        let bogus = Handle { slot: 99, gen: GENERATION_INIT };
        assert_eq!(pool.get(bogus), None);
        assert!(!pool.contains(bogus));
    }

    #[test]
    fn pool_counters() {
        let mut pool = Pool::new(3);
        let a = pool.insert(1).unwrap();
        let b = pool.insert(2).unwrap();
        assert_eq!(pool.live(), 2);
        assert_eq!(pool.free_slots(), 1);
        pool.remove(a).unwrap();
        pool.remove(b).unwrap();
        assert_eq!(pool.live(), 0);
        assert_eq!(pool.free_slots(), 3);
    }

    #[test]
    fn generation_overflow_marks_dead_slot() {
        let mut pool = Pool::new(1);
        let h = pool.insert(1u8).unwrap();
        // 强制 generation 到 u32::MAX，模拟 2^32 次复用后
        pool.force_generation(h.slot(), u32::MAX);
        let h_max = Handle { slot: h.slot(), gen: u32::MAX };
        assert!(pool.contains(h_max));
        pool.remove(h_max).unwrap();
        // 死槽：不再归还 free-list，容量永久减一
        assert_eq!(pool.insert(2u8), Err(PoolError::Full));
        assert_eq!(pool.free_slots(), 0);
    }

    // ---- Arena ----

    #[test]
    fn arena_basic_alloc() {
        let mut arena = Arena::new(64);
        let a = arena.alloc(8, 8).unwrap();
        a.fill(core::mem::MaybeUninit::new(0u8));
        assert_eq!(arena.used_bytes(), 8);
        let b = arena.alloc(8, 8).unwrap();
        b.fill(core::mem::MaybeUninit::new(1u8));
        assert_eq!(arena.used_bytes(), 16);
    }

    #[test]
    fn arena_alignment() {
        let mut arena = Arena::new(64);
        let _a = arena.alloc(3, 1).unwrap();
        assert_eq!(arena.used_bytes(), 3);
        let b = arena.alloc(1, 8).unwrap();
        // b 起始地址按 8 对齐：偏移 3 → 8
        let base = b.as_ptr() as usize;
        assert_eq!(base % 8, 0);
        // 第二次分配占用 [8, 9)
        assert_eq!(arena.used_bytes(), 9);
    }

    #[test]
    fn arena_exhaust_returns_none_no_realloc() {
        let mut arena = Arena::new(16);
        assert!(arena.alloc(16, 1).is_some());
        assert!(arena.alloc(1, 1).is_none()); // 耗尽，不扩容
        arena.reset();
        assert!(arena.alloc(16, 1).is_some());
    }

    #[test]
    fn arena_oversize_request() {
        let mut arena = Arena::new(16);
        assert!(arena.alloc(32, 1).is_none());
        assert_eq!(arena.used_bytes(), 0);
    }
}
