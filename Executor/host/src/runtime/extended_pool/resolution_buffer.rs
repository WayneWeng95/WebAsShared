//! Resolution buffer — software TLB for paged-mode PageIds.
//!
//! When paged mode is active, every PageId `>= DIRECT_LIMIT` is backed
//! by a page in the [`GlobalPool`].  To let the guest dereference such
//! a PageId through an ordinary wasm32 pointer, we "resolve" it by
//! `MAP_FIXED`-overlaying the global page onto a 4 KiB slot inside the
//! wasm32 SHM window.  The set of currently-resident pages is the
//! resolution buffer's live set; it behaves like a software TLB.
//!
//! ## Slot states
//!
//! Each 4 KiB slot in the wasm32 window is in exactly one of three
//! states while paged mode is active:
//!
//! | State       | Meaning                                            | Tracked here?        |
//! |-------------|----------------------------------------------------|----------------------|
//! | LiveDirect  | holds a live direct-mode page (PageId < DIRECT_LIMIT) | No — implicit     |
//! | Resident(id)| currently mapped to global PageId `id`             | Yes, in `residents` |
//! | Free        | available for installation as a residency target   | Yes, in `free_slots`|
//!
//! The buffer never touches LiveDirect slots — they stay as they were
//! before paged mode started, and are converted to Free state only
//! via an explicit [`release_direct_slot`] call from the reclaimer
//! when a direct page is freed during paged mode.
//!
//! ## LRU and pinning
//!
//! Resident slots participate in an LRU ordered by last-access time.
//! When no Free slot is available, `install` evicts the LRU Resident
//! (bumping the generation counter so the guest-side cache can
//! invalidate).  Slots in the `pinned` set are skipped during eviction
//! — used for the hot bump-tail so concurrent writers don't lose
//! their target mid-write.
//!
//! ## Overlay semantics on Linux
//!
//! `mmap(slot_addr, 4096, PROT_READ|PROT_WRITE, MAP_SHARED|MAP_FIXED,
//! global_pool.fd, file_offset)` unconditionally replaces whatever
//! mapping exists at `slot_addr` with a new mapping backed by the
//! global pool file.  This is why the invariant "never overlay a
//! LiveDirect slot" is load-bearing: if we overlaid one, the direct
//! page's data would silently be replaced with whatever lives at the
//! global pool offset.  The reclaimer enforces the invariant by
//! calling [`release_direct_slot`] exactly once per freed direct page
//! before the slot is ever eligible for installation.

#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use common::{DIRECT_LIMIT, PAGE_SIZE, Page, PageId};
use nix::sys::mman::{mmap, MapFlags, ProtFlags};
use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU32, Ordering};

use super::global_pool::GlobalPool;

/// Purely documentary.  The buffer does not actually store this for
/// LiveDirect slots — those are invisible to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotState {
    LiveDirect,
    Resident(PageId),
    Free,
}

pub struct ResolutionBuffer {
    /// Host-side base address of the wasm32 SHM window.  Slot `i`'s
    /// address is `base + i * PAGE_SIZE`.
    base: usize,
    /// Slots currently in state Free (available for overlay).
    free_slots: Vec<usize>,
    /// Paged PageIds currently resident in the window.  Value is the
    /// slot index they occupy.
    residents: HashMap<PageId, usize>,
    /// Reverse lookup used during eviction.
    slot_to_page: HashMap<usize, PageId>,
    /// LRU order over Resident slots.  Front = oldest (next to evict),
    /// back = newest.  A slot present in this deque is always also
    /// in `slot_to_page`.
    lru: VecDeque<usize>,
    /// Slots that cannot be evicted — e.g. the hot bump-tail being
    /// actively written.
    pinned: HashSet<usize>,
    /// Incremented on every eviction.  The guest-side resident cache
    /// watches this so it can drop stale `PageId -> WasmPtr` entries
    /// when the host replaces them.
    generation: AtomicU32,
}

