# WebAssembly Stream Processing Engine

A DAG-based stream processing engine that runs computational workloads inside WebAssembly guest modules, orchestrated by a Rust host process. Data flows between pipeline stages through a **shared memory (SHM) region mapped directly into the WASM address space**, enabling zero-copy routing without serialization overhead. Multi-machine execution is supported via RDMA, with slot data transferred directly between nodes' SHM regions at line rate.

## Overview

Pipelines are defined as JSON DAGs. Each node is a WASM function call, a Python function call, a host-side routing operation, an I/O step, or an RDMA transfer. The host topologically sorts the nodes and executes them in dependency order, grouping independent nodes into parallel waves.

```
Input file
    │
    ▼
[WasmFunc: distribute]  ──► stream slots 10–19
    │
    ├── [WasmFunc: map_0]  ──► slot 110
    ├── [WasmFunc: map_1]  ──► slot 111
    │   ...
    └── [WasmFunc: map_9]  ──► slot 119
              │
              ▼
    [Aggregate: 110–119 → 200]   (host-side, zero-copy)
              │
              ▼
    [WasmFunc: reduce]  ──► I/O slot
              │
              ▼
         Output file
```

## Architecture

### Workspace Members

| Crate | Role |
|-------|------|
| `host` | Orchestrator: DAG runner, WASM executor, routing, I/O, RDMA |
| `guest` | WASM workloads (word count, image pipeline, routing tests) |
| `common` | Shared memory layout definitions (superblock, page, registry) |
| `connect` | RDMA full-mesh: libibverbs FFI, `MeshNode`, atomic ops, SHM-as-MR |

### Shared Memory Layout

The host creates a SHM file and maps it at a fixed offset (`0x80000000`) in each WASM guest's 4 GB address space. Guests access it via ordinary pointer arithmetic — no syscalls needed for data reads/writes. When RDMA is enabled, the same SHM is registered as an RDMA Memory Region so peer nodes can write directly into it.

```
0x80000000  ┌────────────────────────────────┐
            │ Superblock (24 KiB)            │
            │  atomic counters, bump ptr     │
            │  stream heads/tails [2048]     │
            │  I/O heads/tails   [512]       │
            │  free-list shards  [16]        │
            ├────────────────────────────────┤
            │ Registry Arena (1 MiB)         │
            │  name → atomic index mapping   │
            ├────────────────────────────────┤
            │ Atomic Arena (1 MiB)           │
            │  CAS values for named vars     │
            ├────────────────────────────────┤
            │ Log Arena (16 MiB)             │
            │  guest diagnostic output       │
            ├────────────────────────────────┤
            │ RDMA Staging Area              │
            │  N × 1 MiB per peer direction  │
            │  (pre-allocated when RDMA on)  │
            ├────────────────────────────────┤
            │ Stream Pages (bump-allocated)  │
            │  4 KiB pages, linked lists     │
            │  freed via Treiber-stack       │
            └────────────────────────────────┘
```

Each stream slot is a singly-linked list of 4 KiB pages. Routing (aggregate, shuffle, broadcast) is performed by atomically relinking page-chain heads and tails — no data is copied.

### Routing Primitives

| Primitive | Description |
|-----------|-------------|
| `Bridge` | 1→1 zero-copy wire between two stream slots |
| `Aggregate` | N→1 merge; each upstream chain is spliced onto the downstream |
| `Shuffle` | N→M partitioned routing (Modulo, RoundRobin, or FixedMap policy) |
| `Broadcast` | N→M fanout; each upstream is linked to every downstream |

### Node Types (DAG JSON)

