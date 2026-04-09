#![no_std]
extern crate alloc;

use core::sync::atomic::AtomicU32;

// ─── WASM32 / WASM64 dual-mode type aliases ─────────────────────────────────
//
// When the `wasm64` feature is enabled, SHM offsets widen from 32 to 64 bits,
// allowing the shared memory region to exceed 4 GiB.  All page pointers,
// head/tail arrays, and bump allocator fields use these aliases so a single
// codebase serves both targets.
//
// Build:  cargo build --release              (wasm32, default)
//         cargo build --release --features wasm64   (wasm64)

/// SHM byte offset type: `u32` (wasm32) or `u64` (wasm64).
#[cfg(feature = "wasm64")]
pub type ShmOffset = u64;
#[cfg(not(feature = "wasm64"))]
pub type ShmOffset = u32;

/// Atomic variant of [`ShmOffset`].
#[cfg(feature = "wasm64")]
pub type AtomicShmOffset = core::sync::atomic::AtomicU64;
#[cfg(not(feature = "wasm64"))]
pub type AtomicShmOffset = core::sync::atomic::AtomicU32;

/// WASM linear-memory pointer width: matches the guest target pointer size.
#[cfg(feature = "wasm64")]
pub type WasmPtr = u64;
#[cfg(not(feature = "wasm64"))]
pub type WasmPtr = u32;

/// Byte size of one [`ShmOffset`] (4 for wasm32, 8 for wasm64).
pub const SHM_OFFSET_SIZE: usize = core::mem::size_of::<ShmOffset>();

/// Sentinel value for "no page" / empty chain.
pub const SHM_NULL: ShmOffset = 0;

// ── Shared path / buffer constants ──────────────────────────────────────────

/// Path to the compiled guest WASM module (relative to the host working dir).
#[cfg(feature = "wasm64")]
pub const WASM_PATH: &str = "Executor/target/wasm64-unknown-unknown/release/guest.wasm";
#[cfg(not(feature = "wasm64"))]
pub const WASM_PATH: &str = "Executor/target/wasm32-unknown-unknown/release/guest.wasm";

/// Persistent buffer used to extend the lifetime of a returned payload across
/// the WASM ABI boundary.  Shared by any guest function that returns a fat
/// pointer — only one call is active at a time (WASM is single-threaded).
pub static mut READ_BUFFER: alloc::vec::Vec<u8> = alloc::vec::Vec::new();

// -------------------------------------------------------
// Memory Layout Constants (Single Source of Truth)
// -------------------------------------------------------
/// Offset where SHM is mapped into the WASM linear memory.
/// wasm32: 2 GiB.  wasm64: 32 GiB (reserves ample room for local use).
#[cfg(feature = "wasm64")]
pub const TARGET_OFFSET: usize = 0x2000_0000; // 512 MiB (leaves ~512 MiB for guest heap within 1 GiB min)
#[cfg(not(feature = "wasm64"))]
pub const TARGET_OFFSET: usize = 0x8000_0000;   // 2 GiB

pub const KIB: ShmOffset = 1024;
pub const MIB: ShmOffset = 1024 * 1024;
pub const GIB: ShmOffset = 1024 * 1024 * 1024;

/// Initial SHM file size.
/// wasm32: 36 MiB.  wasm64: 256 MiB (larger arenas).
#[cfg(feature = "wasm64")]
pub const INITIAL_SHM_SIZE: ShmOffset = 256 * MIB;
#[cfg(not(feature = "wasm64"))]
pub const INITIAL_SHM_SIZE: ShmOffset = 36 * MIB;

pub const PAGE_SIZE: ShmOffset = 4 * KIB;

// Number of independent stream slots (upstream producers + downstream routing targets).
// Increase this to support more concurrent actors; SUPERBLOCK_SIZE adjusts automatically.
pub const STREAM_SLOT_COUNT: usize = 2048;

// Number of dedicated I/O slots — a separate, non-overlapping area from the stream slots.
pub const IO_SLOT_COUNT: usize = 512;

// Default IO slot assignments for the conventional single-input / single-output workflow.
pub const INPUT_IO_SLOT:  u32 = 0;
pub const OUTPUT_IO_SLOT: u32 = 1;

