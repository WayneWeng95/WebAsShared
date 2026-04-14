//! RDMA pool — dedicated MR2 staging pool for Path C of the extended
//! RDMA integration.
//!
//! The `RdmaPool` is **structurally identical** to [`super::global_pool::GlobalPool`]:
//! same reserve-once / commit-incrementally virtual-address model,
//! same doubling growth strategy, same `MADV_DONTNEED` trim
//! mechanism.  It differs only in:
//!
//! - **Backing file path**: `/dev/shm/webs-rdma-<pid>` so the two
//!   pools never collide on the same tmpfs entry.
//! - **Initial / hard-limit sizes**: drawn from
//!   `common::RDMA_MR2_INITIAL_SIZE` / `common::RDMA_MR2_HARD_LIMIT`
//!   (512 MiB starting, 16 GiB ceiling) rather than the general
//!   pool's 256 MiB / 8 GiB.
//! - **Intended consumer**: the RDMA data plane, not the general
//!   allocator.  In Path C, received data is RDMA-written directly
//!   into this pool, then host-side CPU-copied into MR1 direct pages
//!   before the chain is linked into any slot — so the pool holds
//!   transient staging bytes, never permanent chain state.
//! - **MR registration lifecycle** (future): an `RdmaPool` is the
//!   target of a second `ibv_reg_mr` call that runs at flip time
//!   and whose rkey is broadcast to every peer via the mesh's
//!   existing TCP control channel.  The registration step lives in
//!   `connect/` and is not in this file — this file is just the
//!   host-side storage layer.
//!
//! The two pool types deliberately are **not** unified today because
//! Path C is scaffolded-only: the `RdmaPool` exists for future use,
//! and refactoring both pools onto a shared `PoolFile` base now
//! would couple their lifecycles before we know exactly how Path C
//! wants to interact with mesh registration.  A future unification
//! pass is a sensible follow-up when Path C is actually exercised.

use anyhow::{anyhow, Context, Result};
use common::{DIRECT_LIMIT, PAGE_SIZE, PageId, RDMA_MR2_HARD_LIMIT as _MR2_HARD_LIMIT_U64};

/// `common::RDMA_MR2_HARD_LIMIT` is `u64` (for wasm32 compat).  The
/// host-side code uses it as a `usize` length throughout; do the
/// cast once at the boundary and alias for readability.
const RDMA_MR2_HARD_LIMIT: usize = _MR2_HARD_LIMIT_U64 as usize;
use nix::sys::mman::{madvise, mmap, munmap, MapFlags, MmapAdvise, ProtFlags};
use std::fs::{File, OpenOptions};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

/// RDMA MR2 PageId marker bit.  Pool offsets `o ∈ [0, HARD_LIMIT)`
/// are encoded as `PageId = RDMA_MR2_MARKER + o`.  The marker is
/// large enough (2^48) to sit far above any direct-window PageId
/// and far above any realistic general extended-pool PageId
/// (`GlobalPool` tops out at 8 GiB = 2^33), so the three PageId
/// ranges are trivially distinguishable by comparison.
///
/// Guests that accidentally `as usize` such a PageId in wasm32 will
/// truncate to the low 32 bits, which bears no resemblance to a
/// valid offset and will trip an out-of-bounds fault on dereference.
/// Guests are not supposed to see MR2 PageIds in the first place —
/// Path C staging runs entirely host-side and copies to MR1 before
/// any slot sees a chain.
pub const RDMA_MR2_MARKER: PageId = 1u64 << 48;

/// The host-side RDMA overflow pool.
///
/// Single-threaded access expected: each OS process owns at most one
/// `RdmaPool` accessed through `rdma_pool::runtime::lock()`.  The
/// singleton mutex serializes concurrent `alloc` / `free` calls.
pub struct RdmaPool {
    path: PathBuf,
    file: File,
    /// Base of the virtual-address reservation — never changes.
    base: *mut u8,
    /// Bytes currently overlaid by the backing file.
    committed: usize,
    /// Next unallocated byte within the committed region.
    bump: u64,
    /// Freed MR2 PageIds available for reuse (LIFO).
    freelist: Vec<PageId>,
}