| Kind | Description |
|------|-------------|
| `WasmVoid` / `WasmU32` / `WasmFatPtr` | Call an exported function in the Rust WASM guest |
| `PyFunc` | Call a Python workload function (one subprocess per invocation) |
| `StreamPipeline` | Multi-stage WASM pipeline with persistent worker reuse per stage |
| `PyPipeline` | Multi-stage Python pipeline with one persistent process per node |
| `Input` | Load a file into an I/O slot (optional background prefetch) |
| `Output` | Write an I/O slot to a file |
| `Bridge` | Host-side 1→1 stream wire |
| `Aggregate` | Host-side N→1 stream merge |
| `Shuffle` | Host-side N→M partitioned routing |
| `Broadcast` | Host-side N→M fanout |
| `Watch` | Lightweight snapshot of one stream slot or shared-state entry |
| `Persist` | Snapshot selected SHM state to disk |
| `FreeSlots` | Release stream/I/O slots back to the allocator |
| `RemoteSend` | RDMA-WRITE a slot to a peer node's SHM staging area |
| `RemoteRecv` | Receive a slot written by a peer via RDMA into local SHM |

## Multi-machine Execution (RDMA)

When the DAG JSON includes an `rdma` block, the host establishes a full RC QP mesh across all nodes and registers the SHM as an RDMA Memory Region. `RemoteSend` / `RemoteRecv` node pairs transfer slot data between machines with no TCP memcopy:

```json
{
  "shm_path": "/dev/shm/dag",
  "rdma": { "node_id": 0, "total": 2, "ips": ["10.0.0.1", "10.0.0.2"], "transfer": true },
  "nodes": [
    { "id": "produce", "deps": [],          "kind": { "WasmVoid": { "func": "produce", "arg": 0 } } },
    { "id": "send",    "deps": ["produce"], "kind": { "RemoteSend": { "slot": 0, "slot_kind": "Stream", "peer": 1 } } }
  ]
}
```

### Transfer protocol

1. Sender walks the source slot's page chain and builds an SGE list pointing directly at the page data fields.
2. A single RDMA WRITE (scatter-gather, chunked across multiple WRs if needed) DMAs the data into the peer's staging area — **no CPU copy** of the payload.
3. An RDMA Fetch-and-Add on the peer's staging ready-counter (hardware atomic) signals completion; a TCP byte is sent as a fallback.
4. Receiver spin-polls the counter (100 ms), then falls back to the TCP signal. Appends the raw bytes directly into the target slot.

### RDMA consistency

`RemoteSend` must list the slot producer as a DAG dependency. This guarantees the page chain is sealed (producer finished) before the RDMA DMA reads from it. The CQ poll inside the send path ensures DMA is complete before the node returns and the DAG runner reclaims those pages.

### `rdma.transfer` flag

Set `"transfer": false` to connect the mesh for RDMA atomics (`fetch_and_add`, `compare_and_swap`) only, without pre-allocating the staging area or enabling `RemoteSend`/`RemoteRecv`.

## Example DAGs

### Rust Word Count (`demo_dag/word_count_demo.json`)

```
load (corpus.txt → slot 0)
  → wc_distribute (slot 0 → slots 10–19, round-robin)
  → wc_map × 10   (slots 10–19 → slots 110–119, parallel wave)
  → Aggregate     (slots 110–119 → slot 200, host zero-copy)
  → wc_reduce     (slot 200 → I/O slot)
  → save          (I/O slot → result.txt)
```

### Python Image Pipeline (`demo_dag/py_img_pipeline_demo.json`)

```
load (3 PPM images → slot 10)
  → PyPipeline: img_load_ppm → img_rotate → img_grayscale
              → img_equalize → img_blur → img_export_ppm
  → save (→ 3 PGM output files)
  → FreeSlots
```

## Building

```bash
# Build all crates
cargo build --release

# Build only the host
cargo build --release -p host

# Build only the WASM guest (requires wasm32-wasip1 target)
cargo build --release -p guest --target wasm32-wasip1
```

## Running

```bash
# Run a DAG pipeline
cargo run --release -p host -- dag demo_dag/word_count_demo.json

# Run a single WASM function call (used internally by the DAG runner)
cargo run --release -p host -- wasm-call <wasm_path> <func> <arg> <shm_path>

# Persistent WASM worker loop (used internally by StreamPipeline)
cargo run --release -p host -- wasm-loop <shm_path> <wasm_path> <func>

# Built-in routing / stress tests
cargo run --release -p host -- shuffle_test
cargo run --release -p host -- aggregate_test
cargo run --release -p host -- stream_bridge_test
```

