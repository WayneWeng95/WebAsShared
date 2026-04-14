// Unified host-side SHM page allocator and reclaimer.
//
// This module is the host-side mirror of `guest/src/api/page_allocator.rs`.
// Both sides share the same bump heap and the same sharded Treiber-stack free
// list inside the Superblock, so a page freed by either side is immediately
// available to the other.
//
// # Allocation strategy (`alloc_page`)
//
//   1. Compute a preferred shard via a global round-robin counter.
//   2. Scan shards starting from the preferred one; CAS-pop the first non-empty
//      shard found.  `spin_loop()` on each CAS failure to yield the pipeline.
//   3. If all shards are empty, bump-allocate from `sb.bump_allocator`
//      (`fetch_add` — one atomic op, never retries).
//
// # Free strategy (`free_page_chain`)
//
//   Push the entire chain as a single unit onto the shard determined by
//   `(head_offset / PAGE_SIZE) % FREE_LIST_SHARD_COUNT`.  The shard is
//   deterministic per chain so the same chain always returns to the same
//   shard, keeping related pages cache-local and avoiding unnecessary
//   cross-shard contention.
//
// # Sharding rationale
//
//   A single Treiber-stack head serialises all concurrent alloc/free at one
//   atomic word.  Under N concurrent threads the CAS retry rate is O(N²) in
//   the worst case.  With FREE_LIST_SHARD_COUNT shards, threads that hash to
//   different shards never contend at all; expected retries drop to O(N/shards).
//
// # ABA note
//
//   Because page offsets are 32-bit and a page can cycle through alloc→free
//   faster than a thread can complete its CAS window, the classic Treiber ABA
//   hazard exists.  The practical impact is bounded: a page can only be freed
//   while its refcount (tracked in the DAG runner) is zero, which prevents
//   the most dangerous class of ABA (use-after-free).  Tagged-pointer
//   mitigation (packing a 12-bit generation counter into the always-zero low
//   bits of the 4 KiB-aligned offset) is a future hardening option.
//
// # Thread safety
//
//   Both `alloc_page` and `free_page_chain` use SeqCst CAS retry loops and
//   are safe to call from any number of concurrent threads.

use anyhow::{anyhow, Result};
use std::hint::spin_loop;
use std::sync::atomic::{AtomicUsize, Ordering};

use nix::sys::mman::{madvise, MmapAdvise};

use common::{Page, PageId, ShmOffset, Superblock, DIRECT_LIMIT, FREE_LIST_SHARD_COUNT,
             FREE_LIST_TRIM_ENABLED, FREE_LIST_TRIM_THRESHOLD, PAGE_SIZE};

use crate::runtime::extended_pool;

// ─── Global round-robin shard counter ────────────────────────────────────────

/// Incremented on every `alloc_page` call to spread allocations across shards.
/// `Relaxed` is sufficient — this is purely a load-balancing hint, not a
/// synchronisation point.
static ALLOC_SHARD: AtomicUsize = AtomicUsize::new(0);

// ─── Unified page allocator ───────────────────────────────────────────────────

