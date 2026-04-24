# Extended Pool — Going Past the 4 GiB wasm32 Limit

## The problem

Every `wasm-call` subprocess runs in a wasm32 linear memory capped at 4 GiB,
with the SHM window mapped at `TARGET_OFFSET = 0x8000_0000` giving ~2 GiB of
usable space for stream pages.  Workloads that exceed this hit an
"SHM capacity exhausted" error.

We tried WASM64 first (see [`WASM64.md`](WASM64.md) in this directory) — it compiles but wasmtime's Cranelift
backend generates JIT code so slow for 64-bit modules that one `wasm-call`
takes 3+ minutes before user code runs.  Dead end.

## The approach

Keep each executor's address space at wasm32, but back the page pool with a
**second, much larger host-side file** ("global pool") and demand-page its
contents into the 2 GiB wasm32 window on request.  The guest never sees a
larger linear memory — it sees the same u32 pointers it always did — but the
*population* of pages visible at any moment is a movable view onto a pool
that can be tens or hundreds of GiB.

### Two allocation modes

**Direct mode** (default, small workloads): the bump allocator lives entirely
inside the 2 GiB wasm32 window.  PageId == wasm32 byte offset.  Zero host
calls, zero indirection, identical perf to pre-change code.

**Paged mode** (triggered at 80% fill): new pages allocate out of the global
pool and are demand-paged into a *resolution buffer* carved from the wasm32
window.  Guest pointers still resolve via one branch; values `< DIRECT_LIMIT`
take the fast arithmetic path, values `>= DIRECT_LIMIT` go through a small
guest-side cache backed by a host call.

The key trick: **PageId space overlaps the wasm32 offset space for values
below `DIRECT_LIMIT = 0x8000_0000`**.  A `PageId < DIRECT_LIMIT` literally
*is* its own wasm32 byte offset — no lookup needed.  Only PageIds ≥
`DIRECT_LIMIT` are paged-mode and need resolution.  The branch

```rust
if id < DIRECT_LIMIT { (TARGET_OFFSET + id) as *mut Page }
else                 { resolution_cache.get_or_fetch(id) }
```

is always-taken-fast when the workload fits in direct mode, and the compiler
optimizes it to a single predictable branch.

### Freed direct slots become residency slots

Once paged mode is active, a freed direct page (PageId `< DIRECT_LIMIT`) is
**not** recycled as a direct page — instead its wasm32 slot is handed to the
resolution buffer as an additional residency target.  This grows the
software TLB as the workload churns, up to the full 2 GiB window worth of
residency capacity.  The retired direct PageId is never handed out again,
which preserves the invariant that an in-flight `PageId < DIRECT_LIMIT`
always points at its own data.

### Flip back to direct mode

When `global_live == 0` (no paged-mode PageIds in flight) and direct bump
fill drops below 50%, flip back.  The resolution buffer's "free" slots get
drained back onto the direct freelist.  State-1 (LiveDirect) slots are
untouched across the flip.  No page moves, no pointer rewriting.

## Memory regions affected

Of the six SHM regions, only **stream pages** are scaled by workload data
and need this treatment.  The others are fixed-size and stay in the direct
window forever.

| Region | Size | Action |
|---|---|---|
| Superblock | 45 KiB (grew from 24) | Slot atomics widened to `AtomicPageId` |
| Registry Arena | 1 MiB | unchanged |
| Atomic Arena | 1 MiB | unchanged |
| Log Arena | 16 MiB | unchanged |
| RDMA Staging | N × 1 MiB | stays in direct window |
| Stream Pages | unbounded | **scheme target** |

## RDMA

RDMA runs entirely host-side, so it doesn't mirror the direct/paged split.
Instead RDMA **always** sources from the global pool:

- Single `ibv_reg_mr` over the global pool file at startup.
- DAG analyzer marks slots feeding `RemoteSend` nodes; those slots allocate
  from the global pool regardless of mode.
- Zero-copy RDMA is unconditional (no staged-copy fallback, no MR
  re-registration on mode flip).
- Guest-side cost of going through the resolution buffer for RDMA-bound
  writes is dominated by network speed anyway (~40 GiB/s ceiling vs
  1.1 GiB/s on 10GbE), so the tradeoff is sound.

On ConnectX-3 (no ODP), the operator sizes the global pool conservatively
(e.g. 8 GiB) since `ibv_reg_mr` pins physical memory.  On modern HCAs with
ODP, size it generously.

---

## Master feature flag

The entire feature is gated on `common::EXTENDED_POOL_ENABLED: bool`.
It lives next to `FREE_LIST_TRIM_ENABLED` in `common/src/lib.rs` so
every crate can see it without an extra dep.

```rust
// common/src/lib.rs
pub const EXTENDED_POOL_ENABLED: bool = true;
```

When `false`:

- `extended_pool::runtime::notify_bump_advance` short-circuits at its
  first statement.  The compiler sees the `const false` guard and
  dead-code-eliminates the body, so the reclaimer's direct-mode fast
  path has literally zero extra instructions.
- `current_mode()` always returns `Mode::Direct`.
- `flip_to_paged()` (the explicit force-flip) returns an error.
- The process-wide `OnceLock<Mutex<ExtendedPool>>` is never touched,
  so no singleton state is ever allocated.
