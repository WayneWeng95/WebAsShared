# runtime

Host-side execution engine for WebAsShared.  The runtime manages the WASM execution
environment, shared memory (SHM) page layout, input/output I/O, and the DAG-based
job scheduler.

Each sub-folder has its own `OVERVIEW.md` with detailed documentation.

---

## Directory structure

```
runtime/
├── worker.rs          — Wasmtime engine setup, VMA mapping, host imports, WASM call entry points
├── test.rs            — Legacy integration test worker roles (run_worker, routing tests)
├── manager.rs         — Legacy integration test orchestrator (basic read/write + routing tests)
│
├── dag_runner/        — DAG-based job scheduler (see dag_runner/OVERVIEW.md)
├── input_output/      — File↔SHM I/O layer (see input_output/OVERVIEW.md)
└── mem_operation/     — SHM page allocator, file slicer, and bucket organizer (see mem_operation/OVERVIEW.md)
```

---

## worker.rs — Wasmtime execution environment

Core WASM runtime helpers shared by all execution paths (DAG runner subprocesses,
loop workers, and legacy test workers).  Every WASM subprocess starts here.

### Types

| Type | Description |
|---|---|
| `WorkerState` | Per-store runtime context: the open SHM `File` handle and `splice_addr` (the virtual address where the SHM region is mapped inside the WASM address space). |

### Functions

| Function | Description |
|---|---|
| `create_wasmtime_engine()` | Build a `wasmtime::Engine` configured for shared memory: 4 GB static address space, no guard pages (VMA managed manually), threads enabled. Called once per subprocess before module loading. |
| `setup_vma_environment(store, linker, file)` | Allocate the WASM shared memory (3–4 GB virtual), map the SHM file at `TARGET_OFFSET` inside it, and register all host imports: `host_remap` (SHM grow), `host_resolve_atomic` (atomic name registry), and WASI no-op stubs for MicroPython guest modules. Returns the `Memory` handle for direct host-side SHM reads. |
| `run_wasm_loop(shm_path, wasm_path, func)` | Persistent WASM call loop. Reads `"<arg0> <arg1>\n"` from stdin, calls `func(arg0, arg1)` for each line, writes `"ok\n"` or `"err: …\n"` to stdout, and exits on EOF. Used by `WasmLoopWorker` in the DAG runner's `pipeline.rs` and `grouping.rs`. |
| `run_wasm_call(shm_path, wasm_path, func, ret_type, arg, arg1)` | One-shot WASM execution. Loads the module, calls `func` with the signature selected by `ret_type` (`"void"`, `"void2"`, `"u32"`, `"fatptr"`), prints the result if applicable, and exits. Used by DAG runner one-shot nodes (`WasmVoid`, `WasmU32`, `WasmFatPtr`). |
| `run_worker(role, shm_path, id)` | **Moved to `test.rs`.** See below. |

### Host imports registered by `setup_vma_environment`

| Import | Signature | Behaviour |
|---|---|---|
| `env::host_remap` | `(new_size: u32) → ()` | Calls `expand_mapping` to grow the SHM file and re-mmap it at the existing `splice_addr`. |
| `env::host_resolve_atomic` | `(ptr: u32, len: u32) → u32` | Looks up a name in the SHM Registry under a spinlock; allocates a new entry if absent. Returns the entry's u32 index into the atomic arena. |
| `wasi_snapshot_preview1::fd_write` | WASI stub | No-op — WASM output goes through SHM, not stdout. |
| `wasi_snapshot_preview1::fd_close` | WASI stub | No-op. |
| `wasi_snapshot_preview1::fd_seek` | WASI stub | No-op. |
| `wasi_snapshot_preview1::fd_fdstat_get` | WASI stub | Returns `EBADF`. |

---

## test.rs — Legacy integration test worker roles

Subprocess entry point for all legacy test roles spawned by `manager.rs`.
`run_worker` dispatches on a role string, loads the WASM module, and executes
the corresponding test scenario.  Not used by the DAG runner.

### Function

| Function | Description |
|---|---|
| `run_worker(role, shm_path, id)` | Dispatch on `role`: run the WASM guest function for `"writer"` / `"reader"` / `"func_a"` / `"func_b"`, or execute one of the nine routing test scenarios. Uses `BucketOrganizer` after the write/func_a phases to resolve conflicts and GC pages. |

Routing test roles: `stream_bridge_test`, `aggregate_test`, `shuffle_test`,
`shuffle_roundrobin_test`, `shuffle_fixedmap_test`, `shuffle_heavy_test`,
`shuffle_roundrobin_heavy_test`, `shuffle_fixedmap_heavy_test`, `broadcast_heavy_test`.

