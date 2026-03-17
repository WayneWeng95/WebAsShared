# WebAssembly Stream Processing Engine

A DAG-based stream processing engine that runs computational workloads inside WebAssembly guest modules, orchestrated by a Rust host process. Data flows between pipeline stages through a **shared memory (SHM) region mapped directly into the WASM address space**, enabling zero-copy routing without serialization overhead.

## Overview

Pipelines are defined as JSON DAGs. Each node is either a WASM function call, a host-side routing operation, or an I/O step. The host topologically sorts the nodes and executes them in dependency order, with WASM workloads running in isolated `wasmtime` instances and routing steps performed by the host using atomic pointer splicing.

```
Input file
    │
    ▼
[WasmFunc: distribute]  ──► stream slot 10..19
    │
    ├── [WasmFunc: map_0]  ──► slot 110
    ├── [WasmFunc: map_1]  ──► slot 111
    │   ...
    └── [WasmFunc: map_9]  ──► slot 119
              │
              ▼
    [Aggregate: 110-119 → 200]   (host-side, zero-copy)
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
| `host` | Orchestrator: DAG runner, WASM executor, routing, I/O |
| `guest` | WASM workloads (word count, image pipeline, routing tests) |
| `common` | Shared memory layout definitions (superblock, page, registry) |

### Shared Memory Layout

The host creates a SHM file and maps it at a fixed offset (`0x80000000`) in each WASM guest's 4 GB address space. Guests access it via ordinary pointer arithmetic — no syscalls needed for data reads/writes.

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
| `Input` | Load file(s) into an I/O slot |
| `Output` | Write an I/O slot to file(s) |
| `WasmFunc` | Call an exported function in the Rust WASM guest |
| `PyFunc` | Call a Python function via `python.wasm` under WASI |
| `Aggregate` | Host-side N→1 stream merge |
| `Shuffle` | Host-side N→M partitioned routing |
| `Broadcast` | Host-side N→M fanout |
| `FreeSlots` | Release stream/I/O slots back to the allocator |
| `Persist` | Snapshot the current SHM state to disk |

## Example DAGs

### Rust Word Count (`demo_dag/word_count_demo.json`)

```
load (corpus.txt → slot 0)
  → wc_distribute (slot 0 → slots 10–19, round-robin)
  → wc_map × 10   (slots 10–19 → slots 110–119, parallel)
  → Aggregate     (slots 110–119 → slot 200, host zero-copy)
  → wc_reduce     (slot 200 → I/O slot)
  → save          (I/O slot → result.txt)
```

### Python Image Pipeline (`demo_dag/py_img_pipeline_demo.json`)

```
load (3 PPM images → slot 10)
  → img_load_ppm → img_rotate → img_grayscale
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