- The `extended_pool` module still **compiles** — type widening
  (Phase 1) and the struct-level APIs (`ExtendedPool`, `GlobalPool`,
  `ResolutionBuffer`) remain available for unit tests that exercise
  them directly without going through the singleton.

When `true` (the default), the reclaimer observes bump advances and
the flip machinery engages once the 80% threshold is crossed.

Flipping the flag is a recompile.  It's not a Cargo feature because
the module always compiles (Phase 1's type widening is always in
effect), so a pure `const bool` is simpler than the feature-gate
machinery and produces identical binary output when disabled.

## Phase 1 — Type widening (landed)

Foundation: widen all stream-page references to `u64 PageId` without adding
any behavior change.  Every call site pays one `as ShmOffset` truncation
(safe because direct-mode values still fit in u32).

### Types

- `PageId = u64`, `AtomicPageId = AtomicU64` (new).
- `ShmOffset = u32`, `AtomicShmOffset = AtomicU32` (kept; used for
  offsets inside fixed-size arenas where u32 is sufficient forever).
- `DIRECT_LIMIT = 0x8000_0000` constant.
- `PAGED_MODE_ENTER_NUM/DEN = 8/10` (80% trigger to flip into paged mode).
- `PAGED_MODE_EXIT_NUM/DEN = 5/10` (50% fill + `global_live==0` to flip back).

### Widened fields

- `Page::next_offset: AtomicPageId`.  Header now 12 bytes, `PAGE_DATA_SIZE`
  shrinks 4088 → 4084.
- `Superblock::{free_list_heads, writer_heads, writer_tails, io_heads, io_tails}`
  — all `AtomicPageId`.  Superblock grew 24 KiB → 45 KiB (11 pages).
- `ChainNodeHeader::{next_node, next_payload_page}` — widened.  Struct
  grew 20 → 32 bytes.
- Shared-state bucket array slots widened to `AtomicPageId`.  `BUCKET_COUNT`
  halved from 1024 to 512.

### Compile-time layout asserts

`common/src/lib.rs` now pins every critical offset:

```rust
assert!(size_of::<Page>() == PAGE_SIZE);
assert!(PAGE_HEADER_SIZE == 12 && PAGE_DATA_SIZE == 4084);
assert!(offset_of!(Superblock, free_list_heads) == 32);
assert!(offset_of!(Superblock, writer_heads)    == 160);
assert!(offset_of!(Superblock, writer_tails)    == 16544);
assert!(offset_of!(Superblock, io_heads)        == 32928);
assert!(offset_of!(Superblock, io_tails)        == 37024);
assert!(size_of::<ChainNodeHeader>() == 32);
assert!(offset_of!(ChainNodeHeader, next_payload_page) == 24);
```

Any future layout drift fails the build.  Python `shm.py` mirrors these as
literals; if an assert fires, update shm.py in the same commit.

### Python guest sync

`py_guest/python/shm.py` updated in lockstep:
- New `_SB_*` offsets (160 / 16544 / 32928 / 37024).
- `_SLOT_STRIDE = 8`, every `slot * 4` → `slot * _SLOT_STRIDE`.
- New `_ru64` / `_wu64` helpers; slot reads/writes use 8-byte width.
- Page header accesses use symbolic offsets `_PAGE_NEXT_OFF=0`,
  `_PAGE_CURSOR_OFF=8`, `_PAGE_DATA_OFFSET=12`, `_PAGE_DATA_SIZE=4084`.

### Verified runtime

- `word_count_demo` (Rust) — 767 records, 0.83s.
- `word_count_demo --python` — 767 records, 4.13s (validates Python layout).
- `finra_demo` — 12 records, 11.94s.
- `barrier_test` — 2 waves, concurrent writers on stream slots.

Phase 1 leaves `DIRECT_LIMIT` defined but never checked; the slow path in
`resolve()` is `unreachable!()`.  All PageIds in flight are `< DIRECT_LIMIT`,
so the branch is always-fast.

---

## Phase 2 — Extended pool (host-side only)

Phase 2 provides a second storage tier for pages that overflow the 2 GiB
direct wasm32 window.  The whole mechanism lives under
`host/src/runtime/extended_pool/` and is **invisible to the guest** —
the guest stays strictly inside its 2 GiB direct window and never sees
paged PageIds.  Any host-side allocation site (reclaimer, RDMA receive,
slot loader, organizer) can transparently fall through to paged
allocation when the direct window is exhausted.

### Scope (design choice)

One wasm instance is capped at **2 GiB of visible state at any moment**.
The extended pool does **not** give individual guests more address space
— wasm32 physically cannot address more than 4 GiB and the guest heap
takes the bottom 2 GiB, so the top 2 GiB direct window is the hard
ceiling.  The pool lets the *host* hold more total data (RDMA-received
buffers, accumulated routing chains, etc.) than would fit in a single
wasm window, and hands paged PageIds back through the same `PageId`
type system as direct PageIds.

Explicitly **not** built:

- No swap engine.  Pages do not move between direct and paged storage.
- No DAG-boundary notifications.  The flip happens when a host-side
  bump allocation crosses 80% of the direct window; nothing outside
  the allocator needs to know about the mode.
