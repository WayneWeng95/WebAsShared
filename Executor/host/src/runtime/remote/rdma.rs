// RDMA write helpers — transfer source SGEs to a remote peer's SHM.
//
// Two write modes:
//   page_chain — SI protocol: writes directly into a pre-structured page chain,
//                splitting source data across 4088-byte dest page boundaries.
//   flat       — RI protocol / MR2 destination: writes contiguous raw bytes
//                into a flat buffer; the receiver structures the page chain
//                in-place (RI) or memcpys into MR1 pages afterward (MR2).
//
// Each source SGE carries its own lkey so a single WR can reference any mix of
// MR1, MR-src-ext, and MR-src-stage on the sender side.  The destination side
// always targets a single MR per WR (single remote rkey) — that's an ibverbs
// invariant we respect by ensuring MR1 destinations and MR2 destinations are
// chosen whole-transfer, not per-SGE.

use anyhow::Result;
use connect::SendChannel;
use connect::ffi::ibv_sge;
use connect::rdma::queue_pair::MAX_SEND_SGE;
use common::{PAGE_SIZE, ShmOffset};

use super::PAGE_DATA;
use super::shm::SrcSge;

/// RDMA WRITE source SGEs into a pre-structured remote page chain (SI).
///
/// The source data is sliced to respect 4088-byte dest page boundaries and
/// the 8-byte per-page header offset.  Uses a `Vec<ibv_sge>` per dest page
/// so any number of source page fragments are handled correctly.  Each
/// source SGE carries its own lkey, allowing the source to span MR1 and
/// the sender-side extension / staging MRs.
pub(super) fn rdma_write_page_chain(
    ch:              &SendChannel,
    src_sges:        &[SrcSge],
    remote_dest_off: u64,
    total_bytes:     ShmOffset,
) -> Result<()> {
    let page_size_u64: u64 = PAGE_SIZE as u64;
    let page_header:   u64 = common::PAGE_HEADER_SIZE as u64;
    let page_data_shm: ShmOffset = PAGE_DATA as ShmOffset;

    let n_dest_pages = (total_bytes + page_data_shm - 1) / page_data_shm;
    let remote_rkey  = ch.remote_rkey;

    let mut src_iter   = src_sges.iter();
    let mut src_addr   = 0u64;
    let mut src_remain = 0u32;
    let mut src_lkey   = 0u32;
    if let Some(&(a, l, k)) = src_iter.next() { src_addr = a; src_remain = l; src_lkey = k; }

    let qp = ch.qp.lock().unwrap();
    for dest_idx in 0..n_dest_pages {
        let dest_chunk  = page_data_shm.min(total_bytes - dest_idx * page_data_shm) as u32;
        let remote_addr = ch.remote_mr_base
            + remote_dest_off
            + dest_idx as u64 * page_size_u64
            + page_header;
        let is_last = dest_idx == n_dest_pages - 1;

        let mut sges: Vec<ibv_sge> = Vec::new();
        let mut to_fill = dest_chunk;

        while to_fill > 0 {
            if src_remain == 0 {
                if let Some(&(a, l, k)) = src_iter.next() {
                    src_addr = a; src_remain = l; src_lkey = k;
                } else { break; }
            }
            let take = to_fill.min(src_remain);
            let mut sge: ibv_sge = unsafe { std::mem::zeroed() };
            sge.addr   = src_addr;
            sge.length = take;
            sge.lkey   = src_lkey;
            sges.push(sge);
            src_addr   += take as u64;
            src_remain -= take;
            to_fill    -= take;
        }

        qp.post_rdma_write_sge_list(&mut sges, remote_addr, remote_rkey, is_last)?;
    }
    qp.poll_one_blocking()?;
    println!("[remote] RDMA page-chain write {} bytes done", total_bytes);
    Ok(())
}

/// RDMA WRITE source SGEs into a flat remote buffer at
/// `(remote_mr_base + remote_dest_off, remote_rkey)` (MR1 destination).
pub(super) fn rdma_write_flat(
    ch:              &SendChannel,
    src_sges:        &[SrcSge],
    remote_dest_off: u64,
    total_bytes:     ShmOffset,
) -> Result<()> {
    let remote_base = ch.remote_mr_base + remote_dest_off;
    rdma_write_flat_to(ch, src_sges, remote_base, ch.remote_rkey, total_bytes as u64)
}

/// RDMA WRITE source SGEs into a flat remote buffer at an explicit
/// `(remote_base_addr, remote_rkey)` target.  Used by the MR2 receive
/// path (peer's MR2 has a different rkey than its MR1) and by RI.
pub(super) fn rdma_write_flat_to(
    ch:               &SendChannel,
    src_sges:         &[SrcSge],
    remote_base_addr: u64,
    remote_rkey:      u32,
    total_bytes:      u64,
) -> Result<()> {
    let chunks: Vec<&[SrcSge]> = src_sges.chunks(MAX_SEND_SGE).collect();
    let n_chunks          = chunks.len();
    let mut remote_cursor = remote_base_addr;

    let qp = ch.qp.lock().unwrap();
    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i == n_chunks - 1;
        let mut sges: Vec<ibv_sge> = chunk.iter().map(|&(addr, len, lkey)| {
            let mut s: ibv_sge = unsafe { std::mem::zeroed() };
            s.addr   = addr;
            s.length = len;
            s.lkey   = lkey;
            s
        }).collect();
        qp.post_rdma_write_sge_list(&mut sges, remote_cursor, remote_rkey, is_last)?;
        remote_cursor += chunk.iter().map(|&(_, l, _)| l as u64).sum::<u64>();
    }
    qp.poll_one_blocking()?;
    println!("[remote] RDMA flat write {} bytes done → {:#x}", total_bytes, remote_base_addr);
    Ok(())
}