# Run built-in routing / stress tests
cargo run --release -p host -- shuffle_test
cargo run --release -p host -- aggregate_test
cargo run --release -p host -- stream_bridge_test
```

## Python Guest

Python workloads run `python-3.12.0.wasm` under `wasmtime` with WASI. This works but carries four stacked overheads compared to the Rust guest:

1. **Process spawn per node** — each `PyFunc` node launches a new `wasmtime` subprocess
2. **Fresh JIT per invocation** — `python-3.12.0.wasm` (~50 MB) is compiled from scratch each time
3. **WASI file I/O instead of mmap** — WASI has no `mmap(2)`, so every SHM read is a seek + read pair
4. **Python interpreter warmup** — module imports and ~20–50× slower compute vs. native Rust

**Mitigation:** Pre-AOT-compile the Python WASM to eliminate cost #2:

```bash
wasmtime compile python-3.12.0.wasm -o python-3.12.0.cwasm
```

Then reference the `.cwasm` path in the DAG JSON. Costs #1, #3, and #4 remain fundamental to the WASI-based approach.

> The Python guest is currently in development. Rust/C/C++ guests are recommended for performance-sensitive pipelines.

## Project Structure

```
astest/
├── Cargo.toml              # Workspace (host, guest, common)
├── common/src/lib.rs       # Superblock, Page, RegistryEntry definitions
├── host/src/
│   ├── main.rs             # CLI entry point & subcommand dispatch
│   ├── shm.rs              # SHM creation, mmap, remap
│   ├── runtime/
│   │   ├── dag_runner.rs   # JSON DAG parser, topological sort, executor
│   │   ├── worker.rs       # wasmtime loader, VMA setup, guest function calls
│   │   ├── manager.rs      # Test orchestration
│   │   ├── inputer.rs      # File → I/O slot
│   │   ├── outputer.rs     # I/O slot → file
│   │   ├── reclaimer.rs    # Page allocator, free-list, madvise trim
│   │   ├── organizer.rs    # Conflict resolution (LWW, Majority, MaxId, MinId)
│   │   └── logger.rs       # Shared log arena writer
│   ├── routing/
│   │   ├── stream.rs       # Bridge (1→1)
│   │   ├── shuffle.rs      # Shuffle (N→M)
│   │   ├── aggregate.rs    # Aggregate (N→1)
│   │   ├── broadcast.rs    # Broadcast (N→M fanout)
│   │   └── shm_io.rs       # Low-level chain splicing
│   └── policy/
│       ├── shuffle_policy.rs   # Modulo / RoundRobin / FixedMap
│       └── consumption.rs      # Conflict resolution policies
├── guest/src/
│   ├── api/                # Page allocator, stream/IO/atomic/log APIs
│   └── workloads/          # word_count, img_pipeline, routing_tests
├── py_guest/python/
│   ├── runner.py           # Python entry point (dispatches via env vars)
│   └── shm.py              # WASI-compatible SHM reader (seek+read)
└── demo_dag/               # Example DAG JSON specifications
```

## Key Design Decisions

- **Zero-copy routing**: stream data is never copied between pipeline stages; only page-chain head/tail pointers are atomically updated.
- **Lock-free allocation**: page free-list uses a sharded Treiber stack; registry uses CAS-based bucket chaining.
- **Isolated execution**: each WASM node runs in its own `wasmtime` instance with a fresh linear memory, but shares the same SHM backing file.
- **Fat-pointer returns**: WASM→host data transfer uses 64-bit fat pointers (`ptr << 32 | len`) to pass slice locations without copying.
- **Dynamic SHM growth**: the host can grow the SHM file and remap the VMA in-place via a `host_remap` import callable from guests.


  WebAsShared — WASM-Based Zero-Copy Stream Processing Engine

  Core Idea: A DAG (Directed Acyclic Graph) pipeline executor where each stage runs inside an
  isolated WebAssembly module, but all stages share the same physical memory (via mmap). This
  enables zero-copy data routing — data never gets copied between stages, only pointers are
  spliced.

  Architecture

  Host (Rust)                         Guest (WASM)
  ─────────────────────────────────   ──────────────────────────────────
  • Parses JSON DAG spec               • Each node = isolated wasmtime instance
  • Creates/manages shared memory      • Fixed address 0x80000000 → SHM file
  • Runs topological DAG execution     • Implements algorithms (word count,
  • Zero-copy routing between stages     image processing, etc.)
  • File I/O, allocation, logging      • Calls back host for SHM growth

  Shared Memory Layout (at 0x80000000 in every WASM instance)

  - Superblock — stream/IO slot heads/tails + free-list shards
  - Registry Arena — name → index mapping
  - Stream Pages — 4 KiB linked-list pages, bump-allocated + Treiber stack free-list

  Routing Operations (all zero-copy, lock-free)

  ┌───────────┬──────────────────────────────────────────────┐
  │ Operation │                 Description                  │
  ├───────────┼──────────────────────────────────────────────┤
  │ Bridge    │ 1→1 passthrough (pointer remap)              │
  ├───────────┼──────────────────────────────────────────────┤
  │ Aggregate │ N→1 merge (atomic chain splice)              │
  ├───────────┼──────────────────────────────────────────────┤
  │ Shuffle   │ N→M partitioned (round-robin, modulo, fixed) │
  ├───────────┼──────────────────────────────────────────────┤
  │ Broadcast │ 1→M fan-out                                  │
  └───────────┴──────────────────────────────────────────────┘

  Example Pipelines

  - Word Count: Load corpus → distribute lines → 10 parallel mappers → aggregate → reduce
  - Image Pipeline: Load PPMs → rotate → grayscale → equalize → blur → save

  Tech Stack

  - Rust workspace with 3 crates: host, guest, common
  - wasmtime for WASM execution
  - Python support via python.wasm + WASI (subprocess per node)
  - Lock-free allocation with sharded Treiber stacks

  The key innovation is that all WASM instances share the same physical backing file mapped at
  the same virtual address, so routing between pipeline stages is just atomic pointer
  manipulation — no data copying at all.