- No guest-side resolve fast path.  Guest code continues to dereference
  page chains with `SHM_BASE + id as usize` arithmetic, which is only
  valid for direct PageIds — so the guest must never be handed a paged
  PageId through a stream slot it will read.  In practice this is
  enforced by the fact that guest-facing writers (slot append, routing)
  only produce direct PageIds; paged PageIds arise only at host-side
  allocation points whose results flow through host-side readers.
- No flip-back-to-direct.  `should_exit_paged` is defined in
  `mode.rs` but not wired.  Once a subprocess goes Paged it stays
  Paged for its lifetime.  Subprocesses are short-lived in this
  workload model, so the flip-back optimization is unnecessary.

### Module layout

```
host/src/runtime/extended_pool/
├── mod.rs              — ExtendedPool owned-struct API + flip logic
├── mode.rs             — Mode enum + should_enter_paged / should_exit_paged
├── global_pool.rs      — host-side overflow file, reserve-once VA, doubling
├── resolution_buffer.rs — MAP_FIXED overlay primitive, LRU, pinning
└── runtime.rs          — process-wide singleton + lock-free fast path
```

### Memory layout reminder

| Region | Where | Max size |
|---|---|---|
| Direct SHM window | wasm32 `[0x8000_0000, 0xFFFF_FFFF]` | 2 GiB, cap for a single wasm instance |
| Global pool | host-side mmap at `/dev/shm/webs-global-<pid>` | reserved virtual `GLOBAL_POOL_HARD_LIMIT` (default 8 GiB), committed grows by doubling from `GLOBAL_POOL_INITIAL_SIZE` (256 MiB) |
| Resolution buffer | carved out of the direct window on flip | `FLIP_SEED_SLOTS × PAGE_SIZE` (default 16 MiB) reserved at flip time |

### PageId encoding

- `id < DIRECT_LIMIT (0x8000_0000)` — direct mode, id equals its wasm32
  byte offset from `splice_addr`.  Dereferenceable by simple arithmetic.
- `id >= DIRECT_LIMIT` — paged mode, `(id - DIRECT_LIMIT)` is the byte
  offset inside the global pool file.  Dereferencing requires either
  `GlobalPool::host_addr_of` (host virtual address in the reservation)
  or `ResolutionBuffer::install` (MAP_FIXED overlay into a wasm32
  resolution slot).

One comparison is all it takes to distinguish the two, and in the
common direct-only case the branch is predicted-taken-fast and folds
into the existing arithmetic.

### API surface

```rust
// Per-process singleton (runtime.rs).  Lock-free fast path for the
// mode check; mutex only taken when the flip actually fires or when
// a paged operation is in progress.
pub fn current_mode() -> Mode;                          // atomic, no lock
pub fn notify_bump_advance(bump: ShmOffset, splice_addr: usize);
pub fn resolve(id: PageId, splice_addr: usize) -> Result<*mut Page>;
pub fn alloc_paged_page(rdma_bound: bool) -> Result<(PageId, *mut Page)>;
pub fn free_paged_page(id: PageId) -> Result<()>;
pub fn notify_direct_free(id: PageId);
pub fn flip_to_paged(splice_addr: usize) -> Result<Mode>;

// Owned struct API (mod.rs) — used by tests and could be used for
// per-subprocess state if the singleton ever gets split.
impl ExtendedPool {
    pub fn new() -> Self;
    pub fn mode(&self) -> Mode;
    pub fn check_bump(&mut self, bump: ShmOffset, splice_addr: usize) -> Result<()>;
    pub fn flip_to_paged(&mut self, splice_addr: usize) -> Result<()>;
    pub fn alloc_page(&mut self, rdma_bound: bool) -> Result<(PageId, *mut Page)>;
    pub fn free_page(&mut self, id: PageId) -> Result<()>;
    pub fn resolve(&mut self, id: PageId, splice_addr: usize) -> Result<*mut Page>;
    pub fn on_direct_free(&mut self, id: PageId);
}
```

`reclaimer::alloc_page` returns `Result<PageId>`.  In direct mode it
always returns an id `< DIRECT_LIMIT`.  When the direct bump allocator
is exhausted AND `current_mode() == Paged`, it undoes its speculative
`fetch_add` and falls through to `runtime::alloc_paged_page`, returning
an id `>= DIRECT_LIMIT`.

`reclaimer::free_page_chain` takes a `PageId` head.  In direct mode
it takes the single-CAS fast path (unchanged from pre-Phase 2).  In
paged mode it walks the chain one page at a time via
`free_page_chain_mixed`, dispatching each page to either the shard
freelist (direct) or the extended pool (paged).

### Memory release strategy

The global pool only grows during a single run; it never `ftruncate`s
the file down mid-run.  Two release mechanisms cover the relevant
pressure regimes:

1. **`madvise(MADV_DONTNEED)` on freelist pages** —
   `GlobalPool::trim_freelist()` releases the physical backing of
   every page currently on the freelist without removing those
   PageIds from the freelist.  The virtual slots remain recyclable;
   the OS re-fetches bytes from the backing file on next access.
   Matches the existing `reclaimer::trim_free_list` contract for
   direct-mode pages.  Not yet wired into a periodic caller — invoke
   manually from policy if needed.
