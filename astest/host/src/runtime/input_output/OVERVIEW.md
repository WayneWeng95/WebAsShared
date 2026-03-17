# input_output

Host-side I/O layer between the file system and the shared-memory (SHM) region.

All four modules work with the same SHM page-chain layout used by the WASM guests,
but none of them depend on each other — they may be used independently.

---

## File structure

```
input_output/
├── slot_loader.rs   — File → SHM: memory-map a file and write its contents into an I/O slot
├── slot_flusher.rs  — SHM → File: drain a completed I/O slot's records to a file
├── persistence.rs   — Background snapshot/watch: copy any SHM region to disk asynchronously
├── logger.rs        — SHM log-arena writer: structured host-side log records into SHM
└── OVERVIEW.md      — This file
```

---

## slot_loader.rs — File → SHM

Reads a file from disk and writes its contents into a chosen I/O slot in the SHM
page-chain so that WASM guests can read it via `ShmApi::read_next_io_record` /
`ShmApi::read_all_inputs_from`.

Page allocation goes through `reclaimer::alloc_page` (free-list first, then bump),
so freed pages from earlier DAG nodes are reused before new bump space is consumed.

### Types

| Type | Description |
|---|---|
| `MappedFile` | A read-only memory-mapped view of a file (`MAP_PRIVATE \| PROT_READ`). Released via `munmap` on drop. Safe to share across threads (`Send + Sync`). |
| `SlotLoader` | Writes a `MappedFile` line-by-line (or as a single record) into an SHM I/O slot. |
| `PrefetchHandle` | Handle to a background prefetch thread started by `SlotLoader::prefetch`. |

### Functions / methods

| Symbol | Description |
|---|---|
| `mmap_file(path)` | Open `path` and return a `MappedFile`. Returns an error if the file is empty or `mmap(2)` fails. |
| `SlotLoader::new(splice_addr)` | Create a loader bound to the SHM region at `splice_addr`. |
| `SlotLoader::load(path, slot)` | `mmap_file` the path and write each non-empty line as one length-prefixed record into `slot`. Returns the record count. |
| `SlotLoader::load_as_single_record(path, slot)` | `mmap_file` the path and write the entire file as a single record. Use for binary payloads. |
| `SlotLoader::prefetch(splice_addr, path, slot)` | Spawn a background thread that calls `load`; returns a `PrefetchHandle`. |
| `PrefetchHandle::join()` | Block until the prefetch completes; returns the record count or an error. |

---

## slot_flusher.rs — SHM → File

Reads completed records from an I/O slot's SHM page-chain and writes them to a
file on disk.  Runs synchronously so the output file is guaranteed to exist before
the next DAG node executes.

### Types

| Type | Description |
|---|---|
| `SlotFlusher` | Holds a `splice_addr`; reads records from I/O slots and writes them to files. |

### Methods

| Method | Description |
|---|---|
| `SlotFlusher::new(splice_addr)` | Create a flusher bound to the SHM region at `splice_addr`. |
| `save(path)` | Flush `OUTPUT_IO_SLOT` to `path`, one record per line. Convenience wrapper around `save_slot`. |
| `save_slot(path, slot)` | Flush I/O `slot` to `path`, one record per line. Returns the number of records written. |
| `collect()` | Collect all records from `OUTPUT_IO_SLOT` into memory without writing to disk. |
| `collect_slot(slot)` | Collect all records from I/O `slot` into memory without writing to disk. |

---

## persistence.rs — Background snapshot / watch

Copies SHM data to heap and queues it for background file I/O via an mpsc channel,
so the calling thread is never blocked waiting for disk writes.

Three categories of data can be persisted:

| File | Content |
|---|---|
| `atomics.txt` | One `name=value` line per named atomic variable in the Registry |
| `stream_{id}.txt` | One `[N][src=origin] record` line per record in a stream slot's page-chain |
| `shared_{name}.bin` | Raw committed payload bytes for a Manager-committed shared-state entry |

### Types

| Type | Description |
|---|---|
| `PersistenceOptions` | Selects which categories to persist: `output_dir`, `atomics`, `stream_slots`, `shared_state`. |
| `PersistenceWriter` | Owns the background writer thread and the send side of the mpsc channel. |

### Methods

| Method | Description |
|---|---|
| `PersistenceWriter::new()` | Spawn the background writer thread. |
| `snapshot(splice_addr, opts)` | Copy all requested SHM regions to heap and queue for disk write. Non-blocking. |
| `watch_stream(splice_addr, slot_id, output)` | Copy a single stream slot's records and queue a write to the exact file path `output`. Non-blocking. |
| `watch_shared(splice_addr, name, output)` | Copy a single named shared-state payload and queue a write to `output`. Logs a warning if not found. Non-blocking. |
| `join()` | Signal the background thread to stop and block until all queued writes finish. Called automatically on drop. |

### Internal helpers (pub(super))

| Function | Description |
|---|---|
| `read_stream_records(base, sb, slot)` | Walk the stream slot's page-chain and return every length-prefixed record. |
| `read_io_records(base, sb, slot)` | Walk the I/O slot's page-chain and return every length-prefixed record. Used by `slot_flusher`. |

---

## logger.rs — SHM log-arena writer

Writes structured log records from the host directly into the SHM log arena so
that the full execution trace (both host and guest entries) is visible in one place.

| Type | Description |
|---|---|
| `HostLogger` | Writes log records into the SHM log arena at `splice_addr`. |

| Method | Description |
|---|---|
| `HostLogger::new(splice_addr, level)` | Create a logger bound to the SHM region, filtering below `level`. |
| `info(node_id, msg)` | Write an INFO-level record tagged with `node_id`. |
| `debug(node_id, msg)` | Write a DEBUG-level record (only if level ≤ Debug). |
| `warn(node_id, msg)` | Write a WARN-level record. |
| `error(node_id, msg)` | Write an ERROR-level record. |
