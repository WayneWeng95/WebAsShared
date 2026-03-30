# NodeAgent

The NodeAgent is the entry point for running DAG workloads in the WebAsShared framework. It operates in two modes:

- **Single-node** (`run`): Spawns the [Executor](../Executor/) as a subprocess with live terminal output, collects runtime metrics.
- **Multi-node** (`start`/`submit`): Coordinator-worker daemon that distributes per-node DAGs across a cluster, manages RDMA-coordinated parallel execution.

## Building

```bash
cd NodeAgent
cargo build --release

# Copy the binary to the project root for convenience
cp target/release/node-agent ..
```

## Usage

All commands run from the `WebAsShared/` project root.

### Single-Node Mode

```bash
# Run a single DAG (Rust/WASM by default)
./node-agent run DAGs/workload_dag/finra_demo.json
./node-agent run DAGs/workload_dag/ml_training_demo.json
./node-agent run DAGs/workload_dag/word_count_demo.json
./node-agent run DAGs/workload_dag/tfidf_demo.json

# Same DAGs with Python execution
./node-agent run DAGs/workload_dag/finra_demo.json --python
./node-agent run DAGs/workload_dag/word_count_demo.json --python

# Image pipeline demos
./node-agent run DAGs/demo_dag/img_pipeline_demo.json
./node-agent run DAGs/demo_dag/img_pipeline_demo.json --python

# Custom executor path
./node-agent run DAGs/workload_dag/finra_demo.json --executor path/to/host
```

The `--python` flag tells the NodeAgent to transform the unified DAG for Python execution:
- `Func` → `PyFunc`, `Pipeline` → `PyPipeline`, `Grouping` → `PyGrouping`
- Injects `python_script` and `python_wasm` paths
- Prefixes `shm_path` and output paths with `py_`

The `run` command:
1. Reads the DAG JSON and transforms unified nodes for the target mode
2. Spawns `Executor/target/release/host dag <dag.json>` with stdout/stderr inherited (live output)
3. Samples metrics every 2 seconds (CPU, RSS, SHM state) to `/tmp/node_agent_metrics.jsonl`
4. Reports elapsed time on completion

### Multi-Node Mode

**Step 1: Start the coordinator** (on machine 0):
```bash
./node-agent start --config NodeAgent/agent_coordinator.toml
```

**Step 2: Start workers** (on each other machine):
```bash
./node-agent start --config NodeAgent/agent_worker.toml
```

**Step 3: Submit a job** (from any machine with coordinator access):
```bash
./node-agent submit --config NodeAgent/agent_coordinator.toml --dag NodeAgent/cluster_dags/finra.json
```

**Step 4: Check status**:
```bash
./node-agent status --config NodeAgent/agent_coordinator.toml
```

## Configuration

Agent config files use TOML format. See `agent_coordinator.toml` and `agent_worker.toml` for examples.

```toml
node_id = 0
role = "coordinator"    # or "worker"

[cluster]
ips = ["192.168.1.10", "192.168.1.11"]
agent_port = 9500

[paths]
executor_bin = "../Executor/target/release/host"
executor_work_dir = "../Executor/host"

[metrics]
interval_ms = 2000
log_path = "/tmp/node_agent_metrics.jsonl"

[timeouts]
job_timeout_s = 300
health_check_s = 5
```

## ClusterDag Format

A ClusterDag is a single JSON file that describes an entire distributed workflow. It replaces the need to manually create separate `node0.json` / `node1.json` DAG files.

```json
{
  "shm_path_prefix": "/dev/shm/rdma_finra",
  "log_level": "info",
  "transfer": true,
  "node_dags": {
    "0": [ ... node 0's DAG nodes ... ],
    "1": [ ... node 1's DAG nodes ... ]
  }
}
```

The coordinator splits this into per-node DAGs by:
1. Generating a unique `shm_path` per node (`{prefix}_n{id}`)
2. Injecting `rdma: { node_id, total, ips, transfer }` from the live cluster membership
3. Copying shared fields (`wasm_path`, `python_script`, `python_wasm`, `log_level`)
4. Assigning each node's DAG nodes from the `node_dags` map