2. **Full teardown on subprocess exit** — `Drop` on `GlobalPool`
   unmaps the whole virtual reservation and `remove_file`s the
   backing store, releasing everything in one step.  Since we do not
   implement flip-back-to-direct, this is the only big-release path.

### Feature flag

Gated on `common::EXTENDED_POOL_ENABLED: bool = true`.  When set to
`false`, every `runtime::*` entry point short-circuits as its first
statement.  The compiler sees the `const false` guard and eliminates
the function bodies — the direct path has zero extra instructions.
The module still compiles so unit tests exercise the owned-struct
API regardless of the flag.  See the dedicated §"Master feature flag"
earlier in this document.

### Integration-test threshold override

`WEBS_FORCE_FLIP_AT=<decimal or 0xHEX>` overrides the production 80%
threshold in `mode::should_enter_paged`.  Parsed once via `OnceLock`
on first call.  Used by integration tests to exercise the flip
without a 2 GiB workload.

### Concurrency — single-writer-per-rewrite-point

Phase 2 adds new storage locations but **no new contention points**.
Chain rewrites (free, splice, insert) rely on the
**detach-then-walk** protocol that was already in place for Phase 1:

1. A slot atomic holds the chain head.
2. To rewrite the chain, exactly one thread wins a `swap(0, …)` on
   that atomic.  The swap winner is the sole owner of the chain for
   the duration of the rewrite.
3. Any other thread reading the slot atomic after the swap sees
   `head == 0` and does not enter the chain, so there is no concurrent
   walker.
4. The rewrite proceeds single-writer, mutating `next_offset` links
   freely without page-level locks.

This protocol is preserved verbatim by Phase 2.  `free_page_chain`
fast path unchanged.  `free_page_chain_mixed` documents the same
invariant (`reclaimer.rs` top-of-function doc comment) — it walks
single-writer, reads `page.next_offset` into a local `next` variable
BEFORE `push_single_direct_page` overwrites that field with the
freelist link, so the traversal doesn't lose its way when dispatching
direct pages.

Sites that rely on this protocol:

- **`free_page_chain`** — upstream swap clears the slot atomic before
  the walk starts.
- **`ChainSplicer::chain_onto`** — routing runs between waves, after
  upstream producers have finished.  The slot atomics for the source
  chain are stable for the duration of the splice.
- **`Organizer::process_detached_list`** — explicitly documented as
  "must be called after all writers have finished".
- **Guest stream-slot concurrent appenders** — contend only on the
  tail page's `cursor` and the slot's tail atomic; already-committed
  pages past `cursor` are immutable, so they never race with anyone.
  Not a rewrite point.

### Barrier compatibility

The intra-wave futex barrier (`Superblock::barriers: [AtomicU32; 64]`)
is **independent of the extended pool**.  It stores only atomic
counters, never PageIds, and lives inside the fixed-size `Superblock`
which is always resident in the direct window.  Phase 1's Superblock
widening shifted the byte offset of the `barriers` field to 41120;
`common/src/lib.rs` asserts this offset at compile time so any future
layout drift fails the build.

All Rust code accesses the barrier via `sb.barriers[id]` field
indexing (compiler-computed offset), and there is no Python-side
barrier access, so the widening was transparent.  `barrier_test.json`
runs clean post-widening with 2 waves of concurrent writers.

### Implementation phases (historical)

| Phase | What landed |
|---|---|
| 2.1 | Module scaffold: `Mode`, stub `ExtendedPool`, empty `GlobalPool` / `ResolutionBuffer` types. |
| 2.2 | Real `GlobalPool` with reserve-once / commit-incrementally growth, `trim_freelist` via `MADV_DONTNEED`. 5 unit tests. |
| 2.3 | Real `ResolutionBuffer` with `MAP_FIXED` overlay, LRU eviction, pinning, generation counter. 5 unit tests. |
| 2.4a | Flip trigger: `check_bump` + `runtime` singleton + `notify_bump_advance` wired into `reclaimer::alloc_page`. 5 ExtendedPool tests. |
| 2.4 (flag) | `common::EXTENDED_POOL_ENABLED` feature flag. Verified flag-off path against RDMA loopback. |
| 2.4b | `flip_to_paged` allocates real `GlobalPool` + `ResolutionBuffer`; `alloc_page` / `free_page` / `resolve` bodies implemented. 2 end-to-end tests. |
| 2.4c | Widen `reclaimer::alloc_page → Result<PageId>`; add paged-mode fallback when direct bump is exhausted; widen `free_page_chain` with `free_page_chain_mixed` slow path. |
| 2.4d | Seed residency at flip time (`FLIP_SEED_SLOTS = 4096`); `WEBS_FORCE_FLIP_AT` env var; end-to-end integration test `reclaimer_falls_back_to_paged_when_direct_exhausted` driving the full singleton path. |

**22 unit tests** covering mode FSM, GlobalPool expansion/trim,
ResolutionBuffer LRU/pin/overlay, ExtendedPool flip/alloc/resolve/free,
reclaimer fallback, and flip-time seeding against a real Superblock.

