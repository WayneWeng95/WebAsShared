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
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::rdma::context::RdmaContext;
use crate::rdma::memory_region::MemoryRegion;
use crate::rdma::queue_pair::QueuePair;

mod connect;
mod atomic;
mod data_path;
mod mr2;
mod src_mr;
mod staging;

pub use mr2::{Mr2Info, Mr2Reservation};

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
    /// Receiver-side overflow MR.  Created lazily the first time an
    /// incoming transfer is too large for MR1's bump budget.  Dropped
    /// at `MeshNode::drop` or by idle-timeout shrink.
    pub(in crate::mesh) mr2:       Mutex<Option<mr2::Mr2Storage>>,
    /// Sender-side extension MR over SHM past `INITIAL_SHM_SIZE`.
    /// Lazily registered when the local page chain has pages at offsets
    /// the NIC's MR1 doesn't cover.
    pub(in crate::mesh) src_ext:   Mutex<Option<src_mr::SrcExtStorage>>,
    /// Sender-side staging buffer for paged-mode source pages.  The
    /// source bytes (which live in `GlobalPool`, outside SHM) are
    /// memcpy'd here first so they can be referenced by a registered lkey.
    pub(in crate::mesh) src_stage: Mutex<Option<src_mr::SrcStageStorage>>,
    /// SHM backing file (opened by `connect_all_on_shm` when `shm_path`
    /// is supplied) — enables `ensure_shm_capacity` to grow the direct
    /// window past `INITIAL_SHM_SIZE` for the receiver-side memcpy-back.
    /// `None` means "no capacity growth available" (e.g., `connect_all`
    /// mode with an internally-allocated buffer).
    pub(in crate::mesh) shm_file: Mutex<Option<std::fs::File>>,
    /// VA base of the SHM mapping (same as `mr.addr()` when set).  Kept
    /// alongside `shm_file` because `ensure_shm_capacity`'s `MAP_FIXED`
    /// remap needs this address at call time.
    pub(in crate::mesh) shm_base: Mutex<usize>,
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

    // ── MR2: receiver-side dynamic overflow ──────────────────────────────────

    /// Ensure MR2 exists with capacity `>= at_least` bytes, creating or
    /// growing it as needed.  On growth, the old MR is deregistered and a
    /// fresh one is registered — returns the new `Mr2Info`.
    ///
    /// Calling this with `at_least == 0` is a no-op if MR2 already exists.
    pub fn ensure_mr2(&self, at_least: usize) -> Result<Mr2Info> {
        let initial = common::RDMA_MR2_INITIAL_SIZE as usize;
        let target  = at_least.max(initial);

        let mut guard = self.mr2.lock().expect("MR2 mutex poisoned");
        match guard.as_mut() {
            Some(existing) if existing.len() >= at_least => Ok(existing.info()),
            Some(_existing) => {
                // Grow: drop the existing MR2 and create a new, bigger one.
                // The new file is a fresh tmpfs entry so it starts at 0 bytes
                // — previous staging contents are gone, which is fine because
                // any in-flight RDMA WRITEs would be observed only through
                // a DestReply we're about to send (and have not sent yet).
                *guard = None;
                let pid = std::process::id();
                let path = PathBuf::from(format!("/dev/shm/webs-rdma-mr2-{pid}"));
                let storage = mr2::Mr2Storage::new(&self.ctx, path, target)
                    .context("grow MR2")?;
                let info = storage.info();
                *guard = Some(storage);
                Ok(info)
            }
            None => {
                let pid = std::process::id();
                let path = PathBuf::from(format!("/dev/shm/webs-rdma-mr2-{pid}"));
                let storage = mr2::Mr2Storage::new(&self.ctx, path, target)
                    .context("create MR2")?;
                let info = storage.info();
                *guard = Some(storage);
                Ok(info)
            }
        }
    }

    /// Reserve a region inside MR2 for one RDMA receive.  Caller must have
    /// called `ensure_mr2` first with a size that covers this reservation.
    ///
    /// Returns the reservation plus the host pointer to the MR2 slice —
    /// used by the receiver to memcpy the bytes into a fresh MR1 page chain
    /// after the RDMA WRITE completes.
    pub fn mr2_reserve(&self, size: u64) -> Result<Mr2Reservation> {
        let guard = self.mr2.lock().expect("MR2 mutex poisoned");
        let storage = guard.as_ref().ok_or_else(||
            anyhow::anyhow!("mr2_reserve: MR2 not created (call ensure_mr2 first)")
        )?;
        storage.reserve(size).ok_or_else(||
            anyhow::anyhow!(
                "mr2_reserve: MR2 ({} bytes) has no room for {}-byte reservation",
                storage.len(), size,
            )
        )
    }

    /// Reset MR2's internal bump allocator.  Safe only when no in-flight
    /// transfer is still holding a reservation.  Typically called at the
    /// end of a DAG wave that used MR2.
    pub fn mr2_reset(&self) {
        if let Ok(guard) = self.mr2.lock() {
            if let Some(storage) = guard.as_ref() {
                storage.reset_bump();
            }
        }
    }

    /// Update MR2's last-use timestamp.  Called on every successful use.
    pub fn mr2_touch(&self) {
        if let Ok(guard) = self.mr2.lock() {
            if let Some(storage) = guard.as_ref() {
                storage.touch();
            }
        }
    }

    /// Drop MR2 if it exists and has been idle longer than the configured
    /// timeout (`common::MR2_IDLE_TIMEOUT_NANOS`).  Called at the top of
    /// `recv_si` / `send_si` so pinning is returned without a background
    /// thread.
    pub fn mr2_try_shrink_idle(&self) {
        let timeout = std::time::Duration::from_nanos(common::MR2_IDLE_TIMEOUT_NANOS);
        let mut guard = match self.mr2.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let should_drop = match guard.as_ref() {
            Some(s) => s.idle_duration() >= timeout,
            None    => false,
        };
        if should_drop {
            *guard = None;
        }
    }

    // ── Sender-side MR-src-ext: cover SHM past MR1 ───────────────────────────

    /// Return the lkey for a direct-mode source SGE at absolute address
    /// `abs_addr`, length `len`.  Handles three cases:
    ///
    /// * The address is fully inside MR1 → returns the MR1 lkey.
    /// * The address is in the SHM extension (past `INITIAL_SHM_SIZE`) →
    ///   ensures `MR-src-ext` covers it (creating/growing as needed) and
    ///   returns its lkey.
    /// * The address spans the MR1/extension boundary → errors, since a
    ///   single SGE cannot straddle two MRs.  Callers should split the
    ///   SGE at the boundary when they detect this, but in practice this
    ///   is impossible because source pages are 4 KiB and the boundary
    ///   is page-aligned (`INITIAL_SHM_SIZE` is a multiple of PAGE_SIZE).
    pub fn src_lkey_for_shm(&self, shm_base: u64, abs_addr: u64, len: u32) -> Result<u32> {
        let mr1_end = shm_base + common::INITIAL_SHM_SIZE as u64;
        if abs_addr + len as u64 <= mr1_end {
            return Ok(self.mr.lkey());
        }
        if abs_addr < mr1_end {
            return Err(anyhow::anyhow!(
                "src_lkey_for_shm: SGE {:#x}+{} straddles MR1/MR-src-ext boundary {:#x}",
                abs_addr, len, mr1_end,
            ));
        }
        // Address is in extension region.
        let needed_end = abs_addr + len as u64;
        let needed_len = (needed_end - mr1_end) as usize;

        let mut guard = self.src_ext.lock().expect("MR-src-ext mutex poisoned");
        match guard.as_ref() {
            Some(ext) if ext.covers(abs_addr, len) => {
                let lkey = ext.lkey();
                ext.touch();
                return Ok(lkey);
            }
            _ => {}
        }

        // Need to (re)register with coverage up to needed_end.  Grow by
        // doubling relative to whichever is larger: current coverage or
        // needed_len.
        let new_len = match guard.as_ref() {
            Some(ext) => (ext.covered_len.saturating_mul(2)).max(needed_len),
            None      => needed_len.max(common::PAGE_SIZE as usize),
        };
        *guard = None; // drop the old registration before re-reg
        let start_ptr = mr1_end as *mut u8;
        let storage = src_mr::SrcExtStorage::new(&self.ctx, start_ptr, new_len)?;
        let lkey = storage.lkey();
        storage.touch();
        *guard = Some(storage);
        Ok(lkey)
    }

    /// Tear down MR-src-ext if idle for the configured timeout.
    pub fn src_ext_try_shrink_idle(&self) {
        let timeout = std::time::Duration::from_nanos(common::MR2_IDLE_TIMEOUT_NANOS);
        let mut guard = match self.src_ext.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let should_drop = match guard.as_ref() {
            Some(s) => s.idle_duration() >= timeout,
            None    => false,
        };
        if should_drop {
            *guard = None;
        }
    }

    // ── Sender-side MR-src-stage: host-staging for paged-mode pages ──────────

    /// Memcpy `src_bytes` into the sender-side staging buffer and return
    /// the staging `(addr, lkey)` for use in an SGE.  Creates or grows
    /// the staging file lazily.  Call `src_stage_reset` at the end of
    /// the transfer to free the staged bytes for reuse.
    pub fn src_stage_copy_in(&self, src_bytes: &[u8]) -> Result<(u64, u32)> {
        loop {
            let mut guard = self.src_stage.lock().expect("MR-src-stage mutex poisoned");
            match guard.as_ref() {
                Some(stage) => {
                    if let Some((addr, lkey)) = stage.stage(src_bytes) {
                        stage.touch();
                        return Ok((addr, lkey));
                    }
                    // Stage is full — drop it, double the size, re-create.
                    let doubled = stage.len().saturating_mul(2);
                    let new_len = doubled.max(src_bytes.len())
                                         .max(common::RDMA_MR2_INITIAL_SIZE as usize);
                    *guard = None;
                    let pid = std::process::id();
                    let path = PathBuf::from(format!("/dev/shm/webs-rdma-src-stage-{pid}"));
                    let storage = src_mr::SrcStageStorage::new(&self.ctx, path, new_len)?;
                    *guard = Some(storage);
                    // Retry the stage call under the fresh storage.
                }
                None => {
                    let pid = std::process::id();
                    let path = PathBuf::from(format!("/dev/shm/webs-rdma-src-stage-{pid}"));
                    let initial = src_bytes.len()
                        .max(common::RDMA_MR2_INITIAL_SIZE as usize);
                    let storage = src_mr::SrcStageStorage::new(&self.ctx, path, initial)?;
                    *guard = Some(storage);
                }
            }
        }
    }

    /// Reset the staging bump at the end of a transfer so subsequent
    /// transfers can reuse the buffer.  Safe only once no in-flight
    /// transfer still references the staged bytes.
    pub fn src_stage_reset(&self) {
        if let Ok(guard) = self.src_stage.lock() {
            if let Some(stage) = guard.as_ref() { stage.reset_bump(); }
        }
    }

    /// Tear down MR-src-stage if idle for the configured timeout.
    pub fn src_stage_try_shrink_idle(&self) {
        let timeout = std::time::Duration::from_nanos(common::MR2_IDLE_TIMEOUT_NANOS);
        let mut guard = match self.src_stage.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let should_drop = match guard.as_ref() {
            Some(s) => s.idle_duration() >= timeout,
            None    => false,
        };
        if should_drop {
            *guard = None;
        }
    }

    // ── SHM capacity growth ─────────────────────────────────────────────────

    /// Ensure the SHM direct window covers `required` bytes.  If the current
    /// `global_capacity` is smaller, `ftruncate`'s the backing file and
    /// `MAP_FIXED`-remaps at the same VA, then bumps the capacity atom.
    ///
    /// This is used by the receiver-side MR2 memcpy-back path: when the
    /// memcpy destination would exceed the current direct window, we grow
    /// SHM first so the destination pages land at direct-mode PageIds
    /// (readable by both Rust and Python workloads without paged-mode
    /// resolution).
    ///
    /// `splice_addr` is the guest's view of SHM base; we use it to access
    /// the `Superblock`'s `global_capacity` atomic.  The actual `MAP_FIXED`
    /// uses the stored `shm_base`, which matches `splice_addr` at the host.
    ///
    /// Returns an error if no `shm_path` was provided at `connect_all_on_shm`
    /// time (capacity growth is disabled in that case), or if `required`
    /// exceeds `CAPACITY_HARD_LIMIT`.
    pub fn ensure_shm_capacity(&self, splice_addr: usize, required: usize) -> Result<()> {
        let sb = unsafe { &*(splice_addr as *const common::Superblock) };
        let current = sb.global_capacity.load(std::sync::atomic::Ordering::Acquire) as usize;
        if current >= required {
            return Ok(());
        }

        let mut file_guard = self.shm_file.lock().expect("shm_file mutex poisoned");
        let file = file_guard.as_mut().ok_or_else(||
            anyhow::anyhow!("ensure_shm_capacity: shm_file not initialized — \
                             pass shm_path to connect_all_on_shm")
        )?;

        // Re-check under lock.
        let current = sb.global_capacity.load(std::sync::atomic::Ordering::Acquire) as usize;
        if current >= required {
            return Ok(());
        }

        let hard_limit = common::CAPACITY_HARD_LIMIT as usize;
        if required > hard_limit {
            return Err(anyhow::anyhow!(
                "ensure_shm_capacity: required {} exceeds CAPACITY_HARD_LIMIT {}",
                required, hard_limit,
            ));
        }

        // Grow geometrically to keep the amortized cost low.
        let new_size = (current.saturating_mul(2)).max(required).min(hard_limit);
        file.set_len(new_size as u64)
            .with_context(|| format!("set_len SHM to {}", new_size))?;

        let base = *self.shm_base.lock().expect("shm_base mutex poisoned");
        if base == 0 {
            return Err(anyhow::anyhow!("ensure_shm_capacity: shm_base not set"));
        }

        use std::os::fd::AsRawFd;
        let mapped = unsafe {
            libc::mmap(
                base as *mut libc::c_void,
                new_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                file.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(anyhow::anyhow!(
                "ensure_shm_capacity: MAP_FIXED mmap failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        sb.global_capacity.store(
            new_size as common::ShmOffset,
            std::sync::atomic::Ordering::Release,
        );
        eprintln!(
            "[SHM] expanded {} MiB → {} MiB (for receiver memcpy-back)",
            current / (1024 * 1024), new_size / (1024 * 1024),
        );
        Ok(())
    }
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
