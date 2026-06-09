// RdmaRemote: high-level API for RDMA-based state sharing between two nodes.
//
// Wraps the low-level RDMA primitives (context, memory region, queue pair,
// TCP exchange) into a two-role interface:
//
//   Server (passive side)
//   ─────────────────────
//   let mut r = RdmaRemote::serve(7474, 4096)?;
//   r.wait_peer_write()?;                // block until client signals done
//   println!("{}", r.local_state_str()); // read what the client wrote
//
//   Client (active side)
//   ─────────────────────
//   let mut r = RdmaRemote::connect("192.168.1.10", 7474, 4096)?;
//   r.write_state(b"hello from client")?; // one-sided RDMA WRITE (no server CPU)
//
// The TCP connection used for QP metadata exchange is kept open as a control
// channel: the client sends a one-byte signal after the RDMA write completes
// so the server knows when to read its buffer.

use anyhow::Result;

use crate::rdma::context::RdmaContext;
use crate::rdma::exchange::{self, QpInfo};
use crate::rdma::memory_region::MemoryRegion;
use crate::rdma::queue_pair::QueuePair;

/// RDMA port number to use for device/port addressing (1-based).
const RDMA_PORT: u8 = 2;
/// GID index for RoCE (index 2 = IPv4-mapped GID for 10.10.1.x on CloudLab mlx4).
const GID_INDEX: u8 = 2;
/// Shared CQ size.
const CQ_SIZE: i32 = 16;

pub struct RdmaRemote {
    ctx:         RdmaContext,
    mr:          MemoryRegion,
    qp:          QueuePair,
    ctrl:        std::net::TcpStream,
    remote_addr: u64,
    remote_rkey: u32,
    state_size:  usize,
}

impl RdmaRemote {
    // ── Construction helpers ──────────────────────────────────────────────────

    fn setup_context(size: usize) -> Result<(RdmaContext, MemoryRegion, QueuePair, u32, QpInfo)> {
        let ctx = RdmaContext::new(None, CQ_SIZE)?;
        let mr  = MemoryRegion::alloc_and_register(&ctx, size)?;
        let qp  = QueuePair::create(&ctx)?;

        let psn: u32 = rand_psn();
        let gid = ctx.query_gid(RDMA_PORT, GID_INDEX as i32)?;
        let port_attr = ctx.query_port(RDMA_PORT)?;

        let info = QpInfo {
            qpn:  qp.qpn(),
            psn,
            gid:  gid.raw,
            lid:  port_attr.lid,
            rkey: mr.rkey(),
            addr: mr.addr(),
            len:  mr.len() as u32,
        };
        Ok((ctx, mr, qp, psn, info))
    }

    fn transition_qp(_ctx: &RdmaContext, qp: &QueuePair, psn: u32, remote: &QpInfo) -> Result<()> {
        qp.to_init(RDMA_PORT)?;
        qp.to_rtr(remote, RDMA_PORT, GID_INDEX)?;
        qp.to_rts(psn)?;
        Ok(())
    }

    // ── Public constructors ───────────────────────────────────────────────────

    /// **Server**: listen on `tcp_port`, accept one client, and set up the
    /// RDMA connection.  The server's memory region is the target for the
    /// client's RDMA WRITEs.
    ///
    /// `state_size`: bytes to allocate for the shared state buffer.
    pub fn serve(tcp_port: u16, state_size: usize) -> Result<Self> {
        let (ctx, mr, qp, psn, local_info) = Self::setup_context(state_size)?;
        let (remote, ctrl) = exchange::server_exchange(tcp_port, &local_info)?;
        Self::transition_qp(&ctx, &qp, psn, &remote)?;
        println!("[RdmaRemote] server ready — waiting for client write");
        Ok(RdmaRemote {
            ctx, mr, qp, ctrl,
            remote_addr: remote.addr,
            remote_rkey: remote.rkey,
            state_size,
        })
    }

    /// **Server (multi-stream)**: accept one client from an already-bound
    /// `TcpListener` and set up the RDMA connection.  Call once per expected
    /// client to serve several streams on a single port (the bandwidth
    /// benchmark's incast role).  Equivalent to [`serve`] but does not bind.
    pub fn serve_on(listener: &std::net::TcpListener, state_size: usize) -> Result<Self> {
        let (ctx, mr, qp, psn, local_info) = Self::setup_context(state_size)?;
        let (remote, ctrl) = exchange::server_exchange_on(listener, &local_info)?;
        Self::transition_qp(&ctx, &qp, psn, &remote)?;
        Ok(RdmaRemote {
            ctx, mr, qp, ctrl,
            remote_addr: remote.addr,
            remote_rkey: remote.rkey,
            state_size,
        })
    }

