# WebAssembly Stream Processing Engine

A DAG-based stream processing engine that runs computational workloads inside WebAssembly guest modules, orchestrated by a Rust host process. Data flows between pipeline stages through a **shared memory (SHM) region mapped directly into the WASM address space**, enabling zero-copy routing without serialization overhead. Multi-machine execution is supported via RDMA, with slot data transferred directly between nodes' SHM regions at line rate.

## Overview

Pipelines are defined as JSON DAGs. Each node is a WASM function call, a Python function call, a host-side routing operation, an I/O step, or an RDMA transfer. The host topologically sorts the nodes and executes them in dependency order, grouping independent nodes into parallel waves.

```
Input file
    |
    v
[WasmFunc: distribute]  --> stream slots 10-19
    |
    |-- [WasmFunc: map_0]  --> slot 110
    |-- [WasmFunc: map_1]  --> slot 111
    |   ...
    +-- [WasmFunc: map_9]  --> slot 119
              |
              v
    [Aggregate: 110-119 -> 200]   (host-side, zero-copy)
              |
              v
    [WasmFunc: reduce]  --> I/O slot
              |
              v
         Output file
```

## Project Structure

```
WebAsShared/
|-- build.sh                        # Build all binaries (host, guest, node-agent)
|-- init-node.sh                    # One-shot new node setup (pull, install, build)
|-- node-agent                      # NodeAgent binary (entry point)
|-- scripts/                        # Setup and utility scripts
|   |-- start.sh                    # Rust environment setup (source this)
|   |-- install_wasmtime.sh         # Install wasmtime for Python/WASM execution
|   +-- claude-code-setup.sh        # Claude Code remote server setup
|-- Executor/                       # Execution engine (host + WASM/Python guests)
|   |-- Cargo.toml                  # Workspace (host, guest, common, connect)
|   |-- host/src/                   # Orchestrator: DAG runner, WASM executor, routing, I/O, RDMA
|   |-- guest/src/                  # WASM workloads (word count, FINRA, ML training, TF-IDF, etc.)
|   |   +-- .cargo/config.toml      # WASM build flags (--import-memory, --shared-memory)
|   |-- common/src/                 # Shared memory layout definitions (superblock, page, registry)
|   |-- connect/src/                # RDMA full-mesh: libibverbs FFI, MeshNode, atomic ops
|   |-- py_guest/python/            # Python workloads (runner.py, shm.py, workload modules)
|   +-- data/                       # Test datasets (corpus, trades, MNIST, images)
|-- NodeAgent/                      # Multi-machine deployment agent
|   |-- agent/src/                  # Coordinator/worker daemon, executor interface, metrics
|   |-- scheduler/src/              # SCX sched_ext integration (stats client, cluster view, advisor)
|   |-- agent_coordinator.toml      # Coordinator config (IPs, ports, paths, timeouts, SCX)
|   +-- agent_worker.toml           # Worker config
+-- DAGs/                           # All DAG JSON specifications
    |-- demo_dag/                   # Single-node demos (word count, image pipeline)
    |-- workload_dag/               # Single-node workloads (FINRA, ML training, TF-IDF)
    |-- cluster_dag/                # ClusterDag definitions for distributed execution
    |-- rdma_demo_dag/              # Multi-node RDMA demo pairs (node0 + node1)
    +-- rdma_workload_dag/          # Multi-node RDMA workload pairs (node0 + node1)
```

## Quick Start

### New Node Setup

Run the one-shot init script to set up a fresh node (pulls code, installs packages, builds everything):

```bash
chmod +x init-node.sh && ./init-node.sh
```

This runs: `git pull` → RDMA packages → Rust env (`scripts/start.sh`) → wasmtime (`scripts/install_wasmtime.sh`) → Claude Code (`scripts/claude-code-setup.sh`) → `build.sh`.

### Building

Build all three binaries in one step:

```bash
./build.sh
```

Or build individually:

```bash
# 1. Executor host (standard release build)
cd Executor
cargo build --release

# 2. WASM guest (nightly required for build-std; must build from guest/ directory)
cd guest
cargo +nightly build --release
cd ../..

# 3. NodeAgent (standard release build)
cd NodeAgent
cargo build --release
cp target/release/node-agent ..

# 4. Optional: AOT pre-compile python.wasm for faster Python execution
wasmtime compile /opt/myapp/python-3.12.0.wasm -o /opt/myapp/python-3.12.0.cwasm
```

