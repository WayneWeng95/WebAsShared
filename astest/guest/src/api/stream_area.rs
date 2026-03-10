use core::sync::atomic::Ordering;
use alloc::vec::Vec;
use common::*;
use super::{ShmApi, SHM_BASE, SeekFrom};

extern "C" {
    fn host_remap(new_size: u32);
}

impl ShmApi {
    // =====================================================================
    // Mode 2: Stream (append-only log with private head/tail pointers)
    // Use case: single-point output, log streams, one-to-one data pipelines (Map -> Reduce)
    // =====================================================================

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
    /// Writes a 4-byte LE length header followed by `payload`, allowing `read_latest_stream_data`
    /// to correctly frame records on the read side.
    fn append_bytes_prefixed(id: u32, payload: &[u8]) {
        let record_len = payload.len() as u32;
        Self::append_bytes_unprefixed(id, &record_len.to_le_bytes());
        Self::append_bytes_unprefixed(id, payload);
    }

    /// Scans all length-prefixed records from the stream head and returns the last complete one.
    /// Returns `None` if the stream is empty or the writer has not yet written any data.
    fn read_latest_bytes(id: u32) -> Option<Vec<u8>> {
        let sb = Self::superblock();
        let head_offset = sb.writer_heads[id as usize].load(Ordering::Acquire);
        if head_offset == 0 { return None; }
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

    /// [Output] Append data to own private log stream (lock-free, high throughput).
    pub fn append_stream_data(writer_id: u32, payload: &[u8]) {
        Self::append_bytes_prefixed(writer_id, payload);
    }

    /// [Input] Consume the latest log stream data from the specified upstream Worker.
    pub fn read_latest_stream_data(target_worker_id: u32) -> Option<Vec<u8>> {
        Self::read_latest_bytes(target_worker_id)
    }

    /// Scans every length-prefixed record in `id`'s stream from head to tail
    /// and returns them all in order.  Returns an empty Vec if the stream is
    /// empty.  Used by routing tests where all records from merged / shuffled
    /// chains must be inspected, not just the last one.
    pub fn read_all_stream_records(id: u32) -> Vec<Vec<u8>> {
        let sb = Self::superblock();
        let head_offset = sb.writer_heads[id as usize].load(Ordering::Acquire);
        if head_offset == 0 { return Vec::new(); }
        let mut current_offset = head_offset;
        let mut cursor_in_page = 0u32;
        // Same page-walking closure as read_latest_bytes but collects every record.
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

    /// Reads `length` bytes from writer `id`'s stream starting at absolute byte `offset`.
    /// Traverses the page chain and copies data across page boundaries as needed.
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
            // Ensure the mapping covers this page before dereferencing.
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

            // Copy the overlap between this page's logical range and [offset, end).
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
            if next == 0 { break; } // offset range extends beyond written data
            logical_start = logical_end;
            current_offset = next;
        }

        if bytes_read == 0 { return None; }
        unsafe { result.set_len(bytes_read); }
        Some(result)
    }

    /// Overwrites `data.len()` bytes in writer `id`'s stream starting at absolute byte `offset`.
    /// Only in-place overwrites of already-written data are supported; the stream is not extended.
    /// Returns `true` if the full range `[offset, offset + data.len())` was written,
    /// or `false` if any part of the range falls beyond the current written extent.
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
            if next == 0 { break; } // ran out of written pages before finishing
            logical_start = logical_end;
            current_offset = next;
        }

        bytes_written >= length
    }

    /// Computes a new byte position within writer `id`'s stream.
    /// `current_pos` is the caller-owned cursor; `whence` controls how the new position
    /// is derived.  For `SeekFrom::End`, the total bytes written is determined by
    /// traversing the page chain.  Returns the new absolute offset, or `None` if the
    /// stream is empty or the computed position falls outside `[0, total_bytes_written]`.
    ///
    /// ```ignore
    /// let mut pos: u32 = 0;
    /// pos = ShmApi::lseek_stream(worker_id, pos, SeekFrom::End(0)).unwrap(); // seek to end
    /// ShmApi::write_stream_range(worker_id, pos - 8, &patch);               // overwrite last 8 bytes
    /// ```
    pub fn lseek_stream(writer_id: u32, current_pos: u32, whence: SeekFrom) -> Option<u32> {
        let sb = Self::superblock();
        let head_offset = sb.writer_heads[writer_id as usize].load(Ordering::Acquire);
        if head_offset == 0 { return None; }

        // Walk the page chain to compute total bytes written (needed for End and bounds check).
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