/// Claim one 4 KiB page from the SHM pool.
///
/// Tries each free-list shard (round-robin starting point) before falling
/// back to the bump allocator.  All host-side writers — stream, I/O,
/// shared-state, or any future area — call this so freed pages are reused
/// before consuming fresh bump address space.
///
/// Returns the page's PageId.  In direct mode that's the byte offset
/// from `splice_addr` (`< DIRECT_LIMIT`); in paged mode it's a
/// PageId `>= DIRECT_LIMIT` that must be resolved through
/// [`extended_pool::runtime::resolve`] before being dereferenced.
pub fn alloc_page(splice_addr: usize) -> Result<PageId> {
    let sb = unsafe { &*(splice_addr as *const Superblock) };

    // ── Sharded free-list pop ─────────────────────────────────────────────────
    let start = ALLOC_SHARD.fetch_add(1, Ordering::Relaxed) % FREE_LIST_SHARD_COUNT;

    for i in 0..FREE_LIST_SHARD_COUNT {
        let shard = (start + i) % FREE_LIST_SHARD_COUNT;
        loop {
            // free_list_heads is AtomicPageId (u64); the direct freelist
            // only stores direct-mode values (< DIRECT_LIMIT).
            let head = sb.free_list_heads[shard].load(Ordering::Acquire) as ShmOffset;
            if head == 0 {
                break; // shard empty — try next
            }
            let page = unsafe { &mut *((splice_addr + head as usize) as *mut Page) };
            let next = page.next_offset.load(Ordering::Relaxed) as ShmOffset;
            match sb.free_list_heads[shard]
                .compare_exchange(head as u64, next as u64, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => {
                    page.next_offset.store(0, Ordering::Relaxed);
                    page.cursor.store(0, Ordering::Relaxed);
                    return Ok(head as PageId);
                }
                Err(_) => spin_loop(), // another thread won the CAS — retry
            }
        }
    }

    // ── Bump allocate (all shards empty) ─────────────────────────────────────
    // `fetch_add` is an atomic RMW — no CAS, no retry, O(1) regardless of
    // concurrency.
    let offset = sb.bump_allocator.fetch_add(PAGE_SIZE, Ordering::AcqRel);
    let cap = sb.global_capacity.load(Ordering::Acquire);
    if offset + PAGE_SIZE > cap {
        // Direct-mode bump exhausted.  If paged mode is active, fall
        // back to the extended pool; the returned PageId will be
        // `>= DIRECT_LIMIT` and callers must `resolve` it before use.
        if extended_pool::runtime::current_mode() == extended_pool::Mode::Paged {
            // Undo the speculative fetch_add so the bump pointer
            // doesn't march past `cap` for subsequent callers.
            sb.bump_allocator.fetch_sub(PAGE_SIZE, Ordering::AcqRel);
            let (id, _ptr) = extended_pool::runtime::alloc_paged_page(false)?;
            return Ok(id);
        }
        return Err(anyhow!(
            "SHM capacity exhausted ({} of {} bytes used). \
             Reduce data volume or increase INITIAL_SHM_SIZE.",
            offset + PAGE_SIZE,
            cap,
        ));
    }
    let page = unsafe { &mut *((splice_addr + offset as usize) as *mut Page) };
    page.next_offset.store(0, Ordering::Relaxed);
    page.cursor.store(0, Ordering::Relaxed);

    // Extended-pool flip trigger: observe the new bump high-water mark
    // and transition to paged mode once it crosses the 80% threshold.
    // Lock-free in the common case (atomic mode check + inequality).
    extended_pool::runtime::notify_bump_advance(offset + PAGE_SIZE, splice_addr);

    Ok(offset as PageId)
}

// ─── Slot kind ────────────────────────────────────────────────────────────────

/// Discriminates between the two independent slot arrays in the Superblock.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SlotKind {
    Stream,
    Io,
}

// ─── Core free primitive ─────────────────────────────────────────────────────

/// Push every page in the chain rooted at `head` back onto the SHM free list.
///
/// The target shard is `(head / PAGE_SIZE) % FREE_LIST_SHARD_COUNT` —
/// deterministic per chain so the same chain always lands in the same shard.
/// The entire chain is spliced as one unit (walk to tail, single CAS), so
/// cost is O(chain_length) for the walk and O(1) amortised for the CAS.
///
/// **Precondition**: the slot's `head`/`tail` atomics must already be zeroed
/// before this call so no concurrent reader can follow a pointer into pages
/// that are being freed.
///
/// Safe to call from multiple threads simultaneously.
pub fn free_page_chain(splice_addr: usize, head: PageId) {
    if head == 0 {
        return;
    }

    // Mixed chains (both direct and paged ids) walk the chain one
    // page at a time, dispatching each page to the right freelist.
    // Pure-direct chains (the common case before the mode flip)
    // splice the whole chain onto one shard with a single CAS — the
    // optimized fast path below.
    if extended_pool::runtime::current_mode() == extended_pool::Mode::Paged {
        free_page_chain_mixed(splice_addr, head);
        return;
    }

    // ── Fast path: pure direct-mode chain ─────────────────────────────────────
    debug_assert!(head < DIRECT_LIMIT);
    let head = head as ShmOffset;
    let sb = unsafe { &*(splice_addr as *const Superblock) };

    // Deterministic shard: same page always returns to the same shard.
    let shard = (head / PAGE_SIZE) as usize % FREE_LIST_SHARD_COUNT;

    // Walk to the tail so we can splice the whole chain in one CAS.
    let mut tail = head;
    loop {
        let page = unsafe { &*((splice_addr + tail as usize) as *const Page) };
        let next = page.next_offset.load(Ordering::Acquire) as ShmOffset;
        if next == 0 {
            break;
        }
        tail = next;
    }

    // Treiber-stack push onto the chosen shard.
    loop {
        let old_head = sb.free_list_heads[shard].load(Ordering::Acquire) as ShmOffset;
        let tail_page = unsafe { &*((splice_addr + tail as usize) as *const Page) };
        tail_page.next_offset.store(old_head as u64, Ordering::Relaxed);

        match sb.free_list_heads[shard].compare_exchange(
            old_head as u64,
            head as u64,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => break,
            Err(_) => spin_loop(), // another thread pushed concurrently — retry
        }
    }
}