**Demo workloads verified at baseline** with flag on and off:
`word_count_demo` (Rust + Python), `finra_demo`, `barrier_test`,
`rdma_wasm_img_pipeline` (single-machine loopback, full RDMA mesh).

### Status

Phase 2 is **structurally complete** at 2.4d.  Any host-side
allocation site can transparently exceed the 2 GiB direct cap by
falling through to paged allocation; chain rewrites remain correct
under the pre-existing single-writer protocol; the barrier, RDMA data
plane, and existing demos are unaffected.  The guest stays fully in
direct mode and is unchanged from Phase 1.

---

## Phase 3 — RDMA integration (Path C, live)

Phase 3 extends the two-tier storage principle to the RDMA data path.
Where Phase 2 gave host-side allocators an overflow tier via the
`GlobalPool`, Phase 3 gives the RDMA receiver an overflow tier via a
separately-registered host-side MR (**MR2**), and the sender a pair
of extension MRs that let a single transfer span MR1, SHM past MR1,
and paged-mode source pages.  The design uses **copy-to-MR1 on
receive completion** so the guest consumer remains transparent:
received data transits MR2 during the RDMA DMA, then CPU-copies into
freshly-allocated MR1 (or grown-SHM) pages before the chain is linked
into the target slot.

**Current status (landed):**

- `EXTENDED_RDMA_ENABLED = true` (`common/src/lib.rs`)
- Wire-format extension: `DestReply { SingleMr | UseMr2 }` carried in
  the SI Phase-2 reply (`connect/src/rdma/exchange.rs`).  No
  persistent mesh-wide rkey announcement — rkey piggybacks on each
  transfer's handshake.
- Receiver-side trigger in `recv_si`: when `total_bytes` exceeds
  MR1's remaining bump budget, lazily register/grow MR2
  (`/dev/shm/webs-rdma-mr2-<pid>`), reserve a region, reply
  `UseMr2`, then memcpy MR2 → MR1 page chain after `wait_done`
  (`host/src/runtime/remote/sender_initiated.rs`,
  `host/src/runtime/remote/shm.rs::alloc_and_link_from_buf`).
- Sender-side multi-MR SGE construction in `collect_src_sges`: per-
  page lkey selection picks MR1, MR-src-ext (SHM past
  `INITIAL_SHM_SIZE`), or MR-src-stage (paged-mode memcpy
  staging).  Each lives in `connect/src/mesh/src_mr.rs`.
- Idle-timeout shrink at the top of every `recv_si` / `send_si`:
  MR2, MR-src-ext, and MR-src-stage are dropped after
  `MR2_IDLE_TIMEOUT_NANOS` (default 5s) of no use, returning pinned
  memory without a background thread.
- Python-compat toggle: at DAG load the runtime scans for
  `PyFunc` / `PyPipeline` nodes and calls
  `MeshNode::set_python_compat(...)`.  The MR2 memcpy-back branches
  on this flag:
  - **Rust-only DAGs** → `reclaimer::alloc_page` (free-list +
    direct bump + paged-mode fallback — most efficient).  Chain may
    contain paged-mode PageIds, transparently handled by
    `ResolutionBuffer`.
  - **Python DAGs** → direct bump + `ensure_shm_capacity` (grows
    SHM via `MAP_FIXED` remap, bumps `global_capacity`).  Always
    lands in direct-mode PageIds so `shm.py`'s offset-based file
    reads succeed.

**Ceilings:**

| Knob | Rust DAGs | Python DAGs |
|---|---|---|
| Receive-side SHM growth (`ensure_shm_capacity`) | capped at `CAPACITY_HARD_LIMIT = 2 GiB` (wasm32 direct window) | capped at `CAPACITY_HARD_LIMIT_PYTHON ≈ 4 GiB - PAGE_SIZE` (ShmOffset u32 arithmetic) |
| MR2 backing pool hard limit | `RDMA_MR2_HARD_LIMIT = 16 GiB` (virtual reservation) — actual commit doubles from `RDMA_MR2_INITIAL_SIZE = 512 MiB` |

### Why copy-to-MR1 instead of direct MR2 consumption

When we surveyed the existing RDMA DAGs in `DAGs/rdma_demo_dag/` and
`DAGs/rdma_workload_dag/`, every one of them feeds `RemoteRecv` data
into a wasm guest function either directly or through a host-side
routing primitive (Aggregate, Shuffle, Bridge).  Guests can only
dereference PageIds strictly below `DIRECT_LIMIT` — they do
`SHM_BASE + id as usize` with no resolve step, and values above the
direct window would truncate catastrophically on wasm32.

That means a chain stored in a slot that feeds a guest **must** be
composed of direct-window PageIds.  Path C satisfies this by staging
the RDMA receive in MR2 and copying to MR1 before touching any slot.
The memcpy is a real cost (one pass over each received byte), but it
is the only option that preserves the "guest is transparent"
principle.  Alternative paths — teaching every consumer to dispatch
through a `resolve()` step, using MAP_FIXED overlays into the wasm32
window (incompatible with RDMA MR pinning), or remapping MR2 to
appear VA-adjacent to MR1 — were ruled out on complexity or
correctness grounds.  See the ADR-style trade-off discussion below
for the full argument.

### Memory layout

