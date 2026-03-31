// Full-mesh RDMA connections between N nodes.
//
// Each pair (i, j) shares ONE RC QP (for RDMA WRITEs) and TWO TCP control
// streams — one per transfer direction:
//
//   ctrl_as_sender[j]  : used when THIS node is the SENDER to j.
//                        RemoteSend uses this stream exclusively.
//   ctrl_as_receiver[j]: used when j is the SENDER to THIS node.
//                        RemoteRecv uses this stream exclusively.
//
// Stream isolation eliminates TCP message interleaving for concurrent
// bidirectional transfers (shuffle / all-to-all patterns).  Each QP has its
// own dedicated CQ so poll_one_blocking never races between peers.
//
// See connect.rs for connection setup, atomic.rs for RDMA atomic ops,
// data_path.rs for basic data transfers, and staging.rs for advanced writes.

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use crate::rdma::context::RdmaContext;
use crate::rdma::memory_region::MemoryRegion;
use crate::rdma::queue_pair::QueuePair;

mod connect;
mod atomic;
mod data_path;
mod staging;

// ── Constants ─────────────────────────────────────────────────────────────────

pub(in crate::mesh) const RDMA_PORT: u8  = 2;
pub(in crate::mesh) const GID_INDEX: u8  = 2;
pub(in crate::mesh) const CQ_SIZE:   i32 = 64;

/// Bytes allocated per peer slot in the local MR.
pub const SLOT_SIZE: usize = 256;

/// Base TCP port for conn-1 (QP exchange).  Pair (i,j) uses BASE_PORT + i*MAX_NODES + j.
pub(in crate::mesh) const BASE_PORT:  u16 = 7490;
pub(in crate::mesh) const MAX_NODES:  u16 = common::MAX_MESH_NODES as u16;
/// Base TCP port for conn-2 (reverse-direction ctrl only).
pub(in crate::mesh) const BASE_PORT2: u16 = BASE_PORT + MAX_NODES * MAX_NODES;

// ── Internal peer state ────────────────────────────────────────────────────────

pub(in crate::mesh) struct PeerLink {
    pub(in crate::mesh) qp:               Arc<Mutex<QueuePair>>,
    /// TCP stream for when WE are the sender to this peer (conn-1).
    pub(in crate::mesh) ctrl_as_sender:   Arc<Mutex<TcpStream>>,
    /// TCP stream for when the PEER is the sender to us (conn-2).
    pub(in crate::mesh) ctrl_as_receiver: Arc<Mutex<TcpStream>>,
    pub(in crate::mesh) remote_slot_addr: u64,  // base + self.id * SLOT_SIZE
    pub(in crate::mesh) remote_mr_base:   u64,  // full MR base
    pub(in crate::mesh) remote_rkey:      u32,
}

// ── Channel handle types ───────────────────────────────────────────────────────

/// Thread-safe handle for the sender side of a RemoteSend transfer.
///
/// Holds Arc clones of the per-peer state; the `MeshNode` remains usable
/// on the main thread while the handle lives in a worker thread.
pub struct SendChannel {
    /// Control stream used exclusively when THIS node sends to the peer.
    pub ctrl:           Arc<Mutex<TcpStream>>,
    /// The peer's RC Queue Pair — used to post RDMA WRITEs.
    pub qp:             Arc<Mutex<QueuePair>>,
    /// Base virtual address of the peer's registered MR.
    pub remote_mr_base: u64,
    /// Remote key protecting the peer's MR.
    pub remote_rkey:    u32,
    /// Local key for our MR (needed to build SGEs).
    pub lkey:           u32,
}

/// Thread-safe handle for the receiver side of a RemoteRecv transfer.
///
/// The receiver never posts RDMA WRs — it only uses the TCP control stream.
pub struct RecvChannel {
    /// Control stream used exclusively when the PEER sends to this node.
    pub ctrl: Arc<Mutex<TcpStream>>,
}

/// Thread-safe handle for one-sided RDMA atomic operations to a single peer.
///
/// Contains only Copy values and an `Arc`-wrapped QP — safe to move into
/// a `std::thread::spawn` closure.  Obtain via `MeshNode::atomic_channel`.
pub struct AtomicChannel {
    /// The peer's RC Queue Pair — used to post RDMA FAA/CAS WRs.
    pub qp:             Arc<Mutex<QueuePair>>,
    /// Base virtual address of the peer's registered MR (= peer's SHM base).
    pub remote_mr_base: u64,
    /// Remote key protecting the peer's MR.
    pub remote_rkey:    u32,
    /// Base virtual address of OUR registered MR (= our SHM base = splice_addr).
    pub local_mr_base:  u64,
    /// Local key for our MR (needed to register the result buffer SGE).
    pub local_lkey:     u32,
}

// ── Mesh node ──────────────────────────────────────────────────────────────────

/// Full-mesh RDMA node.
pub struct MeshNode {
    pub id:    usize,
    pub total: usize,
    pub(in crate::mesh) ctx:   RdmaContext,
    pub(in crate::mesh) mr:    MemoryRegion,
    pub(in crate::mesh) peers: HashMap<usize, PeerLink>,
}

// ── Channel handle accessors ──────────────────────────────────────────────────

impl MeshNode {
    /// Build a `SendChannel` for spawning a RemoteSend thread to `peer_id`.
    ///
    /// Clones the Arc handles — the MeshNode remains fully usable on the
    /// calling thread while the SendChannel lives in the spawned thread.
    pub fn send_channel(&self, peer_id: usize) -> SendChannel {
        let link = self.peers.get(&peer_id)
            .unwrap_or_else(|| panic!("no connection to peer {}", peer_id));
        SendChannel {
            ctrl:           Arc::clone(&link.ctrl_as_sender),
            qp:             Arc::clone(&link.qp),
            remote_mr_base: link.remote_mr_base,
            remote_rkey:    link.remote_rkey,
            lkey:           self.mr.lkey(),
        }
    }

    /// Build a `RecvChannel` for spawning a RemoteRecv thread from `peer_id`.
    pub fn recv_channel(&self, peer_id: usize) -> RecvChannel {
        let link = self.peers.get(&peer_id)
            .unwrap_or_else(|| panic!("no connection to peer {}", peer_id));
        RecvChannel {
            ctrl: Arc::clone(&link.ctrl_as_receiver),
        }
    }

    /// Build an `AtomicChannel` for spawning an RDMA-atomic thread to `peer_id`.
    pub fn atomic_channel(&self, peer_id: usize) -> AtomicChannel {
        let link = self.peers.get(&peer_id)
            .unwrap_or_else(|| panic!("no connection to peer {}", peer_id));
        AtomicChannel {
            qp:             Arc::clone(&link.qp),
            remote_mr_base: link.remote_mr_base,
            remote_rkey:    link.remote_rkey,
            local_mr_base:  self.mr.addr(),
            local_lkey:     self.mr.lkey(),
        }
    }

    /// Virtual base address of the local RDMA Memory Region.
    #[inline]
    pub fn mr_addr(&self) -> u64 { self.mr.addr() }

    /// lkey of the local MR.
    #[inline]
    pub fn mr_lkey(&self) -> u32 { self.mr.lkey() }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub(in crate::mesh) fn rand_psn() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0x4321)
        & 0x00FF_FFFF
}
