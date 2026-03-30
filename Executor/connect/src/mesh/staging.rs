// Advanced RDMA write operations on MeshNode.
//
// Three patterns:
//   rdma_write_sge      — scatter-gather RDMA WRITE from multiple local SGEs.
//   rdma_write_staging  — write from a pre-known staging offset, no TCP signal.
//   rdma_signal_staging — FAA-based "data ready" signal after a staging write.
//   wait_staging        — poll atomic or fall back to TCP to detect readiness.
//   rdma_faa_only       — send FAA increment without a backup TCP signal.
//   rdma_write_to_page_chain — page-chain aware scatter write (legacy utility).

use std::sync::atomic;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::ffi::ibv_sge;
use crate::rdma::exchange;
use crate::rdma::queue_pair::MAX_SEND_SGE;
use super::MeshNode;

impl MeshNode {
    /// RDMA-WRITE from a scatter list of local SHM regions to a contiguous range
    /// in `peer_id`'s SHM starting at `remote_off`.
    pub fn rdma_write_sge(
        &mut self,
        peer_id:    usize,
        remote_off: u64,
        sge_pairs:  &[(u64, u32)],
    ) -> Result<()> {
        let lkey = self.mr.lkey();
        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;

        let remote_base = link.remote_mr_base + remote_off;
        let remote_rkey = link.remote_rkey;

        let chunks: Vec<&[(u64, u32)]> = sge_pairs.chunks(MAX_SEND_SGE).collect();
        let n_chunks = chunks.len();
        let mut remote_cursor = remote_base;

        let qp = link.qp.lock().unwrap();
        for (i, chunk) in chunks.iter().enumerate() {
            let is_last = i == n_chunks - 1;
            let mut sges: Vec<ibv_sge> = chunk.iter().map(|&(addr, len)| {
                let mut s: ibv_sge = unsafe { std::mem::zeroed() };
                s.addr   = addr;
                s.length = len;
                s.lkey   = lkey;
                s
            }).collect();
            qp.post_rdma_write_sge_list(&mut sges, remote_cursor, remote_rkey, is_last)?;
            remote_cursor += chunk.iter().map(|&(_, l)| l as u64).sum::<u64>();
        }
        qp.poll_one_blocking()?;

        let total: u32 = sge_pairs.iter().map(|&(_, l)| l).sum();
        println!(
            "[mesh] node {} → node {}: scatter RDMA WRITE {} bytes ({} SGEs, {} WRs)",
            self.id, peer_id, total, sge_pairs.len(), n_chunks
        );
        Ok(())
    }

    /// RDMA-WRITE `len` bytes from our SHM at `staging_offset` into
    /// `peer_id`'s SHM at the same offset.  Data only — no TCP signal.
    pub fn rdma_write_staging(
        &mut self,
        peer_id:        usize,
        staging_offset: u64,
        len:            u32,
    ) -> Result<()> {
        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;

        let src_addr = self.mr.addr() + staging_offset;
        let src_lkey = self.mr.lkey();
        let dst_addr = link.remote_mr_base + staging_offset;
        let dst_rkey = link.remote_rkey;

        {
            let qp = link.qp.lock().unwrap();
            qp.post_rdma_write_raw(src_addr, src_lkey, len, dst_addr, dst_rkey)?;
            qp.poll_one_blocking()?;
        }
        println!(
            "[mesh] node {} → node {}: staging RDMA WRITE {} bytes done",
            self.id, peer_id, len
        );
        Ok(())
    }

    /// Signal `peer_id` that our RDMA WRITE into its staging area is complete.
    ///
    /// Uses RDMA FAA to increment the peer's counter at `staging_offset`, then
    /// sends a TCP done signal as a fallback for peers that poll via TCP.
    pub fn rdma_signal_staging(
        &mut self,
        peer_id:        usize,
        staging_offset: u64,
    ) -> Result<()> {
        let (remote_mr_base, remote_rkey, lkey, mr_addr) = {
            let link = self.peers.get_mut(&peer_id)
                .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;
            (link.remote_mr_base, link.remote_rkey, self.mr.lkey(), self.mr.addr())
        };

        let result_addr = mr_addr + staging_offset;
        let remote_addr = remote_mr_base + staging_offset;

        {
            let link = self.peers.get_mut(&peer_id).unwrap();
            let qp = link.qp.lock().unwrap();
            qp.post_fetch_and_add(result_addr, lkey, remote_addr, remote_rkey, 1)?;
            qp.poll_one_blocking()?;
        }

        atomic::fence(atomic::Ordering::Release);

        let link = self.peers.get_mut(&peer_id).unwrap();
        exchange::send_done(&mut *link.ctrl_as_sender.lock().unwrap())?;
        println!("[mesh] node {} → node {}: staging signal sent", self.id, peer_id);
        Ok(())
    }