Sample ClusterDags are provided in `cluster_dags/`:

| File | Workload |
|------|----------|
| `word_count.json` | 10 map workers per node, aggregate + reduce on node 1 |
| `finra.json` | 8 audit rules per node, global merge on node 1 |
| `ml_training.json` | PCA + 8 training stumps per node, validation on node 1 |

## Architecture

```
agent/src/
├── main.rs             CLI: run, start, submit, status subcommands
├── config.rs           AgentConfig parsed from agent.toml
├── dag_transform.rs    Unified Func/Pipeline/Grouping → native WasmVoid/PyFunc/etc.
├── protocol.rs         Message types + TCP length-prefixed JSON framing
├── cluster_dag.rs      ClusterDag → per-node Dag splitting with RDMA injection
├── executor.rs         Spawn/monitor host dag subprocess (live or captured output)
├── metrics.rs          CPU (/proc/stat), RSS (/proc/self/status), SHM superblock
├── worker.rs           Connect to coordinator, receive jobs, run executor, report
└── coordinator.rs      Accept workers, distribute jobs, aggregate results
```

### Protocol

The control plane uses length-prefixed JSON over TCP:

```
[4-byte big-endian length][JSON payload]
```

Message types:

| Direction | Kind | Purpose |
|-----------|------|---------|
| Coordinator → Worker | `AssignJob` | Per-node DAG JSON + job ID |
| Coordinator → Worker | `AbortJob` | Cancel running job |
| Coordinator → Worker | `Ping` | Health check |
| Worker → Coordinator | `Ready` | Worker connected, idle |
| Worker → Coordinator | `JobStarted` | Executor PID |
| Worker → Coordinator | `JobCompleted` | Duration + stdout tail |
| Worker → Coordinator | `JobFailed` | Exit code + stderr tail |
| Worker → Coordinator | `Metrics` | Periodic CPU/RSS/SHM stats |

### Execution Flow (Multi-Node)

1. Workers connect to coordinator and send `Ready`
2. Client sends `SubmitJob` with a ClusterDag JSON
3. Coordinator splits ClusterDag into per-node DAGs
4. Coordinator sends `AssignJob` to **all workers first** (critical: RDMA mesh bootstrap has a ~200ms TCP handshake window — all nodes must start roughly together)
5. Coordinator launches its own local Executor last
6. Workers launch Executors immediately on receipt, report `JobStarted`
7. Workers send periodic `Metrics` while running
8. On completion, workers send `JobCompleted` or `JobFailed`
9. Coordinator aggregates results and sends `JobResult` back to the client

### Metrics

Sampled every `interval_ms` (default 2 seconds):

| Metric | Source |
|--------|--------|
| CPU usage % | `/proc/stat` delta between samples |
| RSS bytes | `/proc/self/status` VmRSS field |
| SHM bump offset | Superblock byte offset 4 (little-endian u32) |
| Executor PID | From spawned subprocess |
| Job elapsed time | Wall-clock since spawn |

Metrics are written as JSON lines to the configured log path and sent to the coordinator in multi-node mode.

## Design Decisions

- **Coordinator model** (not P2P): Matches the asymmetric structure of existing RDMA DAGs (node 0 sends, node 1 receives/reduces). No consensus needed for 2–32 node research clusters.
- **Separate process**: The Executor is a single-run process (format SHM → run DAG → exit). The NodeAgent is a long-running daemon that survives across jobs.
- **TCP control plane**: The RDMA mesh is set up inside the Executor and torn down per job. The NodeAgent needs communication before and after Executor runs, so TCP is the right tool.
- **Static membership**: A config file lists node IPs. No service discovery needed for a research cluster with a known set of machines.
- **Live output in run mode**: `Stdio::inherit()` passes executor stdout/stderr directly to the terminal. In multi-node mode, output is captured to avoid interleaving.
