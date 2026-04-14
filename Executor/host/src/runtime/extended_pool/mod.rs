//! Extended pool — host-side backing for PageIds that live outside the
//! direct wasm32 window.
//!
//! See `docs/extended_pool.md` in the repo root for the full design.
//!
//! This module has two layers:
//!
//! 1. **`ExtendedPool`** — an owned struct holding per-subprocess state
//!    (mode, live counts, global pool, resolution buffer).  Tests
//!    instantiate this directly so they can run in parallel without
//!    sharing any global.
//! 2. **`runtime::*`** — a thin `OnceLock<Mutex<ExtendedPool>>`-backed
//!    singleton used by the reclaimer and other host subsystems that
//!    are called from many sites and can't easily thread state through.
//!    Exactly one `ExtendedPool` exists per OS process.  The singleton
//!    is initialized lazily on first access.

#![allow(dead_code)]   // many items unused until later phases wire them in

pub mod mode;
pub mod global_pool;
pub mod rdma_pool;
pub mod rdma_runtime;
pub mod resolution_buffer;
pub mod runtime;

pub use mode::Mode;

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use anyhow::{anyhow, Result};

use common::{DIRECT_LIMIT, PAGE_SIZE, PageId, ShmOffset, Superblock};

use global_pool::{GlobalPool, GLOBAL_POOL_INITIAL_SIZE};
use resolution_buffer::ResolutionBuffer;

/// Number of wasm32-window slots to reserve at flip time as initial
/// residency targets.  These are allocated from the direct bump
/// allocator (4096 slots × 4 KiB = 16 MiB of direct capacity sacrificed
/// to the resolution buffer).  Without this seeding, the first
/// `alloc_paged_page` would fail with "no free slots" because the
/// buffer starts empty and there's no prior direct-free activity to
/// populate it.
pub const FLIP_SEED_SLOTS: usize = 4096;

/// Per-subprocess extended-pool state.
///
/// Each `wasm-call` process owns exactly one `ExtendedPool`.  It is NOT
/// shared across processes; the mode flip is a local decision made
/// when *this* subprocess's own direct-window bump pointer crosses the
/// threshold.  Cross-process coordination is unnecessary because each
/// worker has its own wasm32 window to fill.
pub struct ExtendedPool {
    mode: Mode,
    /// Count of live paged-mode PageIds (`>= DIRECT_LIMIT`).  Used as
    /// the correctness gate for `flip_to_direct`.
    global_live: u64,
    /// Last observed direct-bump offset.  Kept so `flip_to_direct` can
    /// check the exit-hysteresis threshold without the caller having
    /// to re-read the superblock.
    last_bump: ShmOffset,
    global_pool: Option<GlobalPool>,
    resolution_buffer: Option<ResolutionBuffer>,
}

impl ExtendedPool {
    /// Create a new extended pool in `Direct` mode with no backing pool
    /// or resolution buffer yet.  Both are allocated lazily when the
    /// first flip to `Paged` happens.
    pub fn new() -> Self {
        Self {
            mode: Mode::Direct,
            global_live: 0,
            last_bump: 0,
            global_pool: None,
            resolution_buffer: None,
        }
    }

    /// Current allocation mode.  `reclaimer.rs` consults this on every
    /// `alloc_page` / `free_page_chain` to decide whether to take the
    /// fast direct path or dispatch here.
    #[inline]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Current count of live paged-mode PageIds.
    #[inline]
    pub fn global_live(&self) -> u64 {
        self.global_live
    }

    /// Last observed direct bump offset.
    #[inline]
    pub fn last_bump(&self) -> ShmOffset {
        self.last_bump
    }

