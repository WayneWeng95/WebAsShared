# Partitioner

The Partitioner takes a **symbolic, location-agnostic DAG** and auto-partitions it into per-node DAGs suitable for the NodeAgent submission path (`ClusterDag.node_dags`).

A symbolic DAG describes the logical computation — nodes and edges only — without explicit `RemoteSend`/`RemoteRecv` pairs or slot numbers. The Partitioner inserts these automatically.

## Pipeline

```
SymbolicDag (JSON)
      │
      ▼
[1. expand_all_placements]   placement:"all" → one copy per machine
      │
      ▼
[2. expand_fanout]           fanout:N → N parallel copies
      │
      ▼
[3. assign_nodes]            auto-placed nodes → concrete node_ids via placement policy
      │
      ▼
[4. edge splitter]           cross-node edges → RemoteSend / RemoteRecv pairs
      │
      ▼
[5. deadlock prevention]     add RemoteSend → RemoteRecv dep when safe (wave-0 fix)
      │
      ▼
[6. slot_assigner]           assign stream/IO slot numbers; auto-adjust for per-machine overlaps
      │
      ▼
ClusterDag  →  NodeAgent coordinator.submit()
```

## Symbolic DAG JSON Format

```json
{
  "description": ["..."],
  "shm_path_prefix": "/dev/shm/my_dag",
  "log_level": "info",
  "transfer": true,
  "total_nodes": 2,
  "placement_policy": "pack",
  "nodes": [
    { "id": "load",       "placement": "all", "deps": [],
      "kind": { "Input": { "path": "TestData/corpus_1gb.txt", "prefetch": true } } },
    { "id": "distribute", "placement": "all", "deps": ["load"],
      "kind": { "Func": { "func": "wc_distribute", "out_base": 50 } } },
    { "id": "map_n",      "placement": "all", "fanout": 4, "deps": ["distribute"],
      "kind": { "Func": { "func": "wc_map", "output_offset": 100 } } },
    { "id": "reduce",     "deps": ["aggregate"],
      "kind": { "Func": { "func": "wc_reduce" } } }
  ]
}
```

### Node fields

| Field | Description |
|-------|-------------|
| `placement: "all"` | Expand to one copy per machine (`{id}_0` … `{id}_{N-1}`) |
| `fanout: N` | Expand to N parallel copies within the same machine |
| `node_id: K` | Pin to machine K explicitly |
| *(absent)* | Auto-placed by `placement_policy` |
| `out_base: S` | Fan-out output slot range starts at S; per-machine overlap auto-adjusted |

## Placement Policies

The `placement_policy` field replaces the legacy `hints` object. It accepts either a string shorthand or a parameterised config object.

### String shorthands

```json
"placement_policy": "balanced"
"placement_policy": "pack"
"placement_policy": "spread"
```

| Policy | Behaviour |
|--------|-----------|
| `balanced` | Equal capacity weight per host; nodes distributed proportionally |
| `pack` | Colocate all auto-placed nodes on the single most-capable host (default when field is omitted) |
| `spread` | At most one auto-placed node per host |

### Parameterised objects

```json
"placement_policy": { "type": "weighted", "weights": [0.8, 0.2] }
"placement_policy": { "type": "balanced", "per_host_limit": 4 }
```

The `per_host_limit` field caps auto-placed sandboxes per host and works with any policy type.

### Priority order

Live capacity hints from the coordinator always take precedence:

```
coordinator live hints  >  placement_policy  >  hints (legacy)  >  default (pack)
```

## Slot Assignment

`slot_assigner::assign_slots` runs after node placement and assigns concrete SHM slot numbers:

- Fan-out nodes (`out_base`) get a slot range starting at `out_base`. If two fan-out nodes on the **same machine** would use overlapping slot ranges, the second node's base is auto-bumped to start immediately after the first's range ends, and the adjusted base is injected as `arg` so the WASM function writes to the correct slots.
- Nodes with `output_offset` receive the slot assigned to their upstream fan-out source plus the offset.
- Auto-placed nodes without `out_base` or `output_offset` get the next available slot from a monotone counter.

## Deadlock Prevention

When a machine has both a `RemoteRecv(from=P)` and a `RemoteSend(to=P)` with no dependencies between them, both land in wave 0 and each machine blocks waiting for the other before any local work has run.

The splitter detects this pattern and adds `RemoteSend` as a dependency of `RemoteRecv` whenever doing so cannot create a cycle (checked via BFS reachability). This ensures sends fire first in wave 0 before the corresponding recv waits.

## Example: word_count_auto_placement.json

`DAGs/symbolic_dag/word_count_auto_placement.json` demonstrates a 2-node word count:

- `load` (placement: all) — each machine reads its locally staged 1 GiB corpus shard
- `distribute` (placement: all, out_base: 50) — splits lines across map workers; writes to slots 50…53 per machine
- `map_n` (placement: all, fanout: 4) — 4 parallel map workers per machine
- `aggregate` — collects map outputs per machine
- `aggregate_global`, `reduce`, `save` — auto-placed by policy (default: `pack`)

The `placement_policy: "pack"` field replaced the old explicit `hints` object:

```json
// Before (legacy hints)
"hints": {
  "capacity":   { "0": 0.5, "1": 0.5 },
  "host_limit": { "0": 12,  "1": 12  }
}

// After (placement_policy)
"placement_policy": "pack"
```
