# Executor

The Executor is the core computation engine of the WebAsShared framework. It parses a DAG JSON specification, sets up shared memory, and executes workload stages (WASM or Python) in topologically sorted parallel waves. For multi-machine workloads, it establishes an RDMA full-mesh and transfers slot data between nodes via one-sided RDMA WRITEs.

The Executor is invoked as a subprocess by the [NodeAgent](../NodeAgent/) and is not typically run directly.

## Workspace Crates

| Crate | Description |
|-------|-------------|
| `host` | Orchestrator binary — DAG runner, WASM executor, routing, I/O, RDMA integration |
| `guest` | Rust no_std WASM workloads compiled to `wasm32-unknown-unknown` |
| `common` | Shared memory layout constants and `Superblock` struct (used by host, guest, and NodeAgent) |
| `connect` | RDMA full-mesh networking — libibverbs FFI, `MeshNode`, QP management, atomic ops |

## Building

```bash
cd Executor

# Build the host binary
cargo +nightly build --release

# Build the WASM guest module
cargo +nightly build --release -p guest --target wasm32-unknown-unknown
```

The host binary is at `target/release/host`. The WASM module is at `target/wasm32-unknown-unknown/release/guest.wasm`.

## CLI

```bash
# Run a DAG pipeline (primary mode)
./target/release/host dag <json_path>

# Internal: single WASM function call
./target/release/host wasm-call <shm_path> <wasm_path> <func> <ret_type> <arg> [arg2]

# Internal: persistent WASM worker loop (used by StreamPipeline)
./target/release/host wasm-loop <shm_path> <wasm_path> <func>
```

## Host Runtime Modules

```
host/src/runtime/
├── dag_runner/         DAG parsing, validation, topo-sort, wave execution, node dispatch
│   ├── mod.rs          run_dag_file, run_dag_json, run_dag
│   ├── types.rs        All JSON schema structs and enums (Dag, DagNode, NodeKind, ...)
│   ├── plan.rs         validate_dag, topo_sort, build_waves
│   ├── dispatch.rs     execute_node — all NodeKind match arms
│   ├── pipeline.rs     StreamPipeline / PyPipeline wave execution with RDMA overlap
│   ├── grouping.rs     WasmGrouping / PyGrouping sequential execution
│   └── workers.rs      spawn_wasm_subprocess, spawn_python_subprocess
├── remote/             RDMA send/recv protocols
│   ├── mod.rs          execute_remote_send, execute_remote_recv dispatcher
│   ├── sender_initiated.rs   SI protocol: send_si, recv_si
│   ├── receiver_initiated.rs RI protocol: send_ri, recv_ri
│   ├── shm.rs          collect_src_sges, alloc_and_link, link_to_slot
│   └── rdma.rs         rdma_write_page_chain, rdma_write_flat
├── input_output/       File I/O, persistence, logging
│   ├── slot_loader.rs  File → I/O slot (with background prefetch)
│   ├── slot_flusher.rs I/O slot → file
│   ├── persistence.rs  PersistenceWriter, record readers
│   └── logger.rs       HostLogger (SHM log arena)
├── mem_operation/      Memory management
│   ├── reclaimer.rs    Page allocator, free-list, madvise trim
│   └── slicer.rs       StreamPipeline tick executor
├── worker.rs           wasmtime loader, VMA setup, wasm-loop
└── manager.rs          Legacy test orchestration
```

## WASM Guest Workloads

```
guest/src/workloads/
├── word_count.rs       wc_distribute, wc_map, wc_reduce
├── tfidf.rs            tfidf_map, tfidf_reduce (floor_log2 IDF for no_std)
├── finra.rs            finra_fetch_private/public, finra_audit_rule ×8, finra_merge_results
├── ml_training.rs      ml_partition, ml_pca (f32 power-iteration), ml_train, ml_validate
├── img_pipeline.rs     img_load_ppm, img_rotate, img_grayscale, img_equalize, img_blur, img_export_ppm
├── stream_pipeline.rs  Pipeline source/filter/transform/sink
├── demos.rs            Basic demo functions
└── routing_tests.rs    Shuffle/aggregate stress tests
```

