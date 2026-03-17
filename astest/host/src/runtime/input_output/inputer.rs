// File loader and SHM input writer.
//
// # LoadedFile / load_file
//
// `load_file` memory-maps a file with `MAP_PRIVATE | PROT_READ`, giving a
// zero-copy, read-only view.  The OS lazily faults in only the pages that are
// actually read, so large files are cheap to open.  The mapping is released on
// drop via `munmap`.  Because the region is read-only it can be shared across
// threads without synchronization (`Send + Sync`).
//
// Used by:
//   - `Inputer::load`  — feeds file lines into an SHM I/O slot.
//   - `Slicer`         — partitions the file into worker slices.
//   - `FileDispatch`   — dispatches slices to parallel workers.
//
// # Inputer
//
// Takes a `LoadedFile` view and writes it line-by-line into the SHM I/O
// page-chain of a chosen slot, making the records readable by the WASM guest
// via `ShmApi::read_all_inputs_from(slot)`.
//
// All page allocation goes through `reclaimer::alloc_page` (free-list first,
// then bump), so freed pages from earlier DAG nodes are reused before new
// bump space is consumed.
//
// # Prefetch
//
// `Inputer::prefetch` spawns a background thread that loads a file into a
// slot while the DAG executor continues with independent nodes.  The caller
// receives a `PrefetchHandle`; calling `.join()` before the consuming node
// runs ensures the data is ready without blocking any sooner than necessary.
//
// Usage:
//   // Synchronous:
//   let n = Inputer::new(splice_addr).load(Path::new("/data/rows.csv"), 0)?;
//
//   // Prefetch:
//   let h = Inputer::prefetch(splice_addr, PathBuf::from("/data/rows.csv"), 42);
//   // ... run independent nodes ...
//   let n = h.join()?;

use std::fs::File;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::thread;

use anyhow::{anyhow, Result};
use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};

use common::{Page, Superblock};

use crate::runtime::mem_operation::reclaimer;

// ─── LoadedFile ───────────────────────────────────────────────────────────────

/// A read-only memory-mapped view of a file on disk.
///
/// Released automatically on drop via `munmap`.  Because the region is
/// `MAP_PRIVATE | PROT_READ`, multiple threads may read it concurrently
/// without synchronization.
pub struct LoadedFile {
    ptr: NonNull<u8>,
    len: usize,
    _file: File,
}

impl LoadedFile {
    /// Total byte length of the mapped file.
    #[inline] pub fn len(&self) -> usize { self.len }

    #[inline] pub fn is_empty(&self) -> bool { self.len == 0 }

    /// Read-only view of the entire file contents.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// Read-only view of the byte range `[offset, offset + len)`.
    #[inline]
    pub fn chunk(&self, offset: usize, len: usize) -> &[u8] {
        assert!(
            offset.saturating_add(len) <= self.len,
            "chunk [{offset}, {offset}+{len}) out of bounds (file len = {})",
            self.len
        );
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(offset), len) }
    }

    /// Raw pointer to the start of the mapped region.
    #[inline] pub fn as_ptr(&self) -> *const u8 { self.ptr.as_ptr() }
}

impl Drop for LoadedFile {
    fn drop(&mut self) {
        if self.len > 0 {
            unsafe { let _ = munmap(self.ptr.as_ptr() as *mut _, self.len); }
        }
    }
}

// SAFETY: region is MAP_PRIVATE | PROT_READ — read-only, safe to share.
unsafe impl Send for LoadedFile {}
unsafe impl Sync for LoadedFile {}

// ─── load_file ────────────────────────────────────────────────────────────────

/// Open `path` and memory-map its contents for zero-copy access.
///
/// Uses `MAP_PRIVATE | PROT_READ`: the backing file is never modified.
/// Returns an error if the file is empty or `mmap(2)` fails.
pub fn load_file(path: &Path) -> Result<LoadedFile> {
    let file = File::open(path)
        .map_err(|e| anyhow!("Cannot open '{}': {}", path.display(), e))?;

    let len = file
        .metadata()
        .map_err(|e| anyhow!("Cannot stat '{}': {}", path.display(), e))?
        .len() as usize;

    if len == 0 {
        return Err(anyhow!("File '{}' is empty — nothing to load", path.display()));
    }

    let size = NonZeroUsize::new(len).expect("len > 0 checked above");

    let raw_ptr = unsafe {
        mmap(None, size, ProtFlags::PROT_READ, MapFlags::MAP_PRIVATE, Some(&file), 0)
            .map_err(|e| anyhow!("mmap failed for '{}': {}", path.display(), e))?
    };

    let ptr = NonNull::new(raw_ptr.cast::<u8>())
        .ok_or_else(|| anyhow!("mmap returned null for '{}'", path.display()))?;

    println!("[Loader] Mapped '{}' ({} bytes @ {:p})", path.display(), len, ptr.as_ptr());
    Ok(LoadedFile { ptr, len, _file: file })
}

