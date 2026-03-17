// Minimal hand-written FFI bindings for libibverbs.
//
// All struct layouts were verified with offsetof/sizeof probes against
// rdma-core 50.0 (the version installed on this system).  Only the fields
// we actually touch are named; the rest are represented as padding arrays.
//
// Opaque handle types (ibv_context, ibv_pd, ibv_cq, ibv_device, ibv_srq)
// are declared as empty enums — the canonical Rust idiom for "C opaque type"
// that can only be used through pointers.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::c_void;

// ─── Opaque handle types ──────────────────────────────────────────────────────

pub enum ibv_context {}
pub enum ibv_pd       {}
pub enum ibv_cq       {}
pub enum ibv_device   {}
pub enum ibv_srq      {}
pub enum ibv_ah       {}
pub enum ibv_comp_channel {}

// ─── ibv_sge (16 bytes) ───────────────────────────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ibv_sge {
    pub addr:   u64,  // +0
    pub length: u32,  // +8
    pub lkey:   u32,  // +12
}

// ─── ibv_gid (16 bytes) ───────────────────────────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ibv_gid {
    pub raw: [u8; 16],
}

// ─── ibv_global_route (24 bytes) ─────────────────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ibv_global_route {
    pub dgid:          ibv_gid,  // +0  (16 bytes)
    pub flow_label:    u32,      // +16
    pub sgid_index:    u8,       // +20
    pub hop_limit:     u8,       // +21
    pub traffic_class: u8,       // +22
    _pad:              u8,       // +23
}

// ─── ibv_ah_attr (32 bytes) ───────────────────────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ibv_ah_attr {
    pub grh:           ibv_global_route, // +0  (24 bytes)
    pub dlid:          u16,              // +24
    pub sl:            u8,               // +26
    pub src_path_bits: u8,               // +27
    pub static_rate:   u8,               // +28
    pub is_global:     u8,               // +29
    pub port_num:      u8,               // +30
    _pad:              u8,               // +31
}

// ─── ibv_qp_cap (20 bytes) ───────────────────────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ibv_qp_cap {
    pub max_send_wr:     u32,  // +0
    pub max_recv_wr:     u32,  // +4
    pub max_send_sge:    u32,  // +8
    pub max_recv_sge:    u32,  // +12
    pub max_inline_data: u32,  // +16
}

// ─── ibv_qp_init_attr (64 bytes) ─────────────────────────────────────────────

#[repr(C)]
pub struct ibv_qp_init_attr {
    pub qp_context: *mut c_void,         // +0
    pub send_cq:    *mut ibv_cq,         // +8
    pub recv_cq:    *mut ibv_cq,         // +16
    pub srq:        *mut ibv_srq,        // +24
    pub cap:        ibv_qp_cap,          // +32 (20 bytes)
    pub qp_type:    u32,                 // +52 (ibv_qp_type)
    pub sq_sig_all: i32,                 // +56
    _pad:           u32,                 // +60
}

// ibv_qp_type
pub const IBV_QPT_RC: u32 = 2;

// ─── ibv_qp_attr (144 bytes) ─────────────────────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ibv_qp_attr {
    pub qp_state:            u32,          // +0
    pub cur_qp_state:        u32,          // +4
    pub path_mtu:            u32,          // +8
    pub path_mig_state:      u32,          // +12
    pub qkey:                u32,          // +16
    pub rq_psn:              u32,          // +20
    pub sq_psn:              u32,          // +24
    pub dest_qp_num:         u32,          // +28
    pub qp_access_flags:     u32,          // +32
    pub cap:                 ibv_qp_cap,   // +36 (20 bytes)
    pub ah_attr:             ibv_ah_attr,  // +56 (32 bytes)
    pub alt_ah_attr:         ibv_ah_attr,  // +88 (32 bytes)
    pub pkey_index:          u16,          // +120
    pub alt_pkey_index:      u16,          // +122
    pub en_sqd_async_notify: u8,           // +124
    pub sq_draining:         u8,           // +125
    pub max_rd_atomic:       u8,           // +126
    pub max_dest_rd_atomic:  u8,           // +127
    pub min_rnr_timer:       u8,           // +128
    pub port_num:            u8,           // +129
    pub timeout:             u8,           // +130
    pub retry_cnt:           u8,           // +131
    pub rnr_retry:           u8,           // +132
    pub alt_port_num:        u8,           // +133
    pub alt_timeout:         u8,           // +134
    _pad1:                   u8,           // +135
    pub rate_limit:          u32,          // +136
    _pad2:                   u32,          // +140
}

