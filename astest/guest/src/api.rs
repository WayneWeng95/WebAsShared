use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const SHM_BASE: usize = 0x8000_0000;
const PAGE_SIZE: u32 = 4096;
const ATOMIC_ARENA_OFFSET: u32 = PAGE_SIZE;
const ATOMIC_ARENA_SIZE: u32 = 2 * 1024 * 1024;
const LOG_ARENA_OFFSET: u32 = ATOMIC_ARENA_OFFSET + ATOMIC_ARENA_SIZE;
const LOG_ARENA_SIZE: u32 = 16 * 1024 * 1024;

extern "C" { fn host_remap(new_size: u32); }

#[repr(C)]
struct Superblock {
    magic: u32,
    bump_allocator: AtomicU32,
    global_capacity: AtomicU32,
    log_offset: AtomicU32,
    writer_heads: [AtomicU32; 4],
    writer_tails: [AtomicU32; 4],
}

#[repr(C, align(4096))]
struct Page {
    next_offset: AtomicU32,
    cursor: AtomicU32, 
    data: [u8; 4088],  
}

static mut LOCAL_CAPACITY: u32 = 36 * 1024 * 1024;

/// Entry point for the guest shared-memory logging and byte-stream API.
pub struct ShmApi;

impl ShmApi {
    fn superblock() -> &'static Superblock { unsafe { &*(SHM_BASE as *const Superblock) } }

