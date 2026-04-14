//! Global pool — second, large backing file for paged-mode pages.
//!
//! The global pool is a host-side-only mmap that holds pages whose
//! PageIds are `>= DIRECT_LIMIT`.  The pool starts at
//! [`GLOBAL_POOL_INITIAL_SIZE`] bytes and doubles on demand, capped at
//! [`GLOBAL_POOL_HARD_LIMIT`].
//!
//! ## Growth strategy — reserve huge, commit incrementally
//!
//! A naive `mremap` could move the base virtual address when it grows
//! the mapping, which would invalidate every `host_addr_of` pointer
//! callers are holding — including any `MAP_FIXED` views installed
//! into the wasm32 window by the resolution buffer.  To avoid that
//! entire class of bug, we instead:
//!
//! 1. At construction, reserve `GLOBAL_POOL_HARD_LIMIT` bytes of
//!    virtual address space with `mmap(NULL, HARD_LIMIT, PROT_NONE,
//!    MAP_PRIVATE|MAP_ANONYMOUS)`.  This consumes no physical memory —
//!    Linux just records the VA range as "owned by us".
//! 2. Overlay the backing file on the first `initial` bytes with
//!    `mmap(base, initial, PROT_READ|PROT_WRITE, MAP_SHARED|MAP_FIXED,
//!    fd, 0)`.
//! 3. On growth, overlay the newly-grown tail with another `MAP_FIXED`
//!    mapping at `base + old_size`.
//!
//! The base virtual address never changes, so `host_addr_of` is a
//! simple `base + (id - DIRECT_LIMIT)` and stays valid forever.

use anyhow::{anyhow, Context, Result};
use common::{DIRECT_LIMIT, PAGE_SIZE, PageId};
use nix::sys::mman::{madvise, mmap, munmap, MapFlags, MmapAdvise, ProtFlags};
use std::fs::{File, OpenOptions};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

/// Initial committed size of a new global pool (256 MiB).
pub const GLOBAL_POOL_INITIAL_SIZE: usize = 256 * 1024 * 1024;

/// Hard upper bound for a global pool's total committed size — also
/// the size of the virtual-address reservation.  8 GiB by default,
/// which gives 5 doublings from the 256 MiB starting size.  Raising
/// this is a recompile; the virtual reservation scales with it.
///
/// On systems with ConnectX-3 or other HCAs that pin physical pages
/// at MR registration time, the operator should keep this value
/// matched to available RAM; on HCAs with On-Demand Paging, it can
/// be raised much higher (64 GiB or more).
pub const GLOBAL_POOL_HARD_LIMIT: usize = 8 * 1024 * 1024 * 1024;

/// The host-side overflow page pool.
///
/// Single-threaded access: each `wasm-call` subprocess owns its own
/// `GlobalPool` and does not share it across threads.  If a future
/// change needs cross-thread access, wrap this in a `Mutex` at the
/// `ExtendedPool` layer rather than adding locking here.
pub struct GlobalPool {
    path: PathBuf,
    file: File,
    /// Base of the virtual-address reservation — never changes.
    base: *mut u8,
    /// Bytes currently overlaid by the backing file.  Grows by doubling.
    committed: usize,
    /// Next unallocated byte within the committed region.
    bump: u64,
    /// Freed PageIds available for reuse (LIFO).
    freelist: Vec<PageId>,
}

// SAFETY: the base pointer refers to a mapping owned by this process;
// sharing across threads is sound as long as higher-level code serializes
// method calls (e.g. via `Mutex<ExtendedPool>`).
unsafe impl Send for GlobalPool {}
unsafe impl Sync for GlobalPool {}

