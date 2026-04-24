// TCP side-channel for exchanging Queue Pair metadata.
//
// RDMA connections require both sides to know each other's:
//   - QPN  (queue pair number)
//   - PSN  (initial packet sequence number)
//   - GID  (global identifier — used for RoCE / routed IB)
//   - LID  (local identifier — used for native IB; 0 for pure RoCE)
//   - rkey (remote key protecting the registered memory region)
//   - addr (virtual address of the registered memory region)
//   - len  (size of the registered memory region)
//
// A TCP socket is the standard "out-of-band" path for this metadata swap
// before the RDMA data-path is established.  After exchange(), both sides
// call ibv_modify_qp to RTR and RTS using the peer's values.
//
// Wire format: two consecutive 46-byte little-endian fixed structs,
// first written by the server, then by the client (or vice versa).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use anyhow::{anyhow, Result};

/// All information a peer needs to target our QP and memory region.
#[derive(Clone, Copy, Debug)]
pub struct QpInfo {
    pub qpn:  u32,
    pub psn:  u32,
    pub gid:  [u8; 16],
    pub lid:  u16,
    pub rkey: u32,
    pub addr: u64,
    pub len:  u32,
}

impl QpInfo {
    const WIRE_LEN: usize = 46; // 4+4+16+2+4+8+4+4 (pad) = 46

    fn to_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut b = [0u8; Self::WIRE_LEN];
        b[0..4].copy_from_slice(&self.qpn.to_le_bytes());
        b[4..8].copy_from_slice(&self.psn.to_le_bytes());
        b[8..24].copy_from_slice(&self.gid);
        b[24..26].copy_from_slice(&self.lid.to_le_bytes());
        // 2 bytes padding at 26..28
        b[28..32].copy_from_slice(&self.rkey.to_le_bytes());
        b[32..40].copy_from_slice(&self.addr.to_le_bytes());
        b[40..44].copy_from_slice(&self.len.to_le_bytes());
        // 2 bytes padding at 44..46
        b
    }

    fn from_bytes(b: &[u8; Self::WIRE_LEN]) -> Self {
        let qpn  = u32::from_le_bytes(b[0..4].try_into().unwrap());
        let psn  = u32::from_le_bytes(b[4..8].try_into().unwrap());
        let gid: [u8; 16] = b[8..24].try_into().unwrap();
        let lid  = u16::from_le_bytes(b[24..26].try_into().unwrap());
        let rkey = u32::from_le_bytes(b[28..32].try_into().unwrap());
        let addr = u64::from_le_bytes(b[32..40].try_into().unwrap());
        let len  = u32::from_le_bytes(b[40..44].try_into().unwrap());
        QpInfo { qpn, psn, gid, lid, rkey, addr, len }
    }
}

/// Server side: listen on `tcp_port`, accept one connection, swap QP info.
///
/// Returns (remote_info, stream).  The caller keeps `stream` open to use
/// as a control channel (e.g. to send a "done" signal after the RDMA write).
pub fn server_exchange(tcp_port: u16, local: &QpInfo) -> Result<(QpInfo, TcpStream)> {
    let listener = TcpListener::bind(("0.0.0.0", tcp_port))
        .map_err(|e| anyhow!("bind {}:{}: {}", "0.0.0.0", tcp_port, e))?;
    println!("[exchange] server listening on :{}", tcp_port);

    let (mut stream, peer) = listener.accept()
        .map_err(|e| anyhow!("accept: {}", e))?;
    println!("[exchange] client connected from {}", peer);

    // server sends first, then reads
    stream.write_all(&local.to_bytes())?;
    stream.flush()?;

    let mut buf = [0u8; QpInfo::WIRE_LEN];
    stream.read_exact(&mut buf)?;
    let remote = QpInfo::from_bytes(&buf);
    println!("[exchange] remote QPN={} PSN={} rkey={:#x} addr={:#x}",
             remote.qpn, remote.psn, remote.rkey, remote.addr);
    Ok((remote, stream))
}

/// Client side: connect to `host:tcp_port` and swap QP info.
/// Retries for up to 30 seconds to allow the peer executor to start.
pub fn client_exchange(host: &str, tcp_port: u16, local: &QpInfo) -> Result<(QpInfo, TcpStream)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut stream = loop {
        match TcpStream::connect((host, tcp_port)) {
            Ok(s) => break s,
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow!("connect {}:{}: {} (timed out after 30s)", host, tcp_port, e));
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    };
    println!("[exchange] connected to {}:{}", host, tcp_port);

    // read server's info first, then send ours
    let mut buf = [0u8; QpInfo::WIRE_LEN];
    stream.read_exact(&mut buf)?;
    let remote = QpInfo::from_bytes(&buf);
    println!("[exchange] remote QPN={} PSN={} rkey={:#x} addr={:#x}",
             remote.qpn, remote.psn, remote.rkey, remote.addr);

    stream.write_all(&local.to_bytes())?;
    stream.flush()?;
    Ok((remote, stream))
}

/// Send a one-byte "done" signal over the control channel.
pub fn send_done(stream: &mut TcpStream) -> Result<()> {
    stream.write_all(&[0xFFu8])?;
    Ok(stream.flush()?)
}

/// Block until the peer sends its "done" signal.
pub fn wait_done(stream: &mut TcpStream) -> Result<()> {
    let mut b = [0u8; 1];
    stream.read_exact(&mut b)?;
    Ok(())
}