---

## organizer.rs — moved to mem_operation/organizer.rs

`BucketOrganizer` now lives in `crate::runtime::mem_operation::organizer`.
See the mem_operation OVERVIEW.md for documentation.

After all WASM writers finish, the `BucketOrganizer` scans the SHM hash-bucket map,
resolves write conflicts using a pluggable `ConsumptionPolicy`, commits the winning
payload to the Registry, and returns all losing pages to the free list.

### Type

| Type | Description |
|---|---|
| `BucketOrganizer<'a>` | Holds the SHM base pointer. Lifetime tied to the `Store` that owns the WASM memory. |

### Methods

| Method | Visibility | Description |
|---|---|---|
| `new(store, memory)` | `pub` | Anchor the organizer at the SHM base (`memory.data_ptr + TARGET_OFFSET`). |
| `consume_all_buckets(policy)` | `pub unsafe` | Scan all `BUCKET_COUNT` hash buckets; for each non-empty bucket: atomically detach its conflict list, apply `policy` to select a winner, commit the winner's offset+length to the Registry, and GC all loser page chains. Must be called after all writers have finished. |
| `process_detached_list(head_offset, bucket_idx, policy)` | `fn` (private) | Deserialize each node's multi-page payload, call `policy.process`, commit the winner, and free losers via `free_lob_chain`. |
| `recycle_chain(list_head)` | `fn` (private) | Push every top-level conflict-list node back onto the free list (walks `next_node` links, not payload page chains). |
| `push_to_free_list(page_offset)` | `fn` (private) | Delegate single-page free to `reclaimer::free_page_chain`. |

---

## manager.rs — Legacy integration test orchestrator

A standalone test harness predating the DAG runner.  `run_manager` initialises SHM,
spawns reader/writer worker subprocesses (basic read/write phase), then runs a
sequence of routing tests — each in its own freshly-formatted SHM region — to
exercise the full host routing API end-to-end.

**Not used by the DAG runner.** Invoked only via the `manager` subcommand of the
host binary.

### Function

| Function | Description |
|---|---|
| `run_manager()` | Entry point: format SHM, spawn 4 readers + 4 writers, wait, run `func_a` / `func_b` sequentially, then run 9 routing tests (HostStream bridge, Aggregate N→1, Shuffle × 3 policies light + heavy, Broadcast heavy). |

### Routing tests executed

| Role string | Scenario |
|---|---|
| `stream_bridge_test` | HostStream 1→1 bridge: P0 → slot 0, host wires slot 0 → slot 1, WASM reads slot 1. |
| `aggregate_test` | AggregateConnection 3→1: P0/P1/P2 → slots 0/1/2, merged into slot 3, 9 records verified. |
| `shuffle_test` | ShuffleConnection ModuloPartition 2→2 (light: 3 rec/upstream). |
| `shuffle_roundrobin_test` | ShuffleConnection RoundRobinPartition 2→2 reversed upstreams (light). |
| `shuffle_fixedmap_test` | ShuffleConnection FixedMapPartition 2→2 swapped `{0→1, 1→0}` (light). |
| `shuffle_heavy_test` | ShuffleConnection ModuloPartition 50→10 (150 rec/upstream, 750 rec/downstream). |
| `shuffle_roundrobin_heavy_test` | ShuffleConnection RoundRobinPartition 50→10 reversed (heavy). |
| `shuffle_fixedmap_heavy_test` | ShuffleConnection FixedMapPartition 50→2 uneven split 0-29→slot50, 30-49→slot51 (heavy). |
| `broadcast_heavy_test` | BroadcastConnection 20→10 (150 rec/upstream, 3000 rec/downstream). |

---

## Sub-folder summaries

| Folder | Purpose |
|---|---|
| `dag_runner/` | JSON-driven DAG executor: topo-sort, wave scheduling, subprocess management, pipeline/grouping execution, node dispatch. See `dag_runner/OVERVIEW.md`. |
| `input_output/` | File↔SHM bridge: `SlotLoader` (file→slot), `SlotFlusher` (slot→file), `PersistenceWriter` (background SHM snapshot), `HostLogger` (structured log into SHM). See `input_output/OVERVIEW.md`. |
| `mem_operation/` | SHM memory management: `reclaimer` (sharded free-list + bump allocator + cursor reset + trim), `slicer` (partition `MappedFile` for parallel dispatch). See `mem_operation/OVERVIEW.md`. |
