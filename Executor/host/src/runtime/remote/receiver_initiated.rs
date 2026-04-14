// Receiver-initiated (RI) protocol implementation.
//
//   Receiver                            Sender
//   ────────                            ──────
//   TCP send(dest_off, avail_cap) ──▶   recv (dest_off, avail_cap)
//                                       RDMA write to flat buffer at dest_off
//                                       TCP send(total_bytes)         ──▶
//                              ◀──      recv total_bytes
//   set page chain metadata in-place
//   link chain to slot
//
// RI avoids deadlock in bidirectional (shuffle) DAGs: both sides send their
// own announcement to their OWN ctrl stream without waiting for the peer.

use std::sync::atomic::Ordering;

use anyhow::{anyhow, Result};
use connect::{SendChannel, RecvChannel};
use connect::rdma::exchange;
use common::{PAGE_SIZE, Page, ShmOffset, Superblock};

use crate::runtime::dag_runner::RemoteSlotKind;

use super::PAGE_DATA;
use super::shm::{collect_src_sges, link_to_slot};
use super::rdma::rdma_write_flat;

pub(super) fn send_ri(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    ch:          &SendChannel,
) -> Result<()> {
    let (src_sges, total_bytes) = collect_src_sges(splice_addr, slot, slot_kind);
    println!(
        "[RemoteSend-RI] slot {} ({:?}): {} pages / {} bytes",
        slot, slot_kind, src_sges.len(), total_bytes
    );

    // Phase 1: receive receiver's pre-announcement (dest_off + avail_cap)
    let dest_off  = exchange::recv_shm_offset(&mut *ch.ctrl.lock().unwrap())?;
    let avail_cap = exchange::recv_shm_offset(&mut *ch.ctrl.lock().unwrap())?;

    if total_bytes > avail_cap {
        return Err(anyhow!(
            "[RemoteSend-RI] transfer size {} exceeds receiver capacity {}",
            total_bytes, avail_cap
        ));
    }

    if total_bytes == 0 {
        // Receiver is waiting for our total_bytes signal.
        exchange::send_shm_offset(&mut *ch.ctrl.lock().unwrap(), 0)?;
        return Ok(());
    }

    // Phase 2: RDMA write into flat buffer at dest_off.
    // Receiver will structure the page chain in-place after we signal.
    rdma_write_flat(ch, &src_sges, dest_off as u64, total_bytes)?;

    // Phase 3: send total_bytes as the done signal
    exchange::send_shm_offset(&mut *ch.ctrl.lock().unwrap(), total_bytes)?;

    Ok(())
}

pub(super) fn recv_ri(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    ch:          &RecvChannel,
) -> Result<()> {
    let sb = unsafe { &*(splice_addr as *const Superblock) };

    // Phase 1: announce dest_off (current bump pointer) and remaining capacity.
    // We don't know total_bytes yet; the sender will check it fits.
    let dest_off  = sb.bump_allocator.load(Ordering::Acquire);
    let cap       = sb.global_capacity.load(Ordering::Acquire);
    let avail_cap = cap.saturating_sub(dest_off);

    println!(
        "[RemoteRecv-RI] slot {} ({:?}): announcing dest_off={} avail_cap={}",
        slot, slot_kind, dest_off, avail_cap
    );

    exchange::send_shm_offset(&mut *ch.ctrl.lock().unwrap(), dest_off)?;
    exchange::send_shm_offset(&mut *ch.ctrl.lock().unwrap(), avail_cap)?;

    // Phase 2: wait for sender's total_bytes (which also signals RDMA done)
    let total_bytes = exchange::recv_shm_offset(&mut *ch.ctrl.lock().unwrap())? as usize;
    println!(
        "[RemoteRecv-RI] slot {} ({:?}): {} bytes received",
        slot, slot_kind, total_bytes
    );

    if total_bytes == 0 { return Ok(()); }

    // Reserve the pages we announced.  A properly-ordered DAG ensures no other
    // allocator runs between our load and this fetch_add for the same wave.
    let n_pages        = (total_bytes + PAGE_DATA - 1) / PAGE_DATA;
    let bytes_to_alloc = (n_pages as ShmOffset) * PAGE_SIZE;
    let reserved       = sb.bump_allocator.fetch_add(bytes_to_alloc, Ordering::AcqRel);

    if reserved != dest_off {
        // Another allocator ran between our announce and our reserve.
        // In a correctly-ordered DAG this should never happen.
        return Err(anyhow!(
            "[RemoteRecv-RI] bump moved between announce ({}) and reserve ({}) — \
             check DAG ordering to prevent concurrent SHM allocation during RI recv",
            dest_off, reserved
        ));
    }
    if dest_off + bytes_to_alloc > cap {
        return Err(anyhow!(
            "SHM capacity exhausted for RDMA receive ({} bytes). \
             Increase INITIAL_SHM_SIZE.", bytes_to_alloc
        ));
    }

    // Phase 3: structure the page chain in-place (data already written by RDMA)
    for i in 0..n_pages as ShmOffset {
        let page_off = dest_off + i * PAGE_SIZE;
        let page = unsafe { &mut *((splice_addr + page_off as usize) as *mut Page) };
        let data_in_page = PAGE_DATA.min(total_bytes - i as usize * PAGE_DATA);
        page.cursor.store(data_in_page as ShmOffset, Ordering::Relaxed);
        let next = if i + 1 < n_pages as ShmOffset { dest_off + (i + 1) * PAGE_SIZE } else { 0 };
        page.next_offset.store(next as u64, Ordering::Relaxed);
    }
    // Fence: ensure page chain metadata is visible before linking to slot.
    std::sync::atomic::fence(Ordering::Release);

    // Phase 4: link chain to slot
    let tail_off = dest_off + (n_pages as ShmOffset - 1) * PAGE_SIZE;
    link_to_slot(sb, slot, slot_kind, dest_off, tail_off);

    Ok(())
}
