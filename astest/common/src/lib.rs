#![no_std]

use core::sync::atomic::AtomicU32;

// -------------------------------------------------------
// Memory Layout Constants (Single Source of Truth)
// -------------------------------------------------------
pub const TARGET_OFFSET: usize = 0x8000_0000;

pub const KIB: u32 = 1024;
pub const MIB: u32 = 1024 * 1024;

pub const INITIAL_SHM_SIZE: u32 = 36 * MIB;

pub const PAGE_SIZE: u32 = 4 * KIB;

// Number of independent stream slots (upstream producers + downstream routing targets).
// Increase this to support more concurrent actors; SUPERBLOCK_SIZE adjusts automatically.
pub const STREAM_SLOT_COUNT: usize = 2048;

// Automatically derived: size of the Superblock struct rounded up to the next full page.
// Rust resolves Superblock (defined below) ahead of source order, so this is always exact.
// With STREAM_SLOT_COUNT=2048: 8 fields×4 + 2×2048×4 = 16416 bytes → rounds to 5 pages (20 KiB).
pub const SUPERBLOCK_SIZE: u32 = {
    let sz = core::mem::size_of::<Superblock>() as u32;
    (sz + PAGE_SIZE - 1) / PAGE_SIZE * PAGE_SIZE
};

// Registry Arena: 1MB
pub const REGISTRY_SIZE: u32 = 1 * MIB;
pub const REGISTRY_OFFSET: u32 = SUPERBLOCK_SIZE;

// Atomic Arena: 1MB
pub const ATOMIC_ARENA_SIZE: u32 = 1 * MIB;
pub const ATOMIC_ARENA_OFFSET: u32 = REGISTRY_OFFSET + REGISTRY_SIZE;

// Log Arena: 16MB
pub const LOG_ARENA_SIZE: u32 = 16 * MIB;
pub const LOG_ARENA_OFFSET: u32 = ATOMIC_ARENA_OFFSET + ATOMIC_ARENA_SIZE;

pub const BUMP_ALLOCATOR_START: u32 = LOG_ARENA_OFFSET + LOG_ARENA_SIZE;

// Dynamic Hash Map
pub const BUCKET_COUNT: usize = (PAGE_SIZE / 4) as usize;

// Shuffle
pub const PARALLEL_THRESHOLD: usize = 50;

// Reserved stream slot for final worker output.
// The guest writes to this slot via `ShmApi::write_output`; the host Outputer
// reads it after the producing node finishes and persists it to a file.
pub const OUTPUT_SLOT_ID: u32 = (STREAM_SLOT_COUNT - 1) as u32;

// -------------------------------------------------------
// Shared Data Structures (Guarantees ABI matching)
// -------------------------------------------------------
#[repr(C)]
pub struct Superblock {
    pub magic: u32,
    pub bump_allocator: AtomicU32,
    pub global_capacity: AtomicU32,
    pub log_offset: AtomicU32,
    pub registry_lock: AtomicU32,
    pub next_atomic_idx: AtomicU32,
    pub shared_map_base: AtomicU32, 
    pub free_list_head: AtomicU32,
    pub writer_heads: [AtomicU32; STREAM_SLOT_COUNT],
    pub writer_tails: [AtomicU32; STREAM_SLOT_COUNT],
}

#[repr(C, align(4096))]
pub struct Page {
    pub next_offset: AtomicU32,
    pub cursor: AtomicU32,
    pub data: [u8; 4088],
}

#[repr(C)]
pub struct ChainNodeHeader {
    pub next_node: AtomicU32,      // Hash Bucket index
    pub writer_id: u32,
    pub data_len: u32,             // data len
    pub registry_index: u32,
    pub next_payload_page: u32,    // For next payload page
}

#[repr(C)]
pub struct RegistryEntry {
    pub name: [u8; 52],               // name truncated to 52 bytes
    pub index: u32,                   // 4 bytes (for Atomic Arena)
    pub payload_offset: AtomicU32,    // 4 bytes (points to the winning data page)
    pub payload_len: AtomicU32,       // 4 bytes (payload length)
} 