// Number of independent Treiber-stack shards for the page free list.
pub const FREE_LIST_SHARD_COUNT: usize = 16;

// Automatically derived: size of the Superblock struct rounded up to the next full page.
pub const SUPERBLOCK_SIZE: ShmOffset = {
    let sz = core::mem::size_of::<Superblock>() as ShmOffset;
    (sz + PAGE_SIZE - 1) / PAGE_SIZE * PAGE_SIZE
};

/// Registry Arena.  wasm32: 1 MiB.  wasm64: 4 MiB.
#[cfg(feature = "wasm64")]
pub const REGISTRY_SIZE: ShmOffset = 4 * MIB;
#[cfg(not(feature = "wasm64"))]
pub const REGISTRY_SIZE: ShmOffset = 1 * MIB;
pub const REGISTRY_OFFSET: ShmOffset = SUPERBLOCK_SIZE;

// Maximum number of nodes in the RDMA full mesh.
pub const MAX_MESH_NODES: usize = 32;

// RDMA Atomic Result Scratch: one 8-byte slot per (node_id, peer_id) pair.
pub const RDMA_SCRATCH_SIZE: ShmOffset = 2 * PAGE_SIZE;
pub const RDMA_SCRATCH_OFFSET: ShmOffset = REGISTRY_OFFSET + REGISTRY_SIZE;

/// Atomic Arena.  wasm32: 1 MiB.  wasm64: 8 MiB.
#[cfg(feature = "wasm64")]
pub const ATOMIC_ARENA_SIZE: ShmOffset = 8 * MIB;
#[cfg(not(feature = "wasm64"))]
pub const ATOMIC_ARENA_SIZE: ShmOffset = 1 * MIB;
pub const ATOMIC_ARENA_OFFSET: ShmOffset = RDMA_SCRATCH_OFFSET + RDMA_SCRATCH_SIZE;

/// Log Arena.  wasm32: 16 MiB.  wasm64: 64 MiB.
#[cfg(feature = "wasm64")]
pub const LOG_ARENA_SIZE: ShmOffset = 64 * MIB;
#[cfg(not(feature = "wasm64"))]
pub const LOG_ARENA_SIZE: ShmOffset = 16 * MIB;
pub const LOG_ARENA_OFFSET: ShmOffset = ATOMIC_ARENA_OFFSET + ATOMIC_ARENA_SIZE;

pub const BUMP_ALLOCATOR_START: ShmOffset = LOG_ARENA_OFFSET + LOG_ARENA_SIZE;

/// SHM byte offset of the `AtomicU64` at `idx` in the atomic arena.
#[inline]
pub const fn atomic_shm_offset(idx: usize) -> ShmOffset {
    ATOMIC_ARENA_OFFSET + (idx as ShmOffset) * 8
}

/// SHM byte offset of the 8-byte RDMA result scratch slot for `(node_id, peer_id)`.
#[inline]
pub const fn rdma_scratch_shm_offset(node_id: usize, peer_id: usize) -> ShmOffset {
    RDMA_SCRATCH_OFFSET + ((node_id * MAX_MESH_NODES + peer_id) as ShmOffset) * 8
}

// Dynamic Hash Map
pub const BUCKET_COUNT: usize = (PAGE_SIZE / 4) as usize;

// Shuffle
pub const PARALLEL_THRESHOLD: usize = 50;

// ─── Intra-wave barrier ──────────────────────────────────────────────────────

/// Maximum number of concurrent barrier slots available in the Superblock.
/// Each slot is a single `AtomicU32` counter used by `host_barrier_wait`
/// (futex-backed) to synchronize nodes within the same execution wave.
pub const BARRIER_COUNT: usize = 64;

// ─── Page data sizing ────────────────────────────────────────────────────────

/// Header bytes consumed by `(next_offset + cursor)` in each [`Page`].
/// wasm32: 4+4 = 8.  wasm64: 8+8 = 16.
pub const PAGE_HEADER_SIZE: usize = 2 * SHM_OFFSET_SIZE;