## Python Guest Workloads

```
py_guest/python/
├── runner.py           Entry point: one-shot and --loop modes
├── shm.py              WASI-compatible SHM access (seek+read, no mmap)
├── word_count.py       wc_distribute, wc_map, wc_reduce
├── ai_workload.py      tfidf_map, tfidf_reduce (math.log IDF)
├── finra_workload.py   finra_fetch_private/public, finra_audit_rule ×8, finra_merge_results
├── ml_workload.py      ml_partition, ml_pca, ml_redistribute, ml_train, ml_validate
└── image_process.py    Image processing pipeline functions
```

## Shared Memory Layout

Defined in `common/src/lib.rs`. All offsets are deterministic:

| Region | Offset | Size |
|--------|--------|------|
| Superblock | 0 | 24 KiB |
| Registry Arena | 24 KiB | 1 MiB |
| RDMA Scratch | ~1 MiB | 8 KiB |
| Atomic Arena | ~1 MiB | 1 MiB |
| Log Arena | ~2 MiB | 16 MiB |
| Bump-allocated pages | ~18 MiB | grows on demand |

Stream and I/O slots are singly-linked lists of 4 KiB pages. Routing operations (Bridge, Aggregate, Shuffle) manipulate page-chain head/tail pointers atomically — no data copying.

### SHM Allocation Flow

The system uses three distinct memory regions, engaged in order as pressure increases:

```
┌─────────────────────────────────────────────────────────────────┐
│  Region 1 — Direct SHM  (PageId < 2 GiB = DIRECT_LIMIT)        │
│  /dev/shm/webs-<pid>   starts at 16 MiB, grows to 2 GiB        │
│  Accessible by WASM guest and host via raw pointer arithmetic    │
├─────────────────────────────────────────────────────────────────┤
│  Region 2 — GlobalPool  (PageId ≥ DIRECT_LIMIT)                 │
│  /dev/shm/webs-global-<pid>   256 MiB → 8 GiB                  │
│  Activated when Direct SHM usage hits 1.6 GiB (80% of 2 GiB)  │
│  Host resolves paged PageIds via extended_pool::runtime::resolve │
├─────────────────────────────────────────────────────────────────┤
│  Region 3 — RdmaPool  (PageId ≥ 2^48 = RDMA_MR2_MARKER)        │
│  /dev/shm/webs-rdma-<pid>   RDMA staging only                   │
│  Used only when an RDMA transfer exceeds the 256 MiB MR1 budget │
│  Never accessible to WASM guests                                 │
└─────────────────────────────────────────────────────────────────┘
```

**Phase 1 — Direct mode (bump offset 0 → 1.6 GiB)**

`reclaimer::alloc_page` checks the free list first, then bumps the `bump_allocator` atomic in the Superblock. If the bump would exceed `global_capacity`, `shm::try_grow_shm` is called: it doubles the SHM file size with `ftruncate` and remaps the VMA in-place with `mmap(MAP_FIXED)` — no pages are copied, and all existing pointers remain valid. Growth continues geometrically (16 MiB → 32 → 64 → … → 2 GiB). All resulting PageIds are plain byte offsets within the SHM file.

**Phase 2 — Paged mode flip (bump offset crosses 1.6 GiB)**

When `extended_pool::mode::should_enter_paged(bump)` returns true (i.e. `bump × 10 ≥ DIRECT_LIMIT × 8`), the extended pool transitions to Paged mode. A GlobalPool backing file is created at `/dev/shm/webs-global-<pid>` and a `ResolutionBuffer` is seeded so the host can map any paged PageId (≥ `DIRECT_LIMIT = 2 GiB`) to a host pointer via `extended_pool::runtime::resolve(id, splice_addr)`.

**Phase 3 — Paged mode (overflow into GlobalPool)**

Once in Paged mode, `reclaimer::alloc_page` rolls back the SHM bump and calls `extended_pool::runtime::alloc_paged_page` instead. The GlobalPool grows geometrically from 256 MiB up to 8 GiB. The host accesses paged pages through `resolve`; WASM guests use `ResolutionBuffer` to translate paged PageIds transparently.