    /// Append a UTF-8 log message into the fixed-size log arena.
    /// The message is written sequentially; if the arena is exhausted the
    /// write is skipped rather than wrapping.
    pub fn append_log(msg: &str) {
        let sb = Self::superblock();
        let bytes = msg.as_bytes();
        let len = bytes.len() as u32;
        let offset = sb.log_offset.fetch_add(len, Ordering::Relaxed);
        if offset + len <= LOG_ARENA_SIZE {
            let dest = (SHM_BASE + LOG_ARENA_OFFSET as usize + offset as usize) as *mut u8;
            unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), dest, bytes.len()); }
        }
    }

    /// Reset the log write cursor back to the start of the log arena.
    pub fn reset_log() {
        Self::superblock()
            .log_offset
            .store(0, Ordering::Release);
    }

    fn allocate_page() -> u32 {
        let sb = Self::superblock();
        loop {
            let current_alloc = sb.bump_allocator.load(Ordering::Acquire);
            let local_cap = unsafe { LOCAL_CAPACITY };

            if current_alloc + PAGE_SIZE > local_cap {
                let global_cap = sb.global_capacity.load(Ordering::Acquire);
                if global_cap > local_cap {
                    unsafe { host_remap(global_cap); LOCAL_CAPACITY = global_cap; }
                    continue; 
                } else {
                    let new_cap = local_cap * 2;
                    if sb.global_capacity.compare_exchange(local_cap, new_cap, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                        unsafe { host_remap(new_cap); LOCAL_CAPACITY = new_cap; }
                        continue; 
                    } else { continue; }
                }
            }
            if sb.bump_allocator.compare_exchange(current_alloc, current_alloc + PAGE_SIZE, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                return current_alloc;
            }
        }
    }

    /// Get a reference to an `AtomicU64` stored in the shared atomic arena.
    /// The caller must ensure `index` is within the bounds of the arena that
    /// the host mapped, and that all participants agree on the meaning of this
    /// slot.
    pub fn get_atomic(index: usize) -> &'static AtomicU64 {
        let base = SHM_BASE + ATOMIC_ARENA_OFFSET as usize;
        unsafe { &*((base as *const AtomicU64).add(index)) }
    }

    /// Low-level helper that appends raw bytes to the per-writer page chain,
    /// transparently allocating and linking new pages as needed.
    fn append_raw_bytes(id: u32, mut data: &[u8]) {
        let sb = Self::superblock();
        let mut tail_offset = sb.writer_tails[id as usize].load(Ordering::Acquire);
        
        // 1. Initialize the first page on first use.
        if tail_offset == 0 {
            tail_offset = Self::allocate_page();
            let new_page = unsafe { &mut *((SHM_BASE + tail_offset as usize) as *mut Page) };
            new_page.next_offset.store(0, Ordering::Relaxed);
            new_page.cursor.store(0, Ordering::Relaxed);
            sb.writer_heads[id as usize].store(tail_offset, Ordering::Release);
            sb.writer_tails[id as usize].store(tail_offset, Ordering::Release);
        }

        // 2. Streamed, chunked writes that split large payloads across pages.
        while !data.is_empty() {
            let mut tail_page = unsafe { &mut *((SHM_BASE + tail_offset as usize) as *mut Page) };
            let current_cursor = tail_page.cursor.load(Ordering::Relaxed);
            let space_left = 4088 - current_cursor;
            
            // If the current page is full, seamlessly allocate and link the next page.
            if space_left == 0 {
                let new_offset = Self::allocate_page();
                let new_page = unsafe { &mut *((SHM_BASE + new_offset as usize) as *mut Page) };
                new_page.next_offset.store(0, Ordering::Relaxed);
                new_page.cursor.store(0, Ordering::Relaxed);
                
                tail_page.next_offset.store(new_offset, Ordering::Release);
                sb.writer_tails[id as usize].store(new_offset, Ordering::Release);
                
                tail_offset = new_offset;
                continue;
            }
            
            let write_len = core::cmp::min(space_left as usize, data.len());
            unsafe {
                let dest = tail_page.data.as_mut_ptr().add(current_cursor as usize);
                core::ptr::copy_nonoverlapping(data.as_ptr(), dest, write_len);
            }
            
            tail_page.cursor.store(current_cursor + write_len as u32, Ordering::Release);
            // Advance the slice by the number of bytes written.
            data = &data[write_len..];
        }
    }

    /// Public API: append a framed record with an arbitrary-sized payload.
    /// The record is written as a 4-byte little-endian length followed by
    /// the payload bytes, potentially spanning multiple pages.
    pub fn append_bytes(id: u32, payload: &[u8]) {
        let record_len = payload.len() as u32;
        // First write the 4-byte frame header.
        Self::append_raw_bytes(id, &record_len.to_le_bytes());
        // Then write the (potentially very large) payload body.
        Self::append_raw_bytes(id, payload);
    }


    /// Read the most recent complete record for the given writer `id`.
    /// Walks the page chain from the head, reassembling each framed record
    /// and returning the last one that can be read in full.
    pub fn read_latest_bytes(id: u32) -> Option<alloc::vec::Vec<u8>> {
        let sb = Self::superblock();
        let head_offset = sb.writer_heads[id as usize].load(Ordering::Acquire);
        if head_offset == 0 { return None; }

        let mut current_offset = head_offset;
        let mut cursor_in_page = 0;
        
        // Helper closure that pulls an exact number of bytes across page boundaries.
        let mut read_exact = |mut dest: &mut [u8]| -> bool {
            while !dest.is_empty() {
                if current_offset == 0 { return false; }
                
                // Ensure the local mapping is large enough for the required address range.
                let required_cap = current_offset + PAGE_SIZE;
                let local_cap = unsafe { LOCAL_CAPACITY };
                if required_cap > local_cap {
                    let global_cap = sb.global_capacity.load(Ordering::Acquire);
                    if global_cap >= required_cap {
                        unsafe { host_remap(global_cap); LOCAL_CAPACITY = global_cap; }
                    } else { return false; } 
                }

                let page = unsafe { &*((SHM_BASE + current_offset as usize) as *const Page) };
                let page_written = page.cursor.load(Ordering::Acquire);
                let available = page_written.saturating_sub(cursor_in_page);
                
                // If we've consumed this page, jump to the next one.
                if available == 0 {
                    current_offset = page.next_offset.load(Ordering::Acquire);
                    cursor_in_page = 0;
                    continue;
                }
                
                let read_len = core::cmp::min(available as usize, dest.len());
                unsafe {
                    let src = page.data.as_ptr().add(cursor_in_page as usize);
                    core::ptr::copy_nonoverlapping(src, dest.as_mut_ptr(), read_len);
                }
                cursor_in_page += read_len as u32;
                dest = &mut dest[read_len..];
            }
            true
        };

        let mut latest_data = None;
        
        // Parse frames until the end of the chain, keeping the last complete record.
        loop {
            let mut len_buf = [0u8; 4];
            if !read_exact(&mut len_buf) { break; }
            let record_len = u32::from_le_bytes(len_buf);
            
            // Allocate a heap buffer to reassemble a potentially fragmented record.
            let mut payload = alloc::vec::Vec::with_capacity(record_len as usize);
            unsafe { payload.set_len(record_len as usize); }
            
            if !read_exact(&mut payload) { break; }
            latest_data = Some(payload);
        }
        
        latest_data
    }
}