// SAFETY: same argument as `GlobalPool` — the base pointer refers to
// a mapping owned by this process; cross-thread access is sound as
// long as higher-level code serializes method calls.
unsafe impl Send for RdmaPool {}
unsafe impl Sync for RdmaPool {}

impl RdmaPool {
    /// Create a new RDMA pool backed by `path` with `initial` bytes
    /// committed up front.  `initial` must be non-zero, a multiple
    /// of `PAGE_SIZE`, and `<= RDMA_MR2_HARD_LIMIT`.
    pub fn new(path: &Path, initial: usize) -> Result<Self> {
        if initial == 0 {
            return Err(anyhow!("RdmaPool initial size must be non-zero"));
        }
        if initial > RDMA_MR2_HARD_LIMIT {
            return Err(anyhow!(
                "RdmaPool initial size {} exceeds hard limit {}",
                initial, RDMA_MR2_HARD_LIMIT,
            ));
        }
        if initial % PAGE_SIZE as usize != 0 {
            return Err(anyhow!(
                "RdmaPool initial size must be a multiple of PAGE_SIZE ({})",
                PAGE_SIZE,
            ));
        }

        let file = OpenOptions::new()
            .read(true).write(true).create(true).truncate(true)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        file.set_len(initial as u64)
            .context("sizing RdmaPool backing file")?;

        // Reserve the full hard-limit VA range as PROT_NONE.  Same
        // reserve-once / commit-incrementally pattern as GlobalPool.
        let base = unsafe {
            mmap(
                None,
                NonZeroUsize::new(RDMA_MR2_HARD_LIMIT).unwrap(),
                ProtFlags::PROT_NONE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
                None::<&File>,
                0,
            )
            .context("reserving RdmaPool virtual address range")?
        } as *mut u8;

        unsafe {
            mmap(
                NonZeroUsize::new(base as usize),
                NonZeroUsize::new(initial).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED | MapFlags::MAP_FIXED,
                Some(&file),
                0,
            )
            .context("overlaying RdmaPool backing file")?;
        }

        Ok(Self {
            path: path.to_path_buf(),
            file,
            base,
            committed: initial,
            bump: 0,
            freelist: Vec::new(),
        })
    }

    /// Bump-allocate one page, growing the committed region by
    /// doubling if needed.  Returns a PageId `>= RDMA_MR2_MARKER`.
    pub fn alloc(&mut self) -> Result<PageId> {
        if let Some(id) = self.freelist.pop() {
            return Ok(id);
        }
        let ps = PAGE_SIZE as u64;
        if self.bump + ps > self.committed as u64 {
            self.expand()?;
        }
        let offset = self.bump;
        self.bump += ps;
        Ok(RDMA_MR2_MARKER + offset)
    }

    /// Bump-allocate `n` contiguous pages.  Used by the eventual
    /// `alloc_and_link` spillover path where the RDMA receiver needs
    /// a single contiguous destination region for a bulk DMA.
    /// Returns the PageId of the first page.
    pub fn alloc_contiguous(&mut self, n: usize) -> Result<PageId> {
        if n == 0 {
            return Err(anyhow!("alloc_contiguous(0) is a programming error"));
        }
        let ps = PAGE_SIZE as u64;
        let bytes = ps * n as u64;
        if self.bump + bytes > self.committed as u64 {
            // Grow until we fit or hit the hard limit.
            while self.bump + bytes > self.committed as u64 {
                self.expand()?;
            }
        }
        let offset = self.bump;
        self.bump += bytes;
        Ok(RDMA_MR2_MARKER + offset)
    }

    /// Return a PageId to the freelist.
    pub fn free(&mut self, id: PageId) {
        debug_assert!(
            id >= RDMA_MR2_MARKER,
            "RdmaPool::free given non-MR2 PageId {:#x}", id,
        );
        self.freelist.push(id);
    }