    /// Update the pool's view of the direct bump pointer and, if the
    /// 80% threshold has now been crossed, flip to `Paged` mode.
    /// Idempotent — repeated calls past the threshold while already
    /// `Paged` do nothing.
    ///
    /// `splice_addr` is the host-side base of the wasm32 SHM window,
    /// needed so that `flip_to_paged` can anchor its
    /// `ResolutionBuffer`.  Callers that never trip the flip can pass
    /// any value — it's only read inside `flip_to_paged`.
    pub fn check_bump(&mut self, new_bump: ShmOffset, splice_addr: usize) -> Result<()> {
        self.last_bump = new_bump;
        if self.mode == Mode::Paged {
            return Ok(());
        }
        if mode::should_enter_paged(new_bump) {
            self.flip_to_paged(splice_addr)?;
        }
        Ok(())
    }

    /// Transition from `Direct` to `Paged`.  Constructs the
    /// `GlobalPool` at `/dev/shm/webs-global-<pid>` with the initial
    /// committed size from `global_pool::GLOBAL_POOL_INITIAL_SIZE`,
    /// builds a fresh `ResolutionBuffer` rooted at `splice_addr`,
    /// and seeds the buffer with [`FLIP_SEED_SLOTS`] residency slots
    /// drawn from the direct bump allocator.  Idempotent — repeated
    /// calls while already `Paged` are no-ops.
    ///
    /// Seeding reserves `FLIP_SEED_SLOTS * PAGE_SIZE` bytes of direct
    /// capacity that will never be returned to the direct allocator.
    /// This is the cost of having paged allocation available from
    /// the moment of the flip.
    pub fn flip_to_paged(&mut self, splice_addr: usize) -> Result<()> {
        if self.mode == Mode::Paged {
            return Ok(());
        }

        let pid = std::process::id();
        let path: PathBuf = PathBuf::from(format!("/dev/shm/webs-global-{pid}"));
        let pool = GlobalPool::new(&path, GLOBAL_POOL_INITIAL_SIZE)
            .map_err(|e| anyhow!(
                "flip_to_paged: cannot create global pool at {}: {e}",
                path.display(),
            ))?;
        let buf = ResolutionBuffer::new(splice_addr);

        self.global_pool       = Some(pool);
        self.resolution_buffer = Some(buf);
        self.mode              = Mode::Paged;

        // Seed residency slots from the direct bump allocator.  Any
        // failure here leaves the pool in Paged mode with an empty
        // buffer — callers can still seed later via `on_direct_free`.
        if let Err(e) = self.seed_residency(splice_addr, FLIP_SEED_SLOTS) {
            eprintln!("[ExtendedPool] flip seeding failed: {e}");
        }

        eprintln!(
            "[ExtendedPool] flipped to Paged mode at bump={:#x} ({:.1}% of DIRECT_LIMIT), \
             global pool at {}, seeded residency slots = {}",
            self.last_bump,
            100.0 * (self.last_bump as f64) / (DIRECT_LIMIT as f64),
            path.display(),
            self.resolution_buffer
                .as_ref()
                .map(|b| b.free_count())
                .unwrap_or(0),
        );
        Ok(())
    }

    /// Reserve `count` pages from the direct bump allocator and hand
    /// them to the resolution buffer as residency targets.  Advances
    /// `sb.bump_allocator` by `count * PAGE_SIZE` in one atomic RMW;
    /// rolls back if the reservation would exceed `global_capacity`.
    fn seed_residency(&mut self, splice_addr: usize, count: usize) -> Result<()> {
        let buf = self.resolution_buffer.as_mut()
            .ok_or_else(|| anyhow!("seed_residency: no ResolutionBuffer"))?;
        let sb = unsafe { &*(splice_addr as *const Superblock) };

        let bytes = PAGE_SIZE * count as ShmOffset;
        let start = sb.bump_allocator.fetch_add(bytes, Ordering::AcqRel);
        let cap   = sb.global_capacity.load(Ordering::Acquire);
        if start + bytes > cap {
            // Roll back — insufficient capacity to seed.
            sb.bump_allocator.fetch_sub(bytes, Ordering::AcqRel);
            return Err(anyhow!(
                "seed_residency: insufficient direct capacity ({} wanted, {} free)",
                bytes,
                cap.saturating_sub(start),
            ));
        }
        for i in 0..count {
            let offset = start + (i as ShmOffset) * PAGE_SIZE;
            let slot_idx = (offset / PAGE_SIZE) as usize;
            buf.release_direct_slot(slot_idx);
        }
        Ok(())
    }