### Running (Single-Node)

All commands run from the `WebAsShared/` root directory:

```bash
# Rust/WASM workloads
./node-agent run DAGs/workload_dag/word_count_demo.json
./node-agent run DAGs/workload_dag/finra_demo.json
./node-agent run DAGs/workload_dag/ml_training_demo.json
./node-agent run DAGs/workload_dag/tfidf_demo.json

# Python execution (--python flag)
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

### Running (Multi-Node with RDMA)

Start the coordinator and worker daemons, then submit jobs:

```bash
# On coordinator machine (node 0):
./node-agent start --config NodeAgent/agent_coordinator.toml

# On worker machine (node 1):
./node-agent start --config NodeAgent/agent_worker.toml

# Submit distributed jobs (from any machine with coordinator access):

# Rust/WASM
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/word_count.json
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/finra.json
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/ml_training.json
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/pipeline_routing.json

# Python
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/word_count.json --python
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/finra.json --python
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/ml_training.json --python
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/pipeline_routing.json --python

# Python with AOT
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/finra.json --python --aot
```

ClusterDag files use unified `Func` nodes that are automatically transformed to `WasmVoid` (Rust) or `PyFunc` (Python) based on the `--python` flag.

## Architecture

### Binaries

| Binary | Source | Build | Role |
|--------|--------|-------|------|
| `node-agent` | `NodeAgent/` | `cargo build --release` | CLI entry point: single-node run, coordinator/worker daemon, job submission |
| `host` | `Executor/` | `cargo build --release` | DAG executor: WASM runtime, routing, I/O, RDMA mesh |
| `guest.wasm` | `Executor/guest/` | `cargo +nightly build --release` | WASM workload modules (shared-memory import requires nightly `build-std`) |

### Executor Crates

| Crate | Role |
|-------|------|
| `host` | Orchestrator: DAG runner, WASM executor, routing, I/O, RDMA |
| `guest` | WASM workloads (word count, image pipeline, FINRA, ML training, TF-IDF) |
| `common` | Shared memory layout definitions (superblock, page, registry) |
| `connect` | RDMA full-mesh: libibverbs FFI, `MeshNode`, atomic ops, SHM-as-MR |

### NodeAgent Modules

| Module | Role |
|--------|------|
| `main.rs` | CLI: `run` (single-node), `start` (daemon), `submit` (job), `status` (query) |
| `dag_transform.rs` | Unified `Func`/`Pipeline`/`Grouping` to native node kinds per mode |
| `coordinator.rs` | Accept workers, distribute per-node DAGs, aggregate results |
| `worker.rs` | Connect to coordinator, receive jobs, launch Executor subprocess |
| `executor.rs` | Spawn/monitor `host dag` subprocesses with live or captured output |
| `cluster_dag.rs` | Split a ClusterDag into per-node DAGs with RDMA config injection |
| `metrics.rs` | CPU, RSS, SHM bump offset sampling; JSON-lines log output |
| `protocol.rs` | Length-prefixed JSON over TCP control plane |
| `config.rs` | Agent config (role, cluster IPs, paths, timeouts, SCX settings) |

### Scheduler Crate (`NodeAgent/scheduler/`)

A separate library crate that integrates Linux [sched_ext](https://lwn.net/Articles/922405/) (SCX) kernel scheduler data into the NodeAgent for scheduling-aware workload placement.

Each worker node runs an SCX scheduler (e.g. `scx_rusty`, `scx_bpfland`) that exposes real-time kernel scheduling statistics via a UNIX domain socket. The `scheduler` crate collects this data and feeds it to the coordinator for cross-node decision-making.

| Module | Role |
|--------|------|
| `scx_client.rs` | Connects to the local SCX stats UNIX socket (`/var/run/scx/root/stats`), fetches `ScxNodeSnapshot` |
| `scx_cluster.rs` | `ScxClusterView`: aggregates SCX snapshots from all workers with staleness tracking |
| `advisor.rs` | Scores nodes for workload placement (CPU busy, NUMA imbalance, migration rate) |

**Data flow:**

```
SCX Scheduler (kernel)
    │
    │ UNIX socket (JSON)
    v
Worker MetricsCollector ──► ScxNodeSnapshot
    │
    │ TCP Metrics message
    v
