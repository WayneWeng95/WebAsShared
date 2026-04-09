// RDMA atomic operations — Fetch-and-Add and Compare-and-Swap.
//
// Two surfaces:
//   AtomicChannel — thread-safe handle obtained via MeshNode::atomic_channel;
//                   operates on a fixed (result_shm_offset, remote_shm_offset) pair.
//   MeshNode SHM  — methods that target any 8-byte-aligned offset in the full
//                   SHM MR (registered via connect_all_on_shm).  Safe to call
//                   from spawned threads via a shared reference.

use std::sync::atomic;

use anyhow::{anyhow, Result};
use common::ShmOffset;

use super::{AtomicChannel, MeshNode, SLOT_SIZE};

// ── AtomicChannel operations ──────────────────────────────────────────────────

impl AtomicChannel {
    /// RDMA Fetch-and-Add on `remote_shm_offset` in the peer's SHM.
    ///
    /// Result (old value) is written to `result_shm_offset` in our SHM by the NIC.
    /// Returns the old value read from that scratch slot after the completion.
    pub fn rdma_fetch_add(
        &self,
        remote_shm_offset: ShmOffset,
        result_shm_offset: ShmOffset,
        add_val:           u64,
    ) -> Result<u64> {
        let result_addr = self.local_mr_base  + result_shm_offset as u64;
        let remote_addr = self.remote_mr_base + remote_shm_offset as u64;

        {
            let qp = self.qp.lock().unwrap();
            qp.post_fetch_and_add(result_addr, self.local_lkey,
                                   remote_addr, self.remote_rkey, add_val)?;
            qp.poll_one_blocking()?;
        }

        atomic::fence(atomic::Ordering::Acquire);
        let old = unsafe { (result_addr as *const u64).read_volatile() };
        Ok(old)
    }

    /// RDMA Compare-and-Swap on `remote_shm_offset` in the peer's SHM.
    ///
    /// Swaps iff remote value == `compare`.  Result (old value) written to
    /// `result_shm_offset` in our SHM.  Returns the old value.
    pub fn rdma_compare_swap(
        &self,
        remote_shm_offset: ShmOffset,
        result_shm_offset: ShmOffset,
        compare:           u64,
        swap:              u64,
    ) -> Result<u64> {
        let result_addr = self.local_mr_base  + result_shm_offset as u64;
        let remote_addr = self.remote_mr_base + remote_shm_offset as u64;

        {
            let qp = self.qp.lock().unwrap();
            qp.post_compare_and_swap(result_addr, self.local_lkey,
                                      remote_addr, self.remote_rkey,
                                      compare, swap)?;
            qp.poll_one_blocking()?;
        }

        atomic::fence(atomic::Ordering::Acquire);
        let old = unsafe { (result_addr as *const u64).read_volatile() };
        Ok(old)
    }
}

// ── MeshNode SHM-targeted RDMA atomics ───────────────────────────────────────
//
// These methods target any 8-byte-aligned offset in the full SHM MR
// (registered via connect_all_on_shm).  The result is written to
// `result_shm_offset` in OUR local SHM — use common::rdma_scratch_shm_offset
// to compute a race-free per-(self.id, peer_id) slot.
//
// Both methods take &self so they can be called from spawned threads that
// hold a shared reference to the MeshNode.

impl MeshNode {
    /// RDMA Fetch-and-Add on any 8-byte-aligned location in `peer_id`'s SHM.
    pub fn rdma_fetch_add_shm(
        &self,
        peer_id:           usize,
        remote_shm_offset: ShmOffset,
        result_shm_offset: ShmOffset,
        add_val:           u64,
    ) -> Result<u64> {
        assert_eq!(remote_shm_offset % 8, 0, "remote_shm_offset must be 8-byte aligned");
        assert_eq!(result_shm_offset % 8, 0, "result_shm_offset must be 8-byte aligned");

        let link = self.peers.get(&peer_id)
            .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;

        let result_addr = self.mr.addr() + result_shm_offset as u64;
        let result_lkey = self.mr.lkey();
        let remote_addr = link.remote_mr_base + remote_shm_offset as u64;
        let remote_rkey = link.remote_rkey;

        {
            let qp = link.qp.lock().unwrap();
            qp.post_fetch_and_add(result_addr, result_lkey, remote_addr, remote_rkey, add_val)?;
            qp.poll_one_blocking()?;
        }

        atomic::fence(atomic::Ordering::Acquire);
        let off = result_shm_offset as usize;
        let old = u64::from_ne_bytes(self.mr.as_bytes()[off..off + 8].try_into().unwrap());
        println!(
            "[mesh] node {} → node {}: FAA shm+{:#x} += {} (old={})",
            self.id, peer_id, remote_shm_offset, add_val, old
        );
        Ok(old)
    }

