use anyhow::{anyhow, Result};
use nix::sys::mman::{mmap, MapFlags, ProtFlags};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::num::NonZeroUsize;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::Ordering;

use common::*;

/// Creates and initializes the shared memory file at `path`.
/// Truncates the file to `INITIAL_SHM_SIZE` and writes the superblock header fields,
/// zeroing all atomic counters and pointers so they are ready for first use.
pub fn format_shared_memory(path: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true).write(true).create(true).truncate(true).open(path)?;

    file.set_len(INITIAL_SHM_SIZE as u64)?;

    let magic: u32 = 0xDEADBEEF;
    let global_capacity: ShmOffset = INITIAL_SHM_SIZE;

    // Initialize Superblock fields.  Only `magic` and `bump_allocator` need
    // non-zero starts; every other field (including the widened
    // `AtomicPageId` slot arrays) is already zero from `set_len`.  Keeping
    // just the two explicit writes avoids re-encoding the byte layout here
    // and lets `common::Superblock`'s compile-time offset asserts remain
    // the single source of truth.
    file.write_all(&magic.to_le_bytes())?;                       // magic            u32 @ 0
    file.write_all(&BUMP_ALLOCATOR_START.to_le_bytes())?;        // bump_allocator   u32 @ 4
    file.write_all(&global_capacity.to_le_bytes())?;             // global_capacity  u32 @ 8
    // Everything from byte 12 onward is zero from set_len: log_offset,
    // registry_lock, next_atomic_idx, shared_map_base, free_list_heads,
    // writer_heads, writer_tails, io_heads, io_tails, barriers.

    Ok(())
}
/// Maps `file` into the process address space at the fixed virtual address `addr` with
/// `MAP_SHARED | MAP_FIXED`, making the shared memory region visible to both host and WASM guest.
pub fn map_into_memory(file: &File, addr: usize, size: usize) -> Result<()> {
    unsafe {
        mmap(
            NonZeroUsize::new(addr),
            NonZeroUsize::new(size).unwrap(),
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_SHARED | MapFlags::MAP_FIXED,
            Some(file),
            0,
        )?;
    }
    Ok(())
}

/// Expands the shared memory backing file to `new_size` and remaps the VMA in-place.
/// Called by the `host_remap` host function when the guest's bump allocator needs more capacity.
pub fn expand_mapping(file: &File, addr: usize, new_size: usize) -> Result<()> {
    // 1. Expand the underlying file
    file.set_len(new_size as u64)?;
    // 2. Use MAP_FIXED to expand the VMA in place
    map_into_memory(file, addr, new_size)
}

// ── Host-driven SHM growth ────────────────────────────────────────────────────

/// Process-global SHM file handle and its mapped base address.
/// Registered once at startup by `register_shm_for_growth`.
static SHM_GROW: OnceLock<Mutex<(File, usize)>> = OnceLock::new();

/// Register the SHM file for host-driven capacity growth.
///
/// Must be called once per process after `setup_vma_environment` sets
/// the `splice_addr`.  Any subsequent call is silently ignored (the
/// `OnceLock` keeps only the first registration).
pub fn register_shm_for_growth(file: File, splice_addr: usize) {
    let _ = SHM_GROW.set(Mutex::new((file, splice_addr)));
}

/// Grow the SHM file so that at least `required` bytes are accessible.
///
/// Uses double-checked locking: the fast path is a single atomic read
/// of `sb.global_capacity`; the slow path acquires the mutex, re-checks,
/// and calls `expand_mapping`.  Capacity doubles geometrically from the
/// current size, capped at `CAPACITY_HARD_LIMIT` (2 GiB).
///
/// Returns `Ok(new_capacity)` on success, or an error if the grower is
/// not registered or the hard limit would be exceeded.
pub fn try_grow_shm(splice_addr: usize, required: ShmOffset) -> Result<ShmOffset> {
    let sb = unsafe { &*(splice_addr as *const Superblock) };

    // Fast path — already large enough.
    let current = sb.global_capacity.load(Ordering::Acquire);
    if current >= required {
        return Ok(current);
    }

    let guard = SHM_GROW.get()
        .ok_or_else(|| anyhow!("SHM grower not registered (call register_shm_for_growth at startup)"))?;
    let (file, base) = &*guard.lock().expect("shm_grow mutex poisoned");

    // Re-check under lock — another thread may have grown already.
    let current = sb.global_capacity.load(Ordering::Acquire);
    if current >= required {
        return Ok(current);
    }

    // Geometric growth capped at the hard limit.
    let new_cap = current.saturating_mul(2).max(required).min(CAPACITY_HARD_LIMIT);
    if new_cap < required {
        return Err(anyhow!(
            "SHM hard limit ({} GiB) reached — cannot grow to {} bytes",
            CAPACITY_HARD_LIMIT as u64 / (1024 * 1024 * 1024),
            required,
        ));
    }

    expand_mapping(file, *base, new_cap as usize)?;
    sb.global_capacity.store(new_cap, Ordering::Release);
    eprintln!(
        "[SHM] grew: {} MiB → {} MiB",
        current / MIB,
        new_cap / MIB,
    );
    Ok(new_cap)
}