    /// **Client**: connect to `host:tcp_port` and set up the RDMA connection.
    ///
    /// `state_size`: bytes to allocate for the local source buffer.
    pub fn connect(host: &str, tcp_port: u16, state_size: usize) -> Result<Self> {
        let (ctx, mr, qp, psn, local_info) = Self::setup_context(state_size)?;
        let (remote, ctrl) = exchange::client_exchange(host, tcp_port, &local_info)?;
        Self::transition_qp(&ctx, &qp, psn, &remote)?;
        println!("[RdmaRemote] client ready — RDMA path established");
        Ok(RdmaRemote {
            ctx, mr, qp, ctrl,
            remote_addr: remote.addr,
            remote_rkey: remote.rkey,
            state_size,
        })
    }

    // ── Data-path ─────────────────────────────────────────────────────────────

    /// **Client**: write `data` directly into the server's registered memory
    /// via a one-sided RDMA WRITE, then signal the server over TCP.
    ///
    /// The server CPU is not involved during the RDMA WRITE itself — the HCA
    /// DMA-engines handle the transfer end-to-end.
    pub fn write_state(&mut self, data: &[u8]) -> Result<()> {
        let n = data.len().min(self.state_size);
        self.mr.as_bytes_mut()[..n].copy_from_slice(&data[..n]);

        self.qp.post_rdma_write(&self.mr, n as u32,
                                 self.remote_addr, self.remote_rkey)?;
        self.qp.poll_one_blocking()?;
        if verbose() {
            println!("[RdmaRemote] RDMA WRITE complete ({} bytes → remote)", n);
        }

        exchange::send_done(&mut self.ctrl)?;
        Ok(())
    }

    /// **Server**: block until the client signals that its RDMA WRITE is done.
    pub fn wait_peer_write(&mut self) -> Result<()> {
        exchange::wait_done(&mut self.ctrl)?;
        if verbose() {
            println!("[RdmaRemote] peer write signal received");
        }
        Ok(())
    }

    /// **Client (bandwidth)**: stream `iters` back-to-back one-sided RDMA WRITEs
    /// of `size` bytes into the peer's MR, pipelined to a fixed window depth.
    ///
    /// Unlike [`write_state`], this does NOT memcpy fresh data per iteration and
    /// does NOT send a per-write TCP signal — the registered buffer is sent
    /// as-is and only the HCA's send queue is exercised.  This isolates the raw
    /// one-directional wire bandwidth (the `ib_write_bw` measurement model) for
    /// the multi-node bandwidth benchmark; the caller times around this call.
    /// All writes target the same remote offset (the bytes are overwritten —
    /// integrity is irrelevant to a throughput test).
    pub fn stream_write(&mut self, size: usize, iters: usize) -> Result<()> {
        if iters == 0 {
            return Ok(());
        }
        let n = size.min(self.state_size) as u32;
        // Keep up to `depth` WRITEs in flight; every WR is signaled so each
        // completion lets us refill the window by one (a simple, correct
        // sliding window).  Bounded by the QP send-queue depth.
        let depth = 128.min(iters);
        for _ in 0..depth {
            self.qp.post_rdma_write(&self.mr, n, self.remote_addr, self.remote_rkey)?;
        }
        let mut posted = depth;
        let mut completed = 0;
        while completed < iters {
            self.qp.poll_one_blocking()?;
            completed += 1;
            if posted < iters {
                self.qp.post_rdma_write(&self.mr, n, self.remote_addr, self.remote_rkey)?;
                posted += 1;
            }
        }
        Ok(())
    }

    /// **Client (bandwidth)**: tell the receiver the whole stream is finished,
    /// so it can stop waiting and tear down.  Pairs with [`wait_peer_write`].
    pub fn signal_done(&mut self) -> Result<()> {
        exchange::send_done(&mut self.ctrl)
    }

    /// Read the current contents of this node's registered memory region.
    ///
    /// On the server this reflects what the client has written via RDMA WRITE.
    pub fn local_state(&self) -> &[u8] {
        self.mr.as_bytes()
    }

    /// Convenience: interpret local state as a UTF-8 string (lossy), trimming nulls.
    pub fn local_state_str(&self) -> std::borrow::Cow<'_, str> {
        let b = self.mr.as_bytes();
        let end = b.iter().position(|&x| x == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end])
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Data-path logging is off by default (it would dominate latency benchmarks).
/// Set `RDMA_VERBOSE=1` to re-enable the per-write diagnostics.
fn verbose() -> bool {
    std::env::var("RDMA_VERBOSE").map(|v| v != "0").unwrap_or(false)
}

/// Generate a random 24-bit PSN from the subsecond nanosecond clock.
fn rand_psn() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0x4321);
    ns & 0x00FF_FFFF
}
