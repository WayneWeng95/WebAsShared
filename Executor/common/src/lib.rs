#![no_std]
extern crate alloc;

use core::sync::atomic::{AtomicU32, AtomicU64};

// ─── Type aliases ────────────────────────────────────────────────────────────

/// SHM byte offset type — used for offsets inside fixed-size arenas (Registry,
/// Atomic, Log, RDMA scratch, Superblock internals).  These arenas live entirely
/// inside the direct wasm32 window and never overflow, so `u32` is sufficient.
pub type ShmOffset = u32;

/// Atomic variant of [`ShmOffset`].
pub type AtomicShmOffset = core::sync::atomic::AtomicU32;

/// Identifier for a stream/IO page.  Unlike [`ShmOffset`], a `PageId` may
/// reference a page that lives outside the direct wasm32 window (i.e. in the
/// host-side extended pool).  A `PageId` with value `< DIRECT_LIMIT` is a
/// direct-mode page and its byte offset in the wasm32 window equals its id;
/// a `PageId` with value `>= DIRECT_LIMIT` is a paged-mode page that must be
/// resolved through the host-side resolution buffer before use.
pub type PageId = u64;

/// Atomic variant of [`PageId`].
pub type AtomicPageId = AtomicU64;

/// WASM linear-memory pointer width.  Always `u32` — this is a wasm32 project.
pub type WasmPtr = u32;

/// Byte size of one [`ShmOffset`] (4 bytes).
pub const SHM_OFFSET_SIZE: usize = core::mem::size_of::<ShmOffset>();

/// Byte size of one [`PageId`] (8 bytes).
pub const PAGE_ID_SIZE: usize = core::mem::size_of::<PageId>();

/// Sentinel value for "no page" / empty chain.
pub const SHM_NULL: ShmOffset = 0;

/// Sentinel PageId for "no page" / empty chain in the widened chain metadata.
pub const PAGE_ID_NULL: PageId = 0;

/// Threshold separating direct-mode PageIds from paged-mode PageIds.
///
/// A `PageId < DIRECT_LIMIT` is equal to a wasm32 SHM byte offset and can be
/// resolved by pure arithmetic (`TARGET_OFFSET + id`) with no host call.
/// A `PageId >= DIRECT_LIMIT` references a page in the host-side extended pool
/// and must go through `host_fetch_page` (with a guest-side resident cache in
/// front of it).  Chosen to match `TARGET_OFFSET` (2 GiB) so the direct window
/// is fully usable before any paging kicks in.
pub const DIRECT_LIMIT: PageId = 0x8000_0000;

/// Fill fraction at which the direct bump allocator flips to paged mode.
/// Expressed as numerator/denominator to stay const-evaluable.
pub const PAGED_MODE_ENTER_NUM: u64 = 8;
pub const PAGED_MODE_ENTER_DEN: u64 = 10;   // 80%

/// Fill fraction below which paged mode may flip back to direct mode
/// (only when `global_live == 0`).
pub const PAGED_MODE_EXIT_NUM: u64 = 5;
pub const PAGED_MODE_EXIT_DEN: u64 = 10;    // 50%

// ── Shared path / buffer constants ──────────────────────────────────────────

/// Path to the compiled guest WASM module (relative to the host working dir).
pub const WASM_PATH: &str = "Executor/target/wasm32-unknown-unknown/release/guest.wasm";

/// Persistent buffer used to extend the lifetime of a returned payload across
/// the WASM ABI boundary.  Shared by any guest function that returns a fat
/// pointer — only one call is active at a time (WASM is single-threaded).
pub static mut READ_BUFFER: alloc::vec::Vec<u8> = alloc::vec::Vec::new();

// -------------------------------------------------------
// Memory Layout Constants (Single Source of Truth)
// -------------------------------------------------------
pub const TARGET_OFFSET: usize = 0x8000_0000;   // 2 GiB

pub const KIB: ShmOffset = 1024;
pub const MIB: ShmOffset = 1024 * 1024;

pub const INITIAL_SHM_SIZE: ShmOffset = 64 * MIB;

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

pub const REGISTRY_SIZE: ShmOffset = 1 * MIB;
pub const REGISTRY_OFFSET: ShmOffset = SUPERBLOCK_SIZE;

// Maximum number of nodes in the RDMA full mesh.
pub const MAX_MESH_NODES: usize = 32;