impl ResolutionBuffer {
    /// Build an empty buffer rooted at `base`.  No slots are eligible
    /// for installation yet — the reclaimer must call
    /// [`release_direct_slot`] (or `seed_free_slots`) after a
    /// flip-to-paged to populate the free pool.
    pub fn new(base: usize) -> Self {
        Self {
            base,
            free_slots: Vec::new(),
            residents: HashMap::new(),
            slot_to_page: HashMap::new(),
            lru: VecDeque::new(),
            pinned: HashSet::new(),
            generation: AtomicU32::new(0),
        }
    }

    /// Cache lookup.  On hit, touches LRU (slot moves to the MRU end)
    /// and returns the wasm32 pointer.  On miss, returns `None` —
    /// the caller must `install` to fetch.
    pub fn get(&mut self, page_id: PageId) -> Option<*mut Page> {
        let slot = *self.residents.get(&page_id)?;
        self.touch(slot);
        Some(self.slot_addr(slot) as *mut Page)
    }

    /// Install a paged PageId into a wasm32 slot, overlaying its
    /// global-pool page via `MAP_FIXED`.  Returns the wasm32 pointer
    /// the caller can use until the next eviction of this slot.
    ///
    /// If `page_id` is already resident, the call degenerates to a
    /// cache hit with LRU touch.  Otherwise, picks a Free slot; if
    /// none is available, evicts the LRU non-pinned Resident and
    /// reuses that slot.  Fails only when every Resident slot is
    /// pinned and no Free slots exist.
    pub fn install(&mut self, page_id: PageId, pool: &GlobalPool) -> Result<*mut Page> {
        debug_assert!(
            page_id >= DIRECT_LIMIT,
            "ResolutionBuffer::install given direct-mode PageId {:#x}", page_id,
        );
        if let Some(hit) = self.get(page_id) {
            return Ok(hit);
        }

        let slot = match self.free_slots.pop() {
            Some(s) => s,
            None => self.evict_lru().ok_or_else(|| {
                anyhow!(
                    "resolution buffer has no free slots and every \
                     resident is pinned (free=0, resident={}, pinned={})",
                    self.residents.len(),
                    self.pinned.len(),
                )
            })?,
        };

        let slot_addr = self.slot_addr(slot);
        let file_offset = pool.file_offset_of(page_id);

        // SAFETY: `slot_addr` is inside the wasm32 SHM window which
        // the caller has already mapped.  MAP_FIXED unconditionally
        // replaces that 4 KiB with a new mapping backed by the
        // global pool file.  The slot is in state Free per the
        // invariant enforced by the reclaimer, so no live data is
        // clobbered.
        unsafe {
            mmap(
                NonZeroUsize::new(slot_addr),
                NonZeroUsize::new(PAGE_SIZE as usize).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED | MapFlags::MAP_FIXED,
                Some(pool.backing_file()),
                file_offset as i64,
            )
            .context("MAP_FIXED overlay for resolution slot")?;
        }

        self.residents.insert(page_id, slot);
        self.slot_to_page.insert(slot, page_id);
        self.lru.push_back(slot);
        Ok(slot_addr as *mut Page)
    }

    /// Move the slot currently holding a direct-mode page into the
    /// Free pool.  Called by the reclaimer when a direct PageId is
    /// freed while paged mode is active — its wasm32 slot becomes an
    /// available residency target instead of returning to the direct
    /// freelist.
    ///
    /// Idempotent: redundant calls with the same `slot_idx` are
    /// ignored.
    pub fn release_direct_slot(&mut self, slot_idx: usize) {
        if self.slot_to_page.contains_key(&slot_idx) || self.free_slots.contains(&slot_idx) {
            return;
        }
        self.free_slots.push(slot_idx);
    }

    /// Explicitly evict a specific Resident PageId, moving its slot
    /// back into the Free pool and bumping the generation counter.
    /// No-op (returns `false`) if the PageId is not currently
    /// resident.  Used by `ExtendedPool::free_page` when a paged
    /// PageId is being freed while still resident.
    pub fn evict(&mut self, page_id: PageId) -> bool {
        let slot = match self.residents.remove(&page_id) {
            Some(s) => s,
            None => return false,
        };
        self.slot_to_page.remove(&slot);
        if let Some(pos) = self.lru.iter().position(|&s| s == slot) {
            self.lru.remove(pos);
        }
        self.pinned.remove(&slot);
        self.free_slots.push(slot);
        self.generation.fetch_add(1, Ordering::Release);
        true
    }