## Python Guest

Python workloads run `python-3.12.0.wasm` under `wasmtime` with WASI. The `PyPipeline` node kind keeps one `runner.py --loop` process alive for the lifetime of the node, eliminating per-stage spawn overhead:

| | `PyFunc` (per node) | `PyPipeline` (N stages) |
|--|--|--|
| Process spawns per run | N | 1 |
| Python startup cost | paid N× | paid once |

WASI has no `mmap(2)`, so SHM reads use seek+read. Pre-AOT-compile Python WASM to reduce JIT overhead:

```bash
wasmtime compile python-3.12.0.wasm -o python-3.12.0.cwasm
```

Then reference the `.cwasm` path in the DAG JSON.

## Project Structure

```
astest/
├── Cargo.toml                  # Workspace (host, guest, common, connect)
├── common/src/lib.rs           # Superblock, Page, RegistryEntry definitions
├── connect/src/
│   ├── ffi.rs                  # Hand-written libibverbs FFI (rdma-core 50.0)
│   ├── ibverbs_helpers.c       # C wrappers for static-inline ibv_ functions
│   ├── mesh.rs                 # MeshNode: full-mesh RC QPs, write/atomic ops
│   ├── remote.rs               # RdmaRemote: two-node high-level API
│   └── rdma/
│       ├── context.rs          # RdmaContext (device, PD, CQ)
│       ├── memory_region.rs    # MemoryRegion (alloc or register-external)
│       ├── queue_pair.rs       # QueuePair (state machine, WRITE, FAA, CAS)
│       └── exchange.rs         # TCP QpInfo exchange, send_done/wait_done
├── host/src/
│   ├── main.rs                 # CLI entry point & subcommand dispatch
│   ├── shm.rs                  # SHM creation, mmap, format
│   └── runtime/
│       ├── dag_runner/
│       │   ├── mod.rs          # run_dag_file, run_dag_json, run_dag
│       │   ├── types.rs        # All JSON schema structs and enums
│       │   ├── plan.rs         # validate_dag, topo_sort, build_waves
│       │   ├── dispatch.rs     # execute_node, all NodeKind arms
│       │   └── workers.rs      # spawn_wasm_subprocess, spawn_python_subprocess
│       ├── remote/mod.rs       # execute_remote_send, execute_remote_recv, staging
│       ├── input_output/
│       │   ├── slot_loader.rs  # File → I/O slot (with prefetch)
│       │   ├── slot_flusher.rs # I/O slot → file
│       │   ├── persistence.rs  # PersistenceWriter, record readers
│       │   └── logger.rs       # HostLogger (SHM log arena)
│       ├── mem_operation/
│       │   ├── reclaimer.rs    # Page allocator, free-list, madvise trim
│       │   └── slicer.rs       # StreamPipeline tick executor
│       ├── worker.rs           # wasmtime loader, VMA setup, wasm-loop
│       ├── manager.rs          # Test orchestration
│       └── organizer.rs        # Shared-state conflict resolution
├── guest/src/
│   ├── api/                    # Page allocator, stream/IO/atomic/log APIs
│   └── workloads/              # word_count, img_pipeline, routing_tests
├── py_guest/python/
│   ├── runner.py               # One-shot and --loop entry point
│   └── shm.py                  # WASI-compatible SHM reader (seek+read)
└── demo_dag/                   # Example DAG JSON specifications
```

## Key Design Decisions

- **Zero-copy local routing**: stream data is never copied between pipeline stages on the same machine; only page-chain head/tail pointers are atomically updated.
- **Zero-copy remote transfer**: RDMA scatter-gather DMAs page data directly from source pages into the peer's SHM staging area — the CPU writes only a 12-byte header.
- **Lock-free allocation**: page free-list uses a sharded Treiber stack; registry uses CAS-based bucket chaining.
- **Isolated WASM execution**: each WASM node runs in its own `wasmtime` instance with a fresh linear memory, but shares the same SHM backing file.
- **Persistent pipeline workers**: `StreamPipeline` and `PyPipeline` keep one worker process alive per stage across all ticks, paying the JIT/startup cost only once per run.
- **Fat-pointer returns**: WASM→host data transfer uses 64-bit fat pointers (`ptr << 32 | len`) to pass slice locations without copying.
- **RDMA signalling**: transfer completion uses RDMA Fetch-and-Add (hardware atomic, no remote CPU involvement) as the primary signal, with a TCP byte as a fallback for slow or non-atomic HCAs.

