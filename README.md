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

## Recent Improvements (2026-06)

Distributed correctness, capacity-aware fan-out, metrics, and streaming fixes:

- **Cross-node aggregation correctness.** A multi-node `Aggregate` no longer
  drops a remote node's contribution. The receiver was freeing a stream
  `RemoteRecv` slot one wave before the consuming `Aggregate` read it (the
  "no-consumers" reclaim path only tracked I/O recvs); it now keeps any recv
  slot alive until its real consumer runs. Verified on a 2-node cluster
  (word-count is now exact).
- **Data-parallel input sharding.** `placement:"all"` inputs are no longer
  replicated-and-recounted on every node (which gave an N× result). Each node
  loads only a **line-aligned fractional slice** of the file
  (`SlotLoader::load_slice`, MapReduce split rule — every line read by exactly
  one node), so N nodes produce the correct **1×** result in parallel with
  per-node SHM ≈ corpus/N. Verified exact vs the single-node baseline.
- **Capacity-weighted fan-out + core-budget cap.** `fanout: N` on a
  `placement:"all"` node now means **N workers across the whole cluster**,
  apportioned by per-host capacity (a host with 80% capacity gets 80% of the
  workers *and* 80% of the input slice). A requested fan-out is also clamped to
  the cluster's usable core budget (`Σ max(1, cores−reserve)`), so e.g.
  `fanout:50` on a 2×16-core cluster is capped to 28. See
  [`Tests/Fan_out_remote/`](Tests/Fan_out_remote/) for the fan-out sweep.
- **Per-wave timing metrics.** The executor prints a per-run, per-wave compute
  breakdown (`[DAG][timing]`), with cross-node file staging timed separately in
  the node-agent worker so it's excluded from compute.
