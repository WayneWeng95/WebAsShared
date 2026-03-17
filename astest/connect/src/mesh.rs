// Full-mesh RDMA connections between N nodes.
//
// Each pair (i, j) where i < j shares exactly ONE QP pair — both sides can
// RDMA WRITE directly into the other's memory region without going through
// any central node.
//
// Layout
// ──────
// Every node allocates one MR of N * SLOT_SIZE bytes.
// Slot k in node i's MR is the landing zone for node k's writes.
// i.e. when node j writes to node i, it targets slot j in i's MR.
// Node i's own slot (slot i) is never written by peers → used as send
// staging buffer so only one MR is needed per node.
//
// Connection setup (avoids deadlock)
// ────────────────────────────────────
// For each pair (i, j) with i < j:
//   node i  →  server on port BASE_PORT + i * MAX_NODES + j
//   node j  →  client connecting to that port
// All server threads are spawned before any client connection is attempted.

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::rdma::context::RdmaContext;
use crate::rdma::exchange::{self, QpInfo};
use crate::ffi::ibv_sge;
use crate::rdma::memory_region::MemoryRegion;
use crate::rdma::queue_pair::{QueuePair, MAX_SEND_SGE};

// ── Constants ─────────────────────────────────────────────────────────────────

const RDMA_PORT: u8  = 1;
const GID_INDEX: u8  = 0;
const CQ_SIZE:   i32 = 64;

/// Bytes allocated per peer slot in the local MR.
pub const SLOT_SIZE: usize = 256;

/// Base TCP port for the mesh exchange.  Pair (i,j) uses BASE_PORT + i*MAX_NODES + j.
const BASE_PORT:  u16 = 7490;
const MAX_NODES:  u16 = 8;

// ── Types ─────────────────────────────────────────────────────────────────────

struct PeerLink {
    qp:               QueuePair,
    remote_slot_addr: u64,  // base + self.id * SLOT_SIZE: where WE write in peer's MR
    remote_mr_base:   u64,  // full MR base: for atomics at absolute offsets
    remote_rkey:      u32,
    ctrl:             TcpStream,
}

/// Full-mesh RDMA node.  Connect with [`MeshNode::connect_all`], then call
/// [`write_to`] / [`wait_writes`] / [`slot`] from any node.
pub struct MeshNode {
    pub id:    usize,
    pub total: usize,
    ctx:       RdmaContext,
    mr:        MemoryRegion,
    peers:     HashMap<usize, PeerLink>,
}

// ── Construction ──────────────────────────────────────────────────────────────

impl MeshNode {
    /// Establish full-mesh RDMA connections with all `total` nodes.
    ///
    /// `ips[k]` is the IP (or hostname) of node k.  This call blocks until
    /// every pair (i,j) has exchanged QP metadata and transitioned to RTS.
    /// Uses an internally allocated MR of `total * SLOT_SIZE` bytes.
    pub fn connect_all(node_id: usize, total: usize, ips: &[&str]) -> Result<Self> {
        Self::connect_all_with_buf(node_id, total, ips, None, total * SLOT_SIZE)
    }

    /// Like [`connect_all`] but registers an externally-owned buffer as the MR.
    ///
    /// Pass the mmap'd SHM base pointer and its size.  RDMA WRITEs from remote
    /// nodes land directly into this memory — no intermediate copy.
    ///
    /// # Safety
    /// `shm_ptr` must remain valid and pinned for the lifetime of the `MeshNode`.
    pub unsafe fn connect_all_on_shm(
        node_id: usize,
        total:   usize,
        ips:     &[&str],
        shm_ptr: *mut u8,
        shm_len: usize,
    ) -> Result<Self> {
        Self::connect_all_with_buf(node_id, total, ips, Some((shm_ptr, shm_len)), shm_len)
    }

