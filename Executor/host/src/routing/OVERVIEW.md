# routing

Host-side SHM page-chain routing layer.  All connections operate zero-copy:
they rewire `writer_heads` / `writer_tails` atomics in the `Superblock` without
moving any page data.

---

## File structure

```
routing/
├── stream.rs         — StreamBridge: 1→1 zero-copy slot redirect (two atomic stores)
├── aggregate.rs      — AggregateConnection: N→1 merge via ChainSplicer
├── shuffle.rs        — ShuffleConnection: N→M partitioned routing via ShufflePolicy
├── broadcast.rs      — BroadcastConnection: N→M full fan-out
├── dispatch.rs       — FileDispatcher: parallel slice distribution to worker threads
├── chain_splicer.rs  — ChainSplicer: internal O(1) splice / parallel tree merge primitive
└── OVERVIEW.md       — This file
```

---

## stream.rs — StreamBridge

Wires a 1→1 connection by pointing the downstream slot's `head`/`tail` atomics
at the upstream slot's existing page chain.  No data is copied — the downstream
slot shares the upstream's pages in-place.

### Type

| Type | Description |
|---|---|
| `StreamBridge` | Holds `splice_addr` (the SHM superblock base). Created with `StreamBridge::new(splice_addr)`. |

### Method

| Method | Description |
|---|---|
| `bridge(upstream_id, downstream_id) → bool` | Atomically redirect `downstream_id`'s head/tail at `upstream_id`'s page chain. Returns `false` if `upstream_id` has not written anything yet (head == 0). |

---

## aggregate.rs — AggregateConnection

Merges N upstream slot chains into a single downstream slot.

### Type

| Type | Description |
|---|---|
| `AggregateConnection` | Holds `upstream_ids: Vec<usize>` and `downstream_id: usize`. |

### Methods

| Method | Description |
|---|---|
| `new(upstream_ids, downstream_id)` | Create the connection. |
| `bridge(splice_addr)` | Merge all upstreams into `downstream_id` via `ChainSplicer::merge_into`. Sequential for N ≤ `PARALLEL_THRESHOLD`; parallel tree merge beyond that. |

---

## shuffle.rs — ShuffleConnection

Routes N upstream slots to M downstream slots, one upstream per downstream,
determined by a pluggable `ShufflePolicy`.  Each downstream group is processed
concurrently in a `thread::scope`.

### Type

| Type | Description |
|---|---|
| `ShuffleConnection` | Holds upstream/downstream ID lists and a boxed `ShufflePolicy`. |

### Methods

| Method | Description |
|---|---|
| `new(upstream_ids, downstream_ids, policy)` | Construct with any `ShufflePolicy + 'static`. |
| `bridge(splice_addr)` | Group upstreams by `policy.partition(upstream_id, num_downstream)`, then `merge_into` each group concurrently. |

### Policies (in `crate::policy`)

| Policy | Description |
|---|---|
| `ModuloPartition` | `upstream_id % num_downstream` — deterministic, balanced. |
| `RoundRobinPartition` | Assigns upstreams round-robin in call order. |
| `FixedMapPartition` | Explicit `HashMap<upstream_id, downstream_slot>` with a fallback. |

---

## broadcast.rs — BroadcastConnection

Fans every upstream into every downstream slot (N×M full fan-out).  Downstream
slots are processed sequentially to avoid concurrent writes to the same upstream
tail-page `next_offset` fields.

### Type

| Type | Description |
|---|---|
| `BroadcastConnection` | Holds `upstream_ids` and `downstream_ids`. |

### Methods

| Method | Description |
|---|---|
| `new(upstream_ids, downstream_ids)` | Create the connection. |
| `bridge(splice_addr)` | For each `dst_id` in order, call `ChainSplicer::merge_into(upstream_ids, dst_id)`. Each downstream receives a complete merged copy of all upstream chains. |

---

## dispatch.rs — FileDispatcher

Distributes slices across parallel worker threads using round-robin assignment.
Works with any type implementing `DispatchSlice`: `FileSlice<'a>` for zero-copy
file views and `OwnedSlice` for worker-produced data.

### Trait

| Trait | Description |
|---|---|
| `DispatchSlice` | Implemented by slice types: provides `index()` (for round-robin placement) and `data()` (byte payload). |

### Types

| Type | Description |
|---|---|
| `OwnedSlice` | Worker-produced payload (`index: usize`, `data: Vec<u8>`). Implements `DispatchSlice`. |
| `WorkerAssignment<S>` | A set of slices assigned to one logical worker: `worker_id`, `slices: Vec<S>`. Helpers: `total_bytes()`, `slice_count()`. |
| `FileDispatcher` | Stateless dispatcher created with `FileDispatcher::new(worker_count)`. |

### Methods on `FileDispatcher`

| Method | Description |
|---|---|
| `assign(slices) → Vec<WorkerAssignment<S>>` | Assign `slices` to workers round-robin by `slice.index() % worker_count`. Returns one entry per worker that received at least one slice. |
| `run(slices, worker_fn)` | Assign then execute `worker_fn` for each `WorkerAssignment` concurrently inside a `thread::scope`. |

### Relationship to `mem_operation/slicer`

```
mmap_file()  →  MappedFile
                    │
                Slicer::slice(policy, N)
                    │
                Vec<FileSlice<'a>>          implements DispatchSlice
                    │
                FileDispatcher::run(slices, callback)
```

---

## chain_splicer.rs — ChainSplicer (internal)

Internal primitive, `pub(super)` — not part of the public routing API.

`ChainSplicer` wraps `splice_addr` and is `Copy` (one `usize`), so it moves
freely into thread closures.

### Type

| Type | Visibility | Description |
|---|---|---|
| `ChainSplicer` | `pub(super)` | SHM page-chain splice helper. |

### Methods

| Method | Description |
|---|---|
| `new(splice_addr)` | Wrap the SHM base pointer. |
| `chain_onto(dst_id, src_id)` | O(1) splice: link `src_id`'s chain onto the end of `dst_id`'s chain using `writer_tails[dst_id]` (no page walking). No-op if `src_id` has no pages. |
| `merge_into(upstream_ids, dst_id)` | Sequential for N ≤ `PARALLEL_THRESHOLD`; parallel tree merge (O(log N) levels) beyond that. Each level chains adjacent pairs of upstream IDs concurrently. |

### Used by

All three multi-source connection types (`AggregateConnection`, `ShuffleConnection`,
`BroadcastConnection`) delegate their merge work to `ChainSplicer`.
`StreamBridge` does the two atomic stores directly (no `ChainSplicer` needed).
