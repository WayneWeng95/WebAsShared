// SHM slot helpers — read source SGEs from a local slot, allocate a fresh
// page chain in SHM, and link the chain's head/tail into the slot pointers.

use std::sync::atomic::Ordering;

use anyhow::{anyhow, Result};
use common::{PAGE_SIZE, Page, Superblock};

use crate::runtime::dag_runner::RemoteSlotKind;

use super::PAGE_DATA;

/// Walk the page chain for `slot` and collect (vaddr, len) SGE pairs.
/// Returns the SGE list and the total occupied byte count.
pub(super) fn collect_src_sges(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
) -> (Vec<(u64, u32)>, u32) {
    let sb = unsafe { &*(splice_addr as *const Superblock) };
    let head = match slot_kind {
        RemoteSlotKind::Stream => sb.writer_heads[slot].load(Ordering::Acquire),
        RemoteSlotKind::Io     => sb.io_heads[slot].load(Ordering::Acquire),
    };
    let mut src_sges: Vec<(u64, u32)> = Vec::new();
    let mut total_bytes: u32 = 0;
    let mut page_off = head;
    while page_off != 0 {
        let page = unsafe { &*((splice_addr + page_off as usize) as *const Page) };
        let used = page.cursor.load(Ordering::Acquire) as u32;
        if used > 0 {
            let data_vaddr = unsafe { page.data.as_ptr() } as u64;
            src_sges.push((data_vaddr, used));
            total_bytes += used;
        }
        page_off = page.next_offset.load(Ordering::Acquire);
    }
    (src_sges, total_bytes)
}

/// Bump-allocate `n_pages` in SHM, initialise each page's cursor and
/// `next_offset`, and link the head/tail into `slot`.
/// Returns the SHM offset of the first (head) page.
pub(super) fn alloc_and_link(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    total_bytes: usize,
) -> Result<u32> {
    let n_pages        = (total_bytes + PAGE_DATA - 1) / PAGE_DATA;
    let bytes_to_alloc = (n_pages * PAGE_SIZE as usize) as u32;

    let sb       = unsafe { &*(splice_addr as *const Superblock) };
    let dest_off = sb.bump_allocator.fetch_add(bytes_to_alloc, Ordering::AcqRel);
    let cap      = sb.global_capacity.load(Ordering::Acquire);
    if dest_off + bytes_to_alloc > cap {
        return Err(anyhow!(
            "SHM capacity exhausted for RDMA receive ({} bytes). \
             Increase INITIAL_SHM_SIZE.", bytes_to_alloc
        ));
    }

    for i in 0..n_pages as u32 {
        let page_off = dest_off + i * PAGE_SIZE;
        let page = unsafe { &mut *((splice_addr + page_off as usize) as *mut Page) };
        let data_in_page = PAGE_DATA.min(total_bytes - i as usize * PAGE_DATA);
        page.cursor.store(data_in_page as u32, Ordering::Relaxed);
        let next = if i + 1 < n_pages as u32 { dest_off + (i + 1) * PAGE_SIZE } else { 0 };
        page.next_offset.store(next, Ordering::Relaxed);
    }

    let tail_off = dest_off + (n_pages as u32 - 1) * PAGE_SIZE;
    link_to_slot(sb, slot, slot_kind, dest_off, tail_off);
    Ok(dest_off)
}

/// Store `head_off` and `tail_off` into the head/tail atomics for `slot`.
pub(super) fn link_to_slot(
    sb:       &Superblock,
    slot:     usize,
    kind:     RemoteSlotKind,
    head_off: u32,
    tail_off: u32,
) {
    match kind {
        RemoteSlotKind::Stream => {
            sb.writer_heads[slot].store(head_off, Ordering::Release);
            sb.writer_tails[slot].store(tail_off, Ordering::Release);
        }
        RemoteSlotKind::Io => {
            sb.io_heads[slot].store(head_off, Ordering::Release);
            sb.io_tails[slot].store(tail_off, Ordering::Release);
        }
    }
}
