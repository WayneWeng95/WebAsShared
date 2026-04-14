# Slot Allocation: Current State & Improvement Ideas

## Current Slot Budget

From `common/src/lib.rs`:
- **Stream slots**: 2048 (indices 0-2047)
- **I/O slots**: 512 (indices 0-511)
- Predefined: `INPUT_IO_SLOT = 0`, `OUTPUT_IO_SLOT = 1`

Stream slots and I/O slots are separate namespaces managed in the Superblock.

---

## Current Slot Usage Per Workload

### Word Count
```
I/O 0           ← Input file (corpus.txt)
Stream 10-19    ← wc_distribute: 10 shards
Stream 110-119  ← wc_map outputs (input + 100)
Stream 200      ← Aggregate [110-119]
I/O 1           ← wc_reduce final output
```

### TF-IDF
```
I/O 0           ← Input file (corpus.txt)
Stream 10-17    ← wc_distribute: 8 shards
Stream 110-117  ← tfidf_map outputs (input + 100)
Stream 200      ← Aggregate [110-117]
I/O 1           ← tfidf_reduce final output
```

### FINRA
```
I/O 0           ← Input file (trades.csv)
Stream 5        ← finra_fetch_private (hardcoded)
Stream 6        ← finra_fetch_public (hardcoded)
Stream 200      ← Aggregate [5, 6]
Stream 10-17    ← finra_audit_rule outputs (rule_id → slot 10 + rule_id)
Stream 300      ← Aggregate [10-17]
I/O 1           ← finra_merge_results output
```

### ML Training
```
I/O 0           ← Input file (mnist_features.csv)
Stream 10-11    ← ml_partition: 2 shards
Stream 110-111  ← ml_pca outputs (input + 100)
Stream 200      ← Aggregate [110-111]
Stream 30-37    ← ml_redistribute: 8 training shards
Stream 130-137  ← ml_train outputs (input + 100)
Stream 300      ← Aggregate [130-137, 200]
I/O 1           ← ml_validate output
```

### Image Pipeline / Grouping
```
I/O 10          ← Input images (PPM)
Stream 20       ← img_load_ppm
Stream 30       ← img_rotate
Stream 40       ← img_grayscale
Stream 50       ← img_equalize
Stream 60       ← img_blur
I/O 1           ← img_export_ppm
```

### RDMA Cluster Convention
```
Node 0: computes → slot N, sends via RDMA
Node 1: receives into slot N+1, aggregates [N, N+1] → N+100
```
Example: Word Count sends 200, Node 1 receives 201, aggregates to 300.

---

## What's Wrong (or Confusing) Today

### 1. No visible system behind the numbers
Slot 5, 6, 10, 30, 200, 300 — why those? A reader has to reverse-engineer the pattern from the guest code. The DAG JSON gives no hints about the allocation logic.

### 2. Hardcoded slots in Rust guest code
The Rust guest has constants like `TRADE_STREAM_SLOT = 5`, `REF_STREAM_SLOT = 6`, `RULE_OUT_BASE = 10`. These are invisible in the DAG JSON. If you change a slot in the JSON without changing the guest code, it silently breaks.

### 3. The "+100 output offset" is implicit
`wc_map(10)` writes to slot 110. This `+100` convention exists only in Rust constants (`WC_MAP_OUT_BASE = 100`, `PCA_OUT_OFFSET = 100`, `TRAIN_OUT_OFFSET = 100`). The DAG JSON must know this to set up the correct Aggregate upstream list. It's a hidden coupling.

### 4. FINRA is different from everything else
- Uses semantic slots (5 = trades, 6 = reference) instead of offset-based.
- `finra_fetch_public` ignores its `arg` entirely (hardcoded to slot 6).
- `finra_audit_rule` uses `arg` as rule_id, not as a slot — opposite of every other function.

### 5. Aggregation slots (200, 300) are convention, not enforced
Nothing prevents a DAG from using slot 200 for something else. It's just a convention that "200 = first aggregation, 300 = second aggregation."

### 6. I/O slots vs stream slots look the same in JSON
`"slot": 0` in an Input node means I/O slot 0. `"arg": 10` in a Func node means stream slot 10. The namespace difference is invisible.

---

## Improvement Ideas

### Idea 1: Named Slot Ranges with a Convention Table

Define a clear tier system and document it in the DAG JSON:

```
Tier    Range       Purpose
────    ─────       ───────
I/O     0           Standard input (file loading)
I/O     1           Standard output (result writing)
I/O     2-9         Additional I/O (multi-file)
I/O     10-19       Binary I/O (images, etc.)

Stream  1-9         Scratch / small intermediates
Stream  10-49       Stage 1 outputs (partitions, shards, fetches)
Stream  50-99       Stage 2 intermediates
Stream  100-199     Worker/mapper outputs (base + offset convention)
Stream  200-299     First-level aggregation
Stream  300-399     Second-level aggregation / RDMA receives
Stream  400-499     Final aggregation (multi-node merge)
Stream  500+        Pipeline stages, demos, misc
```

