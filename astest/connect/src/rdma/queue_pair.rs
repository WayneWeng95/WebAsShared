// RC Queue Pair lifecycle and one-sided RDMA WRITE operation.

use std::ptr::NonNull;

use anyhow::{anyhow, Result};

use crate::ffi::*;
use super::context::RdmaContext;
use super::exchange::QpInfo;
use super::memory_region::MemoryRegion;

/// Maximum SGEs per RDMA WRITE work request.
/// Most modern HCAs support ≥ 16; we request exactly this many at QP creation.
pub const MAX_SEND_SGE: usize = 16;

pub struct QueuePair {
    pub qp: NonNull<ibv_qp>,
    /// Dedicated completion queue for this QP — not shared with any other QP.
    cq:     NonNull<ibv_cq>,
}

unsafe impl Send for QueuePair {}
unsafe impl Sync for QueuePair {}

impl QueuePair {
    /// Allocate an RC QP in RESET state with its own dedicated CQ.
    pub fn create(ctx: &RdmaContext) -> Result<Self> {
        // Each QP owns its CQ so concurrent transfers to different peers
        // never steal each other's completions.
        let cq_ptr = unsafe {
            ibv_create_cq(ctx.ctx.as_ptr(), 16,
                          std::ptr::null_mut(), std::ptr::null_mut(), 0)
        };
        let cq = NonNull::new(cq_ptr)
            .ok_or_else(|| anyhow!("ibv_create_cq for QP failed"))?;

        let mut init_attr: ibv_qp_init_attr = unsafe { std::mem::zeroed() };
        init_attr.send_cq    = cq_ptr;
        init_attr.recv_cq    = cq_ptr;
        init_attr.qp_type    = IBV_QPT_RC;
        init_attr.sq_sig_all = 0;
        init_attr.cap.max_send_wr  = 16;
        init_attr.cap.max_recv_wr  = 16;
        init_attr.cap.max_send_sge = MAX_SEND_SGE as u32;
        init_attr.cap.max_recv_sge = 1;

        let qp_ptr = unsafe { ibv_create_qp(ctx.pd.as_ptr(), &mut init_attr) };
        let qp = NonNull::new(qp_ptr)
            .ok_or_else(|| anyhow!("ibv_create_qp failed"))?;

        println!("[RDMA] QP created: QPN={}", unsafe { (*qp_ptr).qp_num });
        Ok(QueuePair { qp, cq })
    }

    pub fn qpn(&self) -> u32 { unsafe { (*self.qp.as_ptr()).qp_num } }

    /// Transition RESET → INIT.
    pub fn to_init(&self, port: u8) -> Result<()> {
        let mut attr: ibv_qp_attr = unsafe { std::mem::zeroed() };
        attr.qp_state        = IBV_QPS_INIT;
        attr.pkey_index      = 0;
        attr.port_num        = port;
        attr.qp_access_flags = (IBV_ACCESS_LOCAL_WRITE
            | IBV_ACCESS_REMOTE_WRITE
            | IBV_ACCESS_REMOTE_READ
            | IBV_ACCESS_REMOTE_ATOMIC) as u32;

        let mask = IBV_QP_STATE | IBV_QP_PKEY_INDEX | IBV_QP_PORT | IBV_QP_ACCESS_FLAGS;
        let ret = unsafe { ibv_modify_qp(self.qp.as_ptr(), &mut attr, mask) };
        if ret != 0 { return Err(anyhow!("modify_qp→INIT failed: {}", ret)); }
        println!("[RDMA] QP → INIT");
        Ok(())
    }

    /// Transition INIT → RTR (Ready To Receive).
    pub fn to_rtr(&self, remote: &QpInfo, port: u8, gid_idx: u8) -> Result<()> {
        let mut attr: ibv_qp_attr = unsafe { std::mem::zeroed() };
        attr.qp_state           = IBV_QPS_RTR;
        attr.path_mtu           = IBV_MTU_1024;
        attr.dest_qp_num        = remote.qpn;
        attr.rq_psn             = remote.psn;
        attr.max_dest_rd_atomic = 1;
        attr.min_rnr_timer      = 12;

        // Address handle: GID-based routing for RoCE
        attr.ah_attr.grh.dgid.raw  = remote.gid;
        attr.ah_attr.grh.sgid_index   = gid_idx;
        attr.ah_attr.grh.hop_limit    = 1;
        attr.ah_attr.dlid             = remote.lid;
        attr.ah_attr.is_global        = 1;
        attr.ah_attr.port_num         = port;

        let mask = IBV_QP_STATE | IBV_QP_AV | IBV_QP_PATH_MTU | IBV_QP_DEST_QPN
                 | IBV_QP_RQ_PSN | IBV_QP_MAX_DEST_RD_ATOMIC | IBV_QP_MIN_RNR_TIMER;

        let ret = unsafe { ibv_modify_qp(self.qp.as_ptr(), &mut attr, mask) };
        if ret != 0 { return Err(anyhow!("modify_qp→RTR failed: {}", ret)); }
        println!("[RDMA] QP → RTR (peer QPN={})", remote.qpn);
        Ok(())
    }

