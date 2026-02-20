use anyhow::Result;
use nix::sys::mman::{mmap, MapFlags, ProtFlags};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::num::NonZeroUsize;

pub const TARGET_OFFSET: usize = 0x8000_0000;

pub const INITIAL_SHM_SIZE: usize = 36 * 1024 * 1024;
pub const SUPER_BLOCK_SIZE: u32 = 4 * 1024;
pub const ATOMIC_AREA_SIZE: u32 = 2 * 1024 * 1024;
pub const LOG_BUFFER_SIZE: u32 =16 * 1024 * 1024;

// Initial the shared memory area
pub fn format_shared_memory(path: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true).write(true).create(true).truncate(true).open(path)?;

    file.set_len(INITIAL_SHM_SIZE as u64)?;

    let magic: u32 = 0xDEADBEEF;
    
    let bump_allocator_start: u32 = SUPER_BLOCK_SIZE + ATOMIC_AREA_SIZE + LOG_BUFFER_SIZE;
    let global_capacity: u32 = INITIAL_SHM_SIZE as u32;

    file.write_all(&magic.to_le_bytes())?;
    file.write_all(&bump_allocator_start.to_le_bytes())?;
    file.write_all(&global_capacity.to_le_bytes())?;
    
    file.write_all(&(0u32).to_le_bytes())?;

    Ok(())
}

/// Map the file into the virtual memory address specified by Wasm
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

/// Responsible for dynamically expanding the file and updating the VMA mapping at runtime
pub fn expand_mapping(file: &File, addr: usize, new_size: usize) -> Result<()> {
    // 1. Expand the underlying file
    file.set_len(new_size as u64)?;
    // 2. Use MAP_FIXED to expand the VMA in place
    map_into_memory(file, addr, new_size)
}