use anyhow::Result;
use nix::sys::mman::{mmap, MapFlags, ProtFlags};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::num::NonZeroUsize;

use common::*;

/// Creates and initializes the shared memory file at `path`.
/// Truncates the file to `INITIAL_SHM_SIZE` and writes the superblock header fields,
/// zeroing all atomic counters and pointers so they are ready for first use.
pub fn format_shared_memory(path: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true).write(true).create(true).truncate(true).open(path)?;

    file.set_len(INITIAL_SHM_SIZE as u64)?;

    let magic: u32 = 0xDEADBEEF;
    let global_capacity: u32 = INITIAL_SHM_SIZE as u32;

    // Initialize Superblock fields
    file.write_all(&magic.to_le_bytes())?;                // +0: Magic
    file.write_all(&BUMP_ALLOCATOR_START.to_le_bytes())?; // +4: Bump Allocator
    file.write_all(&global_capacity.to_le_bytes())?;      // +8: Global Capacity
    file.write_all(&(0u32).to_le_bytes())?;               // +12: Log Offset
    
    // [NEW] Synchronization primitives for Registry
    file.write_all(&(0u32).to_le_bytes())?;               // +16: Registry Spinlock (0 = Unlocked)
    file.write_all(&(0u32).to_le_bytes())?;               // +20: Next Atomic Index (Starts at 0)
    file.write_all(&(0u32).to_le_bytes())?;               // +24: Shared Map Base
    
    file.write_all(&(0u32).to_le_bytes())?;               // +28: Free List Head

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