    /// Transition RTR → RTS (Ready To Send).
    pub fn to_rts(&self, local_psn: u32) -> Result<()> {
        let mut attr: ibv_qp_attr = unsafe { std::mem::zeroed() };
        attr.qp_state      = IBV_QPS_RTS;
        attr.sq_psn        = local_psn;
        attr.timeout       = 14;
        attr.retry_cnt     = 7;
        attr.rnr_retry     = 7;
        attr.max_rd_atomic = 1;

        let mask = IBV_QP_STATE | IBV_QP_TIMEOUT | IBV_QP_RETRY_CNT
                 | IBV_QP_RNR_RETRY | IBV_QP_SQ_PSN | IBV_QP_MAX_QP_RD_ATOMIC;

        let ret = unsafe { ibv_modify_qp(self.qp.as_ptr(), &mut attr, mask) };
        if ret != 0 { return Err(anyhow!("modify_qp→RTS failed: {}", ret)); }
        println!("[RDMA] QP → RTS");
        Ok(())
    }

    /// Post a one-sided RDMA WRITE work request.
    ///
    /// Writes `len` bytes from `local_mr` directly into remote memory at
    /// `remote_addr` / `remote_rkey`.  `IBV_SEND_SIGNALED` causes a
    /// completion to appear on our CQ when the HCA finishes.
    pub fn post_rdma_write(
        &self,
        local_mr:    &MemoryRegion,
        len:          u32,
        remote_addr:  u64,
        remote_rkey:  u32,
    ) -> Result<()> {
        let mut sge: ibv_sge = unsafe { std::mem::zeroed() };
        sge.addr   = local_mr.addr();
        sge.length = len;
        sge.lkey   = local_mr.lkey();

        let mut wr: ibv_send_wr = unsafe { std::mem::zeroed() };
        wr.wr_id       = 1;
        wr.sg_list     = &mut sge;
        wr.num_sge     = 1;
        wr.opcode      = IBV_WR_RDMA_WRITE;
        wr.send_flags  = IBV_SEND_SIGNALED;
        wr.remote_addr = remote_addr;
        wr.compare_add = remote_rkey as u64; // low 32 bits = wr.rdma.rkey

        let mut bad_wr: *mut ibv_send_wr = std::ptr::null_mut();
        let ret = unsafe { wrap_ibv_post_send(self.qp.as_ptr(), &mut wr, &mut bad_wr) };
        if ret != 0 { return Err(anyhow!("ibv_post_send failed: {}", ret)); }
        Ok(())
    }

    /// Post a one-sided RDMA WRITE using explicit source address and lkey.
    ///
    /// Used by `MeshNode` to write from an offset within a larger MR without
    /// needing a separate `MemoryRegion` handle for each slot.
    pub fn post_rdma_write_raw(
        &self,
        src_addr:    u64,
        src_lkey:    u32,
        len:          u32,
        remote_addr:  u64,
        remote_rkey:  u32,
    ) -> Result<()> {
        let mut sge: ibv_sge = unsafe { std::mem::zeroed() };
        sge.addr   = src_addr;
        sge.length = len;
        sge.lkey   = src_lkey;

        let mut wr: ibv_send_wr = unsafe { std::mem::zeroed() };
        wr.wr_id       = 1;
        wr.sg_list     = &mut sge;
        wr.num_sge     = 1;
        wr.opcode      = IBV_WR_RDMA_WRITE;
        wr.send_flags  = IBV_SEND_SIGNALED;
        wr.remote_addr = remote_addr;
        wr.compare_add = remote_rkey as u64; // low 32 bits = wr.rdma.rkey

        let mut bad_wr: *mut ibv_send_wr = std::ptr::null_mut();
        let ret = unsafe { wrap_ibv_post_send(self.qp.as_ptr(), &mut wr, &mut bad_wr) };
        if ret != 0 { return Err(anyhow!("ibv_post_send failed: {}", ret)); }
        Ok(())
    }