```
host virtual space
────────────────────────────────────────────────────────────────────
  wasm32 linear memory (per subprocess)
  ┌───────────────────────────────────────┐
  │  guest heap / stack    [0, 2 GiB)     │
  │  direct SHM window     [2 GiB, 4 GiB) │  ← MR1
  └───────────────────────────────────────┘

  RdmaPool reservation (per process, lazily created)
  ┌───────────────────────────────────────┐
  │  RDMA_MR2_HARD_LIMIT = 16 GiB virtual │  ← MR2
  │  PROT_NONE → MAP_FIXED committed      │
  │  by doubling from 512 MiB             │
  └───────────────────────────────────────┘
```

`RdmaPool` uses the same reserve-once / commit-incrementally pattern
as `GlobalPool`: at creation it reserves `RDMA_MR2_HARD_LIMIT` bytes
of virtual address space with `mmap(PROT_NONE | MAP_ANONYMOUS)`, then
overlays the first `RDMA_MR2_INITIAL_SIZE` bytes with a backing file
at `/dev/shm/webs-rdma-<pid>`.  Growth is a `MAP_FIXED` overlay onto
the remaining reserved VA.  The base pointer never moves, so
`host_addr_of(id)` stays valid forever.

### PageId encoding

A single high-bit marker distinguishes the three PageId ranges:

| Range | Marker | Meaning |
|---|---|---|
| `[0, DIRECT_LIMIT)` = `[0, 2 GiB)` | — | Direct SHM window.  Host: `splice_addr + id`.  Guest-reachable. |
| `[DIRECT_LIMIT, RDMA_MR2_MARKER)` = `[2 GiB, 2^48)` | — | General extended pool (`GlobalPool`).  Host: `global_pool.host_addr_of(id)`. Guest not reachable. |
| `[RDMA_MR2_MARKER, ∞)` = `[2^48, 2^48 + 16 GiB)` | `RDMA_MR2_MARKER = 1 << 48` | RDMA overflow pool (`RdmaPool`).  Host: `rdma_pool.host_addr_of(id)`.  Guest not reachable; **never stored in slot atomics** (see Path C copy-back). |

The marker is chosen well above `common::GLOBAL_POOL_HARD_LIMIT`
(currently 8 GiB = 2^33) so the three ranges are trivially
distinguishable with a single numeric comparison.  An assertion in
`rdma_pool::tests::rdma_marker_is_safely_above_general_pool_range`
locks in the invariant at test time.

### Feature flag

```rust
pub const EXTENDED_RDMA_ENABLED: bool = false;   // default: OFF
pub const RDMA_MR1_BUDGET:       u64  = 512 * 1024 * 1024;   // 512 MiB
pub const RDMA_MR2_REG_THRESHOLD: u64 = 256 * 1024 * 1024;   // 256 MiB
pub const RDMA_MR2_INITIAL_SIZE:  u64 = 512 * 1024 * 1024;   // 512 MiB
pub const RDMA_MR2_HARD_LIMIT:    u64 = 16  * 1024 * 1024 * 1024; // 16 GiB
```