/// Slow path for paged mode: walk the chain one page at a time,
/// freeing direct pages into the shard freelist (plus transitioning
/// their slot into the resolution buffer via `notify_direct_free`) and
/// paged pages into the extended pool.  Called only when the pool is
/// in `Paged` mode because mixed chains can only exist after the flip.
///
/// # Concurrency
///
/// Like the direct-mode fast path above, this function relies on the
/// **detach-then-walk** protocol: the slot atomic owning this chain
/// must have already been cleared via `swap(0, …)` before we are
/// called.  That swap is the single point of contention; whoever wins
/// it is the sole owner of the chain for its entire lifetime here.
/// No other thread can walk the chain concurrently because the slot
/// atomic reads zero for everyone except the swap winner.
///
/// Per-page dispatch is locally single-writer even when dispatching
/// into the shard freelist (CAS-retry push) or the extended pool
/// (mutex-guarded).  The `next` pointer for each page is read from
/// `page.next_offset` BEFORE that field is overwritten by
/// `push_single_direct_page`, so the chain walk never loses its way.
fn free_page_chain_mixed(splice_addr: usize, head: PageId) {
    let mut current = head;
    while current != 0 {
        // Resolve to a host-side pointer so we can read `next_offset`.
        // Direct ids resolve by arithmetic; paged ids go through the
        // resolution buffer under the singleton lock (potentially
        // re-installing if they were evicted).
        let page_ptr = match extended_pool::runtime::resolve(current, splice_addr) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[Reclaimer] free_page_chain_mixed: resolve({current:#x}) failed: {e}");
                return;
            }
        };
        let page = unsafe { &*page_ptr };
        let next = page.next_offset.load(Ordering::Acquire);

        // Dispose of `current` according to its kind.
        if current < DIRECT_LIMIT {
            // Direct page: push onto its deterministic shard via the
            // single-page helper, then notify the extended pool that
            // its wasm32 slot can now host residency.  We can't call
            // `free_page_chain` recursively because we already know
            // the chain structure is being walked here.
            push_single_direct_page(splice_addr, current as ShmOffset);
            extended_pool::runtime::notify_direct_free(current);
        } else {
            // Paged page: return it to the global pool.
            if let Err(e) = extended_pool::runtime::free_paged_page(current) {
                eprintln!("[Reclaimer] free_paged_page({current:#x}) failed: {e}");
            }
        }

        current = next;
    }
}

/// Push a single direct page onto its deterministic shard (no chain
/// walking).  Used by `free_page_chain_mixed` to dispose of direct
/// pages one at a time when they're interleaved with paged pages.
fn push_single_direct_page(splice_addr: usize, offset: ShmOffset) {
    let sb = unsafe { &*(splice_addr as *const Superblock) };
    let shard = (offset / PAGE_SIZE) as usize % FREE_LIST_SHARD_COUNT;
    let page = unsafe { &*((splice_addr + offset as usize) as *const Page) };
    loop {
        let old_head = sb.free_list_heads[shard].load(Ordering::Acquire);
        page.next_offset.store(old_head, Ordering::Relaxed);
        match sb.free_list_heads[shard]
            .compare_exchange(old_head, offset as u64, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => break,
            Err(_) => spin_loop(),
        }
    }
}

// ─── Slot-level helpers ───────────────────────────────────────────────────────

/// Zero the metadata for stream `slot` **without** freeing any pages.
///
/// Use this after a routing operation (Bridge, Aggregate, Shuffle, Broadcast)
/// has transferred the upstream slot's page chain into one or more downstream
/// slots.  The pages are now owned by the downstream chain(s); freeing them
/// here would corrupt those chains.
///
/// After this call `writer_heads[slot] == 0` and `writer_tails[slot] == 0`,
/// so the slot appears empty if inspected.
pub fn clear_stream_slot(splice_addr: usize, slot: usize) {
    let sb = unsafe { &*(splice_addr as *const Superblock) };
    sb.writer_heads[slot].store(0, Ordering::Release);
    sb.writer_tails[slot].store(0, Ordering::Release);
}