    /// Allocate a new paged-mode page.  Bumps the global pool,
    /// installs the new page into a free resolution-buffer slot, and
    /// returns `(PageId, *mut Page)`.  The PageId is always
    /// `>= DIRECT_LIMIT`.
    ///
    /// Fails if the pool is not in `Paged` mode (programmer error),
    /// or if the resolution buffer has no Free slots and every
    /// Resident is pinned.  The caller is expected to have seeded
    /// the free pool via prior direct-free activity, or via the
    /// flip-time seeding that Phase 2.4c will add.
    ///
    /// `rdma_bound` is accepted for forward compatibility with the
    /// DAG-analyzer plumbing; for Phase 2.4b it has no effect (all
    /// paged allocations take the same path).
    pub fn alloc_page(&mut self, _rdma_bound: bool) -> Result<(PageId, *mut common::Page)> {
        if self.mode != Mode::Paged {
            return Err(anyhow!(
                "ExtendedPool::alloc_page called in {:?} mode", self.mode,
            ));
        }
        let pool = self.global_pool.as_mut()
            .ok_or_else(|| anyhow!("ExtendedPool has no GlobalPool"))?;
        let buf = self.resolution_buffer.as_mut()
            .ok_or_else(|| anyhow!("ExtendedPool has no ResolutionBuffer"))?;

        let id = pool.alloc()?;
        let ptr = buf.install(id, pool)?;
        self.global_live += 1;
        Ok((id, ptr))
    }

    /// Free a paged-mode PageId.  Evicts it from the resolution
    /// buffer (if resident), returns its global-pool offset to the
    /// global freelist, and decrements `global_live`.  Must only be
    /// called with `id >= DIRECT_LIMIT`.
    ///
    /// Flip-back-to-direct is NOT triggered here; that's Phase 2.4c.
    pub fn free_page(&mut self, id: PageId) -> Result<()> {
        debug_assert!(id >= DIRECT_LIMIT);
        if self.mode != Mode::Paged {
            return Err(anyhow!(
                "ExtendedPool::free_page called in {:?} mode", self.mode,
            ));
        }
        let pool = self.global_pool.as_mut()
            .ok_or_else(|| anyhow!("ExtendedPool has no GlobalPool"))?;
        let buf = self.resolution_buffer.as_mut()
            .ok_or_else(|| anyhow!("ExtendedPool has no ResolutionBuffer"))?;

        buf.evict(id);           // no-op if not currently resident
        pool.free(id);
        self.global_live = self.global_live.saturating_sub(1);
        Ok(())
    }

    /// Notify the pool that a direct-mode PageId has been freed
    /// while the pool is in `Paged` mode.  The wasm32 slot for that
    /// page is handed to the resolution buffer as an additional
    /// residency target instead of returning to the direct freelist.
    ///
    /// `id` must be a direct-mode PageId (`< DIRECT_LIMIT`); it is
    /// converted to a slot index via `id / PAGE_SIZE`.  No-op if
    /// the pool is still in `Direct` mode.
    pub fn on_direct_free(&mut self, id: PageId) {
        if self.mode != Mode::Paged {
            return;
        }
        debug_assert!(id < DIRECT_LIMIT);
        let slot_idx = (id / PAGE_SIZE as u64) as usize;
        if let Some(buf) = self.resolution_buffer.as_mut() {
            buf.release_direct_slot(slot_idx);
        }
    }