    /// Host-side virtual address of the [`Page`] identified by `id`.
    /// Valid for the lifetime of this pool — the base never moves.
    pub fn host_addr_of(&self, id: PageId) -> *mut common::Page {
        debug_assert!(id >= RDMA_MR2_MARKER);
        let offset = (id - RDMA_MR2_MARKER) as usize;
        debug_assert!(
            offset + PAGE_SIZE as usize <= self.committed,
            "host_addr_of for uncommitted offset {offset:#x} (committed {:#x})",
            self.committed,
        );
        unsafe { self.base.add(offset) as *mut common::Page }
    }

    /// Byte offset inside the backing file for the page identified
    /// by `id`.  Will be used by the eventual `ibv_reg_mr` + sender-
    /// side rkey branch: the RDMA WRITE's remote address is
    /// `mr2_remote_base + file_offset_of(id)`.
    #[inline]
    pub fn file_offset_of(&self, id: PageId) -> u64 {
        debug_assert!(id >= RDMA_MR2_MARKER);
        id - RDMA_MR2_MARKER
    }

    /// Borrow the backing file for eventual MR registration.
    #[inline]
    pub fn backing_file(&self) -> &File { &self.file }

    /// Pool base virtual address — will be passed to `ibv_reg_mr`
    /// once MR registration is wired up.
    #[inline]
    pub fn base_addr(&self) -> usize { self.base as usize }

    /// Current reservation size (never changes).
    #[inline]
    pub fn reservation_size(&self) -> usize { RDMA_MR2_HARD_LIMIT }

    /// Current committed byte count (may grow via `expand`).
    pub fn committed(&self) -> usize { self.committed }

    /// Current bump pointer (excludes freelist).
    pub fn bump(&self) -> u64 { self.bump }

    /// Number of PageIds in the freelist.
    pub fn free_count(&self) -> usize { self.freelist.len() }

    /// Release physical backing of every freelist page via
    /// `MADV_DONTNEED`.  Freelist length and reuse order are
    /// preserved; only the kernel's page cache is shrunk.
    pub fn trim_freelist(&mut self) -> usize {
        let mut advised = 0usize;
        for &id in &self.freelist {
            let offset = (id - RDMA_MR2_MARKER) as usize;
            if offset + PAGE_SIZE as usize > self.committed {
                continue;
            }
            let addr = unsafe { self.base.add(offset) } as *mut libc::c_void;
            unsafe {
                if madvise(addr, PAGE_SIZE as usize, MmapAdvise::MADV_DONTNEED).is_ok() {
                    advised += 1;
                }
            }
        }
        advised
    }

    /// Double the committed region, capped at `RDMA_MR2_HARD_LIMIT`.
    fn expand(&mut self) -> Result<()> {
        let new_committed = (self.committed * 2).min(RDMA_MR2_HARD_LIMIT);
        if new_committed == self.committed {
            return Err(anyhow!(
                "RdmaPool at hard limit ({} GiB) — cannot grow further",
                RDMA_MR2_HARD_LIMIT / (1024 * 1024 * 1024),
            ));
        }
        self.file.set_len(new_committed as u64)
            .context("growing RdmaPool backing file")?;

        let old = self.committed;
        let overlay_len = new_committed - old;
        let overlay_addr = unsafe { self.base.add(old) } as usize;

        unsafe {
            mmap(
                NonZeroUsize::new(overlay_addr),
                NonZeroUsize::new(overlay_len).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED | MapFlags::MAP_FIXED,
                Some(&self.file),
                old as i64,
            )
            .context("overlaying grown RdmaPool region")?;
        }
        self.committed = new_committed;
        Ok(())
    }
}

