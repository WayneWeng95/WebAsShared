// Registered memory region for RDMA operations.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr::NonNull;

use anyhow::{anyhow, Result};

use crate::ffi::*;
use super::context::RdmaContext;

pub struct MemoryRegion {
    mr:     NonNull<ibv_mr>,
    buf:    NonNull<u8>,
    layout: Layout,
}

unsafe impl Send for MemoryRegion {}
unsafe impl Sync for MemoryRegion {}

impl MemoryRegion {
    /// Register an externally-owned buffer (e.g. a mmap'd SHM region) as an MR.
    ///
    /// The caller retains ownership of the memory — `Drop` will deregister the
    /// MR with the HCA but will NOT free the buffer (since we didn't allocate it).
    /// `ptr` must remain valid and pinned for the lifetime of this `MemoryRegion`.
    pub fn register_external(ctx: &RdmaContext, ptr: *mut u8, size: usize) -> Result<Self> {
        let access = (IBV_ACCESS_LOCAL_WRITE
            | IBV_ACCESS_REMOTE_WRITE
            | IBV_ACCESS_REMOTE_READ
            | IBV_ACCESS_REMOTE_ATOMIC) as i32;

        let mr_ptr = unsafe {
            ibv_reg_mr(ctx.pd.as_ptr(), ptr as *mut _, size, access)
        };
        if mr_ptr.is_null() {
            let errno = std::io::Error::last_os_error();
            return Err(anyhow!("ibv_reg_mr failed for external buffer (addr={:p} size={} errno={})", ptr, size, errno));
        }
        let mr  = unsafe { NonNull::new_unchecked(mr_ptr) };
        // Use a dangling non-null layout so Drop skips dealloc.
        let buf    = unsafe { NonNull::new_unchecked(ptr) };
        let layout = Layout::from_size_align(0, 1).unwrap(); // size=0 signals external

        println!("[RDMA] external MR registered: addr={:p} len={} lkey={:#x} rkey={:#x}",
                 ptr, size,
                 unsafe { (*mr_ptr).lkey },
                 unsafe { (*mr_ptr).rkey });

        Ok(MemoryRegion { mr, buf, layout })
    }

    /// Allocate `size` bytes and register them with `ctx` for local + remote access.
    pub fn alloc_and_register(ctx: &RdmaContext, size: usize) -> Result<Self> {
        let layout = Layout::from_size_align(size, 4096)
            .map_err(|e| anyhow!("layout error: {}", e))?;

        let buf_ptr = unsafe { alloc_zeroed(layout) };
        let buf = NonNull::new(buf_ptr)
            .ok_or_else(|| anyhow!("allocation failed ({} bytes)", size))?;

        let access = IBV_ACCESS_LOCAL_WRITE
            | IBV_ACCESS_REMOTE_WRITE
            | IBV_ACCESS_REMOTE_READ
            | IBV_ACCESS_REMOTE_ATOMIC;

        let mr_ptr = unsafe {
            ibv_reg_mr(ctx.pd.as_ptr(), buf_ptr as *mut _, size, access)
        };
        if mr_ptr.is_null() {
            unsafe { dealloc(buf_ptr, layout) };
            return Err(anyhow!("ibv_reg_mr failed (size={})", size));
        }
        let mr = unsafe { NonNull::new_unchecked(mr_ptr) };

        println!("[RDMA] MR registered: addr={:#x} len={} lkey={:#x} rkey={:#x}",
                 buf_ptr as u64, size,
                 unsafe { (*mr_ptr).lkey },
                 unsafe { (*mr_ptr).rkey });

        Ok(MemoryRegion { mr, buf, layout })
    }

    pub fn addr(&self)  -> u64   { self.buf.as_ptr() as u64 }
    pub fn len(&self)   -> usize { self.layout.size() }
    pub fn lkey(&self)  -> u32   { unsafe { (*self.mr.as_ptr()).lkey } }
    pub fn rkey(&self)  -> u32   { unsafe { (*self.mr.as_ptr()).rkey } }

    pub fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.buf.as_ptr(), self.layout.size()) }
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.buf.as_ptr(), self.layout.size()) }
    }
}

impl Drop for MemoryRegion {
    fn drop(&mut self) {
        unsafe {
            ibv_dereg_mr(self.mr.as_ptr());
            // layout.size() == 0 means the buffer is externally owned; skip dealloc.
            if self.layout.size() > 0 {
                dealloc(self.buf.as_ptr(), self.layout);
            }
        }
    }
}
