//! File loader: memory-maps a file at a given path for zero-copy parallel dispatch.
//!
//! The mapped region is read-only (`MAP_PRIVATE | PROT_READ`), so it can be
//! safely shared across parallel worker threads without synchronization.
//!
//! # Example
//! ```no_run
//! use std::path::Path;
//! use host::runtime::loader::load_file;
//! use host::runtime::slicer::Slicer;
//! use host::policy::LineBoundarySlice;
//!
//! let loaded = load_file(Path::new("/data/input.csv")).unwrap();
//! let slices = Slicer::new(&loaded).slice(&LineBoundarySlice, 8);
//! ```

use anyhow::{anyhow, Result};
use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};
use std::fs::File;
use std::num::NonZeroUsize;
use std::path::Path;
use std::ptr::NonNull;

// ─── LoadedFile ───────────────────────────────────────────────────────────────

/// A read-only memory-mapped view of a file on disk.
///
/// The mapping is released automatically via `Drop` (`munmap`).  Because the
/// region is `MAP_PRIVATE | PROT_READ`, any accidental write stays
/// process-local and never touches the backing file.
///
/// `Send + Sync` is implemented manually: the region is read-only, and the
/// mapping remains valid for the full lifetime of this struct.
pub struct LoadedFile {
    /// Pointer to the start of the mapped region.
    ptr: NonNull<u8>,
    /// Length of the mapped region in bytes (== file size at load time).
    len: usize,
    /// Keeps the file descriptor open for the lifetime of the mapping.
    _file: File,
}

impl LoadedFile {
    /// Total byte length of the mapped file.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Read-only view of the entire file contents.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: ptr is valid and non-null for `len` bytes for the lifetime
        // of `&self`; the region is MAP_PRIVATE so no aliased mutations exist.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// Read-only view of the byte range `[offset, offset + len)`.
    ///
    /// # Panics
    /// Panics if `offset + len > self.len()`.
    #[inline]
    pub fn chunk(&self, offset: usize, len: usize) -> &[u8] {
        assert!(
            offset.saturating_add(len) <= self.len,
            "chunk [{offset}, {offset}+{len}) out of bounds (file len = {})",
            self.len
        );
        // SAFETY: bounds are checked above.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(offset), len) }
    }

    /// Raw pointer to the start of the mapped region.
    ///
    /// Exposed for hot paths that need to work with absolute virtual addresses
    /// (e.g. handing a base pointer + offset directly to a WASM instance).
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }
}

impl Drop for LoadedFile {
    fn drop(&mut self) {
        // SAFETY: ptr and len came from a successful mmap call, and munmap is
        // called exactly once here on Drop.  nix 0.27 munmap still takes a
        // raw *mut c_void and a plain usize (only mmap was updated to NonNull).
        if self.len > 0 {
            unsafe {
                let _ = munmap(self.ptr.as_ptr() as *mut _, self.len);
            }
        }
    }
}

// SAFETY: The mmap region is read-only (MAP_PRIVATE | PROT_READ).
// Multiple threads may read it concurrently without synchronization.
unsafe impl Send for LoadedFile {}
unsafe impl Sync for LoadedFile {}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Open `path` and memory-map its contents for zero-copy parallel dispatch.
///
/// The mapping uses `MAP_PRIVATE | PROT_READ`: the backing file is never
/// modified, even if a worker thread writes into the region (writes remain
/// process-local thanks to copy-on-write semantics).
///
/// The kernel is free to choose the virtual address; no fixed address is
/// required here because workers receive slices, not raw pointers into the
/// WASM address space.
///
/// # Errors
/// Returns an error if the file cannot be opened, if it is empty, or if the
/// `mmap(2)` syscall fails.
pub fn load_file(path: &Path) -> Result<LoadedFile> {
    let file = File::open(path)
        .map_err(|e| anyhow!("Cannot open '{}': {}", path.display(), e))?;

    let len = file
        .metadata()
        .map_err(|e| anyhow!("Cannot stat '{}': {}", path.display(), e))?
        .len() as usize;

    if len == 0 {
        return Err(anyhow!(
            "File '{}' is empty — nothing to dispatch",
            path.display()
        ));
    }

    let size = NonZeroUsize::new(len).expect("len > 0 checked above");

    // SAFETY: file is open and valid; size is non-zero; offset is 0.
    let raw_ptr = unsafe {
        mmap(
            None,                  // let the kernel pick the virtual address
            size,
            ProtFlags::PROT_READ,
            MapFlags::MAP_PRIVATE,
            Some(&file),
            0,
        )
        .map_err(|e| anyhow!("mmap failed for '{}': {}", path.display(), e))?
    };

    let ptr = NonNull::new(raw_ptr.cast::<u8>())
        .ok_or_else(|| anyhow!("mmap returned a null pointer for '{}'", path.display()))?;

    println!(
        "[Loader] Mapped '{}' ({} bytes @ {:p})",
        path.display(),
        len,
        ptr.as_ptr()
    );

    Ok(LoadedFile { ptr, len, _file: file })
}