/// Detach the page chain from stream `slot` and return it to the free pool.
/// Resets `writer_heads[slot]` and `writer_tails[slot]` to `0`.
///
/// **Only call this when the slot has exclusive ownership of its pages** —
/// i.e. no routing operation has spliced those pages into another slot's
/// chain.  For slots that have been routed, use `clear_stream_slot` instead.
pub fn free_stream_slot(splice_addr: usize, slot: usize) {
    let sb = unsafe { &*(splice_addr as *const Superblock) };
    let head: PageId = sb.writer_heads[slot].swap(0, Ordering::AcqRel);
    sb.writer_tails[slot].store(0, Ordering::Release);
    free_page_chain(splice_addr, head);
}

/// Detach the page chain from I/O `slot` and return it to the free pool.
/// Resets `io_heads[slot]` and `io_tails[slot]` to `0`.
///
/// I/O slots are always exclusively owned (the SlotLoader writes, the guest
/// reads, the SlotFlusher drains — no routing splices into them), so freeing
/// their pages is always safe.
pub fn free_io_slot(splice_addr: usize, slot: usize) {
    let sb = unsafe { &*(splice_addr as *const Superblock) };
    let head: PageId = sb.io_heads[slot].swap(0, Ordering::AcqRel);
    sb.io_tails[slot].store(0, Ordering::Release);
    free_page_chain(splice_addr, head);
}

// ─── Slot cursor reset ────────────────────────────────────────────────────────

/// Reset the SHM atomic read-cursor for a stream or I/O slot to zero.
///
/// Cursor-based readers (`read_next_stream_record` / `read_next_io_record` in
/// the Rust guest, `read_next_io_record` in the Python shm module) advance a
/// per-slot named atomic each time a record is consumed:
///
/// | Slot kind | Atomic name        |
/// |-----------|--------------------|
/// | Stream    | `stream_cursor_N`  |
/// | I/O       | `io_cursor_N`      |
///
/// These atomics live in the SHM atomic arena and survive `free_stream_slot` /
/// `free_io_slot` — only the page chain is freed, not the arena.  In `reset`
/// mode, a slot freed between runs is repopulated with fresh data starting at
/// record index 0.  Without resetting the cursor the next run's stage would
/// attempt to read at the old index and immediately return `None` (no-op),
/// skipping the newly loaded data entirely.
///
/// Call this alongside `free_stream_slot` / `free_io_slot` wherever a slot
/// will be reused across runs.  If the cursor atomic has never been registered
/// (the slot was never read with a cursor), the function is a no-op.
pub fn reset_slot_cursor(splice_addr: usize, kind: SlotKind, slot: usize) {
    use std::sync::atomic::AtomicU64;
    use common::{ATOMIC_ARENA_OFFSET, REGISTRY_OFFSET, RegistryEntry};

    let name_str = match kind {
        SlotKind::Stream => format!("stream_cursor_{}", slot),
        SlotKind::Io     => format!("io_cursor_{}", slot),
    };

    // Build the padded 52-byte name key (same layout as RegistryEntry.name).
    let mut name_key = [0u8; 52];
    let src = name_str.as_bytes();
    name_key[..src.len().min(52)].copy_from_slice(&src[..src.len().min(52)]);

    let sb            = unsafe { &*(splice_addr as *const Superblock) };
    let registry_base = (splice_addr + REGISTRY_OFFSET as usize) as *const RegistryEntry;
    let atomic_base   = (splice_addr + ATOMIC_ARENA_OFFSET as usize) as *mut AtomicU64;

    // Acquire the registry spinlock (same protocol as host_resolve_atomic in worker.rs).
    while sb.registry_lock
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        spin_loop();
    }

    let count = sb.next_atomic_idx.load(Ordering::Relaxed);
    let mut found_index: Option<u32> = None;
    for i in 0..count {
        let entry = unsafe { &*registry_base.add(i as usize) };
        if entry.name == name_key {
            found_index = Some(entry.index);
            break;
        }
    }

    sb.registry_lock.store(0, Ordering::Release);

    // Zero the atomic outside the lock — the AtomicU32 itself provides ordering.
    if let Some(idx) = found_index {
        unsafe { (*atomic_base.add(idx as usize)).store(0, Ordering::Release) };
    }
}

// ─── Free-list trim ───────────────────────────────────────────────────────────

/// Count the total number of pages sitting in the free list across all shards.
///
/// Walks every shard's linked list.  This is a **best-effort heuristic** —
/// it is not atomic with respect to concurrent alloc/free, so the result may
/// be slightly stale.  That is acceptable: the trim decision only needs to be
/// approximately correct.
pub fn count_free_list_pages(splice_addr: usize) -> usize {
    let sb = unsafe { &*(splice_addr as *const Superblock) };
    let mut total = 0usize;
    for shard in 0..FREE_LIST_SHARD_COUNT {
        let mut current = sb.free_list_heads[shard].load(Ordering::Acquire) as ShmOffset;
        while current != 0 {
            total += 1;
            let page = unsafe { &*((splice_addr + current as usize) as *const Page) };
            current = page.next_offset.load(Ordering::Relaxed) as ShmOffset;
        }
    }
    total
}