// ibv_qp_state
pub const IBV_QPS_RESET: u32 = 0;
pub const IBV_QPS_INIT:  u32 = 1;
pub const IBV_QPS_RTR:   u32 = 2;
pub const IBV_QPS_RTS:   u32 = 3;

// ibv_mtu
pub const IBV_MTU_1024: u32 = 3;

// ibv_qp_attr_mask — ORed into the attr_mask argument of ibv_modify_qp
pub const IBV_QP_STATE:              i32 = 1 << 0;
pub const IBV_QP_ACCESS_FLAGS:       i32 = 1 << 3;
pub const IBV_QP_PKEY_INDEX:         i32 = 1 << 4;
pub const IBV_QP_PORT:               i32 = 1 << 5;
pub const IBV_QP_AV:                 i32 = 1 << 7;
pub const IBV_QP_PATH_MTU:           i32 = 1 << 8;
pub const IBV_QP_TIMEOUT:            i32 = 1 << 9;
pub const IBV_QP_RETRY_CNT:          i32 = 1 << 10;
pub const IBV_QP_RNR_RETRY:          i32 = 1 << 11;
pub const IBV_QP_RQ_PSN:             i32 = 1 << 12;
pub const IBV_QP_MAX_QP_RD_ATOMIC:   i32 = 1 << 13;
pub const IBV_QP_MIN_RNR_TIMER:      i32 = 1 << 15;
pub const IBV_QP_SQ_PSN:             i32 = 1 << 16;
pub const IBV_QP_MAX_DEST_RD_ATOMIC: i32 = 1 << 17;
pub const IBV_QP_DEST_QPN:           i32 = 1 << 20;

// ibv_access_flags
pub const IBV_ACCESS_LOCAL_WRITE:  i32 = 1 << 0;
pub const IBV_ACCESS_REMOTE_WRITE: i32 = 1 << 1;
pub const IBV_ACCESS_REMOTE_READ:  i32 = 1 << 2;

// ibv_send_flags
pub const IBV_SEND_SIGNALED: u32 = 1 << 1;

// ibv_wr_opcode
pub const IBV_WR_RDMA_WRITE:           u32 = 0;
pub const IBV_WR_ATOMIC_CMP_AND_SWP:   u32 = 5;
pub const IBV_WR_ATOMIC_FETCH_AND_ADD: u32 = 6;

// ibv_access_flags (additional)
pub const IBV_ACCESS_REMOTE_ATOMIC: i32 = 1 << 3;

// ─── ibv_send_wr (128 bytes) ──────────────────────────────────────────────────
//
// The wr union at +40 covers both rdma and atomic variants:
//
//   RDMA WRITE  : remote_addr(+40) compare_add_as_rkey(+48, low 32 bits only)
//   ATOMIC FAA  : remote_addr(+40) compare_add(+48, u64)  atomic_rkey(+64)
//   ATOMIC CAS  : remote_addr(+40) compare_add(+48, u64)  swap(+56)  atomic_rkey(+64)
//
// For RDMA WRITE: set compare_add = rkey as u64 (little-endian puts rkey in
// the correct +48..+51 byte position, matching wr.rdma.rkey in the C struct).

#[repr(C)]
pub struct ibv_send_wr {
    pub wr_id:       u64,              // +0
    pub next:        *mut ibv_send_wr, // +8
    pub sg_list:     *mut ibv_sge,     // +16
    pub num_sge:     i32,              // +24
    pub opcode:      u32,              // +28
    pub send_flags:  u32,              // +32
    _imm:            u32,              // +36  (imm_data / invalidate_rkey)
    pub remote_addr: u64,              // +40  (wr.rdma.remote_addr / wr.atomic.remote_addr)
    pub compare_add: u64,              // +48  (wr.atomic.compare_add; low 32 bits = wr.rdma.rkey)
    pub swap:        u64,              // +56  (wr.atomic.swap)
    pub atomic_rkey: u32,              // +64  (wr.atomic.rkey)
    _tail:           [u8; 60],         // +68..+128
}