    fn connect_all_with_buf(
        node_id:     usize,
        total:       usize,
        ips:         &[&str],
        external_mr: Option<(*mut u8, usize)>,
        mr_size:     usize,
    ) -> Result<Self> {
        assert_eq!(ips.len(), total, "ips.len() must equal total");
        assert!(node_id < total,     "node_id out of range");
        assert!(total <= MAX_NODES as usize, "increase MAX_NODES");

        let ctx = RdmaContext::new(None, CQ_SIZE)?;
        let mr = match external_mr {
            Some((ptr, len)) => unsafe { MemoryRegion::register_external(&ctx, ptr, len)? },
            None             => MemoryRegion::alloc_and_register(&ctx, mr_size)?,
        };

        let gid       = ctx.query_gid(RDMA_PORT, GID_INDEX as i32)?;
        let port_attr = ctx.query_port(RDMA_PORT)?;

        // Pre-create all N-1 QPs in the main thread so they can be moved
        // into server threads or used directly for client connections.
        let mut qp_pool: HashMap<usize, (QueuePair, u32)> = HashMap::new();
        for j in 0..total {
            if j == node_id { continue; }
            let qp  = QueuePair::create(&ctx)?;
            let psn = rand_psn();
            qp_pool.insert(j, (qp, psn));
        }

        // Expose the full MR base address so peers can compute their write slot
        // (base + their_own_id * SLOT_SIZE) and can also address shared atomic
        // variables at fixed absolute offsets known to all nodes.
        let make_info = |qpn: u32, psn: u32| QpInfo {
            qpn,
            psn,
            gid:  gid.raw,
            lid:  port_attr.lid,
            rkey: mr.rkey(),
            addr: mr.addr(),             // full MR base, not a per-slot offset
            len:  (total * SLOT_SIZE) as u32,
        };

        // ── Server threads (pairs where we are the lower-ID node) ────────────

        type ThreadMsg = Result<(usize, QueuePair, QpInfo, TcpStream)>;
        let (tx, rx) = mpsc::channel::<ThreadMsg>();

        for j in (node_id + 1)..total {
            let (qp, psn) = qp_pool.remove(&j).unwrap();
            let local_info = make_info(qp.qpn(), psn);
            let port = BASE_PORT + node_id as u16 * MAX_NODES + j as u16;
            let tx   = tx.clone();

            thread::spawn(move || {
                let res = (|| -> Result<(usize, QueuePair, QpInfo, TcpStream)> {
                    let (remote, ctrl) = exchange::server_exchange(port, &local_info)?;
                    qp.to_init(RDMA_PORT)?;
                    qp.to_rtr(&remote, RDMA_PORT, GID_INDEX)?;
                    qp.to_rts(psn)?;
                    Ok((j, qp, remote, ctrl))
                })();
                tx.send(res).ok();
            });
        }
        drop(tx);

        // Brief pause so all server threads reach accept() before we connect.
        thread::sleep(Duration::from_millis(200));

        // ── Client connections (pairs where we are the higher-ID node) ────────

        let mut peers: HashMap<usize, PeerLink> = HashMap::new();

        for k in 0..node_id {
            let (qp, psn) = qp_pool.remove(&k).unwrap();
            let local_info = make_info(qp.qpn(), psn);
            let port = BASE_PORT + k as u16 * MAX_NODES + node_id as u16;

            let (remote, ctrl) = exchange::client_exchange(ips[k], port, &local_info)?;
            qp.to_init(RDMA_PORT)?;
            qp.to_rtr(&remote, RDMA_PORT, GID_INDEX)?;
            qp.to_rts(psn)?;

            peers.insert(k, PeerLink {
                qp,
                remote_slot_addr: remote.addr + (node_id * SLOT_SIZE) as u64,
                remote_mr_base:   remote.addr,
                remote_rkey:      remote.rkey,
                ctrl,
            });
        }

        // Collect server thread results.
        for msg in rx {
            let (j, qp, remote, ctrl) = msg?;
            peers.insert(j, PeerLink {
                qp,
                remote_slot_addr: remote.addr + (node_id * SLOT_SIZE) as u64,
                remote_mr_base:   remote.addr,
                remote_rkey:      remote.rkey,
                ctrl,
            });
        }

        println!("[mesh] node {} connected to {} peers", node_id, peers.len());
        Ok(MeshNode { id: node_id, total, ctx, mr, peers })
    }
}

// ── Data-path ─────────────────────────────────────────────────────────────────

