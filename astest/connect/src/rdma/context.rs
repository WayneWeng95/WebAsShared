// RDMA device context: wraps ibv_context and ibv_pd.
// CQ is no longer shared here — each QueuePair creates and owns its own CQ.

use std::ffi::CStr;
use std::ptr::NonNull;

use anyhow::{anyhow, Result};

use crate::ffi::*;

pub struct RdmaContext {
    pub ctx: NonNull<ibv_context>,
    pub pd:  NonNull<ibv_pd>,
}

unsafe impl Send for RdmaContext {}
unsafe impl Sync for RdmaContext {}

impl RdmaContext {
    /// Open an RDMA device and allocate a PD.
    /// `dev_name = None` picks the first available device.
    /// `_cq_size` is accepted for backward compatibility but ignored.
    pub fn new(dev_name: Option<&str>, _cq_size: i32) -> Result<Self> {
        let mut num_devices = 0i32;
        let dev_list = unsafe { ibv_get_device_list(&mut num_devices) };
        if dev_list.is_null() || num_devices == 0 {
            return Err(anyhow!("no RDMA devices found"));
        }

        let dev = (0..num_devices as isize).find_map(|i| {
            let d = unsafe { *dev_list.offset(i) };
            match dev_name {
                None => Some(d),
                Some(wanted) => {
                    let raw = unsafe { ibv_get_device_name(d) };
                    let s = unsafe { CStr::from_ptr(raw).to_str().unwrap_or("") };
                    if s == wanted { Some(d) } else { None }
                }
            }
        });

        unsafe { ibv_free_device_list(dev_list) };

        let dev = dev.ok_or_else(|| anyhow!("RDMA device {:?} not found", dev_name))?;

        let ctx_ptr = unsafe { ibv_open_device(dev) };
        let ctx = NonNull::new(ctx_ptr)
            .ok_or_else(|| anyhow!("ibv_open_device failed"))?;

        let pd_ptr = unsafe { ibv_alloc_pd(ctx.as_ptr()) };
        let pd = NonNull::new(pd_ptr)
            .ok_or_else(|| anyhow!("ibv_alloc_pd failed"))?;

        println!("[RDMA] context ready (pd={:p})", pd_ptr);
        Ok(RdmaContext { ctx, pd })
    }

    /// Query port attributes (LID, state, etc.).
    pub fn query_port(&self, port: u8) -> Result<ibv_port_attr> {
        let mut attr = unsafe { std::mem::zeroed::<ibv_port_attr>() };
        let ret = unsafe { wrap_ibv_query_port(self.ctx.as_ptr(), port, &mut attr) };
        if ret != 0 {
            return Err(anyhow!("ibv_query_port port={} failed: {}", port, ret));
        }
        Ok(attr)
    }

    /// Query GID at `gid_index` on `port` — used for RoCE addressing.
    pub fn query_gid(&self, port: u8, gid_index: i32) -> Result<ibv_gid> {
        let mut gid = unsafe { std::mem::zeroed::<ibv_gid>() };
        let ret = unsafe {
            ibv_query_gid(self.ctx.as_ptr(), port as u32, gid_index, &mut gid)
        };
        if ret != 0 {
            return Err(anyhow!("ibv_query_gid failed: {}", ret));
        }
        Ok(gid)
    }
}

impl Drop for RdmaContext {
    fn drop(&mut self) {
        unsafe {
            ibv_dealloc_pd(self.pd.as_ptr());
            ibv_close_device(self.ctx.as_ptr());
        }
    }
}
