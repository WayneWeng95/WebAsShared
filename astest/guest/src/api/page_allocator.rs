use core::sync::atomic::Ordering;
use common::*;
use super::{ShmApi, SHM_BASE};

extern "C" {
    fn host_remap(new_size: u32);
}

impl ShmApi {
    /// Returns a shared reference to the Superblock at the base of shared memory.
    pub(crate) fn superblock() -> &'static Superblock { unsafe { &*(SHM_BASE as *const Superblock) } }

    /// Allocates a 4KB page from the shared memory pool.
    /// First tries to reuse a recycled page from the lock-free free list (Treiber stack CAS pop).
    /// Falls back to bump allocation, triggering a VMA expansion via `host_remap` if needed.
    /// Returns the page's byte offset from the shared memory base.
    pub(crate) fn try_allocate_page() -> u32 {
        let sb = Self::superblock();

        loop {
            let head = sb.free_list_head.load(Ordering::Acquire);
            if head == 0 { break; }

            let page_ptr = unsafe { (SHM_BASE + head as usize) as *const Page };
            let next_free = unsafe { (*page_ptr).next_offset.load(Ordering::Relaxed) };

            if sb.free_list_head.compare_exchange(
                head, next_free, Ordering::SeqCst, Ordering::SeqCst,
            ).is_ok() {
                let mut_page = unsafe { &mut *(page_ptr as *mut Page) };
                mut_page.next_offset.store(0, Ordering::Relaxed);
                mut_page.cursor.store(0, Ordering::Relaxed);
                return head;
            }
        }

        loop {
            let current_alloc = sb.bump_allocator.load(Ordering::Acquire);

            // ==========================================
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
