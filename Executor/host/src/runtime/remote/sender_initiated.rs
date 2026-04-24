// Sender-initiated (SI) protocol implementation.
//
// Two destination modes, chosen dynamically by the receiver based on
// whether `total_bytes` fits in the remaining MR1 bump budget:
//
//   SingleMr (MR1 fits):
//     Sender                              Receiver
//     ──────                              ────────
//     TCP send(total_bytes)       ──▶      alloc MR1 page chain, link to slot
//                                 ◀──      TCP send(DestReply::SingleMr)
//     RDMA write into page chain  ──▶      HCA writes chunks into page[i].data
//     TCP send_done               ──▶      worker can now read from slot
//
//   UseMr2 (MR1 would overflow):
//     TCP send(total_bytes)       ──▶      ensure MR2 registered, reserve region
//                                 ◀──      TCP send(DestReply::UseMr2{addr,rkey,..})
//     RDMA write into MR2         ──▶      HCA writes bytes into a flat MR2 buffer
//     TCP send_done               ──▶      host memcpys MR2 → new MR1 page chain,
//                                          links chain into the target slot

use std::sync::atomic::Ordering;

use anyhow::Result;
use connect::{MeshNode, SendChannel, RecvChannel};
use connect::rdma::exchange;
use connect::rdma::exchange::DestReply;

use crate::runtime::dag_runner::RemoteSlotKind;

use super::shm::{collect_src_sges, alloc_and_link, alloc_and_link_from_buf};
use super::rdma::{rdma_write_page_chain, rdma_write_flat_to};

/// Return how many bytes of MR1 bump capacity are still free.
///
/// Computed against `INITIAL_SHM_SIZE` (the registered MR1 length), not
/// `global_capacity` — MR1 is a fixed-size NIC registration, so bytes past
/// `INITIAL_SHM_SIZE` are unreachable by RDMA even if SHM has expanded.
fn mr1_bump_remaining(splice_addr: usize) -> u64 {
    let sb    = unsafe { &*(splice_addr as *const common::Superblock) };
    let bump  = sb.bump_allocator.load(Ordering::Acquire);
    let mr1   = common::INITIAL_SHM_SIZE;
    if bump >= mr1 { 0 } else { (mr1 - bump) as u64 }
}

pub(super) fn send_si(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    ch:          &SendChannel,
    mesh:        &MeshNode,
) -> Result<()> {
    // Idle-timeout shrink for sender-side extension MRs.
    mesh.src_ext_try_shrink_idle();
    mesh.src_stage_try_shrink_idle();

    let (src_sges, total_bytes) = collect_src_sges(splice_addr, slot, slot_kind, mesh)?;
    println!(
        "[RemoteSend-SI] slot {} ({:?}): {} pages / {} bytes",
        slot, slot_kind, src_sges.len(), total_bytes
    );

    // Phase 1: announce size
    exchange::send_shm_offset(&mut *ch.ctrl.lock().unwrap(), total_bytes)?;

    if total_bytes == 0 {
        mesh.src_stage_reset();
        return Ok(());
    }

    // Phase 2: receive destination spec
    let reply = exchange::recv_dest_reply(&mut *ch.ctrl.lock().unwrap())?;

    // Phase 3: RDMA write to the receiver's allocation
    match reply {
        DestReply::SingleMr { dest_off } => {
            rdma_write_page_chain(ch, &src_sges, dest_off as u64, total_bytes)?;
        }
        DestReply::UseMr2 { dest_off: _, addr, rkey } => {
            // Peer's receive overflowed its MR1 and routed to MR2.  The
            // `addr` already includes the peer's MR2 base plus its per-
            // transfer offset, so we write as a flat contiguous blob.
            // The peer host memcpys MR2 → MR1 page chain after wait_done.
            println!(
                "[RemoteSend-SI] slot {}: routing {} bytes to peer MR2 addr={:#x} rkey={:#x}",
                slot, total_bytes, addr, rkey,
            );
            rdma_write_flat_to(ch, &src_sges, addr, rkey, total_bytes as u64)?;
        }
    }

    // Phase 4: signal done
    exchange::send_done(&mut *ch.ctrl.lock().unwrap())?;

    // All SGEs have been consumed by the NIC; reset staging bump so the
    // next transfer starts fresh.
    mesh.src_stage_reset();

    Ok(())
}

pub(super) fn recv_si(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    ch:          &RecvChannel,
    mesh:        &MeshNode,
) -> Result<()> {
    // Idle-timeout shrink: tear down MR2 if it's been unused long enough.
    mesh.mr2_try_shrink_idle();

    // Phase 1: receive total_bytes
    let total_bytes = exchange::recv_shm_offset(&mut *ch.ctrl.lock().unwrap())? as usize;
    println!(
        "[RemoteRecv-SI] slot {} ({:?}): {} bytes",
        slot, slot_kind, total_bytes
    );

    if total_bytes == 0 { return Ok(()); }

    let remaining = mr1_bump_remaining(splice_addr);
    if (total_bytes as u64) <= remaining {
        // MR1 fits — unchanged path.
        let dest_off = alloc_and_link(splice_addr, slot, slot_kind, total_bytes)?;
        exchange::send_dest_reply(
            &mut *ch.ctrl.lock().unwrap(),
            &DestReply::SingleMr { dest_off },
        )?;
        exchange::wait_done(&mut *ch.ctrl.lock().unwrap())?;
        return Ok(());
    }

    // MR1 overflow — route through MR2.
    println!(
        "[RemoteRecv-SI] slot {}: {} bytes > MR1 remaining {} — routing to MR2",
        slot, total_bytes, remaining
    );
    mesh.ensure_mr2(total_bytes)?;
    let reservation = mesh.mr2_reserve(total_bytes as u64)?;
    mesh.mr2_touch();

    exchange::send_dest_reply(
        &mut *ch.ctrl.lock().unwrap(),
        &DestReply::UseMr2 {
            dest_off: reservation.offset,
            addr:     reservation.addr,
            rkey:     reservation.rkey,
        },
    )?;

    exchange::wait_done(&mut *ch.ctrl.lock().unwrap())?;

    // Bytes are sitting in MR2.  Memcpy out to a fresh MR1 page chain and
    // link that chain into the target slot so guest consumers see the data
    // through the normal page-chain API.
    let src_slice = unsafe { reservation.as_slice() };
    alloc_and_link_from_buf(splice_addr, slot, slot_kind, src_slice)?;
    mesh.mr2_touch();

    Ok(())
}
