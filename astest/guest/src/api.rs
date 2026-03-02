use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use common::*;

const SHM_BASE: usize = 0x8000_0000;
static mut LOCAL_CAPACITY: u32 = 36 * MIB;
static mut ATOMIC_INDEX_CACHE: Option<BTreeMap<String, u32>> = None;

extern "C" {
    fn host_remap(new_size: u32);
    fn host_resolve_atomic(ptr: u32, len: u32) -> u32;
}

pub struct ShmApi;

impl ShmApi {
    /// Returns a shared reference to the Superblock at the base of shared memory.
    fn superblock() -> &'static Superblock { unsafe { &*(SHM_BASE as *const Superblock) } }

    /// Appends a debug message to the shared log arena.
    /// Reserves space atomically; silently drops the message if the arena is full.
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

    /// Allocates a 4KB page from the shared memory pool.
    /// First tries to reuse a recycled page from the lock-free free list (Treiber stack CAS pop).
    /// Falls back to bump allocation, triggering a VMA expansion via `host_remap` if needed.
    /// Returns the page's byte offset from the shared memory base.
    fn try_allocate_page() -> u32 {
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

    /// Returns the byte offset of the shared hash bucket array, initializing it lazily on first call.
    /// Uses a CAS to ensure only one page is ever committed even under concurrent races.
    fn get_bucket_array() -> usize {
        let sb = Self::superblock();
        let mut map_base = sb.shared_map_base.load(Ordering::Acquire);
        if map_base != 0 {
            return map_base as usize;
        }

        let new_page_offset = Self::try_allocate_page();
        
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

    /// Inserts a keyed data node into the shared lock-free hash map.
    /// Multiple writers may concurrently insert under the same `key_hash`; all nodes are
    /// prepended to the bucket's linked list and resolved later by the Manager via a policy.
    pub fn insert_shared_data(key_hash: u32, writer_id: u32, data: &[u8]) {
        let map_base_offset = Self::get_bucket_array();
        let bucket_idx = (key_hash as usize) % BUCKET_COUNT;

        // Insert a node into the per-key lock-free chain.
        // Pointer arithmetic is done via byte pointers to avoid UB.
        let bucket_ptr = unsafe { 
            // Use raw byte pointers for precise control of address calculation.
            let base_ptr = (SHM_BASE + map_base_offset) as *const u8;
            base_ptr.add(bucket_idx * 4) as *const AtomicU32
        };
        let bucket = unsafe { &*bucket_ptr };

        let node_offset = Self::try_allocate_page();
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

    /// Reads all nodes currently stored under `key_hash` from the shared hash map.
    /// Returns a list of `(writer_id, payload)` pairs in insertion order (newest first).
    /// Does not remove nodes; use the Manager's organizer to consume and resolve conflicts.
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
    
    /// Returns a reference to the `AtomicU64` at the given index in the shared atomic arena.
    fn get_atomic_by_index(index: usize) -> &'static AtomicU64 {
        let base = SHM_BASE + ATOMIC_ARENA_OFFSET as usize;
        unsafe { &*((base as *const AtomicU64).add(index)) }
    }

    /// Looks up a named atomic variable, registering it in the host Registry on first access.
    /// The resolved index is cached locally to avoid repeated `host_resolve_atomic` calls.
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
    
    /// Returns a reference to the `AtomicU64` at the given registry index.
    pub fn get_atomic(index: usize) -> &'static AtomicU64 { Self::get_atomic_by_index(index) }

    /// Appends raw bytes to writer `id`'s private stream without a length prefix.
    /// Allocates new pages as needed when the current tail page is full.
    fn append_bytes_unprefixed(id: u32, mut data: &[u8]) {
        let sb = Self::superblock();
        let mut tail_offset = sb.writer_tails[id as usize].load(Ordering::Acquire);
        if tail_offset == 0 {
            tail_offset = Self::try_allocate_page();
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
                let new_offset = Self::try_allocate_page();
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

    /// Appends a length-prefixed record to writer `id`'s private stream.
    /// Writes a 4-byte LE length header followed by `payload`, allowing `read_latest_bytes`
    /// to correctly frame records on the read side.
    pub fn append_bytes_prefixed(id: u32, payload: &[u8]) {
        let record_len = payload.len() as u32;
        Self::append_bytes_unprefixed(id, &record_len.to_le_bytes());
        Self::append_bytes_unprefixed(id, payload);
    }
    
    /// Reads the most recently written record from writer `id`'s private stream.
    /// Scans all length-prefixed records from the stream head and returns the last complete one.
    /// Returns `None` if the stream is empty or the writer has not yet written any data.
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

    /// Resolves a symbolic task name to its registry index, caching the result locally.
    /// Delegates to `host_resolve_atomic` on first lookup; subsequent calls use the cached value.
    fn resolve_name_to_index(name: &str) -> u32 {
        unsafe {
            if ATOMIC_INDEX_CACHE.is_none() { ATOMIC_INDEX_CACHE = Some(BTreeMap::new()); }
            let cache = ATOMIC_INDEX_CACHE.as_mut().unwrap();
            if let Some(&idx) = cache.get(name) { 
                idx 
            } else {
                let idx = host_resolve_atomic(name.as_ptr() as u32, name.len() as u32);
                cache.insert(String::from(name), idx);
                idx
            }
        }
    }
}



impl ShmApi {
    // =====================================================================
    // Mode 1: Consensus (shared variables resolved via Registry and Manager)
    // Use case: aggregated results, global config updates, multi-writer single-reader
    // =====================================================================

    /// [Output] Write shared state with explicit byte range (slice of `data` at `[offset, offset+length)`).
    /// Returns `false` and does nothing if the range is out of bounds.
    pub fn write_shared_state_range(task_name: &str, writer_id: u32, data: &[u8], offset: usize, length: usize) -> bool {
        let end = match offset.checked_add(length) {
            Some(e) if e <= data.len() => e,
            _ => return false,
        };
        Self::write_shared_state(task_name, writer_id, &data[offset..end]);
        true
    }

    /// Writes `data` as shared state for `task_name`.
    /// The node enters the bucket conflict pool; the Manager resolves concurrent writers
    /// using the active `ConsumptionPolicy` and commits the winner to the Registry.
    pub fn write_shared_state(task_name: &str, writer_id: u32, data: &[u8]) {
        let reg_idx = Self::resolve_name_to_index(task_name);
        let map_base_offset = Self::get_bucket_array();
        let bucket_idx = (reg_idx as usize) % BUCKET_COUNT;
        
        let bucket_ptr = unsafe { 
            let base_ptr = (SHM_BASE + map_base_offset) as *const u8;
            base_ptr.add(bucket_idx * 4) as *const core::sync::atomic::AtomicU32
        };
        let bucket = unsafe { &*bucket_ptr };

        let total_len = data.len();
        let mut data_written = 0;

        let head_offset = Self::try_allocate_page();
        let head_ptr = (SHM_BASE + head_offset as usize) as *mut u8;
        
        unsafe {
            let header = head_ptr as *mut ChainNodeHeader;
            (*header).writer_id = writer_id;
            (*header).data_len = total_len as u32;
            (*header).registry_index = reg_idx;
            (*header).next_payload_page = 0; 
            
            
            let head_capacity = 4096 - core::mem::size_of::<ChainNodeHeader>();
            let write_len = core::cmp::min(total_len, head_capacity);
            
            core::ptr::copy_nonoverlapping(data.as_ptr(), head_ptr.add(20), write_len);
            data_written += write_len;

            
            let mut prev_next_ptr = &mut (*header).next_payload_page as *mut u32;

            
            while data_written < total_len {
                let next_offset = Self::try_allocate_page();
                *prev_next_ptr = next_offset; 

                let next_ptr = (SHM_BASE + next_offset as usize) as *mut u8;
                let overflow_header = next_ptr as *mut u32; 
                *overflow_header = 0;

                
                let overflow_capacity = 4096 - 4;
                let remain = total_len - data_written;
                let write_len = core::cmp::min(remain, overflow_capacity);

                core::ptr::copy_nonoverlapping(data.as_ptr().add(data_written), next_ptr.add(4), write_len);
                data_written += write_len;

                prev_next_ptr = overflow_header; 
            }

            let mut old_head = bucket.load(Ordering::Acquire);
            loop {
                (*header).next_node.store(old_head, Ordering::Relaxed);
                if bucket.compare_exchange(old_head, head_offset, Ordering::Release, Ordering::Relaxed).is_ok() { 
                    break; 
                } else { 
                    old_head = bucket.load(Ordering::Acquire); 
                }
            }
        }
    }

    /// Read a sub-range `[offset, offset+length)` from shared state without copying the full data.
    /// Returns `None` if the state does not exist or the range is out of bounds.
    /// Returning a vertor of pointer heads to the chain of pages containing the payload,
    ///  allowing zero-copy traversal of large state.
    pub fn read_shared_state_range(task_name: &str, offset: usize, length: usize) -> Option<Vec<u8>> {
        if length == 0 { return Some(Vec::new()); }

        let reg_idx = Self::resolve_name_to_index(task_name);
        let registry_base = SHM_BASE + REGISTRY_OFFSET as usize;
        let entry_ptr = unsafe { (registry_base + reg_idx as usize * 64) as *const RegistryEntry };
        let entry = unsafe { &*entry_ptr };

        let payload_offset = entry.payload_offset.load(Ordering::Acquire);
        let total_len = entry.payload_len.load(Ordering::Acquire) as usize;

        if payload_offset == 0 { return None; }

        // Bounds check: [offset, end) must fit inside [0, total_len)
        let end = offset.checked_add(length)?;
        if end > total_len { return None; }

        let mut result: Vec<u8> = Vec::with_capacity(length);
        let mut current_offset = payload_offset;
        let mut page_logical_start = 0usize; // logical byte index of the first data byte on this page
        let mut result_written = 0usize;
        let mut is_head = true;

        while result_written < length {
            let page_ptr = (SHM_BASE + current_offset as usize) as *const u8;

            let (header_size, next_page) = if is_head {
                let header = unsafe { &*(page_ptr as *const ChainNodeHeader) };
                (20usize, header.next_payload_page)
            } else {
                let next = unsafe { *(page_ptr as *const u32) };
                (4usize, next)
            };

            let page_capacity = 4096 - header_size;
            let page_logical_end = page_logical_start + page_capacity;

            // Compute the overlap between this page's logical range and [offset, end)
            let copy_start = offset.max(page_logical_start);
            let copy_end   = end.min(page_logical_end);

            if copy_start < copy_end {
                let src_off_in_page = copy_start - page_logical_start;
                let copy_len = copy_end - copy_start;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        page_ptr.add(header_size + src_off_in_page),
                        result.as_mut_ptr().add(result_written),
                        copy_len,
                    );
                }
                result_written += copy_len;
            }

            if page_logical_end >= end { break; }
            page_logical_start = page_logical_end;
            current_offset = next_page;
            is_head = false;
        }

        unsafe { result.set_len(length); }
        Some(result)
    }

    /// Reads the full payload of the Manager-committed winning state for `task_name`.
    /// Returns `None` if the Manager has not yet resolved any writes for this task.
    /// Returning a vertor of pointer heads to the chain of pages containing the payload,
    ///  allowing zero-copy traversal of large state.
    pub fn read_shared_state(task_name: &str) -> Option<Vec<u8>> {
        let reg_idx = Self::resolve_name_to_index(task_name);
        let registry_base = SHM_BASE + REGISTRY_OFFSET as usize;
        let entry_ptr = unsafe { (registry_base + reg_idx as usize * 64) as *const RegistryEntry };
        let entry = unsafe { &*entry_ptr };
        
        let offset = entry.payload_offset.load(Ordering::Acquire);
        let total_len = entry.payload_len.load(Ordering::Acquire) as usize;
        
        if offset == 0 { return None; } 
        
        let mut vec: Vec<u8> = Vec::with_capacity(total_len);
        let mut current_offset = offset;
        let mut bytes_read = 0;
        let mut is_head = true;

        while bytes_read < total_len {
            let page_ptr = (SHM_BASE + current_offset as usize) as *const u8;
            
            let (header_size, next_page) = if is_head {
                let header = unsafe { &*(page_ptr as *const ChainNodeHeader) };
                (20, header.next_payload_page)
            } else {
                let next = unsafe { *(page_ptr as *const u32) };
                (4, next)
            };

            let read_len = core::cmp::min(total_len - bytes_read, 4096 - header_size);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    page_ptr.add(header_size), 
                    vec.as_mut_ptr().add(bytes_read), 
                    read_len
                );
            }
            bytes_read += read_len;
            current_offset = next_page;
            is_head = false;
        }

        unsafe { vec.set_len(total_len); }
        Some(vec)
    }

    // =====================================================================
    // Mode 2: Stream (append-only log with private head/tail pointers)
    // Use case: single-point output, log streams, one-to-one data pipelines (Map -> Reduce)
    // =====================================================================

    /// [Output] Append data to own private log stream (lock-free, high throughput)
    pub fn append_stream_data(writer_id: u32, payload: &[u8]) {
        // Reuses the existing append_bytes_prefixed implementation
        Self::append_bytes_prefixed(writer_id, payload);
    }

    /// [Input] Consume the latest log stream data from the specified upstream Worker
    pub fn read_latest_stream_data(target_worker_id: u32) -> Option<Vec<u8>> {
        // Reuses the existing read_latest_bytes implementation
        Self::read_latest_bytes(target_worker_id)
    }
}