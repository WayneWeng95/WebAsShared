use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

const SHM_BASE: usize = 0x8000_0000;
const KIB: u32 = 1024;
const MIB: u32 = 1024 * 1024;

const PAGE_SIZE: u32 = 4 * KIB; // 4 KiB page size

// Layout offsets within the shared memory region.
const REGISTRY_SIZE: u32 = 64 * KIB; 
const ATOMIC_ARENA_OFFSET: u32 = PAGE_SIZE + REGISTRY_SIZE; 
const ATOMIC_ARENA_SIZE: u32 = 2 * MIB;
const LOG_ARENA_OFFSET: u32 = ATOMIC_ARENA_OFFSET + ATOMIC_ARENA_SIZE;
const LOG_ARENA_SIZE: u32 = 16 * MIB;

// Number of hash buckets in the shared map (one u32 per bucket).
const BUCKET_COUNT: usize = (PAGE_SIZE / 4) as usize; 

extern "C" {
    fn host_remap(new_size: u32);
    fn host_resolve_atomic(ptr: u32, len: u32) -> u32;
}

// Core metadata structures stored in shared memory.
#[repr(C)]
struct Superblock {
    magic: u32,
    bump_allocator: AtomicU32,
    global_capacity: AtomicU32,
    log_offset: AtomicU32,
    registry_lock: AtomicU32,
    next_atomic_idx: AtomicU32,
    shared_map_base: AtomicU32, 
    // Head of the lock-free free-list of reusable pages (offset from SHM_BASE).
    free_list_head: AtomicU32,
    
    // Per-writer queues for append-only byte streams.
    writer_heads: [AtomicU32; 4],
    writer_tails: [AtomicU32; 4],
}

#[repr(C, align(4096))]
struct Page {
    next_offset: AtomicU32,
    cursor: AtomicU32,
    data: [u8; 4088],
}

#[repr(C)]
struct ChainNodeHeader {
    next_node: AtomicU32,
    writer_id: u32,
    data_len: u32,
}

static mut LOCAL_CAPACITY: u32 = 36 * MIB;
static mut ATOMIC_INDEX_CACHE: Option<BTreeMap<String, u32>> = None;

pub struct ShmApi;

impl ShmApi {
    fn superblock() -> &'static Superblock { unsafe { &*(SHM_BASE as *const Superblock) } }