- **Memory metric fix.** `rss_bytes` was measuring the node-agent daemon
  (~4 MiB, flat). It now sums the **private** RSS of the whole executor process
  tree (host + every fanned-out `wasm-call` worker — each with its own wasmtime
  runtime, JIT'd guest, and guest heap), with shared SHM subtracted per process
  and reported once via `shm_bump_offset` (so `total = rss_bytes +
  shm_bump_offset`, no double-counting).
- **Streaming output / demo fixes.** New `Output` `split_records` mode writes
  record *i* → `paths[i]` (one file per record). `dag_demo`'s `FileDispatch`
  wasm path was corrected, and `img_pipeline_demo.json` was un-broken (it used a
  removed `Pipeline` node kind → now a `StreamPipeline`, output verified
  byte-identical to the non-pipelined baseline).

- **StreamPipeline / PyPipeline per-round race fixed + tested.** The
  software-pipelined schedule runs stage *S* (producing round *R*) concurrently
  with stage *S+1* (consuming round *R-1*) on a shared stream slot; consumers
  that read "all records since their cursor" raced into the next round's
  concurrently-appended records, so per-round batching was non-deterministic
  (only the cumulative total survived). The host now publishes a per-slot read
  watermark (`stream_hi_{slot}` = the pre-tick committed count) before each
  tick's scatter, and consumers bound their read to it (`pipe_read_window`);
  cursors are keyed by input slot so they reset between runs. Same fix on the
  Python path. New [`Tests/Streaming/`](Tests/Streaming/) asserts per-round
  correctness against analytic **and** non-pipelined baselines, determinism,
  slot lifecycle, reset/multi-run, `split_records`, and Rust/Python parity.

- **Per-round RDMA output return (streaming, Mode 2).** A worker's processed
  output is now returned to the coordinator (node 0) and written **one file per
  round, as each round arrives** — the streaming analogue of the batch
  `Output`+`split_records` flush. New **`StreamOutput`** node kind: each round it
  optionally `rdma_recv`s the worker's result over the dedicated streaming lane
  (conn-4) and binary-writes it to `paths[round]`. It runs on its own thread, so
  node 0 can stream input **out** (conn-3, source `StreamPipeline` on the main
  thread) while the sink receives output **back** (conn-4) concurrently — safe
  because the streaming control lane is split by direction, so the two never share
  a TCP stream. The worker is a single `StreamPipeline` doing both per-round
  `rdma_recv` (input) and embedded `rdma_send` (output). Cross-node verified: 3/3
  images byte-identical to the single-node baseline, deterministic
  ([`Tests/Streaming_CrossNode/node{0,1}_mode2.json`](Tests/Streaming_CrossNode/),
  `verify_mode2.py`).

See [`problems.md`](problems.md) for remaining open items and
[`docs/updates.md`](docs/updates.md) for the full change log.

## Documentation

Subsystem design notes live in [`docs/`](docs/):

| Doc | Contents |
|---|---|
| [docs/extended_pool.md](docs/extended_pool.md) | Host-side memory extension beyond the 2 GiB wasm32 direct window. Covers the `PageId` type widening (Phase 1), the `GlobalPool` + `ResolutionBuffer` extended-pool mechanism (Phase 2), and the RDMA overflow integration (Phase 3, live — dynamic receiver-triggered MR2 + sender-side extension MRs). Includes the feature-flag system, concurrency invariants, and the barrier-compatibility discussion. |
| [docs/barrier.md](docs/barrier.md) | Intra-wave futex barrier: the `ShmApi::barrier_wait` guest API, DAG `barrier_group` JSON syntax, usage examples, and the single-node constraint. |
| [docs/slots.md](docs/slots.md) | Stream slots and I/O slots — what they are, when to use which, slot lifecycle across DAG waves. |
| [docs/WASM64.md](docs/WASM64.md) | Investigation into wasm64 (memory64) as an alternative path to larger linear memory, why it compiles but doesn't run practically, and the infrastructure left in place in case wasmtime's JIT performance improves. |
| [Partitioner/README.md](Partitioner/README.md) | Auto-partitioner: takes a `SymbolicDag` (slot-free, may omit `node_id`) and produces a `ClusterDag` with placement controlled by `placement_policy` (balanced / pack / spread / weighted) or live sched_ext load, all slot numbers derived automatically with per-machine overlap detection, `RemoteSend`/`RemoteRecv` pairs injected for cross-node edges, and wave-0 deadlock prevention applied automatically. `node-agent submit` ships the DAG as-is to the coordinator, which runs the partitioner server-side (where it can scale to the live node count and use live sched_ext load) whenever the submitted JSON is a `SymbolicDag` (has `nodes`) rather than a pre-partitioned `ClusterDag` (has `node_dags`). |
| [docs/updates.md](docs/updates.md) | Historical change log. |

## Project Structure

```
WebAsShared/
|-- README.md                       # This file
|-- build.sh                        # Build all binaries (host, guest, node-agent)
|-- init-node.sh                    # One-shot new node setup (pull, install, build)
|-- node-agent                      # NodeAgent binary (entry point)
|-- docs/                           # Design notes and subsystem documentation
|   |-- extended_pool.md            # Host-side memory extension past the 2 GiB wasm32 cap
|   |-- barrier.md                  # Intra-wave futex barrier API, examples, constraints
|   |-- slots.md                    # Stream / IO slot design and semantics
|   |-- WASM64.md                   # WASM64 (memory64) investigation and why it was deferred
|   +-- updates.md                  # Historical change log
|-- scripts/                        # Setup and utility scripts
|   |-- start.sh                    # Rust environment setup (source this)
|   |-- install_wasmtime.sh         # Install wasmtime for Python/WASM execution
|   +-- claude-code-setup.sh        # Claude Code remote server setup
|-- Executor/                       # Execution engine (host + WASM/Python guests)
|   |-- Cargo.toml                  # Workspace (host, guest, common, connect)
|   |-- host/src/                   # Orchestrator: DAG runner, WASM executor, routing, I/O, RDMA
|   |   +-- runtime/extended_pool/  # Host-side memory extension (see docs/extended_pool.md)
|   |-- guest/src/                  # WASM workloads (word count, FINRA, ML training, TF-IDF, etc.)
|   |   +-- .cargo/config.toml      # WASM build flags (--import-memory, --shared-memory)
|   |-- common/src/                 # Shared memory layout definitions (superblock, page, registry)
|   |-- connect/src/                # RDMA full-mesh: libibverbs FFI, MeshNode, atomic ops
|   |-- py_guest/python/            # Python workloads (runner.py, shm.py, workload modules)
|   +-- (data/ moved to TestData/)
|-- NodeAgent/                      # Multi-machine deployment agent
|   |-- common/src/lib.rs           # All tunable constants (network, timeouts, metrics, SCX, advisor)
|   |-- agent/src/                  # Coordinator/worker daemon, executor interface, metrics
|   |-- scheduler/src/              # SCX sched_ext integration (stats client, cluster view, advisor)
|   |-- agent_coordinator.toml      # Coordinator config (IPs, ports, paths, timeouts, SCX)
|   +-- agent_worker.toml           # Worker config
|-- Partitioner/                    # Symbolic-DAG auto-partitioner
|   |-- partitioner/src/            # Crate: partition(), SymbolicDag, slot helpers
|   |   |-- placer.rs               # assign_nodes(): capacity-aware host assignment from sched_ext hints
|   |   |-- policies.rs             # PlacementPolicy enum: balanced / pack / spread / weighted; resolve_policy()
|   |   |-- slot_assigner.rs        # assign_slots(): derive all slot numbers; auto-adjust per-machine fan-out overlaps
|   |   |-- splitter.rs             # partition(): edge split, RemoteSend/RemoteRecv injection, wave-0 deadlock fix
|   |   +-- slot.rs                 # output_slot(), collect_slots() helpers
|   +-- README.md                   # Partitioner design, symbolic DAG format, placement policy reference
+-- DAGs/                           # All DAG JSON specifications
    |-- demo_dag/                   # Single-node demos (word count, image pipeline)
    |-- workload_dag/               # Single-node workloads (FINRA, ML training, TF-IDF)
    |-- cluster_dag/                # ClusterDag definitions for distributed execution (hand-authored)
    |-- symbolic_dag/               # SymbolicDag definitions (auto-partitioned by Partitioner)
    |-- rdma_demo_dag/              # Multi-node RDMA demo pairs (node0 + node1)
    +-- rdma_workload_dag/          # Multi-node RDMA workload pairs (node0 + node1)
+-- TestData/                       # Test datasets (corpus, trades, MNIST, images)
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

# On worker machine
./node-agent start --config NodeAgent/agent.toml

# Submit distributed jobs (from any machine with coordinator access):

# SymbolicDag (auto-partitioned) — no manual RemoteSend/RemoteRecv needed
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/symbolic_dag/word_count.json
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/symbolic_dag/word_count.json --python

# Capacity-aware auto-placement — aggregation nodes placed by live sched_ext load
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/symbolic_dag/word_count_auto.json

# ClusterDag (hand-authored) — explicit per-node split with RemoteSend/RemoteRecv
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/word_count.json
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/finra.json
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/ml_training.json
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/pipeline_routing.json

# Python mode
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/word_count.json --python
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/finra.json --python
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/cluster_dag/finra.json --python --aot

# auto-placement

./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/symbolic_dag/word_count_auto_placement.json
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/symbolic_dag/word_count_auto_placement.json --aot
```

**SymbolicDag** files are detected automatically (by the presence of `"total_nodes"`): the Partitioner runs placement, assigns all slot numbers, injects `RemoteSend`/`RemoteRecv` pairs, and stages any `shared_inputs` files to workers before execution starts. To preview the generated ClusterDag JSON without running a job:

```bash
./Partitioner/target/release/partition DAGs/symbolic_dag/word_count.json

# With explicit placement hints (skewed: host 0 idle, host 1 saturated)
./Partitioner/target/release/partition DAGs/symbolic_dag/word_count_auto.json \
  --hints '{"capacity":{"0":0.85,"1":0.15},"host_limit":{"0":12,"1":1}}'
```

ClusterDag files use unified `Func` nodes that are automatically transformed to `WasmVoid` (Rust) or `PyFunc` (Python) based on the `--python` flag.

#### Slot-Free SymbolicDag Format

Nodes in a SymbolicDag need not specify any slot numbers. The `slot_assigner` derives every slot from the DAG topology plus two optional per-function-type declarations:

| Func field | Meaning |
|---|---|
| `"out_base": N` | Fan-out: outputs to slots `[N, N+1, …, N+consumers-1]`; injects `arg = effective_base` (possibly auto-bumped to avoid per-machine slot overlap). |
| `"output_offset": K` | Output = upstream fan-out's effective base + K; inherits input slot from upstream. |
| neither | Terminal: `arg` = dep's output slot, writes to `OUTPUT_IO_SLOT (1)`. |

`Aggregate {}` derives its `upstream_nodes` from `deps` and receives a freshly allocated `downstream` slot. `Input {}` defaults to `slot: 0`. `Output {}` reads the dep's output slot (rewritten to the RemoteRecv slot when the dep is cross-node).

Example — a complete word count pipeline with no slot numbers:

```json
{ "id": "distribute_0", "node_id": 0, "deps": ["load_0"],
  "kind": { "Func": { "func": "wc_distribute", "out_base": 10 } } }

{ "id": "map_0_n0", "node_id": 0, "deps": ["distribute_0"],
  "kind": { "Func": { "func": "wc_map", "output_offset": 100 } } }

{ "id": "aggregate", "deps": ["map_0_n0", "map_1_n0", ...],
  "kind": { "Aggregate": {} } }

{ "id": "reduce", "deps": ["aggregate_global"],
  "kind": { "Func": { "func": "wc_reduce" } } }

{ "id": "save", "deps": ["reduce"],
  "kind": { "Output": { "path": "result.txt" } } }
```

#### Capacity-Aware Auto-Placement

Nodes with `node_id` omitted are auto-assigned by the `placer`. Placement is controlled by the `placement_policy` field in the symbolic DAG JSON (introduced to replace the legacy `hints` object):

```json
"placement_policy": "pack"       // colocate on fewest hosts (default)
"placement_policy": "balanced"   // spread proportionally across all hosts
"placement_policy": "spread"     // at most one auto-node per host
"placement_policy": { "type": "weighted", "weights": [0.8, 0.2] }
"placement_policy": { "type": "balanced", "per_host_limit": 4 }
```

Priority order for hints: **coordinator live sched_ext stats** > **`placement_policy`** > **`hints` (legacy)** > **default (`pack`)**.

When live sched_ext stats are available the coordinator passes `PlacementHints { capacity, host_limit }` computed from `ScxClusterView`, overriding any policy embedded in the file. The placer then:

- **Single-host packing**: if all auto nodes fit within the most-capable host's limit, pack them there to minimise cross-node edges.
- **Proportional spread**: otherwise, allocate proportionally to `capacity` weights using largest-remainder rounding, then assign greedily in topological order with dep-affinity tie-breaking.
- **Fallback**: when no hints are available and no `placement_policy` is set, defaults to `pack`.

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
| `config.rs` | Agent config parsed from `agent.toml` (references `common` crate for defaults) |

### Scheduler Crate (`NodeAgent/scheduler/`)

A separate library crate that integrates Linux [sched_ext](https://lwn.net/Articles/922405/) (SCX) kernel scheduler data into the NodeAgent for scheduling-aware workload placement.

Each worker node runs an SCX scheduler (e.g. `scx_rusty`, `scx_bpfland`) that exposes real-time kernel scheduling statistics via a UNIX domain socket. The `scheduler` crate collects this data and feeds it to the coordinator for cross-node decision-making.

| Module | Role |
|--------|------|
| `scx_client.rs` | Connects to the local SCX stats UNIX socket (`/var/run/scx/root/stats`), fetches `ScxNodeSnapshot` |
| `scx_cluster.rs` | `ScxClusterView`: aggregates per-node status (SCX stats, memory, job state) with staleness tracking |
| `advisor.rs` | Scores nodes for workload placement (CPU busy, memory, NUMA imbalance, migrations, job queue) |

**Data flow:**

```
SCX Scheduler (kernel)
    │
    │ UNIX socket (JSON)
    v
Worker MetricsCollector ──► ScxNodeSnapshot + rss_bytes + job state
    │
    │ TCP Metrics message
    v
Coordinator ScxClusterView (NodeStatus per node) ──► advisor::score_nodes()
    │
    v
StatusResponse (visible via `node-agent status`)
```

**Data collected per node (`NodeStatus`):**

| Field | Source | Description |
|-------|--------|-------------|
| `cpu_busy` | SCX | Overall CPU busy percentage (100.0 = all CPUs fully busy) |
| `load` | SCX | Weighted system load (sum of weight * duty_cycle) |
| `nr_migrations` | SCX | Task migrations from load balancing |
| `slice_us` | SCX | Current scheduling time slice (microseconds) |
| `time_used` | SCX | Time spent in userspace scheduler |
| `numa_nodes` | SCX | Per-NUMA-node load and imbalance |
| `rss_bytes` | `/proc` | Resident memory usage of the agent process |
| `executor_running` | NodeAgent | Whether a job executor is currently active |
| `current_job_id` | NodeAgent | ID of the job currently running (if any) |

**Advisor scoring weights:**

| Factor | Weight | Signal |
|--------|--------|--------|
| CPU busy | 0.30 | Kernel-level CPU utilization from SCX |
| Job running | 0.25 | Penalizes nodes already executing a job |
| Memory | 0.20 | RSS memory pressure relative to cluster |
| NUMA imbalance | 0.15 | Scheduling imbalance across NUMA nodes |
| Migrations | 0.10 | High migration rate indicates thrashing |

When SCX is unavailable, the advisor still scores on memory and job state (SCX factors default to 0).

**Configuration** (`agent.toml`):

```toml
[scx]
enabled = true                                   # default: true
socket_path = "/var/run/scx/root/stats"          # default SCX socket

[metrics]
interval_ms = 2000                               # metrics sampling interval
status_print_interval_s = 5                      # console status printout interval
log_path = "/tmp/node_agent_metrics.jsonl"       # metrics log file

[timeouts]
job_timeout_s = 300                              # max job execution time
health_check_s = 5                               # idle worker health check interval
```

All defaults are defined in `common/src/lib.rs` (shared by both `agent` and `scheduler` crates), following the same pattern as `Executor/common/src/lib.rs`:

| Constant | Default | Description |
|----------|---------|-------------|
| `DEFAULT_AGENT_PORT` | 9500 | TCP control plane port |
| `DEFAULT_METRICS_INTERVAL_MS` | 2000 | Metrics sampling interval (ms) |
| `DEFAULT_STATUS_PRINT_INTERVAL_S` | 5 | Console status printout interval (s) |
| `DEFAULT_JOB_TIMEOUT_S` | 300 | Max job execution time (s) |
| `DEFAULT_HEALTH_CHECK_S` | 5 | Worker health check interval (s) |
| `DEFAULT_SCX_ENABLED` | true | SCX stats collection on/off |
| `DEFAULT_SCX_SOCKET` | `/var/run/scx/root/stats` | SCX stats UNIX socket path |
| `MAX_MSG_SIZE` | 64 MiB | Max control plane message size |
| `WORKER_CONNECT_RETRIES` | 10 | Worker connection retry count |
| `POLL_SLEEP_MS` | 200 | Main loop sleep granularity (ms) |
| `SCX_CONNECT_TIMEOUT_MS` | 500 | SCX socket connect timeout (ms) |
| `SCX_READ_TIMEOUT_MS` | 1000 | SCX socket read timeout (ms) |
| `ADVISOR_W_CPU_BUSY` | 0.30 | Advisor weight: CPU utilization |
| `ADVISOR_W_MEMORY` | 0.20 | Advisor weight: memory pressure |
| `ADVISOR_W_NUMA_IMBAL` | 0.15 | Advisor weight: NUMA imbalance |
| `ADVISOR_W_MIGRATIONS` | 0.10 | Advisor weight: migration rate |
| `ADVISOR_W_JOB_RUNNING` | 0.25 | Advisor weight: job-running penalty |

**Live status output:**

During execution, all modes print periodic node status to the console (default every 5s, adjustable via `--status-interval <secs>` for single-node or `status_print_interval_s` in `agent.toml` for multi-node):

```
# Single-node (run mode):
[status] node=0, cpu=34.2%, rss=128 MiB, shm_bump=4096, job=local_12345, elapsed=3.2s

# Worker:
[worker 1] cpu=45.1%, rss=256 MiB, job=job_170000, elapsed=5.0s, scx(cpu_busy=42.3%, load=180.5, migrations=95)

# Coordinator (cluster overview):
[coordinator] ── Cluster Status ──
  node 1: job=job_170000, rss=256 MiB, cpu_busy=42.3%, load=180.5, migrations=95
  node 2: job=idle, rss=64 MiB, cpu_busy=8.1%, load=25.0, migrations=12
  placement order: [2, 1] (best first)
[coordinator] ─────────────────────
```

When SCX is not running or disabled, the system operates normally -- SCX fields are omitted from metrics and status responses.

**Testing:**

The scheduler crate includes unit tests and integration tests with a mock SCX UNIX socket server:

```bash
# Run all scheduler tests (11 unit + 3 integration)
cargo test -p scheduler -- --nocapture
```

| Test | Type | What it verifies |
|------|------|-----------------|
| `test_parse_scx_stats_full` | Unit | JSON parsing of full SCX stats with NUMA nodes |
| `test_parse_scx_stats_empty` | Unit | Graceful handling of empty/missing fields |
| `test_cluster_view_update` | Unit | Cluster view insert and node count tracking |
| `test_staleness` | Unit | Stale data detection by timestamp |
| `test_busy_count` | Unit | Counting nodes with active executors |
| `test_score_nodes_prefers_less_loaded` | Unit | Advisor ranks lighter node first (all factors) |
| `test_job_running_penalty` | Unit | Busy node penalized vs idle node |
| `test_memory_pressure` | Unit | High-memory node ranks lower |
| `test_no_scx_still_scores` | Unit | Memory + job scoring works without SCX |
| `test_best_nodes` | Unit | Top-N selection from scored nodes |
| `test_empty_view` | Unit | Advisor handles empty cluster view |
| `test_client_fetches_from_mock_socket` | Integration | Spins up mock SCX UNIX socket, verifies client fetches and parses all fields |
| `test_client_returns_none_when_no_server` | Integration | Graceful `None` when SCX socket is unavailable |
| `test_full_pipeline_mock_to_advisor` | Integration | End-to-end: mock socket -> client -> cluster view -> advisor scoring and node ranking |

### Shared Memory Layout

The host creates a SHM file and maps it at a fixed offset (`0x80000000`) in each WASM guest's 4 GB address space. Guests access it via ordinary pointer arithmetic -- no syscalls needed for data reads/writes. When RDMA is enabled, the same SHM is registered as an RDMA Memory Region (MR1) so peer nodes can write directly into it.

```
0x80000000  +--------------------------------+
            | Superblock                     |
            |  atomic counters, bump ptr     |
            |  stream heads/tails [2048]     |
            |  I/O heads/tails   [512]       |
            |  free-list shards  [16]        |
            +--------------------------------+
            | Registry Arena (1 MiB)         |
            |  name -> atomic index mapping  |
            +--------------------------------+
            | RDMA Scratch (8 KiB)           |
            |  one u64 per (node, peer) pair |
            |  — results of one-sided atomic |
            |    FAA / CAS land here         |
            +--------------------------------+
            | Atomic Arena (1 MiB)           |
            |  CAS values for named vars     |
            +--------------------------------+
            | Log Arena (16 MiB)             |
            |  guest diagnostic output       |
            +--------------------------------+
            | Stream Pages (bump-allocated)  |
            |  4 KiB pages, linked lists     |
            |  freed via Treiber-stack       |
            +--------------------------------+
```

Starting SHM size is `INITIAL_SHM_SIZE = 64 MiB` (`common/src/lib.rs`), which grows geometrically on demand via `shm::try_grow_shm` up to `CAPACITY_HARD_LIMIT = 2 GiB`. The NIC registers MR1 over the initial window; MR-src-ext covers pages past the original MR1 boundary as SHM grows. The bump region grows on demand via `host_remap` (local) or via `MeshNode::ensure_shm_capacity` (host-side, used by the MR2 memcpy-back described below). No per-peer RDMA staging region is pre-reserved — receives allocate from the common page pool on demand.

### Host-side overflow MRs (Path C)

When an incoming RDMA transfer wouldn't fit in MR1's remaining bump budget, the receiver routes it through a separately-registered host-side buffer (**MR2**), then CPU-copies the bytes into a fresh page chain so the target slot is readable via the normal page-chain API. Three overflow MRs are lazily created and torn down via idle-timeout (default 5s):

- **MR2** (receiver) — host-only file at `/dev/shm/webs-rdma-mr2-<pid>`, target of oversized `RemoteSend` writes. Grown monotonically; peers learn the fresh `(addr, rkey)` on the next SI `DestReply::UseMr2` handshake.
- **MR-src-ext** (sender) — covers SHM addresses past MR1 when the local bump has grown beyond 64 MiB. Zero-copy: the SGE just uses a different lkey.
- **MR-src-stage** (sender) — host-only file for memcpying paged-mode source pages (pages that live in the extended pool's `GlobalPool` outside SHM) before referencing them in an RDMA SGE.

See [`docs/extended_pool.md`](docs/extended_pool.md) §"Phase 3 — RDMA integration" for the full design.

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
| `Output` | Write an I/O slot to a file (batch; `split_records` → one file per record) |
| `StreamOutput` | Per-round streaming sink: write one file per round (`paths[round]`); with `rdma_recv` it receives the worker's round result over the streaming lane first (coordinator-side output return) |
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

See [docs/barrier.md](docs/barrier.md) for full documentation, guest API examples, and constraints.

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

When the DAG JSON includes an `rdma` block, the host establishes a full RC QP mesh across all nodes and registers the SHM (first 64 MiB) as the primary RDMA Memory Region (**MR1**). `RemoteSend` / `RemoteRecv` node pairs transfer slot data between machines with no TCP data copy of the payload.

### Transfer Protocol (Sender-Initiated)

1. **Phase 1**: sender walks the source slot's page chain, builds a per-SGE list (each SGE carries its own lkey — MR1 for pages inside the 64 MiB register, MR-src-ext for SHM-extension pages, MR-src-stage for paged-mode pages that had to be memcpy-staged), and TCP-announces `total_bytes`.
2. **Phase 2**: receiver checks whether `total_bytes` fits in MR1's remaining bump budget. If yes, it allocates an MR1 page chain and replies with `DestReply::SingleMr { dest_off }`. If no (**MR2 overflow**), it lazily registers MR2, reserves a region, and replies with `DestReply::UseMr2 { addr, rkey, dest_off }`. The rkey piggybacks on this reply — no persistent mesh-wide rkey announcement is needed.
3. **Phase 3**: sender RDMA-WRITEs. `SingleMr` → scatter-gather into the pre-structured page chain. `UseMr2` → contiguous write to `(addr, rkey)`.
4. **Phase 4**: sender signals done over TCP. The receiver:
   - For `SingleMr`: nothing more; the slot is already linked.
   - For `UseMr2`: CPU-copies from MR2 into a fresh page chain linked to the slot (the allocator picks between `reclaimer::alloc_page` for Rust-only DAGs and direct bump + `ensure_shm_capacity` for DAGs containing `PyFunc` / `PyPipeline`, auto-detected at DAG load).

RDMA atomics (`RemoteAtomicFetchAdd`, `RemoteAtomicCmpSwap`) are unchanged by Path C — they target named atoms in the MR1 atomic arena and land results in the MR1 RDMA Scratch region.

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
- **Placement policies**: SymbolicDag nodes with no `node_id` are assigned by a named `placement_policy` field (`balanced`, `pack`, `spread`, `weighted`) embedded in the DAG JSON. Live sched_ext hints from the coordinator override the policy when available. The default policy (`pack`) colocates auto-placed nodes on the single most-capable host to minimise cross-node edges.
- **Slot overlap auto-adjustment**: when two fan-out nodes on the same machine share overlapping `out_base` slot ranges, the Partitioner detects the conflict and shifts the second node's base automatically, injecting the corrected base as `arg` so WASM writes to the right slots without DAG changes.
- **Wave-0 deadlock prevention**: the Partitioner detects `RemoteRecv`/`RemoteSend` pairs targeting the same peer on the same machine and adds a dependency edge (send before recv) when safe, breaking the mutual-block that would otherwise stall both machines in wave 0.
- **Slot-free DAG authoring**: SymbolicDag inputs carry no slot numbers. The `slot_assigner` derives every slot from the DAG topology plus two lightweight per-function declarations (`out_base`, `output_offset`) that encode the WASM function's hardcoded I/O offsets. All `Aggregate`, `Input`, `Output`, and terminal `Func` slots are fully automatic.


## Troubleshooting

### `RDMA mesh setup failed: bind 0.0.0.0:75xx: Address already in use (os error 98)`

A previous job's Executor (`host`) subprocess was left running on one of the
nodes (job killed / timed out / Ctrl-C) and is still holding the RDMA mesh
control ports. The mesh uses **deterministic ports** (`BASE_PORT + node_id *
MAX_NODES + j`, e.g. 7524/7525), so the next job collides with the orphan.

**The error names the node and port** — fix it **on that node**:

```bash
# 1. Find the orphaned executor holding a 75xx mesh port:
ss -ltnp | grep -E ':75[0-9][0-9]'
#   LISTEN ... 0.0.0.0:7525 ... users:(("host",pid=10181,fd=10))

# 2. Kill the orphaned executor(s) — leaves the node-agent daemon running:
pkill -9 -f 'release/host'          # or: kill -9 <pid> from step 1

# 3. Confirm it's gone / ports free:
pgrep -af 'release/host'            # prints nothing
ss -ltn | grep -E ':75[0-9][0-9]'  # empty
```

Then resubmit from the coordinator. If `ss` shows **no** process but the bind
still fails, the port is in `TIME_WAIT` — wait ~60 s, or restart that node's
daemon (`pkill -f node-agent` then `./node-agent start --config NodeAgent/agent.toml`).

Reset-everything before a fresh run (run on **every** node, then restart daemons):
```bash
pkill -9 -f 'release/host'; pkill -f 'node-agent'
```

> Root-cause fixes to make this self-healing (not yet applied): set
> `SO_REUSEADDR` on the mesh listeners in `Executor/connect/src/rdma/exchange.rs`,
> and have the worker reap its Executor child when a job fails/times out.

## LeftOver
  1. the newly transferred ml_training, finra, img_pipeline workloads are not finally tweaked, need additional test and debug.
  2. The corpus 1GB file will lead to allocation bug where the memory is not properly loaded, causing the crash, need to do bug fix
  3. Adding the comparison baseline, faasm and deserilzed/serialization and maybe one default standard practice solutions mentioned in either paper. 