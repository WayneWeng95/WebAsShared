// RemoteSend / RemoteRecv: zero-copy SHM slot transfer between mesh nodes via RDMA.
//
// Two protocols are supported, selected per-node via the `protocol` field:
//
// Sender-initiated (SI, default)   — see si.rs
// Receiver-initiated (RI)          — see ri.rs
//
// Stream isolation
// ─────────────────
// RemoteSend uses SendChannel.ctrl (ctrl_as_sender) exclusively.
// RemoteRecv uses RecvChannel.ctrl (ctrl_as_receiver) exclusively.
// These are separate TCP connections per peer pair, so concurrent sends and
// receives to the same peer never interleave their messages.

use anyhow::Result;
use connect::{MeshNode, SendChannel, RecvChannel};

use crate::runtime::dag_runner::{RemoteSlotKind, RemoteProtocol};

mod shm;
mod rdma;
mod sender_initiated;
mod receiver_initiated;

// ── Shared constant ───────────────────────────────────────────────────────────

/// Usable data bytes per SHM page (PAGE_SIZE minus the header).
const PAGE_DATA: usize = common::PAGE_DATA_SIZE;

// ── Compatibility shim ────────────────────────────────────────────────────────

/// No staging pages — retained for API compatibility with the dag_runner call site.
pub const STAGE_BYTES_PER_PEER: usize = 0;

/// No-op: pre-allocation is no longer needed.
pub fn pre_alloc_staging(_splice_addr: usize, _total: usize) -> Result<()> {
    println!("[remote] dynamic page-chain receive — no staging pre-alloc needed");
    Ok(())
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Walk source page chain, build SGE list, transfer to receiver via RDMA.
///
/// Uses `ch.ctrl` (ctrl_as_sender) for all TCP control messages — this stream
/// is exclusive to this sender direction and safe to use from a thread.
///
/// `mesh` is needed so the sender can target a peer's MR2 (handed back in the
/// Phase-2 reply when the peer's receive overflowed MR1) or, on the sender
/// side, reach into a local sender-side extension MR when sending > MR1
/// worth of local data.
pub fn execute_remote_send(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    ch:          &SendChannel,
    protocol:    RemoteProtocol,
    mesh:        &MeshNode,
) -> Result<()> {
    match protocol {
        RemoteProtocol::SenderInit   => sender_initiated::send_si(splice_addr, slot, slot_kind, ch, mesh),
        RemoteProtocol::ReceiverInit => receiver_initiated::send_ri(splice_addr, slot, slot_kind, ch, mesh),
    }
}

/// Receive slot data from a remote peer, populate a local SHM slot.
///
/// Uses `ch.ctrl` (ctrl_as_receiver) for all TCP control messages — exclusive
/// to this receive direction.  `mesh` is needed to manage MR2 (lazy
/// registration and per-transfer reservation) when incoming data exceeds
/// MR1's remaining bump capacity.
pub fn execute_remote_recv(
    splice_addr: usize,
    slot:        usize,
    slot_kind:   RemoteSlotKind,
    ch:          &RecvChannel,
    protocol:    RemoteProtocol,
    mesh:        &MeshNode,
) -> Result<()> {
    match protocol {
        RemoteProtocol::SenderInit   => sender_initiated::recv_si(splice_addr, slot, slot_kind, ch, mesh),
        RemoteProtocol::ReceiverInit => receiver_initiated::recv_ri(splice_addr, slot, slot_kind, ch),
    }
}
