# SymbolicDag JSON Field Reference

A SymbolicDag is a machine-count-agnostic DAG description. The Partitioner expands
it into a `ClusterDag` (one subgraph per machine) before execution. All slot numbers,
RDMA pairs, and node-to-machine assignments are derived automatically.

The presence of a `"nodes"` key (and `"total_nodes"`) is what identifies a file as a
SymbolicDag rather than a raw ClusterDag.

---

## Top-level fields

| Field | Type | Required | Description |
|---|---|---|---|
| `shm_path_prefix` | string | yes | Prefix for all shared-memory segment names on every machine. Must be unique across concurrently running jobs. |
| `total_nodes` | number | yes | Number of cluster machines to partition across. |
| `nodes` | array | yes | Ordered list of node descriptors. See [Nodes](#nodes). |
| `transfer` | bool | no (default `true`) | Enable RDMA/TCP data transfer between machines. Set to `false` for single-machine dry-runs. |
| `log_level` | string | no | Executor log verbosity: `"error"`, `"warn"`, `"info"`, `"debug"`, `"trace"`. |
| `placement_policy` | string or object | no (default `"pack"`) | Controls where auto-placed nodes land. See [Placement policies](#placement-policies). |
| `max_colocation` | number | no | Hard cap on auto-placed sandboxes per host, overriding the CPU-derived limit. Useful to limit fanout on a single machine. |
| `shared_inputs` | array | no | Files that must be staged to all machines before execution. Auto-derived from `Input` nodes with `"placement": "all"` — only needed for manual node layouts. Each entry: `{ "path": "...", "source_node": 0 }`. |
| `hints` | object | no | Raw `PlacementHints` (legacy). Use `placement_policy` instead. Ignored when `placement_policy` is also set. |
| `wasm_path` | string | no | Path to the WASM module. Overrides the executor default. |
| `python_script` | string | no | Path to the Python script for `--python` mode. |
| `python_wasm` | string | no | Path to the Python WASM interpreter binary. |
| `mode` | string | no | Execution mode hint passed through to the executor. |
| `runs` | number | no | Repeat the full DAG this many times (benchmark mode). |

---

## Nodes

Each entry in `nodes` describes one logical computation unit.

| Field | Type | Required | Description |
|---|---|---|---|
| `id` | string | yes | Unique node identifier. Referenced by other nodes' `deps`. |
| `kind` | object | yes | What this node does. See [Node kinds](#node-kinds). |
| `deps` | array of strings | no (default `[]`) | IDs of nodes that must complete before this one starts. Cross-machine deps are detected automatically and turned into RDMA Send/Recv pairs. |
| `node_id` | number | no | Pin this node to a specific machine (0-indexed). When absent the Partitioner assigns a machine based on `placement_policy`. |
| `placement` | string | no | Set to `"all"` to replicate this node once per machine. Each copy is named `{id}_{machine_id}` and pinned to that machine. See [Placement: all](#placement-all). |
| `fanout` | number | no | Expand this node into N identical copies named `{id}_0` … `{id}_{N-1}`, all placed on the same machine as the original. Any node listing this id in `deps` gets all N copies substituted in. |
| `output_slot` | number | no | Explicit SHM slot number this node writes its output to. Only needed for `Func`/`WasmVoid` nodes that have cross-machine consumers and whose slot cannot be inferred from `arg2` or `output_offset`. |
| `barrier_group` | string | no | Nodes sharing the same barrier group name are guaranteed to complete the same wave before any of them advances. Reserved for future use. |

### Placement: all

```json
{ "id": "load", "placement": "all", "deps": [], "kind": { "Input": { ... } } }
```

The Partitioner expands this into one copy per machine: `load_0`, `load_1`, etc., each
pinned to its machine. Dependency rules for `"all"` nodes:

- **`"all"` → `"all"` dep**: each copy depends only on its same-machine sibling.
  `map_n` depends on `distribute` → `map_n_0` depends on `distribute_0`, etc.
- **regular → `"all"` dep**: the regular node gets all copies substituted into its
  `deps`, so it waits for every machine's copy.
- **`"all"` → regular dep**: each copy depends on the single regular node
  (broadcast fan-in).

`Input` nodes with `placement: "all"` also auto-populate `shared_inputs` so the
coordinator stages the file to every machine before the job starts.

---

## Node kinds

### Input

Reads a file into a SHM slot. Always a source node (`deps: []`).

```json
"kind": { "Input": { "path": "TestData/corpus.txt", "prefetch": true } }
```

| Field | Type | Required | Description |
|---|---|---|---|
| `path` | string | yes | Path to the input file, relative to the executor working directory. |
| `prefetch` | bool | no | Read the file into the OS page cache before execution begins. |
| `slot` | number | no | SHM slot to write into. Auto-assigned to slot `0` when absent. |

---

### Output

Writes a SHM slot to a file. Usually the terminal node of the DAG.

```json
"kind": { "Output": { "path": "TestOutput/result.txt" } }
```

| Field | Type | Required | Description |
|---|---|---|---|
| `path` | string | yes | Output file path. |
| `slot` | number | no | SHM slot to read from. Auto-derived from the upstream dep's output slot. |

---

### Func

Runs a named function from the WASM module (or Python script in `--python` mode).

```json
"kind": { "Func": { "func": "wc_map", "output_offset": 100 } }
```

The Partitioner rewrites `Func` to `WasmVoid` (or `PyFunc`) for the executor. The
`arg` field it injects is always the **input slot** the function should read from.

#### Slot assignment — pick one of three forms:

**`out_base: N`** — fan-out function that writes to multiple output slots.

```json
"kind": { "Func": { "func": "wc_distribute", "out_base": 50 } }
```

The Partitioner assigns consecutive slots `[N, N+1, …]` to each direct consumer
(one slot per consumer, in topological order). Sets `arg = N_consumers` on this node
so the WASM knows how many outputs to write.

**`output_offset: K`** — offset-output function. Output slot = input slot + K.

```json
"kind": { "Func": { "func": "wc_map", "output_offset": 100 } }
```

`arg` is set to the slot assigned by the upstream fan-out. Output slot is `arg + K`.
Useful for functions whose output channel is implicitly `input + fixed_offset`.

**Neither** — terminal function. Reads its dep's output slot, writes to slot `1`
(the fixed output IO slot).

```json
"kind": { "Func": { "func": "wc_reduce" } }
```

#### Legacy / manual slot form

If `arg` is already present in the JSON, the Partitioner skips slot assignment for
this node entirely. `arg2`, when present, is treated as the output slot for cross-node
routing, then dropped before the executor sees the node.

```json
"kind": { "Func": { "func": "wc_map", "arg": 12, "arg2": 112 } }
```

This form exists in older DAGs written before the Partitioner could infer slots
automatically. Prefer the `out_base` / `output_offset` form for new DAGs.

---

### Aggregate

Collects outputs from multiple upstream slots into a single downstream slot.

```json
"kind": { "Aggregate": {} }
```

When the `Aggregate` object is empty (or missing `upstream_nodes`), the Partitioner
fills in `upstream_nodes` from the node's `deps` list, then allocates a fresh
`downstream` slot.

| Field | Type | Description |
|---|---|---|
| `upstream_nodes` | array of strings | Node IDs whose output slots form the upstream list. Auto-derived from `deps` when absent. |
| `upstream` | array of numbers | Explicit upstream slot list. Skips slot inference when present (legacy / manual). |
| `downstream` | number | Slot to write the merged result into. Auto-allocated when absent. |

When an `Aggregate` node has deps on multiple machines, the Partitioner injects a
`RemoteRecv` on the local machine for each remote dep and rewrites `upstream_nodes`
to include those receive slots.

---

## Placement policies

Controls where nodes without an explicit `node_id` are assigned.

```json
"placement_policy": "pack"
```

Or as a parameterised object:

```json
"placement_policy": { "type": "weighted", "weights": [0.8, 0.2] }
```

Live capacity hints from the coordinator (CPU cores × idle %) always override the
embedded policy when available. The policy applies only when no live hints exist.

### String shorthands

| Value | Behaviour |
|---|---|
| `"pack"` | **Default.** All auto-placed nodes go to the single most-capable host (minimises cross-machine edges). |
| `"balanced"` | Equal weight per host — nodes spread proportionally across all machines. |
| `"spread"` | At most one auto-placed node per host (maximises distribution). |
| `"random"` | Each auto-placed node is assigned to a uniformly random host. Dep-affinity is ignored. |

### Config object

```json
"placement_policy": { "type": "balanced", "per_host_limit": 4 }
```

| Field | Type | Description |
|---|---|---|
| `type` | string | One of `"balanced"`, `"pack"`, `"spread"`, `"weighted"`. |
| `weights` | array of numbers | Per-host capacity weights, host 0 first. Used only with `"weighted"`. Missing trailing hosts get weight 0. Normalised internally. |
| `per_host_limit` | number | Hard cap on auto-placed nodes per host. Overrides the CPU-derived limit. |

#### Examples

```json
"placement_policy": { "type": "weighted", "weights": [0.7, 0.3] }
```
Host 0 gets ~70% of auto-placed nodes, host 1 gets ~30%.

```json
"placement_policy": { "type": "balanced", "per_host_limit": 2 }
```
Balanced spread, but no host receives more than 2 auto-placed nodes.

---

## How the Partitioner processes a SymbolicDag

1. **Expand `placement: "all"` nodes** — one copy per machine, with dep rewriting.
2. **Expand `fanout: N` nodes** — N copies on the same machine.
3. **Assign machines** (`assign_nodes`) — fills in `node_id` for every node that
   doesn't have one, using the placement policy and dep-affinity.
4. **Assign slots** (`assign_slots`) — resolves `out_base` / `output_offset` into
   concrete SHM slot numbers; allocates `Aggregate` downstream slots.
5. **Detect cross-machine edges** — any dep that crosses machine boundaries becomes
   a `RemoteSend` / `RemoteRecv` pair.
6. **Wave-0 deadlock prevention** — if machine A sends to B and also receives from B,
   the send is made a dep of the receive to break the cycle.
7. **Output** — a `ClusterDag` JSON with one `node_dags[i]` entry per machine.
