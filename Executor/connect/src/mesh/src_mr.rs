//! Sender-side extension MRs — let `collect_src_sges` build valid SGEs
//! for pages that live outside the 64 MiB MR1 coverage.
//!
//! There are two scenarios on the producer side:
//!
//! 1. **Direct-mode page at SHM offset ≥ INITIAL_SHM_SIZE.**
//!    The guest's bump allocator has grown SHM past 64 MiB via
//!    `host_remap`, so the page still lives in the SHM mmap (at an
//!    address the guest can dereference), but the NIC's MR1
//!    registration does not cover it.  `SrcExtStorage` registers a
//!    second MR over `[shm_base + INITIAL_SHM_SIZE, shm_base + covered)`
//!    so the page's lkey resolves correctly.  Re-registered with a
//!    larger length when the bump grows further.
//!
//! 2. **Paged-mode page (PageId ≥ DIRECT_LIMIT).**
//!    The page is in `GlobalPool`, a separate host file outside SHM
//!    entirely.  The sender host CPU-copies its data into a dedicated
//!    registered staging buffer, then emits the SGE pointing into that
//!    buffer.  `SrcStageStorage` is the backing for this — a file-
//!    backed mmap + `ibv_reg_mr`, bump-allocated per transfer and
//!    reset at the end.
//!
//! Both are lazily created on first need and torn down via idle-timeout
//! or on `MeshNode::drop`.  Local-only registrations (no REMOTE_*
//! flags) are sufficient — the NIC just needs to translate source
//! lkeys on outgoing RDMA WRITEs.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};

use crate::rdma::context::RdmaContext;
use crate::rdma::memory_region::MemoryRegion;

// ── MR-src-ext — covers SHM past MR1 ─────────────────────────────────────────

pub(in crate::mesh) struct SrcExtStorage {
    _mr:           MemoryRegion,
    lkey:          u32,
    /// Absolute address of the first byte this MR covers
    /// (= shm_base + INITIAL_SHM_SIZE).
    covered_start: u64,
    /// Number of bytes currently covered.  Grown on demand by
    /// deregistering and re-registering with a larger length.
    pub(in crate::mesh) covered_len: usize,
    last_use:      Mutex<Instant>,
}

unsafe impl Send for SrcExtStorage {}
unsafe impl Sync for SrcExtStorage {}

impl SrcExtStorage {
    pub(in crate::mesh) fn new(
        ctx:       &RdmaContext,
        start_ptr: *mut u8,
        length:    usize,
    ) -> Result<Self> {
        if length == 0 {
            return Err(anyhow!("SrcExtStorage length must be non-zero"));
        }
        let mr = MemoryRegion::register_external(ctx, start_ptr, length)
            .context("register MR-src-ext with NIC")?;
        let lkey = mr.lkey();
        let addr = mr.addr();
        eprintln!(
            "[MR-src-ext] registered: addr={:#x} len={} MiB lkey={:#x}",
            addr, length / (1024 * 1024), lkey,
        );
        Ok(Self {
            _mr:           mr,
            lkey,
            covered_start: addr,
            covered_len:   length,
            last_use:      Mutex::new(Instant::now()),
        })
    }

    pub(in crate::mesh) fn covers(&self, absolute_addr: u64, len: u32) -> bool {
        let start = self.covered_start;
        let end   = start + self.covered_len as u64;
        absolute_addr >= start && absolute_addr + len as u64 <= end
    }

    pub(in crate::mesh) fn lkey(&self) -> u32 { self.lkey }

    pub(in crate::mesh) fn touch(&self) {
        if let Ok(mut t) = self.last_use.lock() { *t = Instant::now(); }
    }

    pub(in crate::mesh) fn idle_duration(&self) -> std::time::Duration {
        match self.last_use.lock() {
            Ok(t) => t.elapsed(),
            Err(_) => std::time::Duration::ZERO,
        }
    }
}

// ── MR-src-stage — host-side buffer for paged-mode source pages ─────────────

pub(in crate::mesh) struct SrcStageStorage {
    path:     PathBuf,
    _file:    File,
    base:     *mut u8,
    len:      usize,
    _mr:      MemoryRegion,
    addr:     u64,
    lkey:     u32,
    bump:     AtomicU64,
    last_use: Mutex<Instant>,
}

unsafe impl Send for SrcStageStorage {}
unsafe impl Sync for SrcStageStorage {}

impl SrcStageStorage {
    pub(in crate::mesh) fn new(ctx: &RdmaContext, path: PathBuf, size: usize) -> Result<Self> {
        if size == 0 {
            return Err(anyhow!("SrcStageStorage size must be non-zero"));
        }
        let file = OpenOptions::new()
            .read(true).write(true).create(true).truncate(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        file.set_len(size as u64)
            .context("sizing MR-src-stage backing file")?;

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
            return Err(anyhow!("mmap MR-src-stage: {}", std::io::Error::last_os_error()));
        }
        let base = base as *mut u8;

        let mr = MemoryRegion::register_external(ctx, base, size)
            .context("register MR-src-stage with NIC")?;
        let addr = mr.addr();
        let lkey = mr.lkey();

        eprintln!(
            "[MR-src-stage] created: path={} size={} MiB addr={:#x} lkey={:#x}",
            path.display(), size / (1024 * 1024), addr, lkey,
        );

        Ok(Self {
            path,
            _file: file,
            base,
            len: size,
            _mr: mr,
            addr,
            lkey,
            bump: AtomicU64::new(0),
            last_use: Mutex::new(Instant::now()),
        })
    }

    /// Memcpy `src_bytes` into the stage buffer at the next bump offset.
    /// Returns the staging `(addr, lkey)` for SGE construction, or None
    /// if the stage is full.
    pub(in crate::mesh) fn stage(&self, src_bytes: &[u8]) -> Option<(u64, u32)> {
        let size = src_bytes.len() as u64;
        let off  = self.bump.fetch_add(size, Ordering::AcqRel);
        if off + size > self.len as u64 {
            self.bump.fetch_sub(size, Ordering::AcqRel);
            return None;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                src_bytes.as_ptr(),
                self.base.add(off as usize),
                src_bytes.len(),
            );
        }
        Some((self.addr + off, self.lkey))
    }

    pub(in crate::mesh) fn reset_bump(&self) {
        self.bump.store(0, Ordering::Release);
    }

    pub(in crate::mesh) fn len(&self) -> usize { self.len }

    pub(in crate::mesh) fn touch(&self) {
        if let Ok(mut t) = self.last_use.lock() { *t = Instant::now(); }
    }

    pub(in crate::mesh) fn idle_duration(&self) -> std::time::Duration {
        match self.last_use.lock() {
            Ok(t) => t.elapsed(),
            Err(_) => std::time::Duration::ZERO,
        }
    }
}

impl Drop for SrcStageStorage {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::munmap(self.base as *mut _, self.len);
        }
        let _ = std::fs::remove_file(&self.path);
        eprintln!("[MR-src-stage] dropped");
    }
}
