# mem_operation

Host-side shared-memory (SHM) memory management: page allocation/reclamation and
file-to-slice partitioning for parallel dispatch.

---

## File structure

```
mem_operation/
├── reclaimer.rs   — SHM page allocator, free-list, slot-level helpers, cursor reset, free-list trim
├── slicer.rs      — Partition a memory-mapped file into non-overlapping FileSlice views
├── organizer.rs   — SHM hash-bucket conflict resolution and GC (BucketOrganizer)
└── OVERVIEW.md    — This file
```

---

## reclaimer.rs — SHM page allocator and reclaimer

Host-side mirror of `guest/src/api/page_allocator.rs`.  Both host and guest share
the same bump heap and the same sharded Treiber-stack free list inside the
`Superblock`, so a page freed by either side is immediately reusable by the other.

### Allocation strategy

1. Pick a preferred shard via a global round-robin counter (`ALLOC_SHARD`).
2. CAS-pop from the first non-empty shard found (wraps around all shards).
3. If all shards are empty, bump-allocate from `sb.bump_allocator` (`fetch_add` — O(1), no retry).

### Free strategy

Push the entire chain as a single unit onto the shard determined by
`(head_offset / PAGE_SIZE) % FREE_LIST_SHARD_COUNT`.  The shard is deterministic
per chain, keeping related pages cache-local and avoiding cross-shard contention.

### Types

| Type | Description |
|---|---|
| `SlotKind` | Discriminates between `Stream` and `Io` slot arrays in the `Superblock`. Used by `reset_slot_cursor`. |

### Functions

#### Page-level primitives

| Function | Description |
|---|---|
| `alloc_page(splice_addr)` | Claim one 4 KiB page from the SHM pool. Tries each free-list shard before falling back to the bump allocator. Returns the page's byte offset from `splice_addr`. |
| `free_page_chain(splice_addr, head)` | Push every page in the chain rooted at `head` back onto the SHM free list as a single unit. Safe to call concurrently. |

#### Slot-level helpers

| Function | Description |
|---|---|
| `clear_stream_slot(splice_addr, slot)` | Zero `writer_heads[slot]` and `writer_tails[slot]` **without** freeing pages. Use after routing operations (Bridge, Aggregate, Shuffle) that have transferred page ownership to downstream slots. |
| `free_stream_slot(splice_addr, slot)` | Detach the page chain from a stream slot and return pages to the free pool. Only call when the slot has exclusive page ownership (not routed). |
| `free_io_slot(splice_addr, slot)` | Detach the page chain from an I/O slot and return pages to the free pool. I/O slots are always exclusively owned, so this is always safe. |
| `reset_slot_cursor(splice_addr, kind, slot)` | Zero the SHM atomic read-cursor (`stream_cursor_N` / `io_cursor_N`) for a slot. Must be called alongside `free_*_slot` when a slot will be reused across runs, otherwise cursor-based readers skip newly loaded data. No-op if the cursor atomic was never registered. |

#### Free-list trim

| Function | Description |
|---|---|
| `count_free_list_pages(splice_addr)` | Walk all free-list shards and count total pages. Best-effort heuristic — not atomic with concurrent alloc/free. |
| `trim_free_list(splice_addr)` | Release physical memory backing of excess free-list pages to the OS via `MADV_DONTNEED`. Pages stay in the free list (virtual offsets remain recyclable); the OS zero-fills on next write. Controlled by `FREE_LIST_TRIM_ENABLED` and `FREE_LIST_TRIM_THRESHOLD` in `common`. No-op when trim is disabled. |

---

## organizer.rs — SHM hash-bucket conflict resolution

After all WASM writers finish, `BucketOrganizer` scans the SHM hash-bucket map,
resolves write conflicts using a pluggable `ConsumptionPolicy`, commits the winning
payload to the Registry, and returns all losing pages to the free list via `reclaimer`.

### Type

| Type | Description |
|---|---|
| `BucketOrganizer<'a>` | Holds the SHM base pointer. Lifetime tied to the `Store` that owns the WASM memory. |

### Methods

| Method | Visibility | Description |
|---|---|---|
| `new(store, memory)` | `pub` | Anchor the organizer at the SHM base (`memory.data_ptr + TARGET_OFFSET`). |
| `consume_all_buckets(policy)` | `pub unsafe` | Scan all `BUCKET_COUNT` hash buckets; for each non-empty bucket atomically detach its conflict list, apply `policy` to select a winner, commit the winner's offset+length to the Registry, and GC all loser page chains. Must be called after all writers have finished. |
| `process_detached_list(head, idx, policy)` | `fn` (private) | Deserialize each node's multi-page payload, call `policy.process`, commit the winner, and free losers. |
| `recycle_chain(list_head)` | `fn` (private) | Push every top-level conflict-list node back onto the free list (top-level links only, not payload page chains). |
| `push_to_free_list(page_offset)` | `fn` (private) | Delegate single-page free to `reclaimer::free_page_chain`. |

---

## slicer.rs — File partition for parallel dispatch

Applies a `SlicePolicy` to a memory-mapped `MappedFile` to produce a set of
non-overlapping `FileSlice` views ready for `FileDispatcher`.  No data is copied —
all slices borrow directly from the `MappedFile` mapping.

### Types

| Type | Description |
|---|---|
| `FileSlice<'a>` | A read-only view of one contiguous chunk of a `MappedFile`. Carries `index` (position among all slices), `offset` (byte offset in the original file), and `data` (the byte slice). |
| `Slicer<'a>` | Stateless partitioner holding a shared reference to a `MappedFile`. Can be reused with different policies or worker counts without re-reading the file. |

### Methods

| Method | Description |
|---|---|
| `Slicer::new(loaded)` | Create a `Slicer` backed by `loaded`. |
| `Slicer::slice(policy, num_workers)` | Apply `policy` to divide the file into at most `num_workers` `FileSlice` views. Slices are ordered by ascending byte offset and together cover the entire file. Actual count may be less than `num_workers` when fewer natural boundaries exist (e.g. fewer newlines than workers). Panics if `num_workers == 0`. |

### Relationship to other modules

```
mmap_file() → MappedFile
                  │
              Slicer::slice(policy, N)
                  │
              Vec<FileSlice<'a>>
                  │
              FileDispatcher::run(slices, callback)
```