/// CAS-pop one page from the first non-empty shard.  Returns its byte offset
/// from `splice_addr`, or `None` if all shards are empty.
fn pop_one_free_page(splice_addr: usize) -> Option<ShmOffset> {
    let sb = unsafe { &*(splice_addr as *const Superblock) };
    for shard in 0..FREE_LIST_SHARD_COUNT {
        loop {
            let head = sb.free_list_heads[shard].load(Ordering::Acquire) as ShmOffset;
            if head == 0 { break; }
            let page = unsafe { &*((splice_addr + head as usize) as *const Page) };
            let next = page.next_offset.load(Ordering::Relaxed) as ShmOffset;
            match sb.free_list_heads[shard]
                .compare_exchange(head as u64, next as u64, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Some(head),
                Err(_) => spin_loop(),
            }
        }
    }
    None
}

/// CAS-push a single page back onto its deterministic shard.
fn push_page_to_free_list(splice_addr: usize, offset: ShmOffset) {
    let sb = unsafe { &*(splice_addr as *const Superblock) };
    let shard = (offset / PAGE_SIZE) as usize % FREE_LIST_SHARD_COUNT;
    let page = unsafe { &*((splice_addr + offset as usize) as *const Page) };
    loop {
        let old_head = sb.free_list_heads[shard].load(Ordering::Acquire) as ShmOffset;
        // Writing next_offset after MADV_DONTNEED causes a soft page fault
        // that allocates a fresh zero physical page — this is intentional and correct.
        page.next_offset.store(old_head as u64, Ordering::Relaxed);
        match sb.free_list_heads[shard]
            .compare_exchange(old_head as u64, offset as u64, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => break,
            Err(_) => spin_loop(),
        }
    }
}

/// Release the physical memory backing of excess free-list pages to the OS.
///
/// Controlled by two constants in `common`:
///   - [`FREE_LIST_TRIM_ENABLED`]  — master on/off switch (compile-time).
///   - [`FREE_LIST_TRIM_THRESHOLD`] — page count at which trimming fires.
///
/// # What it does
///
/// 1. Counts total pages in the free list (approximate, lock-free walk).
/// 2. If the count ≤ threshold, returns immediately — nothing to do.
/// 3. Otherwise pops **half the excess** pages one by one with a CAS-pop,
///    calls `madvise(MADV_DONTNEED)` on each to release physical RAM, then
///    pushes them back onto the free list.
///
/// The pages are NOT removed from the virtual address space — they stay in
/// the free list so future allocations can reuse their offsets without growing
/// the bump pointer.  On the next write the OS will zero-fill them again via
/// a soft page fault.  Physical RAM is only re-consumed when the page is
/// actually written to again.
///
/// # When to call
///
/// Call this after each DAG wave's post-wave reclamation, or any point where
/// many pages have just been freed and the working set is expected to shrink.
/// The function is a no-op when `FREE_LIST_TRIM_ENABLED = false`.
pub fn trim_free_list(splice_addr: usize) {
    if !FREE_LIST_TRIM_ENABLED {
        return;
    }

    let total = count_free_list_pages(splice_addr);
    if total <= FREE_LIST_TRIM_THRESHOLD {
        return;
    }

    // Release physical backing for half the excess pages.
    let to_advise = (total - FREE_LIST_TRIM_THRESHOLD) / 2;
    let mut advised = 0usize;

    while advised < to_advise {
        let Some(offset) = pop_one_free_page(splice_addr) else { break };

        let page_ptr = (splice_addr + offset as usize) as *mut std::ffi::c_void;
        // SAFETY: `page_ptr` is a valid SHM-backed 4 KiB page just removed
        // from the free list.  MADV_DONTNEED releases its physical backing
        // while keeping the virtual mapping — the OS zero-fills on next access.
        unsafe {
            let _ = madvise(page_ptr, PAGE_SIZE as usize, MmapAdvise::MADV_DONTNEED);
        }

        // Push back so the virtual offset stays recyclable.
        push_page_to_free_list(splice_addr, offset);
        advised += 1;
    }

    if advised > 0 {
        println!(
            "[Reclaimer] Trim: {advised} pages ({} KiB) released to OS \
             (free-list had {total}, threshold {FREE_LIST_TRIM_THRESHOLD})",
            advised * PAGE_SIZE as usize / 1024,
        );
    }
}