/// Usable data bytes per page (PAGE_SIZE minus header).
/// wasm32: 4096 − 8 = 4088.  wasm64: 4096 − 16 = 4080.
pub const PAGE_DATA_SIZE: usize = PAGE_SIZE as usize - PAGE_HEADER_SIZE;

// ─── Capacity guards (replace hardcoded 0x7FF00000 / 0x80000000) ─────────

/// Soft upper bound for bump allocation.
#[cfg(feature = "wasm64")]
pub const BUMP_SOFT_LIMIT: ShmOffset = 0x0000_FFFF_FFF0_0000;
#[cfg(not(feature = "wasm64"))]
pub const BUMP_SOFT_LIMIT: ShmOffset = 0x7FF0_0000;

/// Hard ceiling for `global_capacity` doubling.
#[cfg(feature = "wasm64")]
pub const CAPACITY_HARD_LIMIT: ShmOffset = 0x0001_0000_0000_0000; // 256 TiB
#[cfg(not(feature = "wasm64"))]
pub const CAPACITY_HARD_LIMIT: ShmOffset = 0x8000_0000;           // 2 GiB

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
    pub bump_allocator: AtomicShmOffset,
    pub global_capacity: AtomicShmOffset,
    pub log_offset: AtomicShmOffset,
    pub registry_lock: AtomicU32,            // boolean lock — stays u32
    pub next_atomic_idx: AtomicU32,          // counter — stays u32
    pub shared_map_base: AtomicShmOffset,
    /// Sharded page free list (Treiber stacks).
    ///
    /// Shard index for **push**: `(page_offset / PAGE_SIZE) % FREE_LIST_SHARD_COUNT`.
    /// Shard index for **pop**: round-robin via a per-caller counter, falling
    /// back to the next shard if the preferred one is empty.
    ///
    /// Sharding reduces CAS contention under concurrent alloc/free by
    /// distributing threads across independent atomic words.
    pub free_list_heads: [AtomicShmOffset; FREE_LIST_SHARD_COUNT],
    /// Per-slot head page offsets for the stream area (0..STREAM_SLOT_COUNT).
    pub writer_heads: [AtomicShmOffset; STREAM_SLOT_COUNT],
    /// Per-slot tail page offsets for the stream area.
    pub writer_tails: [AtomicShmOffset; STREAM_SLOT_COUNT],
    /// Per-slot head page offsets for the dedicated I/O area (0..IO_SLOT_COUNT).
    pub io_heads: [AtomicShmOffset; IO_SLOT_COUNT],
    /// Per-slot tail page offsets for the dedicated I/O area.
    pub io_tails: [AtomicShmOffset; IO_SLOT_COUNT],
    /// Intra-wave barrier counters (futex-backed).
    ///
    /// Each slot is an arrival counter for `host_barrier_wait`.  The host
    /// resets all active barrier slots to zero before each wave.  Guest
    /// processes in the same wave call `ShmApi::barrier_wait(id, party_count)`
    /// which increments `barriers[id]` and sleeps (via `futex(FUTEX_WAIT)`)
    /// until the counter reaches `party_count`.  The last arrival wakes all
    /// waiters via `futex(FUTEX_WAKE)`.
    pub barriers: [AtomicU32; BARRIER_COUNT], // counters — stays u32
}

#[repr(C, align(4096))]
pub struct Page {
    pub next_offset: AtomicShmOffset,
    pub cursor: AtomicShmOffset,
    pub data: [u8; PAGE_DATA_SIZE],
}

// Compile-time assertion: Page must be exactly PAGE_SIZE bytes.
const _: () = assert!(core::mem::size_of::<Page>() == PAGE_SIZE as usize);

#[repr(C)]
pub struct ChainNodeHeader {
    pub next_node: AtomicShmOffset,    // Hash Bucket index
    pub writer_id: u32,
    pub data_len: u32,                 // data len
    pub registry_index: u32,
    pub next_payload_page: ShmOffset,  // For next payload page
}

#[repr(C)]
pub struct RegistryEntry {
    pub name: [u8; 52],                    // name truncated to 52 bytes
    pub index: u32,                        // atomic arena index — stays u32
    pub payload_offset: AtomicShmOffset,   // points to the winning data page
    pub payload_len: AtomicU32,            // payload length — stays u32
}
