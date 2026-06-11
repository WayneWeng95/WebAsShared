//! Host-side scatter/gather for widened `StreamPipeline` stages.
//!
//! A stage with `width > 1` runs `W` persistent workers in parallel.  Because a
//! stream slot is single-owner — `read_next_*` (load-then-`fetch_add`) and chain
//! append (load-modify-store on the tail) are NOT safe for concurrent readers or
//! writers of the same slot — the workers cannot share the stage's input/output
//! slot.  Instead, each tick the host:
//!
//!   1. reads the stage's per-tick input batch from `arg0`,
//!   2. SCATTERS those records round-robin into `W` private input sub-slots,
//!   3. runs the `W` workers (each reads its own sub-slot, writes its own
//!      output sub-slot — single-owner, so the proven cursor/append are safe),
//!   4. GATHERS the `W` output sub-slots back into the stage's real output slot
//!      in round-robin record order.
//!
//! Sub-slots come from a reserved high band (`SUB_SLOT_BASE..STREAM_SLOT_COUNT`)
//! and are freed every tick, so they never collide with user DAG slots (which
//! are numbered low) and don't leak.

use anyhow::{anyhow, Result};
use std::sync::atomic::Ordering;

use common::{Page, PageId, PAGE_DATA_SIZE, ShmOffset, Superblock};

use crate::runtime::extended_pool;
use crate::runtime::mem_operation::reclaimer;
use crate::runtime::input_output::persistence::read_stream_records;

/// First stream slot of the reserved scatter/gather band.  User DAG slots are
/// numbered low (tens to low hundreds); this band sits just under
/// `STREAM_SLOT_COUNT` (2048) so the two never overlap.
pub(super) const SUB_SLOT_BASE: usize = 1792;
/// Hard cap on a stage's width (bounds the reserved band usage).
pub(super) const MAX_STAGE_WIDTH: usize = 16;

/// Input sub-slot for (stage `s`, worker `w`).  Layout: `[SUB_SLOT_BASE .. +128)`
/// for inputs, `[+128 .. +256)` for outputs — fits `depth ≤ 8`, `width ≤ 16`.
pub(super) fn sub_in_slot(s: usize, w: usize) -> usize {
    SUB_SLOT_BASE + s * MAX_STAGE_WIDTH + w
}
/// Output sub-slot for (stage `s`, worker `w`).
pub(super) fn sub_out_slot(s: usize, w: usize) -> usize {
    SUB_SLOT_BASE + 128 + s * MAX_STAGE_WIDTH + w
}

/// Returns `true` if every (stage, worker) sub-slot pair fits the reserved band.
pub(super) fn fits_band(depth: usize, max_width: usize) -> bool {
    max_width <= MAX_STAGE_WIDTH && depth * MAX_STAGE_WIDTH <= 128
        && SUB_SLOT_BASE + 256 <= common::STREAM_SLOT_COUNT
}

/// Append one length-prefixed record (`len | origin | payload`, matching the
/// guest's `append_stream_data` framing) to stream `slot`'s page chain.
pub(super) fn append_stream_record(
    splice_addr: usize,
    slot: usize,
    origin: u32,
    payload: &[u8],
) -> Result<()> {
    write_stream_bytes(splice_addr, slot, &(payload.len() as u32).to_le_bytes())?;
    write_stream_bytes(splice_addr, slot, &origin.to_le_bytes())?;
    write_stream_bytes(splice_addr, slot, payload)
}

fn write_stream_bytes(splice_addr: usize, slot: usize, mut data: &[u8]) -> Result<()> {
    if data.is_empty() { return Ok(()); }
    let sb = unsafe { &*(splice_addr as *const Superblock) };

    let mut tail: PageId = sb.writer_tails[slot].load(Ordering::Acquire);
    if tail == 0 {
        tail = reclaimer::alloc_page(splice_addr).map_err(|e| anyhow!("stage_fanout alloc: {e}"))?;
        sb.writer_heads[slot].store(tail, Ordering::Release);
        sb.writer_tails[slot].store(tail, Ordering::Release);
    }

    while !data.is_empty() {
        let page = unsafe { &mut *page_ptr(tail, splice_addr)? };
        let cursor = page.cursor.load(Ordering::Relaxed) as usize;
        let space = PAGE_DATA_SIZE.saturating_sub(cursor);
        if space == 0 {
            let next = reclaimer::alloc_page(splice_addr).map_err(|e| anyhow!("stage_fanout alloc: {e}"))?;
            page.next_offset.store(next, Ordering::Release);
            sb.writer_tails[slot].store(next, Ordering::Release);
            tail = next;
            continue;
        }
        let n = space.min(data.len());
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), page.data.as_mut_ptr().add(cursor), n);
        }
        page.cursor.store((cursor + n) as ShmOffset, Ordering::Release);
        data = &data[n..];
    }
    Ok(())
}

fn page_ptr(id: PageId, splice_addr: usize) -> Result<*mut Page> {
    extended_pool::runtime::resolve(id, splice_addr).map_err(|e| anyhow!("stage_fanout resolve {id:#x}: {e}"))
}

/// Read all records currently committed in stream `slot`.
pub(super) fn read_slot_records(splice_addr: usize, slot: usize) -> Vec<(u32, Vec<u8>)> {
    let sb = unsafe { &*(splice_addr as *const Superblock) };
    read_stream_records(splice_addr, sb, slot)
}

/// Free a sub-slot's page chain and reset its head/tail to 0 (so the next tick
/// starts from an empty chain).
pub(super) fn free_sub_slot(splice_addr: usize, slot: usize) {
    reclaimer::free_stream_slot(splice_addr, slot);
}
