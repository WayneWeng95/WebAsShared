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
const RDMA_PORT: u8 = 1;
/// GID index for RoCE (index 0 = link-local GID on most mlx4/mlx5 setups).
const GID_INDEX: u8 = 0;
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
        println!("[RdmaRemote] RDMA WRITE complete ({} bytes → remote)", n);

        exchange::send_done(&mut self.ctrl)?;
        Ok(())
    }

    /// **Server**: block until the client signals that its RDMA WRITE is done.
    pub fn wait_peer_write(&mut self) -> Result<()> {
        exchange::wait_done(&mut self.ctrl)?;
        println!("[RdmaRemote] peer write signal received");
        Ok(())
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

/// Generate a random 24-bit PSN from the subsecond nanosecond clock.
fn rand_psn() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0x4321);
    ns & 0x00FF_FFFF
}