/// Send a u32 (little-endian) over the control channel.
pub fn send_u32(stream: &mut TcpStream, val: u32) -> Result<()> {
    stream.write_all(&val.to_le_bytes())?;
    Ok(stream.flush()?)
}

/// Receive a u32 (little-endian) from the control channel.
pub fn recv_u32(stream: &mut TcpStream) -> Result<u32> {
    let mut b = [0u8; 4];
    stream.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// Send a [`ShmOffset`] (little-endian) over the control channel.
/// Under wasm32 this sends 4 bytes; under wasm64 it sends 8 bytes.
pub fn send_shm_offset(stream: &mut TcpStream, val: common::ShmOffset) -> Result<()> {
    stream.write_all(&val.to_le_bytes())?;
    Ok(stream.flush()?)
}

/// Receive a [`ShmOffset`] (little-endian) from the control channel.
/// Under wasm32 this reads 4 bytes; under wasm64 it reads 8 bytes.
pub fn recv_shm_offset(stream: &mut TcpStream) -> Result<common::ShmOffset> {
    let mut b = [0u8; core::mem::size_of::<common::ShmOffset>()];
    stream.read_exact(&mut b)?;
    Ok(common::ShmOffset::from_le_bytes(b))
}

// ── Phase-2 SI handshake reply ───────────────────────────────────────────────
//
// The receiver tells the sender where the RDMA WRITE should land:
//
//   SingleMr — page chain allocated inside the existing shared MR (MR1).
//              Sender uses the peer's MR1 rkey (known from QpInfo exchange).
//
//   UseMr2   — region allocated inside a separately-registered host-side
//              MR2 backing file.  Sender must target (addr, rkey) from this
//              reply; the peer's MR1 rkey does not cover MR2.  After the
//              RDMA WRITE completes, the receiver's host memcpys the data
//              out of MR2 into a fresh MR1 page chain before the guest
//              reads the slot.

#[derive(Clone, Copy, Debug)]
pub enum DestReply {
    SingleMr { dest_off: common::ShmOffset },
    UseMr2   { dest_off: u64, addr: u64, rkey: u32 },
}

const DEST_TAG_SINGLE_MR: u8 = 0;
const DEST_TAG_USE_MR2:   u8 = 1;

pub fn send_dest_reply(stream: &mut TcpStream, reply: &DestReply) -> Result<()> {
    match *reply {
        DestReply::SingleMr { dest_off } => {
            stream.write_all(&[DEST_TAG_SINGLE_MR])?;
            stream.write_all(&dest_off.to_le_bytes())?;
        }
        DestReply::UseMr2 { dest_off, addr, rkey } => {
            stream.write_all(&[DEST_TAG_USE_MR2])?;
            stream.write_all(&dest_off.to_le_bytes())?;
            stream.write_all(&addr.to_le_bytes())?;
            stream.write_all(&rkey.to_le_bytes())?;
        }
    }
    Ok(stream.flush()?)
}

pub fn recv_dest_reply(stream: &mut TcpStream) -> Result<DestReply> {
    let mut tag = [0u8; 1];
    stream.read_exact(&mut tag)?;
    match tag[0] {
        DEST_TAG_SINGLE_MR => {
            let mut b = [0u8; core::mem::size_of::<common::ShmOffset>()];
            stream.read_exact(&mut b)?;
            Ok(DestReply::SingleMr { dest_off: common::ShmOffset::from_le_bytes(b) })
        }
        DEST_TAG_USE_MR2 => {
            let mut off = [0u8; 8]; stream.read_exact(&mut off)?;
            let mut a   = [0u8; 8]; stream.read_exact(&mut a)?;
            let mut r   = [0u8; 4]; stream.read_exact(&mut r)?;
            Ok(DestReply::UseMr2 {
                dest_off: u64::from_le_bytes(off),
                addr:     u64::from_le_bytes(a),
                rkey:     u32::from_le_bytes(r),
            })
        }
        other => Err(anyhow!("recv_dest_reply: unknown tag {}", other)),
    }
}

/// Server side: listen on `tcp_port`, accept one connection, return the stream.
///
/// Used for the reverse-direction control channel where no QP metadata
/// exchange is needed — just a plain TCP socket for serialised u32 messages.
pub fn server_ctrl_only(tcp_port: u16) -> Result<TcpStream> {
    let listener = TcpListener::bind(("0.0.0.0", tcp_port))
        .map_err(|e| anyhow!("bind :{}: {}", tcp_port, e))?;
    println!("[exchange] ctrl-only server listening on :{}", tcp_port);
    let (stream, peer) = listener.accept()
        .map_err(|e| anyhow!("accept: {}", e))?;
    println!("[exchange] ctrl-only client connected from {}", peer);
    Ok(stream)
}

/// Client side: connect to `host:tcp_port`, return the stream.
/// Retries for up to 30 seconds to allow the peer executor to start.
pub fn client_ctrl_only(host: &str, tcp_port: u16) -> Result<TcpStream> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let stream = loop {
        match TcpStream::connect((host, tcp_port)) {
            Ok(s) => break s,
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow!("connect {}:{}: {} (timed out after 30s)", host, tcp_port, e));
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    };
    println!("[exchange] ctrl-only connected to {}:{}", host, tcp_port);
    Ok(stream)
}