impl MeshNode {
    /// RDMA WRITE `data` directly into `peer_id`'s memory, then signal done.
    ///
    /// Copies `data` into our own MR slot (slot `self.id`) as the RDMA source
    /// buffer, then posts a one-sided WRITE to the peer's designated landing
    /// zone.  The peer's CPU is not involved during the transfer.
    pub fn write_to(&mut self, peer_id: usize, data: &[u8]) -> Result<()> {
        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;

        let n = data.len().min(SLOT_SIZE);

        // Stage data in our own slot (never written by peers, safe to use as src).
        let my_slot_offset = self.id * SLOT_SIZE;
        self.mr.as_bytes_mut()[my_slot_offset..my_slot_offset + n]
            .copy_from_slice(&data[..n]);

        // Build a sub-MR view: addr = base + my_slot, same lkey.
        // We reuse the full MR but offset the posted addr.
        let src_addr  = self.mr.addr() + my_slot_offset as u64;
        let src_lkey  = self.mr.lkey();

        link.qp.post_rdma_write_raw(src_addr, src_lkey, n as u32,
                                    link.remote_slot_addr, link.remote_rkey)?;
        link.qp.poll_one_blocking(&self.ctx)?;

        exchange::send_done(&mut link.ctrl)?;
        println!("[mesh] node {} → node {}: {} bytes written", self.id, peer_id, n);
        Ok(())
    }

    /// Write `data` to every peer sequentially.
    pub fn broadcast(&mut self, data: &[u8]) -> Result<()> {
        let (id, total) = (self.id, self.total);
        for peer_id in (0..total).filter(|&j| j != id) {
            self.write_to(peer_id, data)?;
        }
        Ok(())
    }

    /// Block until `peer_id` signals that it has finished its RDMA WRITE.
    pub fn wait_from(&mut self, peer_id: usize) -> Result<()> {
        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;
        exchange::wait_done(&mut link.ctrl)?;
        println!("[mesh] node {} ← node {}: write signal received", self.id, peer_id);
        Ok(())
    }

    /// Block until all peers have signalled done.
    pub fn wait_all_writes(&mut self) -> Result<()> {
        let (id, total) = (self.id, self.total);
        for peer_id in (0..total).filter(|&j| j != id) {
            self.wait_from(peer_id)?;
        }
        Ok(())
    }

    /// Read the data written by `peer_id` into this node's MR slot.
    pub fn slot(&self, peer_id: usize) -> &[u8] {
        let off = peer_id * SLOT_SIZE;
        &self.mr.as_bytes()[off..off + SLOT_SIZE]
    }

    /// Atomic Fetch-and-Add on a u64 at `byte_offset` inside `peer_id`'s MR slot.
    ///
    /// `byte_offset` is relative to the start of peer's slot and must be
    /// 8-byte aligned.  Returns the *old* value before the addition.
    /// The peer's CPU is not involved — the HCA handles atomicity end-to-end.
    pub fn fetch_and_add(&mut self, peer_id: usize, byte_offset: usize, add_val: u64)
        -> anyhow::Result<u64>
    {
        assert!(byte_offset % 8 == 0, "byte_offset must be 8-byte aligned");
        assert!(byte_offset + 8 <= SLOT_SIZE);

        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow::anyhow!("no connection to peer {}", peer_id))?;

        // Use the first 8 bytes of our own slot as the result landing buffer.
        let result_addr = self.mr.addr() + (self.id * SLOT_SIZE) as u64;
        let result_lkey = self.mr.lkey();
        let remote_addr = link.remote_mr_base + byte_offset as u64;
        let remote_rkey = link.remote_rkey;

        link.qp.post_fetch_and_add(result_addr, result_lkey,
                                    remote_addr, remote_rkey, add_val)?;
        link.qp.poll_one_blocking(&self.ctx)?;

