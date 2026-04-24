//! Receiver-side MR2 storage — host-owned backing file for RDMA spillover.
//!
//! Created lazily the first time `recv_si` sees an incoming transfer that
//! wouldn't fit in MR1's remaining bump budget.  Registered with the NIC
//! (`ibv_reg_mr`), so peers can RDMA-WRITE into it using the (addr, rkey)
//! carried in `DestReply::UseMr2`.
//!
//! Guest-invisible by design — MR2 lives in the host's VA only, outside the
//! 2 GiB wasm32 direct window.  After an RDMA WRITE lands in MR2, the
//! receiver host CPU-copies the bytes out into a fresh MR1 page chain
//! before linking into the target slot, so guest consumers see no change.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};

use crate::rdma::context::RdmaContext;
use crate::rdma::memory_region::MemoryRegion;

/// Snapshot of the live MR2 registration — values safe to copy into
/// a `DestReply::UseMr2` reply over TCP.
#[derive(Clone, Copy, Debug)]
pub struct Mr2Info {
    pub addr: u64,
    pub rkey: u32,
    pub len:  u64,
}

/// A reservation inside the MR2 buffer for one in-flight transfer.
/// Returned by `MeshNode::mr2_reserve`.  The caller passes `addr` and
/// `rkey` back to the sender in the `DestReply::UseMr2` reply; after
/// the RDMA WRITE completes, the caller reads `len` bytes starting
/// at `host_ptr` to memcpy into the MR1 page chain.
pub struct Mr2Reservation {
    pub addr:     u64,           // absolute VA: mr_base + offset
    pub rkey:     u32,
    pub offset:   u64,           // offset within MR2
    pub len:      u64,
    // SAFETY: valid for `len` bytes while the parent Mr2Storage is live.
    host_ptr:     *mut u8,
}

unsafe impl Send for Mr2Reservation {}
unsafe impl Sync for Mr2Reservation {}

impl Mr2Reservation {
    /// Borrow the reservation's bytes as a host-side slice.  Safe to
    /// call after the RDMA WRITE has been acknowledged (`wait_done`).
    pub unsafe fn as_slice(&self) -> &[u8] {
        std::slice::from_raw_parts(self.host_ptr, self.len as usize)
    }
}

/// Host-side file-backed buffer + its libibverbs registration.
pub(in crate::mesh) struct Mr2Storage {
    path:   PathBuf,
    _file:  File,
    base:   *mut u8,
    len:    usize,
    _mr:    MemoryRegion,
    addr:   u64,
    rkey:   u32,
    bump:   AtomicU64,
    last_use: Mutex<Instant>,
}

// SAFETY: the base pointer refers to a mapping owned by this process;
// interior mutation is protected by atomics and the mutex on last_use.
unsafe impl Send for Mr2Storage {}
unsafe impl Sync for Mr2Storage {}

impl Mr2Storage {
    pub(in crate::mesh) fn new(ctx: &RdmaContext, path: PathBuf, size: usize) -> Result<Self> {
        if size == 0 {
            return Err(anyhow!("Mr2Storage size must be non-zero"));
        }
        let file = OpenOptions::new()
            .read(true).write(true).create(true).truncate(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        file.set_len(size as u64)
            .context("sizing MR2 backing file")?;

        // mmap(NULL, size, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0)
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(anyhow!("mmap MR2 backing file: {}", std::io::Error::last_os_error()));
        }
        let base = base as *mut u8;

        let mr = MemoryRegion::register_external(ctx, base, size)
            .context("register MR2 with NIC")?;
        let addr = mr.addr();
        let rkey = mr.rkey();

        eprintln!(
            "[MR2] created: path={} size={} MiB addr={:#x} rkey={:#x}",
            path.display(), size / (1024 * 1024), addr, rkey,
        );

        Ok(Self {
            path,
            _file: file,
            base,
            len: size,
            _mr: mr,
            addr,
            rkey,
            bump: AtomicU64::new(0),
            last_use: Mutex::new(Instant::now()),
        })
    }

    /// Reserve `size` contiguous bytes.  Returns None if MR2 is full.
    pub(in crate::mesh) fn reserve(&self, size: u64) -> Option<Mr2Reservation> {
        let off = self.bump.fetch_add(size, Ordering::AcqRel);
        if off + size > self.len as u64 {
            self.bump.fetch_sub(size, Ordering::AcqRel);
            return None;
        }
        Some(Mr2Reservation {
            addr:     self.addr + off,
            rkey:     self.rkey,
            offset:   off,
            len:      size,
            host_ptr: unsafe { self.base.add(off as usize) },
        })
    }

    /// Reset the bump allocator.  Caller must guarantee no in-flight
    /// transfer is still reading a region.
    pub(in crate::mesh) fn reset_bump(&self) {
        self.bump.store(0, Ordering::Release);
    }

    pub(in crate::mesh) fn info(&self) -> Mr2Info {
        Mr2Info { addr: self.addr, rkey: self.rkey, len: self.len as u64 }
    }

    pub(in crate::mesh) fn len(&self) -> usize { self.len }

    pub(in crate::mesh) fn touch(&self) {
        if let Ok(mut t) = self.last_use.lock() {
            *t = Instant::now();
        }
    }

    pub(in crate::mesh) fn idle_duration(&self) -> std::time::Duration {
        match self.last_use.lock() {
            Ok(t) => t.elapsed(),
            Err(_) => std::time::Duration::ZERO,
        }
    }
}

impl Drop for Mr2Storage {
    fn drop(&mut self) {
        // _mr Drop deregs the MR.  Then unmap and unlink the file.
        unsafe {
            let _ = libc::munmap(self.base as *mut _, self.len);
        }
        let _ = std::fs::remove_file(&self.path);
        eprintln!("[MR2] dropped (idle shrink or DAG end)");
    }
}