This makes every DAG self-documenting: "slot 135 is in the worker output tier, slot 300 is a second-level aggregation."

### Idea 2: Symbolic Slot Names in DAG JSON

Instead of raw numbers, allow named references that the executor resolves:

```json
{
  "slots": {
    "input_shards": { "base": 10, "count": 10 },
    "map_outputs":  { "base": 110, "count": 10 },
    "aggregated":   200,
    "final_output": "io:1"
  },
  "nodes": [
    { "id": "distribute", "kind": { "Func": { "func": "wc_distribute", "slot_ref": "input_shards" } } },
    { "id": "map_0", "kind": { "Func": { "func": "wc_map", "in": "input_shards[0]", "out": "map_outputs[0]" } } }
  ]
}
```

**Pros**: DAG is self-documenting, no magic numbers, can validate ranges at parse time.
**Cons**: Requires executor changes (resolve names → numbers before execution), more complex JSON schema.

### Idea 3: Auto-Allocation by the NodeAgent

The NodeAgent could assign slot numbers automatically based on the DAG topology:

1. Parse the DAG graph
2. Walk topological order
3. Assign input/output slots per node, respecting the tier convention
4. Inject the concrete slot numbers into the JSON before passing to executor

The DAG author only specifies logical relationships:
```json
{ "id": "map_0", "deps": ["distribute"], "kind": { "Func": { "func": "wc_map" } } }
```

And the NodeAgent figures out that map_0 reads from distribute's output shard 0 and writes to the next available slot.

**Pros**: No manual slot management at all, impossible to have conflicts.
**Cons**: Major refactor of both NodeAgent and Executor, loses the explicit data-flow visibility that makes DAGs debuggable.

### Idea 4: Pass Output Slot as an Explicit Arg (Eliminate Hardcoding)

The simplest incremental fix: make every guest function take `(input_slot, output_slot)` explicitly instead of computing `output = input + 100` internally.

Current (Rust guest):
```rust
const WC_MAP_OUT_BASE: u32 = 100;
pub fn wc_map(input_slot: u32) {
    let output_slot = input_slot + WC_MAP_OUT_BASE; // hidden
    // ...
}
```

Proposed:
```rust
pub fn wc_map(input_slot: u32, output_slot: u32) {
    // both explicit — DAG JSON controls everything
}
```

This is essentially what the Python guest already does (it takes `arg` + `arg2`). The unified `Func` format already carries both:
```json
{ "Func": { "func": "wc_map", "arg": 10, "arg2": 110 } }
```

If the Rust guest also took two args, the `wasm_arg` override hack in `dag_transform.rs` would disappear entirely. Rust and Python would have identical calling conventions.

**Pros**: Eliminates all hidden slot coupling, makes the DAG JSON the single source of truth, removes the need for `wasm_arg` overrides in the unified format.
**Cons**: Requires changing all Rust guest function signatures from `WasmVoid(u32)` to `WasmU32(u32, u32)` or similar. Breaking change to the guest ABI.

### Idea 5: Validation Pass in the NodeAgent

Without changing anything structural, add a `--validate` flag to `node-agent run` that checks:

1. **No slot conflicts**: two nodes don't write to the same slot (unless intentional via Aggregate)
2. **Aggregate upstream exists**: every slot in an Aggregate's `upstream` list is written by some node
3. **Slot range sanity**: all slots are within `[0, STREAM_SLOT_COUNT)` / `[0, IO_SLOT_COUNT)`
4. **Output slot written**: the Output node's slot was actually written to
5. **Tier convention**: warn if slots don't follow the tier table

This catches typos and misconfigurations before the executor runs.

---

## Recommended Path

**Short-term** (no executor changes):
- Adopt Idea 1 (document the tier convention)
- Implement Idea 5 (validation pass in NodeAgent)

**Medium-term** (guest ABI change):
- Implement Idea 4 (explicit two-arg calling convention for Rust guest)
- This eliminates `wasm_arg` overrides and makes the unified DAG format truly clean

**Long-term** (if DAGs get more complex):
- Consider Idea 2 (symbolic slot names) to make large DAGs maintainable
- Idea 3 (auto-allocation) only if the number of workloads grows significantly

---

## Slot Map Quick Reference

| Slot | WC | TFIDF | FINRA | ML | Image |
|------|----|----|-------|-------|-------|
| I/O 0 | input | input | input | input | - |
| I/O 1 | output | output | output | output | output |
| I/O 10 | - | - | - | - | input |
| 5 | - | - | trades | - | - |
| 6 | - | - | reference | - | - |
| 10-19 | shards | shards(8) | rule outs | partitions(2) | - |
| 20-60 | - | - | - | - | pipeline stages |
| 30-37 | - | - | - | train shards | - |
| 110-119 | map outs | map outs(8) | - | PCA outs(2) | - |
| 130-137 | - | - | - | train outs | - |
| 200 | agg | agg | input agg | PCA agg | - |
| 300 | - | - | rule agg | train agg | - |