// ─── ibv_wc (48 bytes) ────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ibv_wc {
    pub wr_id:      u64,  // +0
    pub status:     u32,  // +8   (ibv_wc_status)
    pub opcode:     u32,  // +12
    pub vendor_err: u32,  // +16
    pub byte_len:   u32,  // +20
    _tail:          [u8; 24], // +24..+48
}

// ibv_wc_status
pub const IBV_WC_SUCCESS: u32 = 0;

// ─── ibv_port_attr (56 bytes) ────────────────────────────────────────────────

#[repr(C)]
pub struct ibv_port_attr {
    _head: [u8; 34],  // fields before lid
    pub lid: u16,     // +34
    _tail: [u8; 20],  // remaining fields to fill 56 bytes
}

// ─── ibv_mr (48 bytes) ───────────────────────────────────────────────────────

#[repr(C)]
pub struct ibv_mr {
    _head: [u8; 36],
    pub lkey: u32,    // +36
    pub rkey: u32,    // +40
    _tail: [u8; 4],   // +44..+48
}

// ─── ibv_qp (160 bytes) ──────────────────────────────────────────────────────

#[repr(C)]
pub struct ibv_qp {
    _head:       [u8; 52],
    pub qp_num:  u32,        // +52
    _tail:       [u8; 104],
}

// ─── extern "C" declarations ─────────────────────────────────────────────────

extern "C" {
    // Device enumeration
    pub fn ibv_get_device_list(num_devices: *mut i32) -> *mut *mut ibv_device;
    pub fn ibv_free_device_list(list: *mut *mut ibv_device);
    pub fn ibv_get_device_name(device: *mut ibv_device) -> *const i8;

    // Context lifecycle
    pub fn ibv_open_device(device: *mut ibv_device) -> *mut ibv_context;
    pub fn ibv_close_device(context: *mut ibv_context) -> i32;

    // Protection domain
    pub fn ibv_alloc_pd(context: *mut ibv_context) -> *mut ibv_pd;
    pub fn ibv_dealloc_pd(pd: *mut ibv_pd) -> i32;

    // Completion queue
    pub fn ibv_create_cq(
        context:    *mut ibv_context,
        cqe:        i32,
        cq_context: *mut c_void,
        channel:    *mut ibv_comp_channel,
        comp_vector: i32,
    ) -> *mut ibv_cq;
    pub fn ibv_destroy_cq(cq: *mut ibv_cq) -> i32;

    // Memory registration
    pub fn ibv_reg_mr(pd: *mut ibv_pd, addr: *mut c_void, length: usize, access: i32) -> *mut ibv_mr;
    pub fn ibv_dereg_mr(mr: *mut ibv_mr) -> i32;

    // Queue pair
    pub fn ibv_create_qp(pd: *mut ibv_pd, qp_init_attr: *mut ibv_qp_init_attr) -> *mut ibv_qp;
    pub fn ibv_destroy_qp(qp: *mut ibv_qp) -> i32;
    pub fn ibv_modify_qp(qp: *mut ibv_qp, attr: *mut ibv_qp_attr, attr_mask: i32) -> i32;

    // Port / GID query
    pub fn ibv_query_gid(
        context:  *mut ibv_context,
        port_num: u32,
        index:    i32,
        gid:      *mut ibv_gid,
    ) -> i32;

    // Wrappers for static-inline functions (compiled in ibverbs_helpers.c)
    pub fn wrap_ibv_query_port(
        ctx:  *mut ibv_context,
        port: u8,
        attr: *mut ibv_port_attr,
    ) -> i32;
    pub fn wrap_ibv_post_send(
        qp:     *mut ibv_qp,
        wr:     *mut ibv_send_wr,
        bad_wr: *mut *mut ibv_send_wr,
    ) -> i32;
    pub fn wrap_ibv_poll_cq(cq: *mut ibv_cq, ne: i32, wc: *mut ibv_wc) -> i32;
}