impl Drop for RdmaPool {
    fn drop(&mut self) {
        if !self.base.is_null() {
            unsafe {
                let _ = munmap(
                    self.base as *mut libc::c_void,
                    RDMA_MR2_HARD_LIMIT,
                );
            }
        }
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "webs-rdma-test-{}-{}-{}",
            std::process::id(),
            label,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ))
    }

    #[test]
    fn alloc_starts_from_marker() {
        let mut pool = RdmaPool::new(&test_path("starts_from_marker"), 4 * PAGE_SIZE as usize).unwrap();
        let a = pool.alloc().unwrap();
        let b = pool.alloc().unwrap();
        assert_eq!(a, RDMA_MR2_MARKER);
        assert_eq!(b, RDMA_MR2_MARKER + PAGE_SIZE as u64);
        assert!(a > DIRECT_LIMIT);
    }

    #[test]
    fn alloc_contiguous_returns_base_page_id() {
        let mut pool = RdmaPool::new(&test_path("contiguous"), 8 * PAGE_SIZE as usize).unwrap();
        let head = pool.alloc_contiguous(4).unwrap();
        // The next normal alloc should be just past the contiguous region.
        let next = pool.alloc().unwrap();
        assert_eq!(next, head + 4 * PAGE_SIZE as u64);
    }

    #[test]
    fn expansion_doubles_for_bulk_alloc_past_committed() {
        // Start at 2 pages, bulk-alloc 5 contiguous — must grow enough
        // to fit even if that means multiple doublings.
        let mut pool = RdmaPool::new(&test_path("bulk_grow"), 2 * PAGE_SIZE as usize).unwrap();
        assert_eq!(pool.committed(), 2 * PAGE_SIZE as usize);
        let _head = pool.alloc_contiguous(5).unwrap();
        // 2 → 4 → 8 pages committed (enough for 5).
        assert!(pool.committed() >= 5 * PAGE_SIZE as usize);
    }

    #[test]
    fn freelist_lifo_roundtrip() {
        let mut pool = RdmaPool::new(&test_path("lifo"), 4 * PAGE_SIZE as usize).unwrap();
        let a = pool.alloc().unwrap();
        let b = pool.alloc().unwrap();
        pool.free(a);
        pool.free(b);
        assert_eq!(pool.alloc().unwrap(), b);
        assert_eq!(pool.alloc().unwrap(), a);
    }

    #[test]
    fn host_addr_survives_expansion() {
        let mut pool = RdmaPool::new(&test_path("addr_survives"), 2 * PAGE_SIZE as usize).unwrap();
        let a = pool.alloc().unwrap();
        let ptr_before = pool.host_addr_of(a) as *mut u64;
        let sentinel: u64 = 0x12_34_AB_CD_00_00_00_00;
        unsafe { *ptr_before = sentinel; }
        for _ in 0..4 {
            let _ = pool.alloc().unwrap();
        }
        let ptr_after = pool.host_addr_of(a) as *const u64;
        assert_eq!(ptr_before as *const u64, ptr_after);
        unsafe { assert_eq!(*ptr_after, sentinel); }
    }

    #[test]
    fn file_offset_of_is_inverse_of_marker() {
        let pool = RdmaPool::new(&test_path("file_offset"), 2 * PAGE_SIZE as usize).unwrap();
        // Two theoretical ids.
        let id0 = RDMA_MR2_MARKER;
        let id1 = RDMA_MR2_MARKER + PAGE_SIZE as u64;
        assert_eq!(pool.file_offset_of(id0), 0);
        assert_eq!(pool.file_offset_of(id1), PAGE_SIZE as u64);
    }

    #[test]
    fn rdma_marker_is_safely_above_general_pool_range() {
        // Sanity: RDMA MR2 ids must not collide with the general
        // extended pool's PageId range (which starts at DIRECT_LIMIT
        // and grows up to DIRECT_LIMIT + GLOBAL_POOL_HARD_LIMIT).
        use common::DIRECT_LIMIT;
        use super::super::global_pool::GLOBAL_POOL_HARD_LIMIT;
        let gp_top = DIRECT_LIMIT + GLOBAL_POOL_HARD_LIMIT as u64;
        assert!(
            RDMA_MR2_MARKER > gp_top,
            "RDMA_MR2_MARKER {:#x} must be above general pool ceiling {:#x}",
            RDMA_MR2_MARKER, gp_top,
        );
    }
}