    /// Resolve a PageId to its wasm32 pointer on the host side.
    /// Direct PageIds (`< DIRECT_LIMIT`) resolve by arithmetic against
    /// `splice_addr`; paged PageIds go through the resolution buffer's
    /// cache (triggering an install on miss).  `splice_addr` is
    /// threaded in by the caller so this method doesn't have to
    /// remember it.
    pub fn resolve(&mut self, id: PageId, splice_addr: usize) -> Result<*mut common::Page> {
        if id < DIRECT_LIMIT {
            return Ok((splice_addr + id as usize) as *mut common::Page);
        }
        let pool = self.global_pool.as_ref()
            .ok_or_else(|| anyhow!("ExtendedPool::resolve paged id with no GlobalPool"))?;
        let buf = self.resolution_buffer.as_mut()
            .ok_or_else(|| anyhow!("ExtendedPool::resolve paged id with no ResolutionBuffer"))?;
        if let Some(p) = buf.get(id) {
            return Ok(p);
        }
        buf.install(id, pool)
    }
}

impl Default for ExtendedPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::DIRECT_LIMIT;
    use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};
    use std::num::NonZeroUsize;

    /// Allocate an anonymous RW region to stand in for the wasm32 SHM
    /// window during tests.  Returns (base, size).
    fn make_window(n_pages: usize) -> (usize, usize) {
        let size = n_pages * PAGE_SIZE as usize;
        let base = unsafe {
            mmap(
                None,
                NonZeroUsize::new(size).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
                None::<&std::fs::File>,
                0,
            ).unwrap() as usize
        };
        (base, size)
    }

    fn free_window(base: usize, size: usize) {
        unsafe { let _ = munmap(base as *mut libc::c_void, size); }
    }

    #[test]
    fn starts_in_direct_mode() {
        let pool = ExtendedPool::new();
        assert_eq!(pool.mode(), Mode::Direct);
        assert_eq!(pool.global_live(), 0);
    }

    #[test]
    fn check_bump_below_threshold_stays_direct() {
        let mut pool = ExtendedPool::new();
        pool.check_bump(1024, 0).unwrap();
        assert_eq!(pool.mode(), Mode::Direct);
        // 79% — just under.
        pool.check_bump((DIRECT_LIMIT * 79 / 100) as ShmOffset, 0).unwrap();
        assert_eq!(pool.mode(), Mode::Direct);
    }

    #[test]
    fn check_bump_at_or_above_80_percent_flips_paged() {
        let (base, size) = make_window(4);
        let mut pool = ExtendedPool::new();
        // Integer math: strict threshold is `bump * 10 >= DIRECT_LIMIT * 8`.
        // Nudge by one page to clearly exceed.
        let bump = (DIRECT_LIMIT * 8 / 10) as ShmOffset + PAGE_SIZE;
        pool.check_bump(bump, base).unwrap();
        assert_eq!(pool.mode(), Mode::Paged);
        // Both backing structs should exist post-flip.
        assert!(pool.global_pool.is_some());
        assert!(pool.resolution_buffer.is_some());
        free_window(base, size);
    }

    #[test]
    fn flip_to_paged_is_idempotent() {
        let (base, size) = make_window(4);
        let mut pool = ExtendedPool::new();
        pool.flip_to_paged(base).unwrap();
        assert_eq!(pool.mode(), Mode::Paged);
        pool.flip_to_paged(base).unwrap();  // no-op
        assert_eq!(pool.mode(), Mode::Paged);
        free_window(base, size);
    }

    #[test]
    fn last_bump_updates_even_when_no_flip() {
        let mut pool = ExtendedPool::new();
        pool.check_bump(4096, 0).unwrap();
        assert_eq!(pool.last_bump(), 4096);
        pool.check_bump(8192, 0).unwrap();
        assert_eq!(pool.last_bump(), 8192);
    }

    #[test]
    fn paged_alloc_and_free_roundtrip() {
        // End-to-end: flip to paged, seed the resolution buffer with
        // free slots via on_direct_free, alloc 3 pages, verify IDs
        // and pointers, write/read sentinels, free them all.
        let n_pages = 8;
        let (base, size) = make_window(n_pages);

        let mut pool = ExtendedPool::new();
        pool.flip_to_paged(base).unwrap();
        assert_eq!(pool.mode(), Mode::Paged);

        // Seed free slots by pretending that slot indices 0..4 were
        // direct pages that got freed during paged mode.
        for slot_idx in 0..4u64 {
            let fake_direct_id = slot_idx * PAGE_SIZE as u64;
            pool.on_direct_free(fake_direct_id);
        }
        assert_eq!(
            pool.resolution_buffer.as_ref().unwrap().free_count(),
            4,
        );

        // Allocate 3 paged pages and write sentinels through the
        // returned pointers.
        let mut ids = Vec::new();
        for i in 0..3usize {
            let (id, ptr) = pool.alloc_page(false).unwrap();
            assert!(id >= DIRECT_LIMIT);
            unsafe { *(ptr as *mut u64) = 0xABCD_0000 + i as u64; }
            ids.push(id);
        }
        assert_eq!(pool.global_live(), 3);

        // Read back through resolve() — should hit the cache.
        for (i, &id) in ids.iter().enumerate() {
            let ptr = pool.resolve(id, base).unwrap();
            let v = unsafe { *(ptr as *const u64) };
            assert_eq!(v, 0xABCD_0000 + i as u64, "sentinel mismatch at id {id:#x}");
        }

        // Free them; global_live decrements to 0.
        for id in ids {
            pool.free_page(id).unwrap();
        }
        assert_eq!(pool.global_live(), 0);

        free_window(base, size);
    }

    #[test]
    fn resolve_direct_id_is_pure_arithmetic() {
        // Even in Direct mode, resolve() should work for direct-
        // mode PageIds — it's an id+splice_addr addition.
        let mut pool = ExtendedPool::new();
        let base = 0x1000_0000usize;
        let id: PageId = 0x12000;    // direct-mode (< DIRECT_LIMIT)
        let p = pool.resolve(id, base).unwrap();
        assert_eq!(p as usize, base + id as usize);
    }

    /// Build a window big enough to host a real `Superblock` struct
    /// at offset 0 plus enough page space after it for allocation +
    /// seeding.  The returned base is populated with a valid
    /// superblock pointing at `global_capacity == base + window_len`.
    fn make_superblock_window(extra_pages: usize) -> (usize, usize) {
        use common::{Superblock, BUMP_ALLOCATOR_START, PAGE_SIZE};

        let sb_size = std::mem::size_of::<Superblock>();
        let extra_bytes = extra_pages * PAGE_SIZE as usize;
        // Cover Superblock + enough arena/page space so that BUMP_ALLOCATOR_START
        // lies inside the window and we still have `extra_pages` of room past it.
        let total = (BUMP_ALLOCATOR_START as usize + extra_bytes).max(sb_size);
        // Round up to PAGE_SIZE.
        let total = (total + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize
                    * PAGE_SIZE as usize;
        let (base, size) = make_window(total / PAGE_SIZE as usize);

        // Initialize the superblock in place.  Anonymous mmap starts
        // zero-filled, so we only need to set the non-zero fields.
        let sb = unsafe { &mut *(base as *mut Superblock) };
        sb.magic = 0xDEADBEEF;
        sb.bump_allocator.store(BUMP_ALLOCATOR_START, Ordering::Release);
        sb.global_capacity.store(total as ShmOffset, Ordering::Release);

        (base, size)
    }

    /// End-to-end test of the reclaimer fallback via the singleton.
    ///
    /// WARNING: this is the ONLY test that touches
    /// `extended_pool::runtime::*` state.  The singleton is a
    /// process-global `OnceLock<Mutex<ExtendedPool>>`, so parallel
    /// tests touching it would race.  If a second singleton-using
    /// test is ever added, run tests with `--test-threads=1` or
    /// introduce explicit serialization.
    #[test]
    fn reclaimer_falls_back_to_paged_when_direct_exhausted() {
        use crate::runtime::extended_pool::runtime;
        use crate::runtime::mem_operation::reclaimer;
        use common::BUMP_ALLOCATOR_START;

        // 1. Build a window with only a few pages of direct capacity
        //    past BUMP_ALLOCATOR_START, plus enough room for seeding.
        //    `global_capacity` covers the whole window so seeding can
        //    succeed but only a handful of normal allocs fit before
        //    exhaustion.
        let margin_pages = 6;
        let (base, size) = make_superblock_window(FLIP_SEED_SLOTS + margin_pages);

        // 2. Reset the singleton so we start in a known state.
        runtime::reset_for_test();

        // 3. Force-flip to Paged.  This seeds the resolution buffer
        //    and advances `bump_allocator` by FLIP_SEED_SLOTS pages.
        runtime::flip_to_paged(base).unwrap();
        assert_eq!(runtime::current_mode(), Mode::Paged);

        // 4. Exhaust the remaining direct capacity.  Every call goes
        //    through the reclaimer's direct-bump path and should
        //    return a direct PageId (`< DIRECT_LIMIT`).
        let mut direct_ids = Vec::new();
        for _ in 0..margin_pages {
            let id = reclaimer::alloc_page(base).unwrap();
            assert!(
                id < DIRECT_LIMIT,
                "expected direct id, got paged id {id:#x}",
            );
            direct_ids.push(id);
        }

        // 5. Direct is now exhausted.  The NEXT alloc must fall
        //    through to the extended-pool path.
        let paged_id = reclaimer::alloc_page(base).unwrap();
        assert!(
            paged_id >= DIRECT_LIMIT,
            "expected paged id after direct exhaustion, got {paged_id:#x}",
        );

        // 6. Resolve the paged id and write a sentinel to prove the
        //    wasm32 overlay works end-to-end through the singleton.
        let ptr = runtime::resolve(paged_id, base).unwrap();
        unsafe { *(ptr as *mut u64) = 0xBAD_DEC0DE; }
        let ptr2 = runtime::resolve(paged_id, base).unwrap();
        unsafe { assert_eq!(*(ptr2 as *const u64), 0xBAD_DEC0DE); }

        // 7. Free via the widened free_page_chain.  Because paged
        //    mode is active, this takes the mixed-dispatch path.
        reclaimer::free_page_chain(base, paged_id);

        // 8. Clean up.  Reset the singleton so later test runs in
        //    the same process start clean.
        runtime::reset_for_test();
        free_window(base, size);
    }

    #[test]
    fn flip_with_seeding_populates_free_slots_against_real_superblock() {
        // Drive flip_to_paged against a real Superblock so seed_residency
        // has working bump_allocator / global_capacity atomics.
        let (base, size) = make_superblock_window(FLIP_SEED_SLOTS + 8);
        let mut pool = ExtendedPool::new();
        pool.flip_to_paged(base).unwrap();
        // Seeded with FLIP_SEED_SLOTS free slots.
        assert_eq!(
            pool.resolution_buffer.as_ref().unwrap().free_count(),
            FLIP_SEED_SLOTS,
        );
        // Global pool exists and is empty so far.
        assert_eq!(pool.global_live(), 0);

        // Now allocate a paged page — should succeed and consume one
        // of the seeded free slots.
        let (id, ptr) = pool.alloc_page(false).unwrap();
        assert!(id >= DIRECT_LIMIT);
        assert_eq!(
            pool.resolution_buffer.as_ref().unwrap().free_count(),
            FLIP_SEED_SLOTS - 1,
        );
        assert_eq!(pool.global_live(), 1);
        // Write a sentinel, read it back through resolve().
        unsafe { *(ptr as *mut u64) = 0xFEED_1234_5678_ABCD; }
        let p2 = pool.resolve(id, base).unwrap();
        assert_eq!(p2, ptr);
        unsafe { assert_eq!(*(p2 as *const u64), 0xFEED_1234_5678_ABCD); }

        // Free it.
        pool.free_page(id).unwrap();
        assert_eq!(pool.global_live(), 0);

        free_window(base, size);
    }
}
