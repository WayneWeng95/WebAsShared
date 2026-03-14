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

use common::{Page, Superblock, FREE_LIST_SHARD_COUNT, PAGE_SIZE};

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
/// Returns the page's **byte offset from `splice_addr`**.
pub fn alloc_page(splice_addr: usize) -> Result<u32> {
    let sb = unsafe { &*(splice_addr as *const Superblock) };

    // ── Sharded free-list pop ─────────────────────────────────────────────────
    let start = ALLOC_SHARD.fetch_add(1, Ordering::Relaxed) % FREE_LIST_SHARD_COUNT;

    for i in 0..FREE_LIST_SHARD_COUNT {
        let shard = (start + i) % FREE_LIST_SHARD_COUNT;
        loop {
            let head = sb.free_list_heads[shard].load(Ordering::Acquire);
            if head == 0 {
                break; // shard empty — try next
            }
            let page = unsafe { &mut *((splice_addr + head as usize) as *mut Page) };
            let next = page.next_offset.load(Ordering::Relaxed);
            match sb.free_list_heads[shard]
                .compare_exchange(head, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => {
                    page.next_offset.store(0, Ordering::Relaxed);
                    page.cursor.store(0, Ordering::Relaxed);
                    return Ok(head);
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
    Ok(offset)
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
pub fn free_page_chain(splice_addr: usize, head: u32) {
    if head == 0 {
        return;
    }
    let sb = unsafe { &*(splice_addr as *const Superblock) };

    // Deterministic shard: same page always returns to the same shard.
    let shard = (head / PAGE_SIZE) as usize % FREE_LIST_SHARD_COUNT;

    // Walk to the tail so we can splice the whole chain in one CAS.
    let mut tail = head;
    loop {
        let page = unsafe { &*((splice_addr + tail as usize) as *const Page) };
        let next = page.next_offset.load(Ordering::Acquire);
        if next == 0 {
            break;
        }
        tail = next;
    }

    // Treiber-stack push onto the chosen shard.
    loop {
        let old_head = sb.free_list_heads[shard].load(Ordering::Acquire);
        let tail_page = unsafe { &*((splice_addr + tail as usize) as *const Page) };
        tail_page.next_offset.store(old_head, Ordering::Relaxed);

        match sb.free_list_heads[shard].compare_exchange(
            old_head,
            head,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => break,
            Err(_) => spin_loop(), // another thread pushed concurrently — retry
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
    let head = sb.writer_heads[slot].swap(0, Ordering::AcqRel);
    sb.writer_tails[slot].store(0, Ordering::Release);
    free_page_chain(splice_addr, head);
}

/// Detach the page chain from I/O `slot` and return it to the free pool.
/// Resets `io_heads[slot]` and `io_tails[slot]` to `0`.
///
/// I/O slots are always exclusively owned (the Inputer writes, the guest
/// reads, the Outputer drains — no routing splices into them), so freeing
/// their pages is always safe.
pub fn free_io_slot(splice_addr: usize, slot: usize) {
    let sb = unsafe { &*(splice_addr as *const Superblock) };
    let head = sb.io_heads[slot].swap(0, Ordering::AcqRel);
    sb.io_tails[slot].store(0, Ordering::Release);
    free_page_chain(splice_addr, head);
}
