# policy

Pluggable decision strategies used by the host runtime.  Three independent
concerns live here, each in its own file.

---

## File structure

```
policy/
├── consumption.rs   — ConflictNode, ConsumptionPolicy, ConsumptionResult, five built-in policies
├── shuffle_policy.rs — ShufflePolicy trait and three partition implementations
├── slice_policy.rs  — SlicePolicy trait and three slice implementations (with unit tests)
└── OVERVIEW.md      — This file
```

All public items are re-exported from `policy/mod.rs` so callers write
`use crate::policy::ModuloPartition` rather than qualifying the sub-module.

---

## consumption.rs — Conflict resolution for SHM hash buckets

After all WASM writers finish, `BucketOrganizer::consume_all_buckets` calls a
`ConsumptionPolicy` for each non-empty bucket to pick a single winner from the
set of conflicting writes.

### Types

| Type | Description |
|---|---|
| `ConflictNode` | A single deserialized write from a bucket's conflict list. Carries `offset`, `writer_id`, `data_len`, `registry_index`, and `payload` (the decoded byte content). |
| `ConsumptionResult` | Outcome enum: `Winner(writer_id, payload_string)` or `None` (empty bucket). |

### Trait

| Trait | Method | Description |
|---|---|---|
| `ConsumptionPolicy` | `process(&[ConflictNode]) → ConsumptionResult` | Selects one winner from the conflict list. Implement this to add custom resolution logic. |

### Built-in policies

| Policy | Selection rule |
|---|---|
| `MaxIdWinsPolicy` | Highest `writer_id`. |
| `MinIdWinsPolicy` | Lowest `writer_id`. |
| `MajorityWinsPolicy` | Most frequent payload (ties broken by first occurrence). |
| `LastWriteWinsPolicy` | `nodes[0]` — the last CAS-inserted (most recent) write. |
| `LargestPayloadWinsPolicy` | Largest `data_len`; `writer_id` breaks ties. |

---

## shuffle_policy.rs — Upstream-to-downstream routing decisions

`ShufflePolicy` tells `ShuffleConnection::bridge` which downstream slot each
upstream stream should route to.

### Trait

| Trait | Method | Description |
|---|---|---|
| `ShufflePolicy` | `partition(upstream_id, num_downstream) → usize` | Returns a slot index in `[0, num_downstream)`. Must be `Send + Sync` (called from parallel threads). |

### Built-in policies

| Policy | State | Rule |
|---|---|---|
| `ModuloPartition` | Stateless | `upstream_id % num_downstream` — deterministic, based on ID value. |
| `RoundRobinPartition` | `Arc<AtomicUsize>` counter | Assigns each successive call to the next slot in a cycle; based on call order, not ID value. Thread-safe. |
| `FixedMapPartition` | `HashMap<usize, usize>` + `default_slot` | Explicit lookup table; unmapped upstreams fall back to `default_slot`. |

### Note on `make_partition_fn`

`make_partition_fn` wraps a `ShufflePolicy` into a `Fn(usize) -> usize` closure.
It is not currently used — `ShuffleConnection::new` accepts a `ShufflePolicy`
directly.

---

## slice_policy.rs — File partition for parallel dispatch

`SlicePolicy` tells `Slicer::slice` how to cut a memory-mapped file into
non-overlapping `(offset, length)` pairs for parallel dispatch.

### Trait

| Trait | Method | Description |
|---|---|---|
| `SlicePolicy` | `compute_slices(data, num_workers) → Vec<(usize, usize)>` | Returns a complete, non-overlapping partition of `data`. Result count may be less than `num_workers` when content imposes fewer natural split points. |

### Built-in policies

| Policy | Complexity | Description |
|---|---|---|
| `EqualSlice` | O(1) | Divides into `num_workers` equal-sized chunks; last chunk absorbs the remainder. Capped at one byte per chunk when `num_workers > len`. |
| `FixedSizeSlice { max_bytes }` | O(len/max_bytes) | Chunks of at most `max_bytes`; chunk count is `⌈len / max_bytes⌉`, independent of `num_workers`. |
| `LineBoundarySlice` | O(len) | Chunks aligned to `\n` boundaries so no line is split across workers. Actual chunk count ≤ `num_workers` when there are fewer lines than workers. Falls back to a single chunk if no newline exists. |

### Unit tests

`slice_policy.rs` contains inline tests (`#[cfg(test)]`) for all three
implementations: coverage, length correctness, line-alignment, and the
fewer-lines-than-workers edge case.
