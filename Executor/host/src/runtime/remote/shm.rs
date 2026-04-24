// SHM slot helpers — read source SGEs from a local slot, allocate a fresh
// page chain in SHM, and link the chain's head/tail into the slot pointers.

use std::sync::atomic::Ordering;

use anyhow::{anyhow, Result};
use common::{PAGE_SIZE, Page, PageId, ShmOffset, Superblock, DIRECT_LIMIT};
use connect::MeshNode;

use crate::runtime::dag_runner::RemoteSlotKind;

use super::PAGE_DATA;

/// One entry in the source SGE list — `(host_vaddr, length, lkey)`.  The
/// lkey is per-SGE because a single transfer may span multiple MRs:
/// MR1 for pages < `INITIAL_SHM_SIZE`, MR-src-ext for pages past that,
/// and MR-src-stage for paged-mode pages that were memcpy'd in.
pub(super) type SrcSge = (u64, u32, u32);

/// Walk the page chain for `slot` and collect source SGEs.
///
/// For each page:
///   * Direct-mode, offset < INITIAL_SHM_SIZE: MR1 lkey, no memcpy.
///   * Direct-mode, offset ≥ INITIAL_SHM_SIZE: lazily register/grow
///     MR-src-ext over the SHM extension, pick up its lkey.
///   * Paged-mode (`PageId ≥ DIRECT_LIMIT`): resolve host pointer via
///     the extended-pool runtime, memcpy page data into the sender's
///     staging MR (MR-src-stage), return SGE pointing into the staging
///     buffer.
///
/// Returns the SGE list and the total occupied byte count.  Errors if
/// any per-page MR provisioning fails.
pub(super) fn collect_src_sges(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    mesh:        &MeshNode,
) -> Result<(Vec<SrcSge>, ShmOffset)> {
    let sb = unsafe { &*(splice_addr as *const Superblock) };
    let head: PageId = match slot_kind {
        RemoteSlotKind::Stream => sb.writer_heads[slot].load(Ordering::Acquire),
        RemoteSlotKind::Io     => sb.io_heads[slot].load(Ordering::Acquire),
    };
    let shm_base: u64 = splice_addr as u64;

    let mut src_sges: Vec<SrcSge> = Vec::new();
    let mut total_bytes: ShmOffset = 0;
    let mut page_id = head;

    while page_id != 0 {
        if page_id < DIRECT_LIMIT {
            // Direct-mode: the page lives at shm_base + page_id.
            let page_off = page_id as usize;
            let page = unsafe { &*((splice_addr + page_off) as *const Page) };
            let used = page.cursor.load(Ordering::Acquire) as u32;
            if used > 0 {
                let data_vaddr = page.data.as_ptr() as u64;
                let lkey = mesh.src_lkey_for_shm(shm_base, data_vaddr, used)?;
                src_sges.push((data_vaddr, used, lkey));
                total_bytes += used as ShmOffset;
            }
            page_id = page.next_offset.load(Ordering::Acquire);
        } else {
            // Paged-mode: page is in GlobalPool, outside SHM.  Resolve
            // host pointer, memcpy the page payload into the sender-side
            // staging MR, emit an SGE into the stage.
            let page_ptr = crate::runtime::extended_pool::runtime::resolve(page_id, splice_addr)
                .map_err(|e| anyhow!("collect_src_sges: resolve paged {:#x}: {}", page_id, e))?;
            let page = unsafe { &*page_ptr };
            let used = page.cursor.load(Ordering::Acquire) as u32;
            if used > 0 {
                let src_bytes = unsafe {
                    std::slice::from_raw_parts(page.data.as_ptr(), used as usize)
                };
                let (stage_addr, stage_lkey) = mesh.src_stage_copy_in(src_bytes)
                    .map_err(|e| anyhow!("collect_src_sges: stage paged page: {}", e))?;
                src_sges.push((stage_addr, used, stage_lkey));
                total_bytes += used as ShmOffset;
            }
            page_id = page.next_offset.load(Ordering::Acquire);
        }
    }
    Ok((src_sges, total_bytes))
}

/// Bump-allocate `n_pages` in SHM, initialise each page's cursor and
/// `next_offset`, and link the head/tail into `slot`.
/// Returns the SHM offset of the first (head) page.
pub(super) fn alloc_and_link(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    total_bytes: usize,
) -> Result<ShmOffset> {
    let n_pages        = (total_bytes + PAGE_DATA - 1) / PAGE_DATA;
    let bytes_to_alloc = (n_pages as ShmOffset) * PAGE_SIZE;

    let sb       = unsafe { &*(splice_addr as *const Superblock) };
    let dest_off = sb.bump_allocator.fetch_add(bytes_to_alloc, Ordering::AcqRel);
    let cap      = sb.global_capacity.load(Ordering::Acquire);
    if dest_off + bytes_to_alloc > cap {
        return Err(anyhow!(
            "SHM capacity exhausted for RDMA receive ({} bytes). \
             Increase INITIAL_SHM_SIZE.", bytes_to_alloc
        ));
    }

    for i in 0..n_pages as ShmOffset {
        let page_off = dest_off + i * PAGE_SIZE;
        let page = unsafe { &mut *((splice_addr + page_off as usize) as *mut Page) };
        let data_in_page = PAGE_DATA.min(total_bytes - i as usize * PAGE_DATA);
        page.cursor.store(data_in_page as ShmOffset, Ordering::Relaxed);
        let next = if i + 1 < n_pages as ShmOffset { dest_off + (i + 1) * PAGE_SIZE } else { 0 };
        page.next_offset.store(next as u64, Ordering::Relaxed);
    }

    let tail_off = dest_off + (n_pages as ShmOffset - 1) * PAGE_SIZE;
    link_to_slot(sb, slot, slot_kind, dest_off, tail_off);
    Ok(dest_off)
}

