use core::sync::atomic::{AtomicU32, Ordering};
use alloc::vec::Vec;
use common::*;
use super::{ShmApi, SHM_BASE, SeekFrom};

extern "C" {
    fn host_remap(new_size: u32);
}

// ─── Shared page-chain core helpers ──────────────────────────────────────────
//
// These `pub(super)` free functions implement the page-chain mechanics without
// being tied to a specific slot array.  Both the stream API (`writer_heads` /
// `writer_tails`) and the I/O API (`io_heads` / `io_tails`) delegate to them,
// so the logic lives in exactly one place.

/// Append raw bytes into the page chain identified by `(head, tail)` atomics.
/// Allocates new 4 KiB pages from the bump pool as needed.
pub(super) fn chain_append_raw(head: &AtomicU32, tail: &AtomicU32, mut data: &[u8]) {
    let mut tail_offset = tail.load(Ordering::Acquire);
    if tail_offset == 0 {
        tail_offset = ShmApi::try_allocate_page();
        let new_page = unsafe { &mut *((SHM_BASE + tail_offset as usize) as *mut Page) };
        new_page.next_offset.store(0, Ordering::Relaxed);
        new_page.cursor.store(0, Ordering::Relaxed);
        head.store(tail_offset, Ordering::Release);
        tail.store(tail_offset, Ordering::Release);
    }
    while !data.is_empty() {
        let tail_page = unsafe { &mut *((SHM_BASE + tail_offset as usize) as *mut Page) };
        let current_cursor = tail_page.cursor.load(Ordering::Relaxed);
        let space_left = 4088 - current_cursor;
        if space_left == 0 {
            let new_offset = ShmApi::try_allocate_page();
            let new_page = unsafe { &mut *((SHM_BASE + new_offset as usize) as *mut Page) };
            new_page.next_offset.store(0, Ordering::Relaxed);
            new_page.cursor.store(0, Ordering::Relaxed);
            tail_page.next_offset.store(new_offset, Ordering::Release);
            tail.store(new_offset, Ordering::Release);
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

/// Write a 4-byte LE length header followed by `payload` into `(head, tail)`.
pub(super) fn chain_append_prefixed(head: &AtomicU32, tail: &AtomicU32, payload: &[u8]) {
    chain_append_raw(head, tail, &(payload.len() as u32).to_le_bytes());
    chain_append_raw(head, tail, payload);
}

/// Walk the page chain starting at `head_offset` and return the last complete
/// length-prefixed record.  Returns `None` when the chain is empty.
pub(super) fn chain_read_latest(head_offset: u32) -> Option<Vec<u8>> {
    if head_offset == 0 { return None; }
    let sb = ShmApi::superblock();
    let mut current_offset = head_offset;
    let mut cursor_in_page = 0u32;

    let mut read_exact = |mut dest: &mut [u8]| -> bool {
        while !dest.is_empty() {
            if current_offset == 0 { return false; }
            let required_cap = current_offset + PAGE_SIZE;
            let local_cap = unsafe { super::LOCAL_CAPACITY };
            if required_cap > local_cap {
                let global_cap = sb.global_capacity.load(Ordering::Acquire);
                if global_cap >= required_cap {
                    unsafe { host_remap(global_cap); super::LOCAL_CAPACITY = global_cap; }
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
        let mut payload = Vec::with_capacity(record_len as usize);
        unsafe { payload.set_len(record_len as usize); }
        if !read_exact(&mut payload) { break; }
        latest_data = Some(payload);
    }
    latest_data
}

/// Walk the page chain starting at `head_offset` and return every
/// length-prefixed record in order.  Returns an empty Vec when the chain is empty.
pub(super) fn chain_read_all(head_offset: u32) -> Vec<Vec<u8>> {
    if head_offset == 0 { return Vec::new(); }
    let sb = ShmApi::superblock();
    let mut current_offset = head_offset;
    let mut cursor_in_page = 0u32;

    let mut read_exact = |dest: &mut [u8]| -> bool {
        let mut dest = dest;
        while !dest.is_empty() {
            if current_offset == 0 { return false; }
            let required_cap = current_offset + PAGE_SIZE;
            let local_cap = unsafe { super::LOCAL_CAPACITY };
            if required_cap > local_cap {
                let global_cap = sb.global_capacity.load(Ordering::Acquire);
                if global_cap >= required_cap {
                    unsafe { host_remap(global_cap); super::LOCAL_CAPACITY = global_cap; }
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

    let mut records = Vec::new();
    loop {
        let mut len_buf = [0u8; 4];
        if !read_exact(&mut len_buf) { break; }
        let record_len = u32::from_le_bytes(len_buf);
        let mut payload = Vec::with_capacity(record_len as usize);
        unsafe { payload.set_len(record_len as usize); }
        if !read_exact(&mut payload) { break; }
        records.push(payload);
    }
    records
}

// ─── Stream-slot ShmApi methods ───────────────────────────────────────────────

impl ShmApi {
    // Use case: single-point output, log streams, one-to-one data pipelines (Map → Reduce).
    // Stream slots (0..STREAM_SLOT_COUNT) are entirely free for application use —
    // no slots are reserved.  Host input/output uses the separate I/O area instead.

    /// Append a length-prefixed record to stream `writer_id`.
    ///
    /// All stream slot IDs in `[0, STREAM_SLOT_COUNT)` are valid.
    pub fn append_stream_data(writer_id: u32, payload: &[u8]) {
        debug_assert!(
            (writer_id as usize) < STREAM_SLOT_COUNT,
            "stream slot {} out of range (STREAM_SLOT_COUNT={})",
            writer_id, STREAM_SLOT_COUNT,
        );
        let sb = Self::superblock();
        chain_append_prefixed(
            &sb.writer_heads[writer_id as usize],
            &sb.writer_tails[writer_id as usize],
            payload,
        );
    }

    /// Return the most recent record from stream `target_worker_id`.
    pub fn read_latest_stream_data(target_worker_id: u32) -> Option<Vec<u8>> {
        let head = Self::superblock().writer_heads[target_worker_id as usize]
            .load(Ordering::Acquire);
        chain_read_latest(head)
    }

    /// Return every record from stream `id` in order.
    pub fn read_all_stream_records(id: u32) -> Vec<Vec<u8>> {
        let head = Self::superblock().writer_heads[id as usize].load(Ordering::Acquire);
        chain_read_all(head)
    }

    /// Read `length` bytes from stream `writer_id` starting at absolute byte `offset`.
    /// Returns `None` if the stream is empty or `offset` is beyond all written data.
    pub fn read_stream_range(writer_id: u32, offset: u32, length: usize) -> Option<Vec<u8>> {
        if length == 0 { return Some(Vec::new()); }
        let sb = Self::superblock();
        let head_offset = sb.writer_heads[writer_id as usize].load(Ordering::Acquire);
        if head_offset == 0 { return None; }

        let mut result: Vec<u8> = Vec::with_capacity(length);
        let mut bytes_read = 0usize;
        let end = offset as usize + length;
        let mut current_offset = head_offset;
        let mut logical_start = 0usize;

        while bytes_read < length {
            let required_cap = current_offset + PAGE_SIZE;
            let local_cap = unsafe { super::LOCAL_CAPACITY };
            if required_cap > local_cap {
                let global_cap = sb.global_capacity.load(Ordering::Acquire);
                if global_cap >= required_cap {
                    unsafe { host_remap(global_cap); super::LOCAL_CAPACITY = global_cap; }
                } else { break; }
            }
            let page = unsafe { &*((SHM_BASE + current_offset as usize) as *const Page) };
            let written = page.cursor.load(Ordering::Acquire) as usize;
            let logical_end = logical_start + written;
            let copy_start = (offset as usize).max(logical_start);
            let copy_end   = end.min(logical_end);
            if copy_start < copy_end {
                let src_off  = copy_start - logical_start;
                let copy_len = copy_end - copy_start;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        page.data.as_ptr().add(src_off),
                        result.as_mut_ptr().add(bytes_read),
                        copy_len,
                    );
                }
                bytes_read += copy_len;
            }
            if logical_end >= end { break; }
            let next = page.next_offset.load(Ordering::Acquire);
            if next == 0 { break; }
            logical_start = logical_end;
            current_offset = next;
        }

        if bytes_read == 0 { return None; }
        unsafe { result.set_len(bytes_read); }
        Some(result)
    }

    /// Overwrite `data.len()` bytes in stream `writer_id` starting at absolute byte `offset`.
    /// Only in-place overwrites of already-written data are supported; the stream is not extended.
    /// Returns `true` if the full range was written.
    pub fn write_stream_range(writer_id: u32, offset: u32, data: &[u8]) -> bool {
        if data.is_empty() { return true; }
        let sb = Self::superblock();
        let head_offset = sb.writer_heads[writer_id as usize].load(Ordering::Acquire);
        if head_offset == 0 { return false; }

        let mut bytes_written = 0usize;
        let length = data.len();
        let end = offset as usize + length;
        let mut current_offset = head_offset;
        let mut logical_start = 0usize;

        while bytes_written < length {
            let page = unsafe { &mut *((SHM_BASE + current_offset as usize) as *mut Page) };
            let written = page.cursor.load(Ordering::Acquire) as usize;
            let logical_end = logical_start + written;
            let copy_start = (offset as usize).max(logical_start);
            let copy_end   = end.min(logical_end);
            if copy_start < copy_end {
                let dst_off  = copy_start - logical_start;
                let copy_len = copy_end - copy_start;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        data.as_ptr().add(bytes_written),
                        page.data.as_mut_ptr().add(dst_off),
                        copy_len,
                    );
                }
                bytes_written += copy_len;
            }
            if logical_end >= end { break; }
            let next = page.next_offset.load(Ordering::Acquire);
            if next == 0 { break; }
            logical_start = logical_end;
            current_offset = next;
        }
        bytes_written >= length
    }

    /// Compute a new byte position within stream `writer_id`.
    ///
    /// ```ignore
    /// let mut pos: u32 = 0;
    /// pos = ShmApi::lseek_stream(id, pos, SeekFrom::End(0)).unwrap();
    /// ShmApi::write_stream_range(id, pos - 8, &patch);
    /// ```
    pub fn lseek_stream(writer_id: u32, current_pos: u32, whence: SeekFrom) -> Option<u32> {
        let sb = Self::superblock();
        let head_offset = sb.writer_heads[writer_id as usize].load(Ordering::Acquire);
        if head_offset == 0 { return None; }

        let mut total = 0u32;
        let mut current_offset = head_offset;
        loop {
            let page = unsafe { &*((SHM_BASE + current_offset as usize) as *const Page) };
            total += page.cursor.load(Ordering::Acquire);
            let next = page.next_offset.load(Ordering::Acquire);
            if next == 0 { break; }
            current_offset = next;
        }

        let new_pos: i64 = match whence {
            SeekFrom::Start(pos) => pos as i64,
            SeekFrom::Current(d) => current_pos as i64 + d as i64,
            SeekFrom::End(d)     => total as i64 + d as i64,
        };
        if new_pos < 0 || new_pos as u64 > total as u64 { return None; }
        Some(new_pos as u32)
    }
}
