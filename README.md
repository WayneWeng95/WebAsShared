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

## Project Structure

```
WebAsShared/
├── node-agent                      # NodeAgent binary (entry point)
├── Executor/                       # Execution engine (host + WASM/Python guests)
│   ├── Cargo.toml                  # Workspace (host, guest, common, connect)
│   ├── host/src/                   # Orchestrator: DAG runner, WASM executor, routing, I/O, RDMA
│   ├── guest/src/                  # WASM workloads (word count, FINRA, ML training, TF-IDF, etc.)
│   ├── common/src/                 # Shared memory layout definitions (superblock, page, registry)
│   ├── connect/src/                # RDMA full-mesh: libibverbs FFI, MeshNode, atomic ops
│   ├── py_guest/python/            # Python workloads (runner.py, shm.py, workload modules)
│   └── data/                       # Test datasets (corpus, trades, MNIST, images)
├── NodeAgent/                      # Multi-machine deployment agent
│   ├── agent/src/                  # Coordinator/worker daemon, executor interface, metrics
│   ├── cluster_dags/               # ClusterDag definitions (word_count, finra, ml_training)
│   ├── agent_coordinator.toml      # Sample coordinator config
│   └── agent_worker.toml           # Sample worker config
└── DAGs/                           # All DAG JSON specifications
    ├── demo_dag/                   # Single-node demos (word count, image pipeline)
    ├── workload_dag/               # Single-node workloads (FINRA, ML training, TF-IDF)
    ├── rdma_demo_dag/              # Multi-node RDMA demo pairs (node0 + node1)
    └── rdma_workload_dag/          # Multi-node RDMA workload pairs (node0 + node1)
```

## Quick Start

### Building

```bash
# Build the Executor
cd Executor
cargo +nightly build --release
cargo +nightly build --release -p guest --target wasm32-unknown-unknown

# Build the NodeAgent
cd ../NodeAgent
cargo build --release
cp target/release/node-agent ..
```

### Running (Single-Node via NodeAgent)

All commands run from the `WebAsShared/` root directory:

```bash
cd /path/to/WebAsShared

# Rust/WASM workloads (default)
./node-agent run DAGs/workload_dag/word_count_demo.json
./node-agent run DAGs/workload_dag/finra_demo.json
./node-agent run DAGs/workload_dag/ml_training_demo.json
./node-agent run DAGs/workload_dag/tfidf_demo.json

# Same DAGs, Python execution (--python flag)
./node-agent run DAGs/workload_dag/word_count_demo.json --python
./node-agent run DAGs/workload_dag/finra_demo.json --python
./node-agent run DAGs/workload_dag/ml_training_demo.json --python
./node-agent run DAGs/workload_dag/tfidf_demo.json --python

# Python with AOT pre-compilation (skips JIT on each spawn)
./node-agent run DAGs/workload_dag/finra_demo.json --python --aot

# Image pipeline demos
./node-agent run DAGs/demo_dag/img_pipeline_demo.json
./node-agent run DAGs/demo_dag/img_pipeline_demo.json --python
```

Results are written to `/tmp/` (e.g., `/tmp/finra_result.txt`, `/tmp/ml_training_result.txt`).

### Running (Multi-Node via NodeAgent)

```bash
# On coordinator machine (node 0):
./node-agent start --config NodeAgent/agent_coordinator.toml

# On worker machine (node 1):
./node-agent start --config NodeAgent/agent_worker.toml

# Submit a distributed job (from any machine with coordinator access):
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag NodeAgent/cluster_dags/finra.json
```

## Architecture

### Workspace Members (Executor)

| Crate | Role |
|-------|------|
| `host` | Orchestrator: DAG runner, WASM executor, routing, I/O, RDMA |
| `guest` | WASM workloads (word count, image pipeline, FINRA, ML training, TF-IDF) |
| `common` | Shared memory layout definitions (superblock, page, registry) |
| `connect` | RDMA full-mesh: libibverbs FFI, `MeshNode`, atomic ops, SHM-as-MR |

### NodeAgent

The NodeAgent is a higher-level daemon that orchestrates multi-machine DAG execution:

| Module | Role |
|--------|------|
| `main.rs` | CLI: `run` (single-node), `start` (daemon), `submit` (job), `status` (query) |
| `dag_transform.rs` | Unified `Func`/`Pipeline`/`Grouping` → native node kinds per mode |
| `coordinator.rs` | Accept workers, distribute per-node DAGs, aggregate results |
| `worker.rs` | Connect to coordinator, receive jobs, launch Executor subprocess |
| `executor.rs` | Spawn/monitor `host dag` subprocesses with live or captured output |
| `cluster_dag.rs` | Split a ClusterDag into per-node DAGs with RDMA config injection |
| `metrics.rs` | CPU, RSS, SHM bump offset sampling; JSON-lines log output |
| `protocol.rs` | Length-prefixed JSON over TCP control plane |
| `config.rs` | Agent config (role, cluster IPs, paths, timeouts) |

In single-node mode (`run`), the NodeAgent spawns the Executor directly with live terminal output. In multi-node mode (`start`/`submit`), a coordinator distributes per-node DAGs to workers who each spawn their own Executor.

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
| `Bridge` / `Aggregate` / `Shuffle` / `Broadcast` | Host-side zero-copy routing |
| `RemoteSend` | RDMA-WRITE a slot to a peer node's SHM staging area |
| `RemoteRecv` | Receive a slot written by a peer via RDMA into local SHM |

### Workloads

| Workload | Description | Rust | Python |
|----------|-------------|------|--------|
| Word Count | Distribute → parallel map → aggregate → reduce | Yes | Yes |
| TF-IDF | Per-shard TF/DF → merge → IDF scoring → top-50 | Yes | Yes |
| FINRA Audit | FetchPrivate ∥ FetchPublic → 8 audit rules → merge results | Yes | Yes |
| ML Training | Partition → PCA ×2 → redistribute → train ×8 stumps → validate | Yes | Yes |
| Image Pipeline | Load PPM → rotate → grayscale → equalize → blur → export PGM | Yes | Yes |

## Multi-machine Execution (RDMA)

When the DAG JSON includes an `rdma` block, the host establishes a full RC QP mesh across all nodes and registers the SHM as an RDMA Memory Region. `RemoteSend` / `RemoteRecv` node pairs transfer slot data between machines with no TCP memcopy.

### Transfer protocol

1. Sender walks the source slot's page chain and builds an SGE list pointing directly at the page data fields.
2. A single RDMA WRITE (scatter-gather, chunked across multiple WRs if needed) DMAs the data into the peer's staging area — **no CPU copy** of the payload.
3. An RDMA Fetch-and-Add on the peer's staging ready-counter (hardware atomic) signals completion; a TCP byte is sent as a fallback.
4. Receiver spin-polls the counter (100 ms), then falls back to the TCP signal. Appends the raw bytes directly into the target slot.

### ClusterDag (NodeAgent)

For multi-node deployment, the NodeAgent introduces the **ClusterDag** format — a single JSON file that describes the entire distributed workflow. The coordinator splits it into per-node DAGs, injects RDMA config from the live cluster membership, and distributes them:

```json
{
  "shm_path_prefix": "/dev/shm/rdma_finra",
  "transfer": true,
  "node_dags": {
    "0": [ ... node 0 DAG nodes ... ],
    "1": [ ... node 1 DAG nodes ... ]
  }
}
```

## Key Design Decisions

- **Zero-copy local routing**: stream data is never copied between pipeline stages on the same machine; only page-chain head/tail pointers are atomically updated.
- **Zero-copy remote transfer**: RDMA scatter-gather DMAs page data directly from source pages into the peer's SHM staging area — the CPU writes only a 12-byte header.
- **Lock-free allocation**: page free-list uses a sharded Treiber stack; registry uses CAS-based bucket chaining.
- **Isolated WASM execution**: each WASM node runs in its own `wasmtime` instance with a fresh linear memory, but shares the same SHM backing file.
- **Persistent pipeline workers**: `StreamPipeline` and `PyPipeline` keep one worker process alive per stage across all ticks, paying the JIT/startup cost only once per run.
- **NodeAgent as entry point**: the NodeAgent binary is the single interface for both single-node and multi-node execution, spawning the Executor as a subprocess.
- **Coordinator-worker model**: static membership, TCP control plane (separate from RDMA data plane), no consensus needed for research-scale clusters (2–32 nodes).