    /// Post one RDMA WRITE work request covering `sges`.
    ///
    /// If `signaled` is true the HCA generates a CQ completion when done;
    /// pass false for intermediate WRs in a multi-WR chain and true only for
    /// the last one (RC QPs complete in order, so the single CQ entry for the
    /// last WR implies all preceding writes have also landed at the remote).
    pub fn post_rdma_write_sge_list(
        &self,
        sges:        &mut [ibv_sge],
        remote_addr: u64,
        remote_rkey: u32,
        signaled:    bool,
    ) -> Result<()> {
        assert!(
            sges.len() <= MAX_SEND_SGE,
            "SGE count {} exceeds MAX_SEND_SGE {}",
            sges.len(), MAX_SEND_SGE
        );

        let mut wr: ibv_send_wr = unsafe { std::mem::zeroed() };
        wr.wr_id       = 1;
        wr.sg_list     = sges.as_mut_ptr();
        wr.num_sge     = sges.len() as i32;
        wr.opcode      = IBV_WR_RDMA_WRITE;
        wr.send_flags  = if signaled { IBV_SEND_SIGNALED } else { 0 };
        wr.remote_addr = remote_addr;
        wr.compare_add = remote_rkey as u64; // low 32 bits = wr.rdma.rkey

        let mut bad_wr: *mut ibv_send_wr = std::ptr::null_mut();
        let ret = unsafe { wrap_ibv_post_send(self.qp.as_ptr(), &mut wr, &mut bad_wr) };
        if ret != 0 { return Err(anyhow!("ibv_post_send (SGE list) failed: {}", ret)); }
        Ok(())
    }

    /// Atomic Fetch-and-Add on a remote u64.
    ///
    /// Atomically adds `add_val` to the u64 at `remote_addr` (must be 8-byte
    /// aligned) and writes the *old* value into the local buffer at
    /// `result_addr` / `result_lkey`.  Completes with a CQ entry.
    pub fn post_fetch_and_add(
        &self,
        result_addr:  u64,
        result_lkey:  u32,
        remote_addr:  u64,
        remote_rkey:  u32,
        add_val:      u64,
    ) -> Result<()> {
        let mut sge: ibv_sge = unsafe { std::mem::zeroed() };
        sge.addr   = result_addr;
        sge.length = 8;
        sge.lkey   = result_lkey;

        let mut wr: ibv_send_wr = unsafe { std::mem::zeroed() };
        wr.wr_id       = 1;
        wr.sg_list     = &mut sge;
        wr.num_sge     = 1;
        wr.opcode      = IBV_WR_ATOMIC_FETCH_AND_ADD;
        wr.send_flags  = IBV_SEND_SIGNALED;
        wr.remote_addr = remote_addr;
        wr.compare_add = add_val;
        wr.atomic_rkey = remote_rkey;

        let mut bad_wr: *mut ibv_send_wr = std::ptr::null_mut();
        let ret = unsafe { wrap_ibv_post_send(self.qp.as_ptr(), &mut wr, &mut bad_wr) };
        if ret != 0 { return Err(anyhow!("ibv_post_send (FAA) failed: {}", ret)); }
        Ok(())
    }

    /// Atomic Compare-and-Swap on a remote u64.
    ///
    /// If the u64 at `remote_addr` equals `compare_val`, atomically replaces
    /// it with `swap_val`.  Either way, the *old* remote value is written into
    /// `result_addr` / `result_lkey`.
    pub fn post_compare_and_swap(
        &self,
        result_addr:   u64,
        result_lkey:   u32,
        remote_addr:   u64,
        remote_rkey:   u32,
        compare_val:   u64,
        swap_val:      u64,
    ) -> Result<()> {
        let mut sge: ibv_sge = unsafe { std::mem::zeroed() };
        sge.addr   = result_addr;
        sge.length = 8;
        sge.lkey   = result_lkey;

        let mut wr: ibv_send_wr = unsafe { std::mem::zeroed() };
        wr.wr_id       = 1;
        wr.sg_list     = &mut sge;
        wr.num_sge     = 1;
        wr.opcode      = IBV_WR_ATOMIC_CMP_AND_SWP;
        wr.send_flags  = IBV_SEND_SIGNALED;
        wr.remote_addr = remote_addr;
        wr.compare_add = compare_val;
        wr.swap        = swap_val;
        wr.atomic_rkey = remote_rkey;

        let mut bad_wr: *mut ibv_send_wr = std::ptr::null_mut();
        let ret = unsafe { wrap_ibv_post_send(self.qp.as_ptr(), &mut wr, &mut bad_wr) };
        if ret != 0 { return Err(anyhow!("ibv_post_send (CAS) failed: {}", ret)); }
        Ok(())
    }

    /// Busy-poll this QP's dedicated CQ until one completion arrives.
    pub fn poll_one_blocking(&self) -> Result<u64> {
        let mut wc: ibv_wc = unsafe { std::mem::zeroed() };
        loop {
            let n = unsafe { wrap_ibv_poll_cq(self.cq.as_ptr(), 1, &mut wc) };
            if n < 0  { return Err(anyhow!("ibv_poll_cq error: {}", n)); }
            if n == 0 { std::hint::spin_loop(); continue; }
            if wc.status != IBV_WC_SUCCESS {
                return Err(anyhow!(
                    "completion error status={} vendor_err={:#x}",
                    wc.status, wc.vendor_err
                ));
            }
            return Ok(wc.wr_id);
        }
    }
}

impl Drop for QueuePair {
    fn drop(&mut self) {
        // QP must be destroyed before its CQ.
        unsafe {
            ibv_destroy_qp(self.qp.as_ptr());
            ibv_destroy_cq(self.cq.as_ptr());
        }
    }
}