    // ==========================================
    // Append a debug message to the shared log.
    // ==========================================
    pub fn append_log(msg: &str) {
        let sb = Self::superblock();
        let bytes = msg.as_bytes();
        let len = bytes.len() as u32;
        
        // Reserve space atomically in the log arena.
        let offset = sb.log_offset.fetch_add(len, Ordering::Relaxed);
        
        if offset + len <= LOG_ARENA_SIZE {
            // Compute the physical destination address inside shared memory.
            let dest_addr = SHM_BASE + LOG_ARENA_OFFSET as usize + offset as usize;
            let dest = dest_addr as *mut u8;
            unsafe { 
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), dest, bytes.len()); 
            }
        }
    }

    // ==========================================
    // Core page allocator with free-list reuse.
    // ==========================================
    fn allocate_page() -> u32 {
        let sb = Self::superblock();


        loop {
            let head = sb.free_list_head.load(Ordering::Acquire);
            if head == 0 {
                break; 
            }

            let page_ptr = unsafe { (SHM_BASE + head as usize) as *const Page };
            let next_free = unsafe { (*page_ptr).next_offset.load(Ordering::Relaxed) };

            if sb.free_list_head.compare_exchange(
                head, 
                next_free, 
                Ordering::SeqCst, 
                Ordering::SeqCst
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
            if current_alloc >= 0x7FF0_0000 { 
                continue; 
            }

            let local_cap = unsafe { LOCAL_CAPACITY };

            if current_alloc + PAGE_SIZE > local_cap {
                 let global_cap = sb.global_capacity.load(Ordering::Acquire);
                 if global_cap > local_cap {
                     unsafe { host_remap(global_cap); LOCAL_CAPACITY = global_cap; }
                     continue; 
                 } else {
                     let new_cap = local_cap * 2;
                     
                     if new_cap >= 0x8000_0000 {
                         
                         continue;
                     }

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

    fn get_or_init_bucket_array() -> usize {
        let sb = Self::superblock();
        let mut map_base = sb.shared_map_base.load(Ordering::Acquire);
        if map_base != 0 {
            return map_base as usize;
        }

        let new_page_offset = Self::allocate_page();
        
        match sb.shared_map_base.compare_exchange(
            0, 
            new_page_offset, 
            Ordering::SeqCst, 
            Ordering::SeqCst
        ) {
            Ok(_) => new_page_offset as usize,
            Err(existing_base) => existing_base as usize
        }
    }

    pub fn insert_shared_data(key_hash: u32, writer_id: u32, data: &[u8]) {
        let map_base_offset = Self::get_or_init_bucket_array();
        let bucket_idx = (key_hash as usize) % BUCKET_COUNT;

        // ==========================================
        // Insert a node into the per-key lock-free chain.
        // Pointer arithmetic is done via byte pointers to avoid UB.
        // ==========================================
        let bucket_ptr = unsafe { 
            // Use raw byte pointers for precise control of address calculation.
            let base_ptr = (SHM_BASE + map_base_offset) as *const u8;
            base_ptr.add(bucket_idx * 4) as *const AtomicU32
        };
        let bucket = unsafe { &*bucket_ptr };

        let node_offset = Self::allocate_page();
        let node_ptr = (SHM_BASE + node_offset as usize) as *mut u8;
        
        unsafe {
            let header = node_ptr as *mut ChainNodeHeader;
            (*header).writer_id = writer_id;
            (*header).data_len = data.len() as u32;
            let data_dest = node_ptr.add(12);
            core::ptr::copy_nonoverlapping(data.as_ptr(), data_dest, data.len());
        }

        let header = unsafe { &*(node_ptr as *const ChainNodeHeader) };
        let mut old_head = bucket.load(Ordering::Acquire);

        loop {
            header.next_node.store(old_head, Ordering::Relaxed);
            match bucket.compare_exchange(old_head, node_offset, Ordering::Release, Ordering::Relaxed) {
                Ok(_) => break,
                Err(val) => old_head = val,
            }
        }
    }

    pub fn read_shared_chain(key_hash: u32) -> Vec<(u32, Vec<u8>)> {
        let sb = Self::superblock();
        let map_base_offset = sb.shared_map_base.load(Ordering::Acquire);
        
        if map_base_offset == 0 {
            return Vec::new(); 
        }

        let bucket_idx = (key_hash as usize) % BUCKET_COUNT;
        
        // Use the same pointer arithmetic pattern as in the write path.
        let bucket_ptr = unsafe { 
            let base_ptr = (SHM_BASE + map_base_offset as usize) as *const u8;
            base_ptr.add(bucket_idx * 4) as *const AtomicU32 
        };
        
        let mut current_offset = unsafe { (*bucket_ptr).load(Ordering::Acquire) };
        let mut results = Vec::new();

        while current_offset != 0 {
            // Ensure the backing mapping is large enough before dereferencing.
            let required_cap = current_offset + PAGE_SIZE;
            let local_cap = unsafe { LOCAL_CAPACITY };
            if required_cap > local_cap {
                 let global_cap = sb.global_capacity.load(Ordering::Acquire);
                 unsafe { host_remap(global_cap); LOCAL_CAPACITY = global_cap; }
            }

            let node_ptr = (SHM_BASE + current_offset as usize) as *const u8;
            let header = unsafe { &*(node_ptr as *const ChainNodeHeader) };
            
            let id = header.writer_id;
            let len = header.data_len;
            let mut data = Vec::with_capacity(len as usize);
            unsafe {
                data.set_len(len as usize);
                let src = node_ptr.add(12);
                core::ptr::copy_nonoverlapping(src, data.as_mut_ptr(), len as usize);
            }
            results.push((id, data));
            current_offset = header.next_node.load(Ordering::Acquire);
        }
        results
    }
    
    // Remaining helper functions for atomics and log streams.
    fn get_atomic_by_index(index: usize) -> &'static AtomicU64 {
        let base = SHM_BASE + ATOMIC_ARENA_OFFSET as usize;
        unsafe { &*((base as *const AtomicU64).add(index)) }
    }

    pub fn get_named_atomic(name: &str) -> &'static AtomicU64 {
        unsafe {
            if ATOMIC_INDEX_CACHE.is_none() { ATOMIC_INDEX_CACHE = Some(BTreeMap::new()); }
            let cache = ATOMIC_INDEX_CACHE.as_mut().unwrap();
            let index = if let Some(&idx) = cache.get(name) { idx } else {
                let idx = host_resolve_atomic(name.as_ptr() as u32, name.len() as u32);
                cache.insert(String::from(name), idx);
                idx
            };
            Self::get_atomic_by_index(index as usize)
        }
    }
    
    pub fn get_atomic(index: usize) -> &'static AtomicU64 { Self::get_atomic_by_index(index) }

    fn append_raw_bytes(id: u32, mut data: &[u8]) {
        let sb = Self::superblock();
        let mut tail_offset = sb.writer_tails[id as usize].load(Ordering::Acquire);
        if tail_offset == 0 {
            tail_offset = Self::allocate_page();
            let new_page = unsafe { &mut *((SHM_BASE + tail_offset as usize) as *mut Page) };
            new_page.next_offset.store(0, Ordering::Relaxed);
            new_page.cursor.store(0, Ordering::Relaxed);
            sb.writer_heads[id as usize].store(tail_offset, Ordering::Release);
            sb.writer_tails[id as usize].store(tail_offset, Ordering::Release);
        }
        while !data.is_empty() {
            let mut tail_page = unsafe { &mut *((SHM_BASE + tail_offset as usize) as *mut Page) };
            let current_cursor = tail_page.cursor.load(Ordering::Relaxed);
            let space_left = 4088 - current_cursor;
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
            data = &data[write_len..]; 
        }
    }

    pub fn append_bytes(id: u32, payload: &[u8]) {
        let record_len = payload.len() as u32;
        Self::append_raw_bytes(id, &record_len.to_le_bytes());
        Self::append_raw_bytes(id, payload);
    }
    
    pub fn read_latest_bytes(id: u32) -> Option<alloc::vec::Vec<u8>> {
        let sb = Self::superblock();
        let head_offset = sb.writer_heads[id as usize].load(Ordering::Acquire);
        if head_offset == 0 { return None; }
        let mut current_offset = head_offset;
        let mut cursor_in_page = 0;
        let mut read_exact = |mut dest: &mut [u8]| -> bool {
            while !dest.is_empty() {
                if current_offset == 0 { return false; }
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
        loop {
            let mut len_buf = [0u8; 4];
            if !read_exact(&mut len_buf) { break; }
            let record_len = u32::from_le_bytes(len_buf);
            let mut payload = alloc::vec::Vec::with_capacity(record_len as usize);
            unsafe { payload.set_len(record_len as usize); }
            if !read_exact(&mut payload) { break; }
            latest_data = Some(payload);
        }
        latest_data
    }
}