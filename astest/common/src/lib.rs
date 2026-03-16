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

// Number of dedicated I/O slots — a separate, non-overlapping area from the stream slots.
// The host Inputer writes file data into IO slots before WASM execution; the guest reads
// from them via `ShmApi::read_all_io_records(slot)`.  The guest writes results to IO
// slots via `ShmApi::append_io_data(slot, data)`; the host Outputer reads them after.
// Increase to support more concurrent I/O channels.
pub const IO_SLOT_COUNT: usize = 512;

// Default IO slot assignments for the conventional single-input / single-output workflow.
// Pass these to the DAG Input/Output nodes when no explicit slot is specified.
pub const INPUT_IO_SLOT:  u32 = 0;
pub const OUTPUT_IO_SLOT: u32 = 1;

// Number of independent Treiber-stack shards for the page free list.
// Sharding reduces CAS contention by ~FREE_LIST_SHARD_COUNT under concurrent
// allocation/free.  Must be a power of two so `% FREE_LIST_SHARD_COUNT`
// can be replaced by a bitmask by the compiler.
pub const FREE_LIST_SHARD_COUNT: usize = 16;

// Automatically derived: size of the Superblock struct rounded up to the next full page.
// With STREAM_SLOT_COUNT=2048, IO_SLOT_COUNT=512, FREE_LIST_SHARD_COUNT=16:
//   7 fields×4 + 16×4 + 2×2048×4 + 2×512×4 = 28 + 64 + 16384 + 4096 = 20572 → 6 pages (24 KiB).
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

// ─── Free-list trim policy ────────────────────────────────────────────────────

/// Master switch for free-list trimming.
///
/// Set to `false` to disable `trim_free_list` entirely (it becomes a no-op).
/// Useful when profiling or when the working set is small enough that the
/// free list never grows large.
pub const FREE_LIST_TRIM_ENABLED: bool = true;

/// Maximum total pages across all free-list shards before a trim fires.
///
/// When `trim_free_list` is called and the total free-list page count exceeds
/// this threshold, the physical backing of half the excess pages is released
/// to the OS via `madvise(MADV_DONTNEED)`.  The pages stay in the free list
/// (virtual address space is preserved) so they remain immediately recyclable;
/// the OS will zero-fill them again on the next write (soft page fault).
///
/// Sizing guide:
///   Increase for workloads with large bursty allocations to reduce page-fault
///   frequency; decrease to keep resident memory tighter.
///   Set to 0 to trim unconditionally whenever the free list is non-empty.
pub const FREE_LIST_TRIM_THRESHOLD: usize = 204800; // 800 MiB total free list (across all shards)

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
    /// Sharded page free list (Treiber stacks).
    ///
    /// Shard index for **push**: `(page_offset / PAGE_SIZE) % FREE_LIST_SHARD_COUNT`.
    /// Shard index for **pop**: round-robin via a per-caller counter, falling
    /// back to the next shard if the preferred one is empty.
    ///
    /// Sharding reduces CAS contention under concurrent alloc/free by
    /// distributing threads across independent atomic words.
    pub free_list_heads: [AtomicU32; FREE_LIST_SHARD_COUNT],
    /// Per-slot head page offsets for the stream area (0..STREAM_SLOT_COUNT).
    pub writer_heads: [AtomicU32; STREAM_SLOT_COUNT],
    /// Per-slot tail page offsets for the stream area.
    pub writer_tails: [AtomicU32; STREAM_SLOT_COUNT],
    /// Per-slot head page offsets for the dedicated I/O area (0..IO_SLOT_COUNT).
    pub io_heads: [AtomicU32; IO_SLOT_COUNT],
    /// Per-slot tail page offsets for the dedicated I/O area.
    pub io_tails: [AtomicU32; IO_SLOT_COUNT],
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