- **Default is `false`** because the mesh integration isn't done
  yet — enabling the flag without the wiring would not cause
  corruption (it's the scaffold short-circuit path), but it would
  initialize a pool that nothing actually fills.
- Independent of `EXTENDED_POOL_ENABLED`: either flag can be on or
  off in any combination.
- The `u64` typing is deliberate: `common` is shared with the
  wasm32 guest crate where `usize` is 32-bit, so `16 * 1024^3`
  overflows `usize`.  Host-side code aliases the constant to
  `usize` at the module boundary.

### Path C flow (when fully wired)

```
Sender                           Receiver
──────                           ────────
collect_src_sges()               alloc_and_link()
                                   ├── MR1 has room?
                                   │    └── yes: direct bump as today,
                                   │              return AllocLoc::Mr1
                                   │
                                   └── no: spillover path
                                        ├── ensure_mr2()   (lazy)
                                        ├── mr2_alloc_contiguous(n_pages)
                                        └── return AllocLoc::Mr2
                                             (dest_addr, mr2_rkey,
                                              n_pages)

           ── TCP: dest descriptor ───────►

rdma_write_page_chain()          (HCA DMAs into MR1 or MR2 based on
  ├── if MR1 target: existing       receiver's rkey choice)
  │     rkey/base pair
  └── if MR2 target: use
        SendChannel::remote_mr2
        (rkey, base)

           ── TCP: done signal ────────────►

                                 post_receive_stage_to_mr1()
                                   ├── for each MR2 page in the
                                   │   staged region:
                                   │     alloc direct MR1 page
                                   │     memcpy mr2_page.data →
                                   │       direct_page.data
                                   │     chain direct_page into
                                   │       target slot
                                   ├── free MR2 pages back to
                                   │   rdma_pool's freelist
                                   └── link_to_slot(direct head/tail)
                                          (slot now contains only
                                           direct-window PageIds)
```

The key invariant: **the slot atomic is only written after the
copy-to-MR1 step completes**.  Downstream consumers — guest or
host — observe only direct-window PageIds and are fully transparent
to the existence of MR2.

### What's implemented (scaffold, Phase 3.0)

- `common::EXTENDED_RDMA_ENABLED` flag and sizing constants.
- `extended_pool::rdma_pool::RdmaPool` — full host-side storage
  implementation.  `alloc` / `alloc_contiguous` / `free` /
  `host_addr_of` / `file_offset_of` / `backing_file` /
  `trim_freelist` / `expand`.  6 unit tests covering bump, LIFO
  freelist, expansion via doubling, host-addr invariance across
  expansion, file-offset inverse, and the range-separation
  assertion above.
- `extended_pool::rdma_runtime` — process-wide singleton with
  `RDMA_MR1_USED: AtomicU64` pressure counter, `POOL:
  OnceLock<Mutex<RdmaPool>>` lazy pool, and lock-free
  `should_use_mr2` / `should_register_mr2` / `record_mr1_usage`
  predicates.  `ensure_pool` / `alloc_contiguous` / `free` /
  `host_addr_of` helpers.  3 unit tests for the threshold
  arithmetic.
- Every entry point in `rdma_runtime` short-circuits at its first
  statement when `EXTENDED_RDMA_ENABLED == false`, so the compiler
  eliminates the bodies.  Enabling the flag at build time changes
  nothing observable yet because nothing calls into the new module
  from production code paths.

### Path C integration TODO (future work)

When a real workload demands this capability, the following are
the specific integration points.  They are ordered by dependency:

1. **Add an `additional_mr` facility to `connect::MeshNode`.**
   A new `register_additional_mr(base, size)` method that calls
   `ibv_reg_mr` on the given virtual address range and returns the
   resulting rkey.  Broadcast the new `(mr2_base, mr2_rkey)` pair
   to every peer via a new `MR2Announce` control-plane message
   over the existing `ctrl_as_sender` / `ctrl_as_receiver` TCP
   channels.  Store the received peer pair in `SendChannel::remote_mr2`.
   This must work for the full N-peer mesh, not just loopback
   pairs.

2. **Hook MR2 registration into `rdma_runtime::ensure_pool`.**
   After creating the `RdmaPool`, call
   `mesh.register_additional_mr(pool.base_addr(),
   pool.reservation_size())`.  This is the `TODO(Path C)` marker
   currently in `rdma_runtime::ensure_pool`.  Error handling:
   register failure (HCA capacity exhausted, ODP unsupported,
   etc.) should roll back to MR1-only operation for this subprocess
   and log a warning.

3. **Extend the wire protocol in `sender_initiated.rs` /
   `receiver_initiated.rs`.**
   Today the handshake message is `dest_off: u32` (or `u64`).
   Widen it to a struct:

   ```rust
   struct DestDescriptor {
       which_mr: u8,      // 0 = MR1, 1 = MR2
       dest_addr: u64,    // remote virtual address offset
       n_pages: u32,
   }
   ```

   Bump the protocol version byte at the start of every control
   message so old peers can refuse cleanly.

4. **Branch rkey selection in `rdma::rdma_write_page_chain` /
   `rdma::rdma_write_flat`.**  At the point where the sender picks
   `(remote_base, remote_rkey, lkey)`, consult the descriptor's
   `which_mr`:

   ```rust
   let (remote_base, remote_rkey) = match desc.which_mr {
       0 => (ch.remote_mr_base, ch.remote_rkey),
       1 => ch.remote_mr2.expect("MR2 not registered"),
       _ => unreachable!(),
   };
   ```

   Everything else in the write helper is unchanged.  The HCA DMAs
   into whichever MR the receiver chose.

5. **Add the spillover branch to `shm::alloc_and_link`.**
   Pseudocode:

   ```rust
   let needed = n_pages * PAGE_SIZE;
   if !rdma_runtime::should_use_mr2(needed) {
       // Existing MR1 path.
       let dest_off = sb.bump_allocator.fetch_add(needed, AcqRel);
       // capacity check, structure chain headers, link to slot.
       rdma_runtime::record_mr1_usage(needed);
       return AllocLoc::Mr1 { dest_off };
   }
   // Spillover path.
   rdma_runtime::ensure_pool()?;   // lazy first-time creation
   let mr2_head = rdma_runtime::alloc_contiguous(n_pages)?;
   // Structure headers via rdma_pool::runtime::host_addr_of(...)
   // DO NOT link to slot yet — that happens after the post-
   // receive copy completes.
   return AllocLoc::Mr2 { mr2_head, n_pages };
   ```

   Note: the `n_pages` must fit inside a single contiguous MR2
   allocation.  Very large transfers exceeding the current MR2
   committed size will trigger doubling growth inside
   `alloc_contiguous` — that can fail at `RDMA_MR2_HARD_LIMIT`.
   Operators who need more should raise the hard limit.

6. **Implement `post_receive_stage_to_mr1`.**  After the RDMA WRITE
   completes and the sender signals done:

   ```rust
   fn post_receive_stage_to_mr1(splice_addr: usize,
                                mr2_head: PageId,
                                n_pages: usize,
                                slot: usize,
                                slot_kind: RemoteSlotKind) -> Result<()> {
       let mut direct_head = 0;
       let mut direct_prev: *mut Page = null_mut();
       for i in 0..n_pages {
           let mr2_id = mr2_head + (i as u64) * PAGE_SIZE as u64;
           let mr2_ptr = rdma_runtime::host_addr_of(mr2_id)?;
           let direct_id = reclaimer::alloc_page(splice_addr)?;
           let direct_ptr = (splice_addr + direct_id as usize) as *mut Page;
           // Copy the data portion.  Header is reconstructed below.
           unsafe {
               ptr::copy_nonoverlapping(
                   (*mr2_ptr).data.as_ptr(),
                   (*direct_ptr).data.as_mut_ptr(),
                   PAGE_DATA,
               );
               (*direct_ptr).cursor.store(
                   (*mr2_ptr).cursor.load(Relaxed),
                   Relaxed,
               );
           }
           if i == 0 { direct_head = direct_id; }
           if !direct_prev.is_null() {
               unsafe { (*direct_prev).next_offset.store(direct_id, Relaxed); }
           }
           direct_prev = direct_ptr;
           // Free the MR2 page back to the pool.
           rdma_runtime::free(mr2_id)?;
       }
       // Terminal next_offset.
       unsafe { (*direct_prev).next_offset.store(0, Release); }
       // Finally link into the slot — this is the publication point.
       link_to_slot(sb, slot, slot_kind, direct_head, /*tail*/ direct_prev);
       Ok(())
   }
   ```

   Memory footprint during the copy is approximately `2 × n_pages
   × PAGE_SIZE` (the MR2 staging bytes plus the freshly-allocated
   MR1 direct pages).  This is the cost of the transparency
   guarantee.  If MR1 is too full to hold the final result, step 6
   fails and the data is lost — callers should treat this as a
   retry-or-fail scenario.

7. **End-to-end test**: a single-machine RDMA loopback transfer
   with a payload large enough to exceed `RDMA_MR1_BUDGET`.  Use
   a synthetic DAG or a large test input file.  Verify the
   received data is correct and the slot atomic ends up pointing
   at direct-window PageIds (not MR2).

8. **Flip the flag on**: change the default in
   `common::EXTENDED_RDMA_ENABLED` to `true`.  Run the full RDMA
   suite and the regular demo suite to confirm no regressions.

9. **Documentation polish**: update this section to reflect the
   completed integration.  Add a "status: integrated and live"
   note here and in the Phase 3 ledger row below.

### Design trade-off notes (ADR)

The design discussion that led to Path C, for future maintainers:

- **Option A (resolve-cascade in every reader)** — teach
  `persistence.rs`, `chain_splicer.rs`, `broadcast.rs`, `shuffle.rs`,
  `organizer.rs`, and the guest `SHM_BASE + id` arithmetic to
  dispatch through a `resolve()` step on every chain walk.
  **Rejected** because it requires touching every host-side
  reader AND fundamentally cannot work in the guest (wasm32 can't
  address past the direct window).  Guests would still be capped
  at 2 GiB.
- **Option B (contiguous host VA for MR1 + MR2)** — mmap MR2
  immediately after MR1 in host virtual memory so `splice_addr + id`
  works for both.  **Rejected** because it depends on the host VA
  layout (wasmtime's wasm32 reservation is ~4-6 GiB, and whether
  the adjacent region is free is implementation-defined) and
  because host `splice_addr + id` still has the same wasm32
  truncation problem for `id >= 4 GiB`.
- **Option C (copy-to-MR1 on receive)** — adopted.  Trades CPU
  memcpy for full consumer transparency and zero guest changes.
  The copy is O(bytes), unavoidable, and runs entirely on the
  receiver host between RDMA completion and slot publication.
  No cross-thread coordination beyond the existing receive-then-
  link pattern.

### Implementation phases (Phase 3 ledger)

| Phase | What landed |
|---|---|
| 3.0 (this commit) | Scaffold: `common::EXTENDED_RDMA_ENABLED` flag + sizing constants, `RdmaPool` struct (6 tests), `rdma_runtime` singleton (3 tests), PageId range assertion. `cargo test` 37/37 pass, all demos + RDMA loopback unchanged. |
| 3.1 | TODO: `connect::MeshNode::register_additional_mr` + TCP `MR2Announce` control message. |
| 3.2 | TODO: hook registration into `rdma_runtime::ensure_pool`. |
| 3.3 | TODO: widen wire protocol in sender-initiated and receiver-initiated protocols; add protocol version byte. |
| 3.4 | TODO: rkey-selection branch in `rdma_write_page_chain` / `rdma_write_flat`. |
| 3.5 | TODO: spillover branch in `shm::alloc_and_link`. |
| 3.6 | TODO: `post_receive_stage_to_mr1` copy-and-link helper. |
| 3.7 | TODO: end-to-end test with >512 MiB loopback transfer. |
| 3.8 | TODO: default-enable the flag, regression pass. |

### Phase 3 status

**Scaffold is complete.**  `RdmaPool` and `rdma_runtime` are fully
functional at the host-side storage layer and unit-tested.  The
flag `EXTENDED_RDMA_ENABLED` is wired through the feature-gate
machinery so enabling it is purely a build-time change.  Phase 3.1
through 3.8 are documented above as a specific, ordered integration
checklist; when a real workload demands >2 GiB per RDMA transfer
into a guest-consumed slot, the work is clearly bounded and can
be completed without any preceding refactor.