    /// Drain every slot currently in the Free pool and return them.
    /// Used on `flip_to_direct` to repopulate the reclaimer's direct
    /// freelist.  Resident slots are NOT drained — flip-back must
    /// only happen when `residents` is already empty (global_live == 0).
    pub fn drain_free_slots(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.free_slots)
    }

    /// Pin `slot_idx` so eviction skips it.
    pub fn pin(&mut self, slot_idx: usize) { self.pinned.insert(slot_idx); }
    /// Unpin `slot_idx`.
    pub fn unpin(&mut self, slot_idx: usize) { self.pinned.remove(&slot_idx); }

    /// Current generation counter.  Bumped on every eviction.
    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn free_count(&self)     -> usize { self.free_slots.len() }
    pub fn resident_count(&self) -> usize { self.residents.len() }
    pub fn pinned_count(&self)   -> usize { self.pinned.len() }

    /// Evict the oldest non-pinned Resident.  Returns the freed slot
    /// index for immediate reuse, or `None` if every Resident is
    /// pinned.  Bumps `generation`.
    fn evict_lru(&mut self) -> Option<usize> {
        // Linear scan from the front for the first non-pinned slot.
        let len = self.lru.len();
        for i in 0..len {
            let slot = self.lru[i];
            if !self.pinned.contains(&slot) {
                self.lru.remove(i);
                let page_id = self.slot_to_page.remove(&slot)
                    .expect("lru and slot_to_page out of sync");
                self.residents.remove(&page_id);
                self.generation.fetch_add(1, Ordering::Release);
                return Some(slot);
            }
        }
        None
    }

    /// Touch a slot: move it to the MRU end of the LRU deque.  O(n)
    /// linear scan; fine for the expected working set sizes (hundreds
    /// to low thousands of Resident slots).
    fn touch(&mut self, slot: usize) {
        if let Some(pos) = self.lru.iter().position(|&s| s == slot) {
            self.lru.remove(pos);
        }
        self.lru.push_back(slot);
    }

    #[inline]
    fn slot_addr(&self, slot_idx: usize) -> usize {
        self.base + slot_idx * PAGE_SIZE as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::extended_pool::global_pool::GlobalPool;
    use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};
    use std::num::NonZeroUsize;
    use std::path::PathBuf;

    /// Allocate an anonymous read/write region of `n_pages` pages that
    /// stands in for the wasm32 SHM window.  Returns its base address.
    /// The caller must `munmap` it at the end of the test.
    fn make_window(n_pages: usize) -> usize {
        unsafe {
            mmap(
                None,
                NonZeroUsize::new(n_pages * PAGE_SIZE as usize).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
                None::<&std::fs::File>,
                0,
            )
            .unwrap() as usize
        }
    }

    fn free_window(base: usize, n_pages: usize) {
        unsafe {
            let _ = munmap(base as *mut libc::c_void, n_pages * PAGE_SIZE as usize);
        }
    }

    fn test_pool_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "webs-resbuf-test-{}-{}-{}",
            std::process::id(),
            label,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ))
    }

    #[test]
    fn install_miss_then_hit() {
        let n_pages = 4;
        let base = make_window(n_pages);
        let mut pool = GlobalPool::new(
            &test_pool_path("miss_then_hit"),
            4 * PAGE_SIZE as usize,
        ).unwrap();
        let mut buf = ResolutionBuffer::new(base);

        // Seed two free slots.
        buf.release_direct_slot(0);
        buf.release_direct_slot(1);
        assert_eq!(buf.free_count(), 2);

        let id = pool.alloc().unwrap();
        let p = buf.install(id, &pool).unwrap();
        assert_eq!(buf.resident_count(), 1);
        assert_eq!(buf.free_count(), 1);

        // Write through the overlay; it should land in the global
        // pool's file-backed page.
        unsafe { *(p as *mut u64) = 0x1234_5678_9abc_def0; }
        let pool_ptr = pool.host_addr_of(id) as *const u64;
        assert_eq!(unsafe { *pool_ptr }, 0x1234_5678_9abc_def0);

        // Cache hit: same pointer, no new slot consumed.
        let p2 = buf.install(id, &pool).unwrap();
        assert_eq!(p, p2);
        assert_eq!(buf.resident_count(), 1);
        assert_eq!(buf.free_count(), 1);

        // Generation did not advance on hit.
        assert_eq!(buf.generation(), 0);

        free_window(base, n_pages);
    }

    #[test]
    fn lru_eviction_when_slots_exhausted() {
        let n_pages = 4;
        let base = make_window(n_pages);
        let mut pool = GlobalPool::new(
            &test_pool_path("lru"),
            8 * PAGE_SIZE as usize,
        ).unwrap();
        let mut buf = ResolutionBuffer::new(base);

        // Two slots available.
        buf.release_direct_slot(0);
        buf.release_direct_slot(1);

        let a = pool.alloc().unwrap();
        let b = pool.alloc().unwrap();
        let c = pool.alloc().unwrap();

        buf.install(a, &pool).unwrap();
        buf.install(b, &pool).unwrap();
        assert_eq!(buf.free_count(), 0);
        assert_eq!(buf.resident_count(), 2);

        // Touch `a` so `b` becomes the LRU victim.
        buf.get(a).unwrap();

        let gen_before = buf.generation();
        // Install `c` → evicts `b`.
        buf.install(c, &pool).unwrap();
        assert_eq!(buf.resident_count(), 2);
        assert!(buf.get(b).is_none(), "b should have been evicted");
        assert!(buf.get(a).is_some(), "a should still be resident");
        assert!(buf.get(c).is_some(), "c should be resident");
        assert_eq!(buf.generation(), gen_before + 1);

        free_window(base, n_pages);
    }

    #[test]
    fn pinned_slots_never_evict() {
        let n_pages = 2;
        let base = make_window(n_pages);
        let mut pool = GlobalPool::new(
            &test_pool_path("pinned"),
            4 * PAGE_SIZE as usize,
        ).unwrap();
        let mut buf = ResolutionBuffer::new(base);

        buf.release_direct_slot(0);
        buf.release_direct_slot(1);

        let a = pool.alloc().unwrap();
        let b = pool.alloc().unwrap();
        let c = pool.alloc().unwrap();

        let pa = buf.install(a, &pool).unwrap();
        buf.install(b, &pool).unwrap();

        // Pin whichever slot holds `a`.
        let slot_of_a = ((pa as usize) - base) / PAGE_SIZE as usize;
        buf.pin(slot_of_a);

        // Install c → must evict b (a is pinned).
        buf.install(c, &pool).unwrap();
        assert!(buf.get(a).is_some(), "a was pinned");
        assert!(buf.get(b).is_none(),  "b should be the eviction victim");
        assert!(buf.get(c).is_some());

        // Unpinning c's eviction slot lets future installs evict it.
        buf.unpin(slot_of_a);

        free_window(base, n_pages);
    }

    #[test]
    fn drain_free_slots_returns_all_free() {
        let base = make_window(1);
        let mut buf = ResolutionBuffer::new(base);
        buf.release_direct_slot(7);
        buf.release_direct_slot(42);
        buf.release_direct_slot(100);
        let drained = buf.drain_free_slots();
        assert_eq!(drained.len(), 3);
        assert_eq!(buf.free_count(), 0);
        // drained contains the three slot ids (order not strictly guaranteed).
        assert!(drained.contains(&7));
        assert!(drained.contains(&42));
        assert!(drained.contains(&100));
        free_window(base, 1);
    }

    #[test]
    fn release_direct_slot_is_idempotent() {
        let base = make_window(1);
        let mut buf = ResolutionBuffer::new(base);
        buf.release_direct_slot(5);
        buf.release_direct_slot(5);
        buf.release_direct_slot(5);
        assert_eq!(buf.free_count(), 1);
        free_window(base, 1);
    }
}