// RDMA Atomic Result Scratch: one 8-byte slot per (node_id, peer_id) pair.
pub const RDMA_SCRATCH_SIZE: ShmOffset = 2 * PAGE_SIZE;
pub const RDMA_SCRATCH_OFFSET: ShmOffset = REGISTRY_OFFSET + REGISTRY_SIZE;

pub const ATOMIC_ARENA_SIZE: ShmOffset = 1 * MIB;
pub const ATOMIC_ARENA_OFFSET: ShmOffset = RDMA_SCRATCH_OFFSET + RDMA_SCRATCH_SIZE;

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

// Dynamic Hash Map — one bucket per `PageId`-sized atomic slot in a single
// 4 KiB backing page.  After widening bucket slots to `AtomicPageId` this
// halves from 1024 to 512 buckets; distribution is still ample for the
// shared-state write workloads.
pub const BUCKET_COUNT: usize = (PAGE_SIZE as usize) / PAGE_ID_SIZE;

// Shuffle
pub const PARALLEL_THRESHOLD: usize = 50;

// ─── Intra-wave barrier ──────────────────────────────────────────────────────

/// Maximum number of concurrent barrier slots available in the Superblock.
pub const BARRIER_COUNT: usize = 64;

// ─── Page data sizing ────────────────────────────────────────────────────────

/// Header bytes consumed by `(next_offset + cursor)` in each [`Page`].
/// `next_offset` is a [`PageId`] (8 bytes) and `cursor` is a [`ShmOffset`] (4 bytes).
pub const PAGE_HEADER_SIZE: usize = PAGE_ID_SIZE + SHM_OFFSET_SIZE;

/// Usable data bytes per page (PAGE_SIZE minus header).
/// wasm32 direct mode: 4096 - 12 = 4084 bytes per page.
pub const PAGE_DATA_SIZE: usize = PAGE_SIZE as usize - PAGE_HEADER_SIZE;

// ─── Capacity guards ─────────────────────────────────────────────────────────

/// Soft upper bound for bump allocation.
pub const BUMP_SOFT_LIMIT: ShmOffset = 0x7FF0_0000;

/// Hard ceiling for `global_capacity` doubling (Rust workloads).
///
/// Bound to `DIRECT_LIMIT` because a wasm32 Rust guest can only address
/// `TARGET_OFFSET .. TARGET_OFFSET + 2 GiB` as direct memory; pages past
/// that must go through the extended-pool paged-mode resolver.
pub const CAPACITY_HARD_LIMIT: ShmOffset = 0x8000_0000;           // 2 GiB

/// Hard ceiling for `global_capacity` growth when the DAG contains
/// Python workloads.
///
/// Python accesses SHM through `open(path, "r+b") + seek/read` (see
/// `py_guest/python/shm.py`), so it is not bound by the wasm32 2 GiB
/// direct window — it is bound only by the tmpfs file size and by
/// `ShmOffset = u32` arithmetic.  We stop a page short of `u32::MAX`
/// to keep bump-allocator arithmetic safely below overflow.
///
/// Raising this above 4 GiB would require widening `ShmOffset` (and
/// every slot-head / bump-allocator / cursor field derived from it) to
/// `u64`.  That is a sizeable refactor; defer until a workload needs it.
pub const CAPACITY_HARD_LIMIT_PYTHON: ShmOffset = 0xFFFF_F000;    // ≈ 4 GiB - PAGE_SIZE

// ─── Extended pool feature flag ───────────────────────────────────────────────

/// Master switch for the extended-pool feature (paged-mode allocation
/// backed by a host-side overflow file).
///
/// When `false`, every `extended_pool::runtime::*` entry point short-
/// circuits to a no-op as its first statement.  The compiler sees the
/// `const false` guard and dead-code-eliminates the entire call, so
/// there is no runtime cost and no singleton is ever initialized.
/// The `extended_pool` module itself stays compiled — unit tests that
/// exercise `ExtendedPool`/`GlobalPool`/`ResolutionBuffer` through
/// their owned-struct API continue to run regardless of this flag.
///
/// When `true`, `reclaimer::alloc_page` observes each direct-mode
/// bump advance through `notify_bump_advance`, which triggers a flip
/// to paged mode once the 80% threshold is crossed.  See
/// `docs/extended_pool.md` for the full design.
pub const EXTENDED_POOL_ENABLED: bool = true;

// ─── RDMA extended pool (Path C — dynamic overflow) ──────────────────────────