Coordinator ScxClusterView ──► advisor::score_nodes()
    │
    v
StatusResponse (visible via `node-agent status`)
```

**SCX stats collected per node:**

| Field | Description |
|-------|-------------|
| `cpu_busy` | Overall CPU busy percentage (100.0 = all CPUs fully busy) |
| `load` | Weighted system load (sum of weight * duty_cycle) |
| `nr_migrations` | Task migrations from load balancing |
| `slice_us` | Current scheduling time slice (microseconds) |
| `time_used` | Time spent in userspace scheduler |
| `numa_nodes` | Per-NUMA-node load and imbalance |

**Configuration** (`agent.toml`):

```toml
[scx]
enabled = true                                   # default: true
socket_path = "/var/run/scx/root/stats"          # default SCX socket
```

When SCX is not running or disabled, the system operates normally -- SCX fields are omitted from metrics and status responses.

### Shared Memory Layout

The host creates a SHM file and maps it at a fixed offset (`0x80000000`) in each WASM guest's 4 GB address space. Guests access it via ordinary pointer arithmetic -- no syscalls needed for data reads/writes. When RDMA is enabled, the same SHM is registered as an RDMA Memory Region so peer nodes can write directly into it.

```
0x80000000  +--------------------------------+
            | Superblock (24 KiB)            |
            |  atomic counters, bump ptr     |
            |  stream heads/tails [2048]     |
            |  I/O heads/tails   [512]       |
            |  free-list shards  [16]        |
            +--------------------------------+
            | Registry Arena (1 MiB)         |
            |  name -> atomic index mapping  |
            +--------------------------------+
            | Atomic Arena (1 MiB)           |
            |  CAS values for named vars     |
            +--------------------------------+
            | Log Arena (16 MiB)             |
            |  guest diagnostic output       |
            +--------------------------------+
            | RDMA Staging Area              |
            |  N x 1 MiB per peer direction  |
            |  (pre-allocated when RDMA on)  |
            +--------------------------------+
            | Stream Pages (bump-allocated)  |
            |  4 KiB pages, linked lists     |
            |  freed via Treiber-stack       |
            +--------------------------------+
```

### Routing Primitives

| Primitive | Description |
|-----------|-------------|
| `Bridge` | 1-to-1 zero-copy wire between two stream slots |
| `Aggregate` | N-to-1 merge; each upstream chain is spliced onto the downstream |
| `Shuffle` | N-to-M partitioned routing (Modulo, RoundRobin, or FixedMap policy) |
| `Broadcast` | N-to-M fanout; each upstream is linked to every downstream |

### Node Types (DAG JSON)

| Kind | Description |
|------|-------------|
| `Func` | Unified node: transforms to `WasmVoid` (Rust) or `PyFunc` (Python) at submit time |
| `WasmVoid` / `WasmU32` / `WasmFatPtr` | Call an exported function in the Rust WASM guest |
| `PyFunc` | Call a Python workload function (one subprocess per invocation) |
| `StreamPipeline` / `PyPipeline` | Multi-stage pipeline with persistent worker reuse per stage |
| `Input` | Load a file into an I/O slot (optional background prefetch) |
| `Output` | Write an I/O slot to a file |
| `Bridge` / `Aggregate` / `Shuffle` / `Broadcast` | Host-side zero-copy routing |
| `RemoteSend` | RDMA-WRITE a slot to a peer node's SHM staging area |
| `RemoteRecv` | Receive a slot written by a peer via RDMA into local SHM |

### Intra-Wave Barrier Synchronization

Nodes in the same execution wave normally run in isolation.  When a workload requires peer-to-peer communication within a wave (e.g. produce partial results → exchange → reduce), the **barrier** mechanism enables this without breaking the wave model.

Add `"barrier_group"` to nodes that need to synchronize:

```json
{ "id": "w0", "deps": [], "barrier_group": "sync1",
  "kind": { "WasmVoid": { "func": "collaborative_worker", "arg": 0 } } },
{ "id": "w1", "deps": [], "barrier_group": "sync1",
  "kind": { "WasmVoid": { "func": "collaborative_worker", "arg": 1 } } }
