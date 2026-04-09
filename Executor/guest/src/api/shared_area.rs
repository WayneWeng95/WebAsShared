use core::sync::atomic::Ordering;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use common::*;
use super::{ShmApi, SHM_BASE, SeekFrom};

extern "C" {
    fn host_remap(new_size: ShmOffset);
    fn host_resolve_atomic(ptr: WasmPtr, len: WasmPtr) -> u32;
}

impl ShmApi {
    // =====================================================================
    // Mode 1: Consensus (shared variables resolved via Registry and Manager)
    // Use case: aggregated results, global config updates, multi-writer single-reader
    // =====================================================================

    /// Resolves a symbolic task name to its registry index, caching the result locally.
    /// Delegates to `host_resolve_atomic` on first lookup; subsequent calls use the cached value.
    fn resolve_name_to_index(name: &str) -> u32 {
        unsafe {
            if super::ATOMIC_INDEX_CACHE.is_none() { super::ATOMIC_INDEX_CACHE = Some(BTreeMap::new()); }
            let cache = super::ATOMIC_INDEX_CACHE.as_mut().unwrap();
            if let Some(&idx) = cache.get(name) {
                idx
            } else {
                let idx = host_resolve_atomic(name.as_ptr() as WasmPtr, name.len() as WasmPtr);
                cache.insert(String::from(name), idx);
                idx
            }
        }
    }

