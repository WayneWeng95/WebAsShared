// Basic data-path operations on MeshNode.
//
// Covers the simple slot-level RDMA WRITE + TCP done-signal pattern,
// blocking wait for peer writes, slot reads, and u32 framing helpers.

use anyhow::{anyhow, Result};

use crate::rdma::exchange;
use super::{MeshNode, SLOT_SIZE};

impl MeshNode {
    /// RDMA WRITE `data` directly into `peer_id`'s memory, then signal done.
    pub fn write_to(&mut self, peer_id: usize, data: &[u8]) -> Result<()> {
        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;

        let n = data.len().min(SLOT_SIZE);
        let my_slot_offset = self.id * SLOT_SIZE;
        self.mr.as_bytes_mut()[my_slot_offset..my_slot_offset + n]
            .copy_from_slice(&data[..n]);

        let src_addr = self.mr.addr() + my_slot_offset as u64;
        let src_lkey = self.mr.lkey();

        {
            let qp = link.qp.lock().unwrap();
            qp.post_rdma_write_raw(src_addr, src_lkey, n as u32,
                                   link.remote_slot_addr, link.remote_rkey)?;
            qp.poll_one_blocking()?;
        }

        exchange::send_done(&mut *link.ctrl_as_sender.lock().unwrap())?;
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
        exchange::wait_done(&mut *link.ctrl_as_receiver.lock().unwrap())?;
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

    /// Trim null bytes and return peer's slot as a UTF-8 string (lossy).
    pub fn slot_str(&self, peer_id: usize) -> std::borrow::Cow<'_, str> {
        let b = self.slot(peer_id);
        let end = b.iter().position(|&x| x == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end])
    }

    /// Send a one-byte TCP "done" signal to `peer_id` on the sender stream.
    pub fn signal_peer(&mut self, peer_id: usize) -> Result<()> {
        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;
        exchange::send_done(&mut *link.ctrl_as_sender.lock().unwrap())
    }

    /// Send a `u32` (little-endian) to `peer_id` on the sender control stream.
    pub fn send_u32_to(&mut self, peer_id: usize, val: u32) -> Result<()> {
        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;
        exchange::send_u32(&mut *link.ctrl_as_sender.lock().unwrap(), val)
    }

    /// Receive a `u32` (little-endian) from `peer_id` on the receiver control stream.
    pub fn recv_u32_from(&mut self, peer_id: usize) -> Result<u32> {
        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;
        exchange::recv_u32(&mut *link.ctrl_as_receiver.lock().unwrap())
    }
}