```

The host validates that all nodes in a `barrier_group` land in the same wave and assigns a barrier slot ID (printed at startup).  Guest code calls `ShmApi::barrier_wait(barrier_id, party_count)` to synchronize.  The implementation uses Linux `futex` — waiters sleep with zero CPU cost until the last party arrives.

Up to 64 concurrent barrier slots are available.  Multiple barriers can be used within a single workload function for multi-phase synchronization.

Run the barrier test:

```bash
./node-agent run DAGs/demo_dag/barrier_test.json
```

See [barrier.md](barrier.md) for full documentation, guest API examples, and constraints.

#### Cross-Node Barrier Limitations (RDMA)

The barrier is a **single-node primitive**.  It relies on Linux `futex` over a shared `mmap`'d SHM file, which has no cross-machine equivalent.  Additionally, the RDMA mesh (`MeshNode`) is owned by the main DAG runner process, but `host_barrier_wait` executes inside subprocess WASM workers spawned via `wasm-call` — these subprocesses have no access to the RDMA connections.

For cross-node data exchange, use the existing `RemoteSend`/`RemoteRecv` between waves.  If a future workload requires mid-function synchronization across machines, the path forward is a **two-tier barrier**:

```
Subprocess (node A)           DAG runner (node A)           DAG runner (node B)
       │                              │                             │
 barrier_wait_remote()                │                             │
       │                              │                             │
       ├─futex-wake─► local arrival   │                             │
       │              tracker         │                             │
       │                 │            │                             │
       │          all local workers   │                             │
       │          arrived?            │                             │
       │                 ├──TCP──────►│◄──TCP──── local workers     │
       │                              │           arrived           │
       │                 ◄──TCP───────┤──TCP────►                   │
       │                "proceed"     │         "proceed"           │
       ├─futex-wait ◄── futex-wake    │                             │
       │                              │                             │
     continues                        │                             │
```

1. Subprocesses use the existing local futex to sync with a **host-side barrier watcher thread** on the same node.
2. When all local workers on a node arrive, the watcher thread sends a TCP message to the coordinator via the mesh control channel.
3. The coordinator collects arrivals from all nodes and broadcasts "proceed".
4. Each watcher thread futex-wakes its local subprocesses.

This keeps the subprocess API unchanged (`barrier_wait`) and pushes cross-node logic into the DAG runner which already owns the mesh.  This is not yet implemented — the single-node barrier covers the current use cases, and cross-node synchronization between waves is handled by `RemoteSend`/`RemoteRecv`.

### Workloads

| Workload | Description | Rust | Python |
|----------|-------------|------|--------|
| Word Count | Distribute -> parallel map -> aggregate -> reduce | Yes | Yes |
| TF-IDF | Per-shard TF/DF -> merge -> IDF scoring -> top-50 | Yes | Yes |
| FINRA Audit | FetchPrivate || FetchPublic -> 8 audit rules -> merge results | Yes | Yes |
| ML Training | Partition -> PCA x2 -> redistribute -> train x8 stumps -> validate | Yes | Yes |
| Image Pipeline | Load PPM -> rotate -> grayscale -> equalize -> blur -> export PGM | Yes | Yes |
| Pipeline + Routing | Distributed: StreamPipeline (image) + Shuffle + Aggregate + Bridge across 2 nodes | Yes | Yes |

## Multi-machine Execution (RDMA)

When the DAG JSON includes an `rdma` block, the host establishes a full RC QP mesh across all nodes and registers the SHM as an RDMA Memory Region. `RemoteSend` / `RemoteRecv` node pairs transfer slot data between machines with no TCP data copy.

### Transfer Protocol

1. Sender walks the source slot's page chain and builds an SGE list pointing directly at the page data fields.
2. A single RDMA WRITE (scatter-gather, chunked across multiple WRs if needed) DMAs the data into the peer's staging area -- **no CPU copy** of the payload.
3. An RDMA Fetch-and-Add on the peer's staging ready-counter (hardware atomic) signals completion; a TCP byte is sent as a fallback.
4. Receiver spin-polls the counter (100 ms), then falls back to the TCP signal. Appends the raw bytes directly into the target slot.

### ClusterDag Format

For multi-node deployment, the NodeAgent introduces the **ClusterDag** format -- a single JSON file that describes the entire distributed workflow using unified `Func` nodes. The `submit` command transforms these to native node kinds (`WasmVoid` or `PyFunc`) based on the `--python` flag, then the coordinator splits it into per-node DAGs, injects RDMA config from the live cluster membership, and distributes them:

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

## RDMA Performance Testing

Use the `perftest` suite to verify RDMA connectivity and measure raw link performance between nodes.

### Prerequisites

```bash
sudo apt-get install -y libibverbs-dev pkg-config librdmacm-dev ibverbs-utils perftest
```

### Check RDMA Devices

```bash
ibv_devices        # List available RDMA devices
ibv_devinfo        # Show device details (ports, MTU, link type, state)
```

### Single-Node Loopback Test

Run both server and client on the same machine to verify the RDMA stack:

```bash
# Write latency (loopback)
ib_write_lat -d mlx4_0 -i 1 -x 0 --report_gbits &
sleep 2 && ib_write_lat -d mlx4_0 -i 1 -x 0 --report_gbits localhost