---

```
  WebAsShared — WASM-Based Zero-Copy Stream Processing Engine

  Core Idea: A DAG pipeline executor where each stage runs inside an isolated WebAssembly
  module, but all stages share the same physical memory (via mmap). Data routing between
  stages is just atomic pointer splicing — no copying. Across machines, slot data is
  transferred via RDMA directly into the peer's SHM — still no CPU copy of the payload.

  Architecture

  Host (Rust)                           Guest (WASM / Python)
  ───────────────────────────────────   ──────────────────────────────────────
  • Parses JSON DAG spec                 • Each node = isolated wasmtime instance
  • Creates/manages shared memory        • Fixed address 0x80000000 → SHM file
  • Topological sort + wave execution    • Implements algorithms (word count,
  • Zero-copy routing between stages       image processing, etc.)
  • File I/O, allocation, logging        • Calls back host for SHM growth
  • RDMA full-mesh (MeshNode)            • Python via runner.py --loop (persistent)
  • RemoteSend / RemoteRecv via RDMA

  Shared Memory Layout (at 0x80000000 in every WASM instance)

  - Superblock      — stream/IO slot heads/tails, bump ptr, free-list shards
  - Registry Arena  — name → atomic index mapping
  - Atomic Arena    — CAS values for named shared variables
  - Log Arena       — guest + host diagnostic output
  - Staging Area    — N × 1 MiB per peer direction (RDMA landing zone)
  - Stream Pages    — 4 KiB linked-list pages, bump-allocated + Treiber stack

  Local Routing Operations (all zero-copy, lock-free)

  ┌────────────────┬──────────────────────────────────────────────┐
  │   Operation    │                 Description                  │
  ├────────────────┼──────────────────────────────────────────────┤
  │ Bridge         │ 1→1 passthrough (pointer remap)              │
  ├────────────────┼──────────────────────────────────────────────┤
  │ Aggregate      │ N→1 merge (atomic chain splice)              │
  ├────────────────┼──────────────────────────────────────────────┤
  │ Shuffle        │ N→M partitioned (round-robin, modulo, fixed) │
  ├────────────────┼──────────────────────────────────────────────┤
  │ Broadcast      │ 1→M fan-out                                  │
  ├────────────────┼──────────────────────────────────────────────┤
  │ RemoteSend     │ RDMA scatter-gather → peer SHM (no CPU copy) │
  ├────────────────┼──────────────────────────────────────────────┤
  │ RemoteRecv     │ Wait on RDMA FAA signal, append to slot      │
  └────────────────┴──────────────────────────────────────────────┘

  Example Pipelines

  - Word Count:      Load corpus → distribute → 10× parallel map → aggregate → reduce
  - Image Pipeline:  Load PPMs → PyPipeline (rotate, gray, equalize, blur) → save
  - Multi-machine:   Node 0 produces → RemoteSend → Node 1 RemoteRecv → process

  Tech Stack

  - Rust workspace with 4 crates: host, guest, common, connect
  - wasmtime for WASM execution
  - Python support via python.wasm + WASI (persistent --loop subprocess per pipeline)
  - Lock-free allocation with sharded Treiber stacks
  - RDMA via hand-crafted libibverbs FFI (rdma-core 50.0, RoCE / InfiniBand)

  The key innovation is that all WASM instances share the same physical backing file mapped
  at the same virtual address, so local routing is just atomic pointer manipulation — no
  data copying at all. RDMA extends this across machines: the SHM itself is the RDMA MR,
  so a RemoteSend DMA-writes page data straight into the peer's SHM at the same offset.
```