    /// Block until `peer_id`'s ready counter becomes non-zero, then fence.
    ///
    /// Spins on the atomic for up to 100 ms, then falls back to a TCP wait.
    pub fn wait_staging(
        &mut self,
        peer_id:     usize,
        counter_ptr: *const atomic::AtomicU64,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            let v = unsafe { (*counter_ptr).load(atomic::Ordering::Acquire) };
            if v != 0 {
                println!("[mesh] node {} ← node {}: staging ready (RDMA atomic)", self.id, peer_id);
                return Ok(());
            }
            if Instant::now() >= deadline { break; }
            std::hint::spin_loop();
        }

        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;
        exchange::wait_done(&mut *link.ctrl_as_receiver.lock().unwrap())?;
        atomic::fence(atomic::Ordering::Acquire);
        println!("[mesh] node {} ← node {}: staging ready (TCP fallback)", self.id, peer_id);
        Ok(())
    }

    /// RDMA Fetch-and-Add without a TCP backup signal.
    pub fn rdma_faa_only(
        &mut self,
        peer_id:       usize,
        remote_offset: u64,
    ) -> Result<()> {
        let (remote_mr_base, remote_rkey, lkey, mr_addr) = {
            let link = self.peers.get_mut(&peer_id)
                .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;
            (link.remote_mr_base, link.remote_rkey, self.mr.lkey(), self.mr.addr())
        };

        let result_addr = mr_addr + remote_offset;
        let remote_addr = remote_mr_base + remote_offset;

        let link = self.peers.get_mut(&peer_id).unwrap();
        {
            let qp = link.qp.lock().unwrap();
            qp.post_fetch_and_add(result_addr, lkey, remote_addr, remote_rkey, 1)?;
            qp.poll_one_blocking()?;
        }

        atomic::fence(atomic::Ordering::Release);
        println!("[mesh] node {} → node {}: FAA signal sent (no TCP)", self.id, peer_id);
        Ok(())
    }

    /// RDMA-WRITE from scattered source SGEs into a pre-structured page chain.
    /// Kept for callers that still use MeshNode directly (non-threaded path).
    pub fn rdma_write_to_page_chain(
        &mut self,
        peer_id:         usize,
        src_sges:        &[(u64, u32)],
        remote_dest_off: u64,
        total_bytes:     u32,
    ) -> Result<()> {
        const PAGE_SIZE:   u64 = common::PAGE_SIZE as u64;
        const PAGE_HEADER: u64 = 8;
        const PAGE_DATA:   u32 = (PAGE_SIZE - PAGE_HEADER) as u32;

        let n_dest_pages = (total_bytes + PAGE_DATA - 1) / PAGE_DATA;

        let (remote_mr_base, remote_rkey, lkey) = {
            let link = self.peers.get_mut(&peer_id)
                .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;
            (link.remote_mr_base, link.remote_rkey, self.mr.lkey())
        };

        let mut src_iter   = src_sges.iter();
        let mut src_addr   = 0u64;
        let mut src_remain = 0u32;
        if let Some(&(a, l)) = src_iter.next() { src_addr = a; src_remain = l; }

        let link = self.peers.get_mut(&peer_id).unwrap();
        let qp = link.qp.lock().unwrap();

        for dest_idx in 0..n_dest_pages {
            let dest_chunk = PAGE_DATA.min(total_bytes - dest_idx * PAGE_DATA);
            let remote_addr = remote_mr_base
                + remote_dest_off
                + dest_idx as u64 * PAGE_SIZE
                + PAGE_HEADER;
            let is_last = dest_idx == n_dest_pages - 1;

            let mut sges: [ibv_sge; 2] = unsafe { std::mem::zeroed() };
            let mut n_sge = 0usize;
            let mut to_fill = dest_chunk;

            while to_fill > 0 {
                if src_remain == 0 {
                    if let Some(&(a, l)) = src_iter.next() { src_addr = a; src_remain = l; }
                    else { break; }
                }
                let take = to_fill.min(src_remain);
                sges[n_sge].addr   = src_addr;
                sges[n_sge].length = take;
                sges[n_sge].lkey   = lkey;
                n_sge += 1;
                src_addr   += take as u64;
                src_remain -= take;
                to_fill    -= take;
            }

            qp.post_rdma_write_sge_list(
                &mut sges[..n_sge], remote_addr, remote_rkey, is_last,
            )?;
        }

        qp.poll_one_blocking()?;
        println!(
            "[mesh] node {} → node {}: page-chain RDMA WRITE {} bytes ({} pages, zero-copy)",
            self.id, peer_id, total_bytes, n_dest_pages
        );
        Ok(())
    }
}
