// RDMA write helpers — transfer source SGEs to a remote peer's SHM.
//
// Two write modes:
//   page_chain — SI protocol: writes directly into a pre-structured page chain,
//                splitting source data across 4088-byte dest page boundaries.
//   flat       — RI protocol: writes contiguous raw bytes into a flat buffer;
//                the receiver structures the page chain in-place afterward.

use anyhow::Result;
use connect::SendChannel;
use connect::ffi::ibv_sge;
use connect::rdma::queue_pair::MAX_SEND_SGE;
use common::PAGE_SIZE;

use super::PAGE_DATA;

/// RDMA WRITE source SGEs into a pre-structured remote page chain (SI).
///
/// The source data is sliced to respect 4088-byte dest page boundaries and
/// the 8-byte per-page header offset.  Uses a `Vec<ibv_sge>` per dest page
/// so any number of source page fragments are handled correctly.
pub(super) fn rdma_write_page_chain(
    ch:              &SendChannel,
    src_sges:        &[(u64, u32)],
    remote_dest_off: u64,
    total_bytes:     u32,
) -> Result<()> {
    const PAGE_SIZE_U64: u64 = PAGE_SIZE as u64;
    const PAGE_HEADER:   u64 = 8;
    const PAGE_DATA_U32: u32 = PAGE_DATA as u32;

    let n_dest_pages = (total_bytes + PAGE_DATA_U32 - 1) / PAGE_DATA_U32;
    let remote_rkey  = ch.remote_rkey;
    let lkey         = ch.lkey;

    let mut src_iter   = src_sges.iter();
    let mut src_addr   = 0u64;
    let mut src_remain = 0u32;
    if let Some(&(a, l)) = src_iter.next() { src_addr = a; src_remain = l; }

    let qp = ch.qp.lock().unwrap();
    for dest_idx in 0..n_dest_pages {
        let dest_chunk  = PAGE_DATA_U32.min(total_bytes - dest_idx * PAGE_DATA_U32);
        let remote_addr = ch.remote_mr_base
            + remote_dest_off
            + dest_idx as u64 * PAGE_SIZE_U64
            + PAGE_HEADER;
        let is_last = dest_idx == n_dest_pages - 1;

        let mut sges: Vec<ibv_sge> = Vec::new();
        let mut to_fill = dest_chunk;

        while to_fill > 0 {
            if src_remain == 0 {
                if let Some(&(a, l)) = src_iter.next() { src_addr = a; src_remain = l; }
                else { break; }
            }
            let take = to_fill.min(src_remain);
            let mut sge: ibv_sge = unsafe { std::mem::zeroed() };
            sge.addr   = src_addr;
            sge.length = take;
            sge.lkey   = lkey;
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

/// RDMA WRITE source SGEs into a flat remote buffer (RI).
///
/// Chunks the SGE list into `MAX_SEND_SGE`-sized batches and advances the
/// remote cursor after each batch.  The receiver structures the page chain
/// in-place once it receives the `total_bytes` done signal.
pub(super) fn rdma_write_flat(
    ch:              &SendChannel,
    src_sges:        &[(u64, u32)],
    remote_dest_off: u64,
    total_bytes:     u32,
) -> Result<()> {
    let remote_rkey   = ch.remote_rkey;
    let lkey          = ch.lkey;
    let remote_base   = ch.remote_mr_base + remote_dest_off;

    let chunks: Vec<&[(u64, u32)]> = src_sges.chunks(MAX_SEND_SGE).collect();
    let n_chunks      = chunks.len();
    let mut remote_cursor = remote_base;

    let qp = ch.qp.lock().unwrap();
    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i == n_chunks - 1;
        let mut sges: Vec<ibv_sge> = chunk.iter().map(|&(addr, len)| {
            let mut s: ibv_sge = unsafe { std::mem::zeroed() };
            s.addr   = addr;
            s.length = len;
            s.lkey   = lkey;
            s
        }).collect();
        qp.post_rdma_write_sge_list(&mut sges, remote_cursor, remote_rkey, is_last)?;
        remote_cursor += chunk.iter().map(|&(_, l)| l as u64).sum::<u64>();
    }
    qp.poll_one_blocking()?;
    println!("[remote] RDMA flat write {} bytes done", total_bytes);
    Ok(())
}
