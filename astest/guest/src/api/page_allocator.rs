use core::hint::spin_loop;
use core::sync::atomic::{AtomicU32, Ordering};
use common::*;
use super::{ShmApi, SHM_BASE};

extern "C" {
    fn host_remap(new_size: u32);
}

/// Guest-local round-robin counter for shard selection.
/// `Relaxed` is sufficient — this is a load-balancing hint, not a sync point.
/// WASM instances are single-threaded per instance today; the AtomicU32 is
/// future-proof for when WASM threading is enabled.
static ALLOC_SHARD: AtomicU32 = AtomicU32::new(0);

impl ShmApi {
    /// Returns a shared reference to the Superblock at the base of shared memory.
    pub(crate) fn superblock() -> &'static Superblock { unsafe { &*(SHM_BASE as *const Superblock) } }

    /// Allocates a 4 KiB page from the shared memory pool.
    ///
    /// # Strategy
    ///
    /// 1. **Sharded free-list pop** — try each of the `FREE_LIST_SHARD_COUNT`
    ///    Treiber-stack shards starting from a round-robin preferred shard.
    ///    `spin_loop()` on each CAS failure to yield the CPU pipeline.
    /// 2. **Bump allocation** — if all shards are empty, claim the next page
    ///    from `sb.bump_allocator` and expand the VMA via `host_remap` if
    ///    the current capacity is exhausted.
    ///
    /// Returns the page's byte offset from the shared memory base.
    pub(crate) fn try_allocate_page() -> u32 {
        let sb = Self::superblock();

        // ── Sharded free-list pop ─────────────────────────────────────────────
        let start = ALLOC_SHARD.fetch_add(1, Ordering::Relaxed) as usize % FREE_LIST_SHARD_COUNT;

        for i in 0..FREE_LIST_SHARD_COUNT {
            let shard = (start + i) % FREE_LIST_SHARD_COUNT;
            loop {
                let head = sb.free_list_heads[shard].load(Ordering::Acquire);
                if head == 0 {
                    break; // shard empty — try next
                }
                let page_ptr = unsafe { (SHM_BASE + head as usize) as *const Page };
                let next_free = unsafe { (*page_ptr).next_offset.load(Ordering::Relaxed) };

                match sb.free_list_heads[shard].compare_exchange(
                    head, next_free, Ordering::SeqCst, Ordering::SeqCst,
                ) {
                    Ok(_) => {
                        let mut_page = unsafe { &mut *(page_ptr as *mut Page) };
                        mut_page.next_offset.store(0, Ordering::Relaxed);
                        mut_page.cursor.store(0, Ordering::Relaxed);
                        return head;
                    }
                    Err(_) => spin_loop(), // another thread won — retry same shard
                }
            }
        }

        // ── Bump allocate (all shards empty) ─────────────────────────────────
        loop {
            let current_alloc = sb.bump_allocator.load(Ordering::Acquire);

            if current_alloc >= 0x7FF0_0000 { continue; }

            let local_cap = unsafe { super::LOCAL_CAPACITY };

            if current_alloc + PAGE_SIZE > local_cap {
                let global_cap = sb.global_capacity.load(Ordering::Acquire);
                if global_cap > local_cap {
                    unsafe { host_remap(global_cap); super::LOCAL_CAPACITY = global_cap; }
                    continue;
                } else {
                    let new_cap = local_cap * 2;
                    if new_cap >= 0x8000_0000 { continue; }
                    if sb.global_capacity.compare_exchange(local_cap, new_cap, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                        unsafe { host_remap(new_cap); super::LOCAL_CAPACITY = new_cap; }
                        continue;
                    } else { continue; }
                }
            }

            if sb.bump_allocator.compare_exchange(
                current_alloc, current_alloc + PAGE_SIZE, Ordering::SeqCst, Ordering::SeqCst,
            ).is_ok() {
                return current_alloc;
            }
        }
    }
}
