// Sender-initiated (SI) protocol implementation.
//
//   Sender                              Receiver
//   ──────                              ────────
//   TCP send(total_bytes)       ──▶      alloc page chain, set metadata,
//                                       link chain to slot (head/tail)
//                              ◀──      TCP send(dest_off)
//   RDMA write into page chain ──▶      HCA writes chunks into page[i].data
//   TCP send_done              ──▶      worker can now read from slot

use anyhow::Result;
use connect::{SendChannel, RecvChannel};
use connect::rdma::exchange;

use crate::runtime::dag_runner::RemoteSlotKind;

use super::shm::{collect_src_sges, alloc_and_link};
use super::rdma::rdma_write_page_chain;

pub(super) fn send_si(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    ch:          &SendChannel,
) -> Result<()> {
    let (src_sges, total_bytes) = collect_src_sges(splice_addr, slot, slot_kind);
    println!(
        "[RemoteSend-SI] slot {} ({:?}): {} pages / {} bytes",
        slot, slot_kind, src_sges.len(), total_bytes
    );

    // Phase 1: announce size
    exchange::send_shm_offset(&mut *ch.ctrl.lock().unwrap(), total_bytes)?;

    if total_bytes == 0 { return Ok(()); }

    // Phase 2: receive dest_off
    let dest_off = exchange::recv_shm_offset(&mut *ch.ctrl.lock().unwrap())?;

    // Phase 3: RDMA write into receiver's pre-structured page chain
    rdma_write_page_chain(ch, &src_sges, dest_off as u64, total_bytes)?;

    // Phase 4: signal done
    exchange::send_done(&mut *ch.ctrl.lock().unwrap())?;

    Ok(())
}

pub(super) fn recv_si(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    ch:          &RecvChannel,
) -> Result<()> {
    // Phase 1: receive total_bytes
    let total_bytes = exchange::recv_shm_offset(&mut *ch.ctrl.lock().unwrap())? as usize;
    println!(
        "[RemoteRecv-SI] slot {} ({:?}): {} bytes",
        slot, slot_kind, total_bytes
    );

    if total_bytes == 0 { return Ok(()); }

    // Phase 2: alloc page chain, set metadata, link to slot, reply with dest_off
    let dest_off = alloc_and_link(splice_addr, slot, slot_kind, total_bytes)?;
    exchange::send_shm_offset(&mut *ch.ctrl.lock().unwrap(), dest_off)?;

    // Phase 3: wait for RDMA write to complete
    exchange::wait_done(&mut *ch.ctrl.lock().unwrap())?;

    Ok(())
}