    /// RDMA Compare-and-Swap on any 8-byte-aligned location in `peer_id`'s SHM.
    pub fn rdma_compare_swap_shm(
        &self,
        peer_id:           usize,
        remote_shm_offset: ShmOffset,
        result_shm_offset: ShmOffset,
        compare:           u64,
        swap:              u64,
    ) -> Result<u64> {
        assert_eq!(remote_shm_offset % 8, 0, "remote_shm_offset must be 8-byte aligned");
        assert_eq!(result_shm_offset % 8, 0, "result_shm_offset must be 8-byte aligned");

        let link = self.peers.get(&peer_id)
            .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;

        let result_addr = self.mr.addr() + result_shm_offset as u64;
        let result_lkey = self.mr.lkey();
        let remote_addr = link.remote_mr_base + remote_shm_offset as u64;
        let remote_rkey = link.remote_rkey;

        {
            let qp = link.qp.lock().unwrap();
            qp.post_compare_and_swap(
                result_addr, result_lkey,
                remote_addr, remote_rkey,
                compare, swap,
            )?;
            qp.poll_one_blocking()?;
        }

        atomic::fence(atomic::Ordering::Acquire);
        let off = result_shm_offset as usize;
        let old = u64::from_ne_bytes(self.mr.as_bytes()[off..off + 8].try_into().unwrap());
        let swapped = old == compare;
        println!(
            "[mesh] node {} → node {}: CAS shm+{:#x} ({} → {}) {}",
            self.id, peer_id, remote_shm_offset, compare, swap,
            if swapped { "swapped" } else { "not swapped" }
        );
        Ok(old)
    }

    /// Atomic Fetch-and-Add on a u64 at `byte_offset` inside `peer_id`'s MR slot.
    pub fn fetch_and_add(&mut self, peer_id: usize, byte_offset: usize, add_val: u64)
        -> Result<u64>
    {
        assert!(byte_offset % 8 == 0, "byte_offset must be 8-byte aligned");
        assert!(byte_offset + 8 <= SLOT_SIZE);

        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;

        let result_addr = self.mr.addr() + (self.id * SLOT_SIZE) as u64;
        let result_lkey = self.mr.lkey();
        let remote_addr = link.remote_mr_base + byte_offset as u64;
        let remote_rkey = link.remote_rkey;

        {
            let qp = link.qp.lock().unwrap();
            qp.post_fetch_and_add(result_addr, result_lkey,
                                   remote_addr, remote_rkey, add_val)?;
            qp.poll_one_blocking()?;
        }

        atomic::fence(atomic::Ordering::Acquire);
        let old_val = u64::from_ne_bytes(
            self.mr.as_bytes()[self.id * SLOT_SIZE..self.id * SLOT_SIZE + 8]
                .try_into().unwrap()
        );
        Ok(old_val)
    }

    /// Atomic Compare-and-Swap on a u64 at `byte_offset` inside `peer_id`'s MR slot.
    pub fn compare_and_swap(&mut self, peer_id: usize, byte_offset: usize,
                             compare_val: u64, swap_val: u64)
        -> Result<u64>
    {
        assert!(byte_offset % 8 == 0, "byte_offset must be 8-byte aligned");
        assert!(byte_offset + 8 <= SLOT_SIZE);

        let link = self.peers.get_mut(&peer_id)
            .ok_or_else(|| anyhow!("no connection to peer {}", peer_id))?;

        let result_addr = self.mr.addr() + (self.id * SLOT_SIZE) as u64;
        let result_lkey = self.mr.lkey();
        let remote_addr = link.remote_mr_base + byte_offset as u64;
        let remote_rkey = link.remote_rkey;

        {
            let qp = link.qp.lock().unwrap();
            qp.post_compare_and_swap(result_addr, result_lkey,
                                      remote_addr, remote_rkey,
                                      compare_val, swap_val)?;
            qp.poll_one_blocking()?;
        }

        atomic::fence(atomic::Ordering::Acquire);
        let old_val = u64::from_ne_bytes(
            self.mr.as_bytes()[self.id * SLOT_SIZE..self.id * SLOT_SIZE + 8]
                .try_into().unwrap()
        );
        Ok(old_val)
    }
}