# Write bandwidth (loopback)
ib_write_bw -d mlx4_0 -i 1 -x 0 --report_gbits &
sleep 2 && ib_write_bw -d mlx4_0 -i 1 -x 0 --report_gbits localhost
```

### Two-Node Test

Start the server on one node, then the client on the other. Use `-i 2` for port 2 if that is the port on the RDMA network (e.g., `10.10.1.x`).

```bash
# On the server node (e.g., 10.10.1.1):
ib_write_lat -d mlx4_0 -i 2 -x 0 --report_gbits
ib_write_bw  -d mlx4_0 -i 2 -x 0 --report_gbits

# On the client node (e.g., 10.10.1.2):
ib_write_lat -d mlx4_0 -i 2 -x 0 --report_gbits 10.10.1.1
ib_write_bw  -d mlx4_0 -i 2 -x 0 --report_gbits 10.10.1.1
```

### Benchmark Results

**Hardware**: Mellanox ConnectX-3 (`mlx4_0`), dual-port, RoCE (Ethernet link layer), MTU 1024

#### Loopback (single-node, PCIe/internal)

| Test | Result |
|------|--------|
| Write Latency | 0.75 usec typical (0.74 min, 0.78 p99) |
| Write Bandwidth | 35.97 Gb/s |
| Read Bandwidth | 35.64 Gb/s |

#### Two-Node (10.10.1.1 <-> 10.10.1.2, 10GbE)

| Test | Result |
|------|--------|
| Write Latency | 1.67 usec typical (1.63 min, 1.74 p99) |
| Write Bandwidth | 9.14 Gb/s (~line rate for 10GbE) |

### Tuning Notes

- **MTU**: Currently 1024. Setting jumbo frames (4096 or 9000) on NICs and switch can reduce per-packet overhead.
- **CPU governor**: Set to `performance` (`sudo cpupower frequency-set -g performance`) for stable latency measurements.
- **Multiple QPs**: Use `-q 4` with `ib_write_bw` to test with multiple queue pairs.

## Key Design Decisions

- **Zero-copy local routing**: stream data is never copied between pipeline stages on the same machine; only page-chain head/tail pointers are atomically updated.
- **Zero-copy remote transfer**: RDMA scatter-gather DMAs page data directly from source pages into the peer's SHM staging area -- the CPU writes only a 12-byte header.
- **Lock-free allocation**: page free-list uses a sharded Treiber stack; registry uses CAS-based bucket chaining.
- **Isolated WASM execution**: each WASM node runs in its own wasmtime instance with a fresh linear memory, but shares the same SHM backing file.
- **Persistent pipeline workers**: `StreamPipeline` and `PyPipeline` keep one worker process alive per stage across all ticks, paying the JIT/startup cost only once per run.
- **Unified DAG format**: ClusterDag and single-node DAGs use `Func` nodes that are transformed to `WasmVoid` or `PyFunc` at submission time, enabling the same DAG to run in both Rust and Python modes.
- **NodeAgent as entry point**: the NodeAgent binary is the single interface for both single-node and multi-node execution, spawning the Executor as a subprocess.
- **Coordinator-worker model**: static membership, TCP control plane (separate from RDMA data plane), RDMA mesh connects with 30s retry window for robustness. No consensus needed for research-scale clusters (2-32 nodes).
- **Kernel-aware scheduling**: workers collect real-time SCX sched_ext stats (CPU load, NUMA imbalance, migration pressure) and report them to the coordinator, enabling scheduling decisions informed by actual kernel scheduling state rather than coarse `/proc/stat` samples alone.