**SlotLoader (Phase 2.4d)**

`SlotLoader` calls `reclaimer::alloc_page` which may return a paged PageId (≥ `DIRECT_LIMIT`). All page accesses go through `self.page_ptr(id)` → `extended_pool::runtime::resolve(id, splice_addr)`, so both direct and paged allocations are handled uniformly without any special-casing in the I/O path.

**Example: 1 GiB word-count corpus**

With a 1 GiB input file the SHM starts at 16 MiB and grows: 16 → 32 → 64 → 128 → 256 → 512 MiB → 1 GiB. All pages remain in Direct mode (PageId < 2 GiB). GlobalPool and RdmaPool are never touched. The corpus bytes land in SHM page chains that the WASM `wc_distribute` function reads directly by pointer arithmetic — zero copies after the initial `read()` from disk.

## Partitioner and Auto Placement

The Partitioner (`../Partitioner/`) converts a symbolic, location-agnostic DAG into a concrete `ClusterDag` for NodeAgent submission. Instead of hand-writing `RemoteSend`/`RemoteRecv` pairs and slot numbers, you write a logical DAG and the Partitioner inserts them automatically.

### placement_policy field

The `placement_policy` field in a symbolic DAG JSON controls where auto-placed nodes land. It replaces the old `hints` object:

```json
// Legacy (still supported, lower priority)
"hints": { "capacity": { "0": 0.5, "1": 0.5 }, "host_limit": { "0": 12, "1": 12 } }

// New — string shorthand
"placement_policy": "balanced"   // proportional across all hosts
"placement_policy": "pack"       // colocate on fewest hosts (default when field is omitted)
"placement_policy": "spread"     // at most one auto-node per host

// New — parameterised object
"placement_policy": { "type": "weighted", "weights": [0.8, 0.2] }
"placement_policy": { "type": "balanced", "per_host_limit": 4 }
```

Priority order: coordinator live hints > `placement_policy` > `hints` > default (`pack`).

### Key Partitioner changes

- **Slot overlap auto-adjustment** — fan-out nodes on the same machine that share `out_base` ranges are detected at partition time; the second node's base is shifted up automatically, and the corrected base is injected as `arg` into the WASM call so it writes to the right slots.
- **Wave-0 deadlock prevention** — when a machine has both a `RemoteRecv(from=P)` and a `RemoteSend(to=P)` with no ordering dependency, the Partitioner adds `RemoteSend → dep of RemoteRecv` (if cycle-safe) so sends fire before the matching recv blocks.
- **Empty `host_limit` semantics** — an absent `host_limit` map now means "no per-host cap" rather than defaulting to 1, allowing all auto-placed nodes to land on one host when using `pack`.

See `../Partitioner/README.md` for the full symbolic DAG format and policy reference.

## DAG Node Types

The full set of supported node kinds (from `types.rs`):

- **Compute**: `WasmVoid`, `WasmU32`, `WasmFatPtr`, `PyFunc`
- **Pipeline**: `StreamPipeline`, `PyPipeline`, `WasmGrouping`, `PyGrouping`
- **Routing**: `Bridge`, `Aggregate`, `Shuffle`, `Broadcast`
- **I/O**: `Input`, `Output`, `Persist`, `Watch`, `FreeSlots`
- **Dispatch**: `FileDispatch`, `OwnedDispatch`
- **RDMA**: `RemoteSend`, `RemoteRecv`, `RemoteAtomicFetchAdd`, `RemoteAtomicCmpSwap`, `RemoteAtomicPush`

## Data Files

```
data/
├── corpus.txt              Text corpus for word count / TF-IDF
├── trades.csv              50K synthetic trades for FINRA audit
├── mnist_features.csv      10K samples, 28 features for ML training
├── sample.txt              Small test file
├── img_gradient.ppm        Test image (PPM format)
├── img_checkerboard.ppm    Test image (PPM format)
└── img_rings.ppm           Test image (PPM format)
```
