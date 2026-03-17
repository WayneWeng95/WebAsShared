// RemoteSend / RemoteRecv: selective SHM slot sharing between mesh nodes via RDMA.
//
// Architecture
// ─────────────
// Both nodes register the full SHM as their RDMA Memory Region (via
// `MeshNode::connect_all_on_shm`).  A small *staging header* (12 bytes) lives
// at the start of each peer's staging area; the bulk page data is DMA'd
// directly from the source slot pages — no CPU copy of the payload.
//
// Staging area layout
// ───────────────────
// The very first pages allocated from the bump allocator (before any normal
// DAG activity) are reserved as staging pages.  Because both nodes start with
// identical SHM state (format_shared_memory → same BUMP_ALLOCATOR_START) and
// pre-allocate the same number of pages in the same order, the staging page
// offsets are identical on every machine.
//
//   [BUMP_ALLOCATOR_START + peer_id * STAGE_BYTES_PER_PEER]
//       ← staging area for data FROM node peer_id
//
//   Staging area header (12 bytes):
//     u64 ready_counter  (LE) — 0 initially; RDMA FAA sets to 1 when data arrives
//     u32 total_bytes    (LE) — number of raw content bytes that follow
//   Followed by raw page-chain content (written directly by the HCA via
//   scatter-gather; no sender-CPU involvement for the bulk data):
//     Concatenated page.data[..cursor] bytes from the source slot's page chain
//     (same byte format the WASM guest uses — no re-serialisation needed)
//
// Transfer protocol
// ──────────────────
// RemoteSend (sender side):
//   1. Zero the ready counter at staging[0..8]; write total_bytes to [8..12].
//      (The source slot is sealed at this point — its producer is a DAG dep
//      of this node, so all writes are complete and the page chain is stable.)
//   2. Build an SGE list: SGE[0] = header (12 bytes from local staging),
//      SGE[1..N] = each source page's data field (page.data[..cursor]).
//   3. RDMA WRITE with scatter-gather (MeshNode::rdma_write_sge).
//      HCA gathers from the source pages and writes them contiguously into
//      peer's staging[0+] — zero CPU copy of the payload.
//      CQ poll inside rdma_write_sge blocks until the DMA is acknowledged.
//   4. RDMA FAA on peer's staging[0..8] counter → wakes the receiver atomically
//      (MeshNode::rdma_signal_staging).  TCP 1-byte sent as backup.
//
//   Consistency guarantee:
//     • DAG ordering ensures the slot producer has finished before RemoteSend
//       runs.  The page chain is immutable for the duration of the RDMA.
//     • The CQ poll inside rdma_write_sge ensures all DMA reads from source
//       pages are complete before we signal the peer and before the DAG runner
//       can reclaim those pages.
//
//   Fallback:
//     If the page count exceeds MAX_SEND_SGE-1 (header takes one slot), the
//     sender falls back to assembling a local staging copy and using a single
//     RDMA WRITE (rdma_write_staging) for the whole buffer.
//
// RemoteRecv (receiver side):
//   1. Spin-poll staging[0..8] until non-zero; TCP fallback after 100 ms
//      (MeshNode::wait_staging).
//   2. Acquire fence ensures RDMA-written bytes are visible.
//   3. Read total_bytes from staging[8..12].
//   4. Append raw bytes from staging[12+] directly to the target SHM slot
//      (write_bytes) — same byte format, no deserialisation.
//
// Pre-allocation (called from run_dag before mesh setup, only when transfer=true):
//   pub fn pre_alloc_staging(splice_addr, total) — allocates
//   total * STAGE_PAGES_PER_PEER pages, advancing the bump pointer past
//   the staging area so normal allocations never collide with it.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Result};
use common::{BUMP_ALLOCATOR_START, Page, Superblock};
use connect::MeshNode;

use crate::runtime::dag_runner::RemoteSlotKind;
use crate::runtime::mem_operation::reclaimer;

// ── Staging constants ─────────────────────────────────────────────────────────

/// Pages reserved per (sender → any_receiver) direction.
/// 256 pages × 4 KiB = 1 MiB per peer.  Increase if slot payloads are larger.
pub const STAGE_PAGES_PER_PEER: usize = 256;
pub const STAGE_BYTES_PER_PEER: usize = STAGE_PAGES_PER_PEER * 4096;