/// Master switch for the RDMA two-MR extension (Path C).
///
/// Dynamic overflow design: when a receiver's `recv_si` sees a
/// `total_bytes` announcement larger than the remaining MR1 bump budget,
/// it lazily registers (or grows) a host-side MR2 backing file, replies
/// to the sender with `DestReply::UseMr2 { addr, rkey, dest_off }`, and
/// the sender RDMA-WRITEs into MR2 instead of MR1.  After the transfer
/// completes, the receiver's host CPU-copies the data from MR2 into a
/// fresh MR1 page chain and links that chain into the target slot — so
/// the guest consumer only ever sees direct-window PageIds and is
/// unaware MR2 exists.
///
/// The rkey piggybacks on the existing SI Phase-2 reply; no persistent
/// peer-wide rkey announcement is needed.
///
/// When `false`, every `rdma_pool::runtime::*` entry point short-
/// circuits at its first statement and the compiler eliminates its
/// body.  No RdmaPool is ever created, no MR2 is registered, and the
/// RDMA data plane is strictly the pre-Path-C MR1-only behavior.
///
/// Independent of `EXTENDED_POOL_ENABLED`: the RdmaPool is its own
/// storage (separate backing file, separate VA reservation, separate
/// MR registration).  Either flag can be on or off in any combination.
pub const EXTENDED_RDMA_ENABLED: bool = true;

/// How much of MR1 can be consumed by RDMA receives before MR2
/// spillover kicks in.  Tracked by a dedicated `rdma_mr1_used` counter
/// inside `rdma_pool::runtime`, not by `sb.bump_allocator` — so this
/// budget does not reduce the direct allocator's capacity; it just
/// caps the RDMA-owned slice of it.  512 MiB is large enough for
/// typical per-transfer chunks while leaving the rest of the direct
/// window for guest allocations and general host-side use.
pub const RDMA_MR1_BUDGET: u64 = 512 * 1024 * 1024;

/// RDMA MR1 usage level at which MR2 is lazily registered.  Set at
/// 50% of the budget so the pool is ready for spillover BEFORE the
/// first allocation that would actually need it.  Registering MR2 is
/// a heavy operation (`ibv_reg_mr` pins physical memory, TCP
/// announcement to every peer), so we want to pay that cost when
/// there's time rather than on the critical path of an incoming
/// transfer.
pub const RDMA_MR2_REG_THRESHOLD: u64 = 256 * 1024 * 1024;

/// Initial committed size of the RdmaPool backing file (512 MiB).
/// Doubles on demand up to `RDMA_MR2_HARD_LIMIT`.  Typed as `u64`
/// so it survives being compiled against the wasm32 guest (where
/// `usize` is 32-bit) — host code casts to `usize` at use sites.
pub const RDMA_MR2_INITIAL_SIZE: u64 = 512 * 1024 * 1024;

/// Hard upper bound on total RdmaPool committed size.  Also the
/// size of the virtual address reservation (reserve-once, commit-
/// incrementally pattern).  Larger than `GLOBAL_POOL_HARD_LIMIT`
/// because RDMA bursts can be genuinely large on modern links.
/// On ConnectX-3 without ODP this pins physical RAM — tune down if
/// the operator's budget is tighter.  Typed as `u64` for wasm32
/// compatibility (see [`RDMA_MR2_INITIAL_SIZE`]).
pub const RDMA_MR2_HARD_LIMIT: u64 = 16 * 1024 * 1024 * 1024;

/// Idle timeout after which an MR2 (or sender-side extension MR) is
/// torn down to return pinned memory.  Checked lazily at the top of
/// `recv_si` / `send_si`; no background thread.  Typed as `u64` for
/// wasm32 compatibility; the host reads it as `Duration` via
/// `Duration::from_nanos(MR2_IDLE_TIMEOUT_NANOS)`.
pub const MR2_IDLE_TIMEOUT_NANOS: u64 = 5_000_000_000; // 5s

// ─── Free-list trim policy ────────────────────────────────────────────────────

/// Master switch for free-list trimming.
pub const FREE_LIST_TRIM_ENABLED: bool = true;

