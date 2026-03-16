// SHM bridge — Rust-side implementations of the C-callable functions declared
// in shm_module.c.
//
// These mirror the logic in guest/src/api/ but are exported with #[no_mangle]
// so MicroPython's native `shm` C module can call them directly.
//
// Record buffer pattern: `rust_load_*_records` fills RECORD_CACHE; the C module
// then queries records by index with `rust_get_record_*`.  This avoids complex
// ownership across the C/Rust boundary.

use core::sync::atomic::Ordering;

use common::{Page, Superblock, PAGE_SIZE, OUTPUT_IO_SLOT};

const SHM_BASE: usize = 0x8000_0000;

// ── Record cache (filled per rust_load_* call) ────────────────────────────────

static mut RECORD_CACHE: alloc::vec::Vec<(u32, alloc::vec::Vec<u8>)> = alloc::vec::Vec::new();

// ── Internal page-chain reader ────────────────────────────────────────────────

fn read_chain(head: u32) -> alloc::vec::Vec<(u32, alloc::vec::Vec<u8>)> {
    if head == 0 { return alloc::vec::Vec::new(); }

    let mut records = alloc::vec::Vec::new();
    let mut page_off = head;
    let mut cursor_in_page: u32 = 0;

    macro_rules! read_bytes {
        ($dest:expr) => {{
            let dest: &mut [u8] = $dest;
            let mut written = 0usize;
            let mut ok = true;
            while written < dest.len() {
                if page_off == 0 { ok = false; break; }
                let page = unsafe { &*((SHM_BASE + page_off as usize) as *const Page) };
                let page_written = page.cursor.load(Ordering::Acquire);
                let available = page_written.saturating_sub(cursor_in_page);
                if available == 0 {
                    page_off = page.next_offset.load(Ordering::Acquire);
                    cursor_in_page = 0;
                    continue;
                }
                let n = (available as usize).min(dest.len() - written);
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        page.data.as_ptr().add(cursor_in_page as usize),
                        dest.as_mut_ptr().add(written),
                        n,
                    );
                }
                cursor_in_page += n as u32;
                written        += n;
            }
            ok
        }};
    }

    loop {
        let mut len_buf = [0u8; 4];
        if !read_bytes!(&mut len_buf) { break; }
        let payload_len = u32::from_le_bytes(len_buf) as usize;

        let mut origin_buf = [0u8; 4];
        if !read_bytes!(&mut origin_buf) { break; }
        let origin = u32::from_le_bytes(origin_buf);

        let mut payload = alloc::vec![0u8; payload_len];
        if !read_bytes!(&mut payload) { break; }

        records.push((origin, payload));
    }
    records
}

// ── Internal page-chain writer ────────────────────────────────────────────────

fn alloc_page(sb: &Superblock) -> u32 {
    // Bump allocator — mirrors guest/src/api/page_allocator.rs.
    // Free-list reuse is omitted here; add it if write-heavy workloads
    // exhaust the bump space.
    let offset = sb.bump_allocator.fetch_add(PAGE_SIZE, Ordering::AcqRel);
    let page = unsafe { &mut *((SHM_BASE + offset as usize) as *mut Page) };
    page.next_offset.store(0, Ordering::Relaxed);
    page.cursor.store(0, Ordering::Relaxed);
    offset
}

/// Append `buf` to the page chain identified by `(head, tail)`.
/// Both are updated in place; allocates new pages as needed.
fn chain_write(sb: &Superblock, head: &mut u32, tail: &mut u32, buf: &[u8]) {
    let mut remaining = buf;
    while !remaining.is_empty() {
        if *tail == 0 {
            let new_off = alloc_page(sb);
            *head = new_off;
            *tail = new_off;
        }

        let page = unsafe { &mut *((SHM_BASE + *tail as usize) as *mut Page) };
        let cursor = page.cursor.load(Ordering::Acquire) as usize;
        let space  = page.data.len() - cursor;

        if space == 0 {
            let new_off = alloc_page(sb);
            page.next_offset.store(new_off, Ordering::Release);
            *tail = new_off;
            continue;
        }

        let n = space.min(remaining.len());
        unsafe {
            core::ptr::copy_nonoverlapping(
                remaining.as_ptr(),
                page.data.as_mut_ptr().add(cursor),
                n,
            );
        }
        page.cursor.store((cursor + n) as u32, Ordering::Release);
        remaining = &remaining[n..];
    }
}

/// Write `[4-byte len][4-byte origin][payload]` to a stream or I/O slot.
fn append_record(
    sb:      &Superblock,
    head_at: &core::sync::atomic::AtomicU32,
    tail_at: &core::sync::atomic::AtomicU32,
    origin:  u32,
    payload: &[u8],
) {
    let mut head = head_at.load(Ordering::Acquire);
    let mut tail = tail_at.load(Ordering::Acquire);

    chain_write(sb, &mut head, &mut tail, &(payload.len() as u32).to_le_bytes());
    chain_write(sb, &mut head, &mut tail, &origin.to_le_bytes());
    chain_write(sb, &mut head, &mut tail, payload);

    head_at.store(head, Ordering::Release);
    tail_at.store(tail, Ordering::Release);
}

// ── C-callable exports ────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn rust_load_stream_records(slot: u32) -> u32 {
    let sb   = unsafe { &*(SHM_BASE as *const Superblock) };
    let head = sb.writer_heads[slot as usize].load(Ordering::Acquire);
    let recs = read_chain(head);
    let count = recs.len() as u32;
    unsafe { RECORD_CACHE = recs; }
    count
}

#[no_mangle]
pub extern "C" fn rust_load_io_records(io_slot: u32) -> u32 {
    let sb   = unsafe { &*(SHM_BASE as *const Superblock) };
    let head = sb.io_heads[io_slot as usize].load(Ordering::Acquire);
    let recs = read_chain(head);
    let count = recs.len() as u32;
    unsafe { RECORD_CACHE = recs; }
    count
}

#[no_mangle]
pub extern "C" fn rust_get_record_origin(idx: u32) -> u32 {
    unsafe { RECORD_CACHE.get(idx as usize).map(|(o, _)| *o).unwrap_or(0) }
}

#[no_mangle]
pub extern "C" fn rust_get_record_ptr(idx: u32) -> *const u8 {
    unsafe {
        RECORD_CACHE.get(idx as usize)
            .map(|(_, v)| v.as_ptr())
            .unwrap_or(core::ptr::null())
    }
}

#[no_mangle]
pub extern "C" fn rust_get_record_len(idx: u32) -> u32 {
    unsafe {
        RECORD_CACHE.get(idx as usize)
            .map(|(_, v)| v.len() as u32)
            .unwrap_or(0)
    }
}

#[no_mangle]
pub extern "C" fn rust_append_stream_data(slot: u32, data: *const u8, len: u32) {
    let sb      = unsafe { &*(SHM_BASE as *const Superblock) };
    let payload = unsafe { core::slice::from_raw_parts(data, len as usize) };
    append_record(sb, &sb.writer_heads[slot as usize],
                      &sb.writer_tails[slot as usize], slot, payload);
}

#[no_mangle]
pub extern "C" fn rust_write_output(data: *const u8, len: u32) {
    let sb      = unsafe { &*(SHM_BASE as *const Superblock) };
    let payload = unsafe { core::slice::from_raw_parts(data, len as usize) };
    append_record(sb, &sb.io_heads[OUTPUT_IO_SLOT as usize],
                      &sb.io_tails[OUTPUT_IO_SLOT as usize],
                      OUTPUT_IO_SLOT, payload);
}