/// Like `alloc_and_link` but also memcpys `src` into the newly-allocated
/// page-chain data areas.  Used by the MR2 receive path: after RDMA WRITE
/// has landed bytes in an MR2 region (outside the guest's direct window),
/// we pull them back into an SHM page chain so the slot can be read normally.
///
/// Grows SHM via `mesh.ensure_shm_capacity` first if the needed pages would
/// overflow the current `global_capacity`.  Then bumps the allocator and
/// lays down direct-mode pages — never touches the extended-pool paged
/// path — so the resulting PageIds are always `< DIRECT_LIMIT` and both
/// Rust and Python workloads can read the slot without paged-mode
/// resolution.
///
/// Returns the head byte-offset of the new chain (also installed into the
/// slot's `writer_heads` / `io_heads` atomic by `link_to_slot`).
pub(super) fn alloc_and_link_from_buf(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    src:         &[u8],
    mesh:        &MeshNode,
) -> Result<ShmOffset> {
    let total_bytes    = src.len();
    let n_pages        = (total_bytes + PAGE_DATA - 1) / PAGE_DATA;
    if n_pages == 0 { return Ok(0); }
    let bytes_to_alloc = (n_pages as ShmOffset) * PAGE_SIZE;

    let sb = unsafe { &*(splice_addr as *const Superblock) };

    // Reserve the bump range atomically.  If we'd exceed global_capacity,
    // grow SHM first and retry.
    let dest_off = loop {
        let candidate = sb.bump_allocator.fetch_add(bytes_to_alloc, Ordering::AcqRel);
        let cap       = sb.global_capacity.load(Ordering::Acquire);
        if candidate + bytes_to_alloc <= cap {
            break candidate;
        }
        // Roll back and expand.
        sb.bump_allocator.fetch_sub(bytes_to_alloc, Ordering::AcqRel);
        let required = (candidate + bytes_to_alloc) as usize;
        mesh.ensure_shm_capacity(splice_addr, required)
            .map_err(|e| anyhow!("MR2 memcpy-back: expand SHM to {}: {}", required, e))?;
        // Loop: re-contend for the bump; another thread may have allocated
        // in between, but after the expansion there is room for both.
    };

    for i in 0..n_pages {
        let page_off     = dest_off + i as ShmOffset * PAGE_SIZE;
        let page         = unsafe { &mut *((splice_addr + page_off as usize) as *mut Page) };
        let src_start    = i * PAGE_DATA;
        let data_in_page = PAGE_DATA.min(total_bytes - src_start);

        unsafe {
            std::ptr::copy_nonoverlapping(
                src.as_ptr().add(src_start),
                page.data.as_mut_ptr(),
                data_in_page,
            );
        }

        page.cursor.store(data_in_page as ShmOffset, Ordering::Relaxed);
        let next = if i + 1 < n_pages {
            dest_off + (i + 1) as ShmOffset * PAGE_SIZE
        } else { 0 };
        page.next_offset.store(next as u64, Ordering::Relaxed);
    }

    let tail_off = dest_off + (n_pages as ShmOffset - 1) * PAGE_SIZE;
    link_to_slot(sb, slot, slot_kind, dest_off, tail_off);
    Ok(dest_off)
}

/// Store `head_off` and `tail_off` into the head/tail atomics for `slot`.
pub(super) fn link_to_slot(
    sb:       &Superblock,
    slot:     usize,
    kind:     RemoteSlotKind,
    head_off: ShmOffset,
    tail_off: ShmOffset,
) {
    link_to_slot_pageid(sb, slot, kind, head_off as u64, tail_off as u64);
}

/// Like `link_to_slot` but accepts full-width PageIds — needed when the
/// chain may contain paged-mode pages whose IDs are `>= DIRECT_LIMIT` and
/// do not fit in `ShmOffset` (u32).
pub(super) fn link_to_slot_pageid(
    sb:   &Superblock,
    slot: usize,
    kind: RemoteSlotKind,
    head: common::PageId,
    tail: common::PageId,
) {
    match kind {
        RemoteSlotKind::Stream => {
            sb.writer_heads[slot].store(head, Ordering::Release);
            sb.writer_tails[slot].store(tail, Ordering::Release);
        }
        RemoteSlotKind::Io => {
            sb.io_heads[slot].store(head, Ordering::Release);
            sb.io_tails[slot].store(tail, Ordering::Release);
        }
    }
}