// ─── Inputer ──────────────────────────────────────────────────────────────────

pub struct Inputer {
    splice_addr: usize,
}

// SAFETY: all mutations go through atomics; concurrent writes to different
// slots are safe — the caller must not write to the same slot from two threads.
unsafe impl Send for Inputer {}
unsafe impl Sync for Inputer {}

impl Inputer {
    pub fn new(splice_addr: usize) -> Self {
        Self { splice_addr }
    }

    /// Memory-map `path` and write each non-empty line as one length-prefixed
    /// record into `slot`.  Returns the number of records written.
    pub fn load(&self, path: &Path, slot: u32) -> Result<usize> {
        let loaded = load_file(path)
            .map_err(|e| anyhow!("Inputer: {}", e))?;

        let mut count = 0usize;
        for line in loaded.as_bytes().split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            self.append_record(slot, line)?;
            count += 1;
        }
        println!(
            "[Inputer] '{}' ({} bytes, {} records) → slot {}",
            path.display(), loaded.len(), count, slot,
        );
        Ok(count)
    }

    /// Memory-map `path` and write the entire file as a single record into
    /// `slot`.  Use for binary payloads where line-splitting is inappropriate.
    pub fn load_as_single_record(&self, path: &Path, slot: u32) -> Result<()> {
        let loaded = load_file(path)
            .map_err(|e| anyhow!("Inputer: {}", e))?;
        let len = loaded.len();
        self.append_record(slot, loaded.as_bytes())?;
        println!(
            "[Inputer] '{}' ({} bytes) → slot {} (1 record)",
            path.display(), len, slot,
        );
        Ok(())
    }

    /// Spawn a background thread to load `path` into `slot`.
    ///
    /// Returns a `PrefetchHandle`; call `.join()` before the consuming node
    /// executes to ensure the data is ready.
    pub fn prefetch(splice_addr: usize, path: PathBuf, slot: u32) -> PrefetchHandle {
        let handle = thread::spawn(move || Inputer::new(splice_addr).load(&path, slot));
        PrefetchHandle { slot, handle }
    }

    // ── internals ──────────────────────────────────────────────────────────

    fn sb(&self) -> &Superblock {
        unsafe { &*(self.splice_addr as *const Superblock) }
    }

    fn page_at(&self, offset: u32) -> &mut Page {
        unsafe { &mut *((self.splice_addr + offset as usize) as *mut Page) }
    }

    fn alloc_page(&self) -> Result<u32> {
        reclaimer::alloc_page(self.splice_addr)
            .map_err(|e| anyhow!("Inputer: {}", e))
    }

    fn append_record(&self, slot: u32, payload: &[u8]) -> Result<()> {
        self.write_bytes(slot, &(payload.len() as u32).to_le_bytes())?;
        self.write_bytes(slot, &slot.to_le_bytes())?;  // origin = slot
        self.write_bytes(slot, payload)
    }

    fn write_bytes(&self, slot: u32, mut data: &[u8]) -> Result<()> {
        if data.is_empty() { return Ok(()); }
        let sb = self.sb();
        let s = slot as usize;

        let mut tail = sb.io_tails[s].load(Ordering::Acquire);
        if tail == 0 {
            tail = self.alloc_page()?;
            sb.io_heads[s].store(tail, Ordering::Release);
            sb.io_tails[s].store(tail, Ordering::Release);
        }

        while !data.is_empty() {
            let page = self.page_at(tail);
            let cursor = page.cursor.load(Ordering::Relaxed) as usize;
            let space = 4088usize.saturating_sub(cursor);

            if space == 0 {
                let next = self.alloc_page()?;
                page.next_offset.store(next, Ordering::Release);
                sb.io_tails[s].store(next, Ordering::Release);
                tail = next;
                continue;
            }

            let n = space.min(data.len());
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), page.data.as_mut_ptr().add(cursor), n);
            }
            page.cursor.store((cursor + n) as u32, Ordering::Release);
            data = &data[n..];
        }
        Ok(())
    }
}

// ─── PrefetchHandle ───────────────────────────────────────────────────────────

/// Handle to a background prefetch started by `Inputer::prefetch`.
/// Call `join()` before the consuming node executes.
pub struct PrefetchHandle {
    pub slot: u32,
    handle: thread::JoinHandle<Result<usize>>,
}

impl PrefetchHandle {
    /// Block until the prefetch completes and return the number of records written.
    pub fn join(self) -> Result<usize> {
        self.handle
            .join()
            .map_err(|_| anyhow!("prefetch thread for slot {} panicked", self.slot))?
    }
}