impl GlobalPool {
    /// Create a new global pool backed by `path`, committed to
    /// `initial` bytes.  `initial` must be non-zero, a multiple of
    /// `PAGE_SIZE`, and `<= GLOBAL_POOL_HARD_LIMIT`.
    ///
    /// If `path` already exists it is truncated — global pools have
    /// no durable state between runs.
    pub fn new(path: &Path, initial: usize) -> Result<Self> {
        if initial == 0 {
            return Err(anyhow!("global pool initial size must be non-zero"));
        }
        if initial > GLOBAL_POOL_HARD_LIMIT {
            return Err(anyhow!(
                "global pool initial size {} exceeds hard limit {}",
                initial, GLOBAL_POOL_HARD_LIMIT,
            ));
        }
        if initial % PAGE_SIZE as usize != 0 {
            return Err(anyhow!(
                "global pool initial size must be a multiple of PAGE_SIZE ({})",
                PAGE_SIZE,
            ));
        }

        // 1. Open / recreate the backing file and size it.
        let file = OpenOptions::new()
            .read(true).write(true).create(true).truncate(true)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        file.set_len(initial as u64)
            .context("sizing global pool backing file")?;

        // 2. Reserve the full hard-limit VA range as PROT_NONE.
        //    Linux treats this as a pure virtual reservation until we
        //    overlay pages.  No physical memory is committed here.
        let base = unsafe {
            mmap(
                None,
                NonZeroUsize::new(GLOBAL_POOL_HARD_LIMIT).unwrap(),
                ProtFlags::PROT_NONE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
                None::<&File>,
                0,
            )
            .context("reserving global pool virtual address range")?
        } as *mut u8;

        // 3. Overlay the backing file on the first `initial` bytes.
        unsafe {
            mmap(
                NonZeroUsize::new(base as usize),
                NonZeroUsize::new(initial).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED | MapFlags::MAP_FIXED,
                Some(&file),
                0,
            )
            .context("overlaying global pool backing file")?;
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
    /// doubling if the bump pointer runs past it.  Returns a
    /// `PageId >= DIRECT_LIMIT`.
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
        Ok(DIRECT_LIMIT + offset)
    }

    /// Return a PageId to the freelist.  Panics in debug builds if
    /// called with a direct-mode PageId — such PageIds are never
    /// owned by the global pool.
    pub fn free(&mut self, id: PageId) {
        debug_assert!(
            id >= DIRECT_LIMIT,
            "GlobalPool::free given direct-mode PageId {:#x}", id,
        );
        self.freelist.push(id);
    }

    /// Host-side virtual address of the [`Page`] identified by `id`.
    /// Valid for the lifetime of this GlobalPool — the base never
    /// moves across expansions.
    pub fn host_addr_of(&self, id: PageId) -> *mut common::Page {
        debug_assert!(id >= DIRECT_LIMIT);
        let offset = (id - DIRECT_LIMIT) as usize;
        debug_assert!(
            offset + PAGE_SIZE as usize <= self.committed,
            "host_addr_of for uncommitted offset {offset:#x} (committed {:#x})",
            self.committed,
        );
        unsafe { self.base.add(offset) as *mut common::Page }
    }

    /// Byte offset inside the backing file for the page identified
    /// by `id`.  Used by the resolution buffer to build the `MAP_FIXED`
    /// overlay arguments.
    #[inline]
    pub fn file_offset_of(&self, id: PageId) -> u64 {
        debug_assert!(id >= DIRECT_LIMIT);
        id - DIRECT_LIMIT
    }

    /// Borrow the backing file for `mmap` / RDMA registration.
    #[inline]
    pub fn backing_file(&self) -> &File { &self.file }

    /// Current committed byte count.
    pub fn committed(&self) -> usize { self.committed }

    /// Current bump pointer (next free byte, ignoring freelist).
    pub fn bump(&self) -> u64 { self.bump }

    /// Number of PageIds in the freelist.
    pub fn free_count(&self) -> usize { self.freelist.len() }

    /// Release the physical backing of every page currently on the
    /// freelist via `madvise(MADV_DONTNEED)`.  The pages stay on the
    /// freelist (their virtual slots remain recyclable) — only the
    /// physical RAM is returned to the OS.  The kernel will zero-fill
    /// them again on the next write via a soft page fault, so
    /// correctness is unaffected.
    ///
    /// Call this when the freelist grows large under workload
    /// pressure but the direct flip-back condition
    /// (`global_live == 0`) is not yet met.  When the pool is fully
    /// idle and we flip back to direct mode, `Drop` handles the
    /// big-release path by unmapping the entire reservation.
    pub fn trim_freelist(&mut self) -> usize {
        let mut advised = 0usize;
        for &id in &self.freelist {
            let offset = (id - DIRECT_LIMIT) as usize;
            if offset + PAGE_SIZE as usize > self.committed {
                // Defensive: skip any PageId outside the committed
                // region.  Should not happen with correct use.
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

    /// Double the committed region, capped at `GLOBAL_POOL_HARD_LIMIT`.
    fn expand(&mut self) -> Result<()> {
        let new_committed = (self.committed * 2).min(GLOBAL_POOL_HARD_LIMIT);
        if new_committed == self.committed {
            return Err(anyhow!(
                "global pool at hard limit ({} GiB) — cannot grow further",
                GLOBAL_POOL_HARD_LIMIT / (1024 * 1024 * 1024),
            ));
        }

        self.file.set_len(new_committed as u64)
            .context("growing global pool backing file")?;

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
            .context("overlaying grown global pool region")?;
        }

        self.committed = new_committed;
        Ok(())
    }
}

impl Drop for GlobalPool {
    fn drop(&mut self) {
        if !self.base.is_null() {
            unsafe {
                // Unmap the entire virtual reservation in one call.
                // Linux handles partially-committed reservations fine.
                let _ = munmap(
                    self.base as *mut libc::c_void,
                    GLOBAL_POOL_HARD_LIMIT,
                );
            }
        }
        // Best-effort cleanup of the backing file.
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
            "webs-global-test-{}-{}-{}",
            std::process::id(),
            label,
            // Best-effort per-test uniqueness.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ))
    }

    #[test]
    fn alloc_bumps_from_direct_limit() {
        let path = test_path("alloc_bumps");
        let mut pool = GlobalPool::new(&path, 2 * PAGE_SIZE as usize).unwrap();

        let a = pool.alloc().unwrap();
        let b = pool.alloc().unwrap();
        assert!(a >= DIRECT_LIMIT);
        assert_eq!(a, DIRECT_LIMIT);
        assert_eq!(b, DIRECT_LIMIT + PAGE_SIZE as u64);
    }

    #[test]
    fn expansion_doubles_committed_region() {
        let path = test_path("expansion");
        // Start at 2 pages; 3rd alloc must double to 4 pages.
        let mut pool = GlobalPool::new(&path, 2 * PAGE_SIZE as usize).unwrap();
        let _a = pool.alloc().unwrap();
        let _b = pool.alloc().unwrap();
        assert_eq!(pool.committed(), 2 * PAGE_SIZE as usize);

        let c = pool.alloc().unwrap();
        assert_eq!(pool.committed(), 4 * PAGE_SIZE as usize);
        assert_eq!(c, DIRECT_LIMIT + 2 * PAGE_SIZE as u64);

        // One more alloc stays in the now-bigger region.
        let d = pool.alloc().unwrap();
        assert_eq!(pool.committed(), 4 * PAGE_SIZE as usize);
        assert_eq!(d, c + PAGE_SIZE as u64);
    }

    #[test]
    fn freelist_is_lifo() {
        let path = test_path("freelist");
        let mut pool = GlobalPool::new(&path, 4 * PAGE_SIZE as usize).unwrap();
        let a = pool.alloc().unwrap();
        let b = pool.alloc().unwrap();
        let c = pool.alloc().unwrap();

        pool.free(a);
        pool.free(b);
        assert_eq!(pool.free_count(), 2);

        // LIFO → b comes back before a.
        assert_eq!(pool.alloc().unwrap(), b);
        assert_eq!(pool.alloc().unwrap(), a);
        // Now freelist is drained, next alloc advances the bump.
        assert_eq!(pool.alloc().unwrap(), c + PAGE_SIZE as u64);
    }

    #[test]
    fn host_addr_survives_expansion() {
        let path = test_path("addr_survives");
        let mut pool = GlobalPool::new(&path, 2 * PAGE_SIZE as usize).unwrap();
        let a = pool.alloc().unwrap();
        let ptr_before = pool.host_addr_of(a);

        // Write a sentinel into page `a`.
        unsafe {
            let slot = ptr_before as *mut u32;
            *slot = 0xDEADBEEF;
        }

        // Force several expansions.
        for _ in 0..5 {
            let _ = pool.alloc().unwrap();
        }

        // Base pointer invariant: `host_addr_of(a)` still returns the
        // same virtual address and still reads the sentinel.
        let ptr_after = pool.host_addr_of(a);
        assert_eq!(ptr_before, ptr_after);
        unsafe {
            assert_eq!(*(ptr_after as *const u32), 0xDEADBEEF);
        }
    }

    #[test]
    fn trim_freelist_preserves_reusability() {
        // `trim_freelist` must not invalidate PageIds on the freelist.
        // Under Linux semantics for MAP_SHARED file mappings, MADV_DONTNEED
        // releases the page cache back to the kernel but does NOT zero
        // the bytes on next access — subsequent reads re-fetch from the
        // backing file.  That is the same contract the existing
        // direct-mode `reclaimer::trim_free_list` relies on.  What trim
        // guarantees: freelist length is unchanged and reuse order is
        // preserved.  What it does NOT guarantee: any particular byte
        // contents in the trimmed pages.
        let path = test_path("trim_freelist");
        let mut pool = GlobalPool::new(&path, 8 * PAGE_SIZE as usize).unwrap();
        let ids: Vec<_> = (0..4).map(|_| pool.alloc().unwrap()).collect();
        for &id in &ids {
            pool.free(id);
        }
        assert_eq!(pool.free_count(), 4);

        let advised = pool.trim_freelist();
        assert_eq!(advised, 4);
        assert_eq!(pool.free_count(), 4);   // length preserved

        // LIFO reuse order preserved after trim.
        assert_eq!(pool.alloc().unwrap(), ids[3]);
        assert_eq!(pool.alloc().unwrap(), ids[2]);
        assert_eq!(pool.alloc().unwrap(), ids[1]);
        assert_eq!(pool.alloc().unwrap(), ids[0]);
    }

    #[test]
    fn host_addr_write_reaches_file() {
        // Round-trip through the backing file: write via host_addr_of,
        // close the pool, reopen the file directly, verify bytes.
        let path = test_path("write_reaches_file");
        let a_id;
        let expected: [u8; 16] = *b"extended_pool!\0\0";
        {
            let mut pool = GlobalPool::new(&path, PAGE_SIZE as usize).unwrap();
            a_id = pool.alloc().unwrap();
            let ptr = pool.host_addr_of(a_id) as *mut u8;
            unsafe {
                std::ptr::copy_nonoverlapping(expected.as_ptr(), ptr, expected.len());
            }
            // Drop cleans up the file — skip the file-readback branch
            // by reopening the path before drop runs.
            let mut file_contents = vec![0u8; expected.len()];
            let off = (a_id - DIRECT_LIMIT) as u64;
            use std::io::{Read, Seek, SeekFrom};
            let mut f = std::fs::OpenOptions::new().read(true).open(&path).unwrap();
            f.seek(SeekFrom::Start(off)).unwrap();
            f.read_exact(&mut file_contents).unwrap();
            assert_eq!(&file_contents, &expected);
        }
    }
}