    /// Returns the byte offset of the shared hash bucket array, initializing it lazily on first call.
    /// Uses a CAS to ensure only one page is ever committed even under concurrent races.
    fn get_bucket_array() -> usize {
        let sb = Self::superblock();
        let map_base = sb.shared_map_base.load(Ordering::Acquire);
        if map_base != 0 { return map_base as usize; }

        let new_page_offset = Self::try_allocate_page();
        match sb.shared_map_base.compare_exchange(0, new_page_offset, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_)              => new_page_offset as usize,
            Err(existing_base) => existing_base as usize,
        }
    }

    /// Inserts a keyed data node into the shared lock-free hash map.
    /// Multiple writers may concurrently insert under the same `key_hash`; all nodes are
    /// prepended to the bucket's linked list and resolved later by the Manager via a policy.
    pub fn insert_shared_data(key_hash: u32, writer_id: u32, data: &[u8]) {
        let map_base_offset = Self::get_bucket_array();
        let bucket_idx = (key_hash as usize) % BUCKET_COUNT;

        // Pointer arithmetic is done via byte pointers to avoid UB.
        let bucket_ptr = unsafe {
            let base_ptr = (SHM_BASE + map_base_offset) as *const u8;
            base_ptr.add(bucket_idx * core::mem::size_of::<AtomicShmOffset>()) as *const AtomicShmOffset
        };
        let bucket = unsafe { &*bucket_ptr };

        let node_offset = Self::try_allocate_page();
        let node_ptr = (SHM_BASE + node_offset as usize) as *mut u8;
        unsafe {
            let header = node_ptr as *mut ChainNodeHeader;
            (*header).writer_id = writer_id;
            (*header).data_len = data.len() as u32;
            let data_dest = node_ptr.add(core::mem::size_of::<ChainNodeHeader>());
            core::ptr::copy_nonoverlapping(data.as_ptr(), data_dest, data.len());
        }

        let header = unsafe { &*(node_ptr as *const ChainNodeHeader) };
        let mut old_head = bucket.load(Ordering::Acquire);
        loop {
            header.next_node.store(old_head, Ordering::Relaxed);
            match bucket.compare_exchange(old_head, node_offset, Ordering::Release, Ordering::Relaxed) {
                Ok(_)    => break,
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
        if map_base_offset == 0 { return Vec::new(); }

        let bucket_idx = (key_hash as usize) % BUCKET_COUNT;
        let bucket_ptr = unsafe {
            let base_ptr = (SHM_BASE + map_base_offset as usize) as *const u8;
            base_ptr.add(bucket_idx * core::mem::size_of::<AtomicShmOffset>()) as *const AtomicShmOffset
        };

        let mut current_offset: ShmOffset = unsafe { (*bucket_ptr).load(Ordering::Acquire) };
        let mut results = Vec::new();

        while current_offset != 0 {
            // Ensure the backing mapping is large enough before dereferencing.
            let required_cap = current_offset + PAGE_SIZE;
            let local_cap = unsafe { super::LOCAL_CAPACITY };
            if required_cap > local_cap {
                let global_cap = sb.global_capacity.load(Ordering::Acquire);
                unsafe { host_remap(global_cap); super::LOCAL_CAPACITY = global_cap; }
            }

            let node_ptr = (SHM_BASE + current_offset as usize) as *const u8;
            let header = unsafe { &*(node_ptr as *const ChainNodeHeader) };
            let len = header.data_len;
            let mut data = Vec::with_capacity(len as usize);
            unsafe {
                data.set_len(len as usize);
                core::ptr::copy_nonoverlapping(node_ptr.add(core::mem::size_of::<ChainNodeHeader>()), data.as_mut_ptr(), len as usize);
            }
            results.push((header.writer_id, data));
            current_offset = header.next_node.load(Ordering::Acquire);
        }
        results
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
            base_ptr.add(bucket_idx * core::mem::size_of::<AtomicShmOffset>()) as *const AtomicShmOffset
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
            core::ptr::copy_nonoverlapping(data.as_ptr(), head_ptr.add(core::mem::size_of::<ChainNodeHeader>()), write_len);
            data_written += write_len;

            let mut prev_next_ptr = &mut (*header).next_payload_page as *mut ShmOffset;
            while data_written < total_len {
                let next_offset = Self::try_allocate_page();
                *prev_next_ptr = next_offset;

                let next_ptr = (SHM_BASE + next_offset as usize) as *mut u8;
                let overflow_header = next_ptr as *mut ShmOffset;
                *overflow_header = 0;

                let overflow_capacity = 4096 - core::mem::size_of::<ShmOffset>();
                let remain = total_len - data_written;
                let write_len = core::cmp::min(remain, overflow_capacity);
                core::ptr::copy_nonoverlapping(data.as_ptr().add(data_written), next_ptr.add(core::mem::size_of::<ShmOffset>()), write_len);
                data_written += write_len;

                prev_next_ptr = overflow_header;
            }

            let mut old_head: ShmOffset = bucket.load(Ordering::Acquire);
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

    /// Reads the full payload of the Manager-committed winning state for `task_name`.
    /// Returns `None` if the Manager has not yet resolved any writes for this task.
    pub fn read_shared_state(task_name: &str) -> Option<Vec<u8>> {
        let reg_idx = Self::resolve_name_to_index(task_name);
        let entry_ptr = unsafe {
            let base = SHM_BASE + REGISTRY_OFFSET as usize;
            (base + reg_idx as usize * 64) as *const RegistryEntry
        };
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
                (core::mem::size_of::<ChainNodeHeader>(), header.next_payload_page)
            } else {
                let next = unsafe { *(page_ptr as *const ShmOffset) };
                (core::mem::size_of::<ShmOffset>(), next)
            };

            let read_len = core::cmp::min(total_len - bytes_read, 4096 - header_size);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    page_ptr.add(header_size),
                    vec.as_mut_ptr().add(bytes_read),
                    read_len,
                );
            }
            bytes_read += read_len;
            current_offset = next_page;
            is_head = false;
        }

        unsafe { vec.set_len(total_len); }
        Some(vec)
    }

    /// Read a sub-range `[offset, offset+length)` from shared state without copying the full data.
    /// Returns `None` if the state does not exist or the range is out of bounds.
    pub fn read_shared_state_range(task_name: &str, offset: usize, length: usize) -> Option<Vec<u8>> {
        if length == 0 { return Some(Vec::new()); }

        let reg_idx = Self::resolve_name_to_index(task_name);
        let entry_ptr = unsafe {
            let base = SHM_BASE + REGISTRY_OFFSET as usize;
            (base + reg_idx as usize * 64) as *const RegistryEntry
        };
        let entry = unsafe { &*entry_ptr };

        let payload_offset = entry.payload_offset.load(Ordering::Acquire);
        let total_len = entry.payload_len.load(Ordering::Acquire) as usize;
        if payload_offset == 0 { return None; }

        // Bounds check: [offset, end) must fit inside [0, total_len)
        let end = offset.checked_add(length)?;
        if end > total_len { return None; }

        let mut result: Vec<u8> = Vec::with_capacity(length);
        let mut current_offset = payload_offset;
        let mut page_logical_start = 0usize;
        let mut result_written = 0usize;
        let mut is_head = true;

        while result_written < length {
            let page_ptr = (SHM_BASE + current_offset as usize) as *const u8;
            let (header_size, next_page) = if is_head {
                let header = unsafe { &*(page_ptr as *const ChainNodeHeader) };
                (core::mem::size_of::<ChainNodeHeader>(), header.next_payload_page)
            } else {
                let next = unsafe { *(page_ptr as *const ShmOffset) };
                (core::mem::size_of::<ShmOffset>(), next)
            };

            let page_capacity = 4096 - header_size;
            let page_logical_end = page_logical_start + page_capacity;

            // Copy the overlap between this page's logical range and [offset, end).
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

    /// Computes a new byte position within the committed shared state for `task_name`.
    /// `current_pos` is the caller-owned cursor; `whence` controls how the new position
    /// is derived.  Returns the resulting absolute offset, or `None` if `task_name` has
    /// no committed state or the computed position falls outside `[0, total_len]`.
    ///
    /// ```ignore
    /// let mut pos: u32 = 0;
    /// pos = ShmApi::lseek_shared_state("key", pos, SeekFrom::End(-64)).unwrap();
    /// let chunk = ShmApi::read_shared_state_range("key", pos as usize, 64);
    /// ```
    pub fn lseek_shared_state(task_name: &str, current_pos: u32, whence: SeekFrom) -> Option<u32> {
        let reg_idx = Self::resolve_name_to_index(task_name);
        let entry_ptr = {
            let base = SHM_BASE + REGISTRY_OFFSET as usize;
            (base + reg_idx as usize * 64) as *const RegistryEntry
        };
        let entry = unsafe { &*entry_ptr };

        if entry.payload_offset.load(Ordering::Acquire) == 0 { return None; }
        let total_len = entry.payload_len.load(Ordering::Acquire);

        let new_pos: i64 = match whence {
            SeekFrom::Start(pos) => pos as i64,
            SeekFrom::Current(d) => current_pos as i64 + d as i64,
            SeekFrom::End(d)     => total_len as i64 + d as i64,
        };
        if new_pos < 0 || new_pos as u64 > total_len as u64 { return None; }
        Some(new_pos as u32)
    }
}