/// Maximum total pages across all free-list shards before a trim fires.
///
/// When `trim_free_list` is called and the total free-list page count exceeds
/// this threshold, the physical backing of half the excess pages is released
/// to the OS via `madvise(MADV_DONTNEED)`.  The pages stay in the free list
/// (virtual address space is preserved) so they remain immediately recyclable;
/// the OS will zero-fill them again on the next write (soft page fault).
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
    pub registry_lock: AtomicU32,
    pub next_atomic_idx: AtomicU32,
    pub shared_map_base: AtomicShmOffset,
    // 4 bytes of implicit padding here to 8-align the next AtomicU64 array.
    /// Sharded page free list (Treiber stacks).  Head PageIds are u64 so a
    /// direct-freelist slot retired during paged mode may host any PageId.
    pub free_list_heads: [AtomicPageId; FREE_LIST_SHARD_COUNT],
    /// Per-slot head PageIds for the stream area (0..STREAM_SLOT_COUNT).
    pub writer_heads: [AtomicPageId; STREAM_SLOT_COUNT],
    /// Per-slot tail PageIds for the stream area.
    pub writer_tails: [AtomicPageId; STREAM_SLOT_COUNT],
    /// Per-slot head PageIds for the dedicated I/O area (0..IO_SLOT_COUNT).
    pub io_heads: [AtomicPageId; IO_SLOT_COUNT],
    /// Per-slot tail PageIds for the dedicated I/O area.
    pub io_tails: [AtomicPageId; IO_SLOT_COUNT],
    /// Intra-wave barrier counters (futex-backed).
    pub barriers: [AtomicU32; BARRIER_COUNT],
}

#[repr(C, align(4096))]
pub struct Page {
    /// PageId of the next page in this chain.  Widened to 8 bytes so that
    /// pages in the host-side extended pool can be referenced by id.  In the
    /// direct-mode fast path the stored value is always `< DIRECT_LIMIT` and
    /// resolves to `(TARGET_OFFSET + next_offset) as *mut Page` with no host
    /// call.  In paged mode, values `>= DIRECT_LIMIT` go through the guest
    /// resident cache + `host_fetch_page` slow path.
    pub next_offset: AtomicPageId,
    pub cursor: AtomicShmOffset,
    pub data: [u8; PAGE_DATA_SIZE],
}

// Compile-time assertions: Page must be exactly PAGE_SIZE bytes and the
// header layout must be { next_offset: u64 @ 0, cursor: u32 @ 8, data @ 12 }.
const _: () = assert!(core::mem::size_of::<Page>() == PAGE_SIZE as usize);
const _: () = assert!(PAGE_HEADER_SIZE == 12);
const _: () = assert!(PAGE_DATA_SIZE == 4084);

// Compile-time assertions for the Superblock layout after widening slot
// atomics to AtomicPageId (u64).  These offsets are mirrored by the Python
// guest (`py_guest/python/shm.py`) as literals; if any assert fires, update
// shm.py in the same commit.
const _: () = assert!(core::mem::offset_of!(Superblock, bump_allocator)  == 4);
const _: () = assert!(core::mem::offset_of!(Superblock, free_list_heads) == 32);
const _: () = assert!(core::mem::offset_of!(Superblock, writer_heads)    == 160);
const _: () = assert!(core::mem::offset_of!(Superblock, writer_tails)    == 16544);
const _: () = assert!(core::mem::offset_of!(Superblock, io_heads)        == 32928);
const _: () = assert!(core::mem::offset_of!(Superblock, io_tails)        == 37024);
const _: () = assert!(core::mem::offset_of!(Superblock, barriers)        == 41120);

#[repr(C)]
pub struct ChainNodeHeader {
    /// Next entry in the per-bucket conflict list (shared-state write
    /// contention chain).  Widened to [`PageId`] so overflow entries may
    /// live in the extended pool.
    pub next_node: AtomicPageId,
    pub writer_id: u32,
    pub data_len: u32,
    pub registry_index: u32,
    // 4 bytes of implicit padding to 8-align `next_payload_page`.
    /// Next page in this entry's payload chain.  Overflow pages carry a
    /// bare [`PageId`] at byte 0 as their own next-pointer (see
    /// `guest/src/api/shared_area.rs`), so widening here must stay in sync
    /// with the `size_of::<PageId>()` used in that traversal.
    pub next_payload_page: PageId,
}

// ChainNodeHeader after widening: next_node u64 @ 0, writer_id @ 8,
// data_len @ 12, registry_index @ 16, (4 pad), next_payload_page @ 24.
// Total size = 32 bytes (was 20).
const _: () = assert!(core::mem::size_of::<ChainNodeHeader>() == 32);
const _: () = assert!(core::mem::offset_of!(ChainNodeHeader, next_payload_page) == 24);

#[repr(C)]
pub struct RegistryEntry {
    pub name: [u8; 52],
    pub index: u32,
    pub payload_offset: AtomicShmOffset,
    pub payload_len: AtomicU32,
}