/// Byte offset within the SHM where the staging area for `peer_id` starts.
#[inline]
fn staging_offset(peer_id: usize) -> usize {
    BUMP_ALLOCATOR_START as usize + peer_id * STAGE_BYTES_PER_PEER
}

// ── Public setup helper ───────────────────────────────────────────────────────

/// Pre-allocate the staging pages from the SHM bump allocator.
///
/// Must be called once, before any other SHM allocation and before DAG nodes
/// execute, with the **same** `total` value on every machine.  Because all
/// machines start from an identical `format_shared_memory` state, each machine
/// will allocate the staging pages at the **same** SHM byte offsets.
pub fn pre_alloc_staging(splice_addr: usize, total: usize) -> Result<()> {
    let pages = total * STAGE_PAGES_PER_PEER;
    for _ in 0..pages {
        reclaimer::alloc_page(splice_addr)
            .map_err(|e| anyhow!("staging pre-alloc failed: {}", e))?;
    }
    println!(
        "[remote] reserved {} staging pages ({} MiB) for {} peers",
        pages,
        pages * 4096 / (1024 * 1024),
        total,
    );
    Ok(())
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Walk `slot`'s page chain, zero-copy RDMA-WRITE the data into `peer`'s
/// staging area via scatter-gather, then signal readiness via RDMA FAA.
///
/// The HCA gathers from the source page chain directly — no CPU copy of the
/// bulk payload.  Only the 12-byte staging header is written locally first.
///
/// **Consistency requirement**: the slot's producer node must be listed as a
/// DAG dependency of this `RemoteSend` node.  That guarantees the page chain
/// is fully written and immutable when this function runs.  The CQ poll inside
/// `rdma_write_sge` / `rdma_write_staging` ensures the DMA is complete before
/// we signal the peer and before the DAG runner can reclaim those pages.
pub fn execute_remote_send(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    peer:        usize,
    mesh:        &mut MeshNode,
) -> Result<()> {
    let stage_off  = staging_offset(mesh.id);
    let stage_base = splice_addr + stage_off;
    let mr_base    = mesh.mr_addr();   // virtual base of the SHM MR

    // ── 1. Prepare header in local staging (12 bytes, CPU-written) ───────────
    // SAFETY: staging area is valid SHM memory pre-allocated by pre_alloc_staging.
    unsafe { std::ptr::write(stage_base as *mut u64, 0u64) }; // counter = 0

    // ── 2. Walk source page chain, collect SGE pairs (no data copy) ───────────
    // The slot's page chain is sealed: its producer is a dep of this node, so
    // all writes completed before this wave.  We hold an Acquire fence on each
    // page cursor to ensure visibility across cores.
    let sb = unsafe { &*(splice_addr as *const Superblock) };
    let head = match slot_kind {
        RemoteSlotKind::Stream => sb.writer_heads[slot].load(Ordering::Acquire),
        RemoteSlotKind::Io    => sb.io_heads[slot].load(Ordering::Acquire),
    };

    // Build SGE list: SGE[0] = 12-byte header from local staging,
    // SGE[1..N] = each source page's data field (direct, no copy).
    // rdma_write_sge chunks this into MAX_SEND_SGE-sized WRs automatically.
    let mut page_sges: Vec<(u64, u32)> = Vec::new();
    let mut total_bytes: u32 = 0;
    let mut page_off = head;

    while page_off != 0 {
        let page = unsafe { &*((splice_addr + page_off as usize) as *const Page) };
        let used = page.cursor.load(Ordering::Acquire) as u32;

        if used > 0 {
            if 12 + total_bytes as usize + used as usize > STAGE_BYTES_PER_PEER {
                return Err(anyhow!(
                    "RemoteSend: slot {} data ({} bytes) exceeds staging budget \
                     ({} bytes).  Increase STAGE_PAGES_PER_PEER.",
                    slot, 12 + total_bytes + used, STAGE_BYTES_PER_PEER
                ));
            }
            // Virtual address of page.data[] within the registered MR.
            let data_vaddr = unsafe { page.data.as_ptr() } as u64;
            page_sges.push((data_vaddr, used));
            total_bytes += used;
        }
        page_off = page.next_offset.load(Ordering::Acquire);
    }

    // Write total_bytes into local header now that we know it.
    unsafe { std::ptr::write((stage_base + 8) as *mut u32, total_bytes.to_le()) };

    println!(
        "[RemoteSend] slot {} ({:?}): {} pages / {} bytes → peer {}",
        slot, slot_kind, page_sges.len(), total_bytes, peer
    );

    // ── 3. Scatter-gather RDMA WRITE (zero CPU copy of payload) ──────────────
    // Header SGE first, then all page data SGEs.  rdma_write_sge chunks
    // into multiple work requests if needed — no page count ceiling.
    let mut sges: Vec<(u64, u32)> = Vec::with_capacity(page_sges.len() + 1);
    sges.push((mr_base + stage_off as u64, 12)); // header
    sges.extend_from_slice(&page_sges);
    mesh.rdma_write_sge(peer, stage_off as u64, &sges)?;

    // ── 4. RDMA FAA signal (+ TCP backup) ────────────────────────────────────
    mesh.rdma_signal_staging(peer, stage_off as u64)?;

    Ok(())
}

/// Wait for `peer`'s RDMA WRITE and FAA signal, then append the raw staging
/// bytes directly to `slot`'s page chain — no deserialisation needed.
pub fn execute_remote_recv(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    peer:        usize,
    mesh:        &mut MeshNode,
) -> Result<()> {
    let stage_off  = staging_offset(peer);
    let stage_base = splice_addr + stage_off;

    // ── 1. Spin-poll ready counter (RDMA FAA primary, TCP fallback) ───────────
    // SAFETY: staging area was pre-allocated and is valid for the lifetime of mesh.
    let counter_ptr = stage_base as *const AtomicU64;
    mesh.wait_staging(peer, counter_ptr)?;

    // ── 2. Read total_bytes header ────────────────────────────────────────────
    let total_bytes = unsafe {
        u32::from_le(std::ptr::read((stage_base + 8) as *const u32))
    } as usize;

    println!(
        "[RemoteRecv] slot {} ({:?}): {} bytes ← peer {}",
        slot, slot_kind, total_bytes, peer
    );

    if total_bytes == 0 { return Ok(()); }

    // ── 3. Append raw bytes to target slot (same format, no deserialisation) ──
    let data = unsafe {
        std::slice::from_raw_parts((stage_base + 12) as *const u8, total_bytes)
    };
    write_bytes(splice_addr, slot, slot_kind, data)?;

    Ok(())
}

// ── SHM write helper ──────────────────────────────────────────────────────────

/// Write raw bytes into `slot`'s page chain, allocating new pages as needed.
fn write_bytes(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    mut data:    &[u8],
) -> Result<()> {
    if data.is_empty() { return Ok(()); }

    let sb = unsafe { &*(splice_addr as *const Superblock) };

    let mut tail = match slot_kind {
        RemoteSlotKind::Stream => sb.writer_tails[slot].load(Ordering::Acquire),
        RemoteSlotKind::Io    => sb.io_tails[slot].load(Ordering::Acquire),
    };
    if tail == 0 {
        tail = reclaimer::alloc_page(splice_addr)
            .map_err(|e| anyhow!("RemoteRecv alloc: {}", e))?;
        match slot_kind {
            RemoteSlotKind::Stream => {
                sb.writer_heads[slot].store(tail, Ordering::Release);
                sb.writer_tails[slot].store(tail, Ordering::Release);
            }
            RemoteSlotKind::Io => {
                sb.io_heads[slot].store(tail, Ordering::Release);
                sb.io_tails[slot].store(tail, Ordering::Release);
            }
        }
    }

    while !data.is_empty() {
        let page = unsafe { &mut *((splice_addr + tail as usize) as *mut Page) };
        let cursor = page.cursor.load(Ordering::Relaxed) as usize;
        let space  = 4088usize.saturating_sub(cursor);

        if space == 0 {
            let next = reclaimer::alloc_page(splice_addr)
                .map_err(|e| anyhow!("RemoteRecv alloc: {}", e))?;
            page.next_offset.store(next, Ordering::Release);
            match slot_kind {
                RemoteSlotKind::Stream => sb.writer_tails[slot].store(next, Ordering::Release),
                RemoteSlotKind::Io    => sb.io_tails[slot].store(next, Ordering::Release),
            }
            tail = next;
            continue;
        }

        let n = space.min(data.len());
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                page.data.as_mut_ptr().add(cursor),
                n,
            );
        }
        page.cursor.store((cursor + n) as u32, Ordering::Release);
        data = &data[n..];
    }
    Ok(())
}
