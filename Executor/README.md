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