        // Acquire fence: ensure the HCA's DMA into result_addr is visible.
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);

        let old_val = u64::from_ne_bytes(
            self.mr.as_bytes()[self.id * SLOT_SIZE..self.id * SLOT_SIZE + 8]
                .try_into().unwrap()
        );
        Ok(old_val)
    }

    /// Atomic Compare-and-Swap on a u64 at `byte_offset` inside `peer_id`'s MR slot.
    ///
    /// If the remote u64 equals `compare_val`, it is atomically replaced by
    /// `swap_val`.  Returns the *old* value (before any swap).
    pub fn compare_and_swap(&mut self, peer_id: usize, byte_offset: usize,
                             compare_val: u64, swap_val: u64)
        -> anyhow::Result<u64>
    {
        assert!(byte_offset % 8 == 0, "byte_offset must be 8-byte aligned");
        assert!(byte_offset + 8 <= SLOT_SIZE);

        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow::anyhow!("no connection to peer {}", peer_id))?;

        let result_addr = self.mr.addr() + (self.id * SLOT_SIZE) as u64;
        let result_lkey = self.mr.lkey();
        let remote_addr = link.remote_mr_base + byte_offset as u64;
        let remote_rkey = link.remote_rkey;

        link.qp.post_compare_and_swap(result_addr, result_lkey,
                                       remote_addr, remote_rkey,
                                       compare_val, swap_val)?;
        link.qp.poll_one_blocking(&self.ctx)?;

        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);

        let old_val = u64::from_ne_bytes(
            self.mr.as_bytes()[self.id * SLOT_SIZE..self.id * SLOT_SIZE + 8]
                .try_into().unwrap()
        );
        Ok(old_val)
    }

    /// Virtual base address of the local RDMA Memory Region.
    ///
    /// When the node was created with [`connect_all_on_shm`], this equals the
    /// `splice_addr` of the SHM mapping, so `mr_addr() + shm_offset` gives the
    /// virtual address of any SHM byte.
    #[inline]
    pub fn mr_addr(&self) -> u64 { self.mr.addr() }

    /// lkey of the local MR (used to build SGEs for scatter-gather writes).
    #[inline]
    pub fn mr_lkey(&self) -> u32 { self.mr.lkey() }

    /// RDMA-WRITE from an arbitrary scatter list of local SHM regions to a
    /// contiguous byte range starting at `remote_off` in `peer_id`'s SHM.
    ///
    /// `sge_pairs`: `(local_virtual_addr, byte_len)` entries — each address
    /// must fall within the registered MR.  There is **no limit** on the
    /// number of entries: if the list exceeds `MAX_SEND_SGE`, this method
    /// automatically splits it into multiple work requests.  All but the last
    /// WR are posted unsignaled; only the last is signaled.  Because RC QPs
    /// deliver completions in order, the single CQ entry implies every
    /// preceding write has landed at the remote — one `poll_one_blocking` call
    /// covers the entire transfer regardless of how many chunks were needed.
    pub fn rdma_write_sge(
        &mut self,
        peer_id:    usize,
        remote_off: u64,
        sge_pairs:  &[(u64, u32)],   // (local_vaddr, len)
    ) -> anyhow::Result<()> {
        let lkey = self.mr.lkey();

        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow::anyhow!("no connection to peer {}", peer_id))?;

        let remote_base = link.remote_mr_base + remote_off;
        let remote_rkey = link.remote_rkey;

        let chunks: Vec<&[(u64, u32)]> = sge_pairs.chunks(MAX_SEND_SGE).collect();
        let n_chunks = chunks.len();
        let mut remote_cursor = remote_base;

        for (i, chunk) in chunks.iter().enumerate() {
            let is_last = i == n_chunks - 1;

            let mut sges: Vec<ibv_sge> = chunk.iter().map(|&(addr, len)| {
                let mut s: ibv_sge = unsafe { std::mem::zeroed() };
                s.addr   = addr;
                s.length = len;
                s.lkey   = lkey;
                s
            }).collect();

            link.qp.post_rdma_write_sge_list(
                &mut sges, remote_cursor, remote_rkey, is_last,
            )?;

            // Advance the remote pointer for the next chunk.
            remote_cursor += chunk.iter().map(|&(_, l)| l as u64).sum::<u64>();
        }

        // A single CQ poll suffices: the signaled last WR completing implies
        // all prior unsignaled WRs have also completed at the remote.
        link.qp.poll_one_blocking(&self.ctx)?;

        let total: u32 = sge_pairs.iter().map(|&(_, l)| l).sum();
        println!(
            "[mesh] node {} → node {}: scatter RDMA WRITE {} bytes ({} SGEs, {} WRs)",
            self.id, peer_id, total, sge_pairs.len(), n_chunks
        );
        Ok(())
    }

    /// RDMA-WRITE `len` bytes from our SHM at `staging_offset` directly into
    /// `peer_id`'s SHM at the same byte offset.
    ///
    /// Data-only: no TCP signal is sent.  Call [`rdma_signal_staging`] after
    /// this to wake the receiver.  Both nodes must have the SHM registered as
    /// the RDMA MR via [`connect_all_on_shm`].
    pub fn rdma_write_staging(
        &mut self,
        peer_id:        usize,
        staging_offset: u64,
        len:            u32,
    ) -> anyhow::Result<()> {
        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow::anyhow!("no connection to peer {}", peer_id))?;

        let src_addr = self.mr.addr() + staging_offset;
        let src_lkey = self.mr.lkey();
        let dst_addr = link.remote_mr_base + staging_offset;
        let dst_rkey = link.remote_rkey;

        link.qp.post_rdma_write_raw(src_addr, src_lkey, len, dst_addr, dst_rkey)?;
        link.qp.poll_one_blocking(&self.ctx)?;
        println!(
            "[mesh] node {} → node {}: staging RDMA WRITE {} bytes done",
            self.id, peer_id, len
        );
        Ok(())
    }

    /// Signal `peer_id` that our RDMA WRITE into its staging area is complete.
    ///
    /// Primary signal: RDMA Fetch-and-Add (hardware atomic) on the u64 ready
    /// counter at `peer_id`'s SHM `staging_offset + 0`, incrementing it from
    /// 0 to 1.  The FAA result lands at the same `staging_offset` in our own
    /// SHM (harmless scratch — the sender already reset it to 0).
    ///
    /// Backup signal: TCP 1-byte `send_done` so systems without RDMA atomic
    /// support or with a slow HCA can still wake the receiver reliably.
    pub fn rdma_signal_staging(
        &mut self,
        peer_id:        usize,
        staging_offset: u64,
    ) -> anyhow::Result<()> {
        let (remote_mr_base, remote_rkey, lkey, mr_addr) = {
            let link = self.peers.get_mut(&peer_id)
                .ok_or_else(|| anyhow::anyhow!("no connection to peer {}", peer_id))?;
            (link.remote_mr_base, link.remote_rkey, self.mr.lkey(), self.mr.addr())
        };

        // FAA result scratch: our own staging[0..8] (already zeroed by sender).
        let result_addr = mr_addr + staging_offset;
        // Target: peer's ready counter at staging[0..8].
        let remote_addr = remote_mr_base + staging_offset;

        {
            let link = self.peers.get_mut(&peer_id).unwrap();
            link.qp.post_fetch_and_add(result_addr, lkey, remote_addr, remote_rkey, 1)?;
            link.qp.poll_one_blocking(&self.ctx)?;
        }

        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);

        // TCP backup signal — in case the receiver times out on the spin-poll.
        let link = self.peers.get_mut(&peer_id).unwrap();
        exchange::send_done(&mut link.ctrl)?;

        println!("[mesh] node {} → node {}: staging signal sent", self.id, peer_id);
        Ok(())
    }

    /// Block until `peer_id`'s ready counter at `counter_ptr` becomes non-zero,
    /// then issue an Acquire fence so the RDMA-written data is visible.
    ///
    /// Spin-polls `counter_ptr` (in the local SHM) for up to 100 ms.  If the
    /// counter is still zero after that deadline, falls back to the TCP
    /// `wait_done` signal (the sender sends this as a backup after the FAA).
    pub fn wait_staging(
        &mut self,
        peer_id:     usize,
        counter_ptr: *const std::sync::atomic::AtomicU64,
    ) -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};

        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            // SAFETY: caller guarantees the pointer is valid (SHM staging area).
            let v = unsafe { (*counter_ptr).load(Ordering::Acquire) };
            if v != 0 {
                println!("[mesh] node {} ← node {}: staging ready (RDMA atomic)", self.id, peer_id);
                return Ok(());
            }
            if Instant::now() >= deadline { break; }
            std::hint::spin_loop();
        }

        // Fallback: TCP signal.
        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow::anyhow!("no connection to peer {}", peer_id))?;
        exchange::wait_done(&mut link.ctrl)?;
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        println!("[mesh] node {} ← node {}: staging ready (TCP fallback)", self.id, peer_id);
        Ok(())
    }

    /// Trim null bytes and return peer's slot as a UTF-8 string (lossy).
    pub fn slot_str(&self, peer_id: usize) -> std::borrow::Cow<'_, str> {
        let b = self.slot(peer_id);
        let end = b.iter().position(|&x| x == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end])
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rand_psn() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0x4321)
        & 0x00FF_FFFF
}
