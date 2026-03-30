# Guest API Reference

All functions are static methods on `ShmApi`.  Import with:

```rust
use crate::api::ShmApi;
```

---

## Input

Host-loaded data read by workloads.  The default input slot is `INPUT_IO_SLOT`.

| Method | Returns | Description |
|---|---|---|
| `ShmApi::read_input()` | `Option<(u32, Vec<u8>)>` | Most recent record from the default input slot |
| `ShmApi::read_all_inputs()` | `Vec<(u32, Vec<u8>)>` | All records from the default input slot |
| `ShmApi::read_input_str()` | `Option<(u32, String)>` | Most recent input record decoded as UTF-8 |
| `ShmApi::read_input_from(slot: u32)` | `Option<(u32, Vec<u8>)>` | Most recent record from an explicit I/O slot |
| `ShmApi::read_all_inputs_from(slot: u32)` | `Vec<(u32, Vec<u8>)>` | All records from an explicit I/O slot |
| `ShmApi::read_input_str_from(slot: u32)` | `Option<(u32, String)>` | Most recent record as UTF-8 from an explicit slot |

Each returned tuple is `(origin_id, payload)`.

---

## Output

Results written by workloads and later flushed to disk by the host `Output` node.
The default output slot is `OUTPUT_IO_SLOT`.

| Method | Description |
|---|---|
| `ShmApi::write_output(data: &[u8])` | Append bytes to the default output slot |
| `ShmApi::write_output_str(s: &str)` | Append a string to the default output slot |
| `ShmApi::write_output_to(slot: u32, data: &[u8])` | Append bytes to an explicit I/O slot |
| `ShmApi::write_output_str_to(slot: u32, s: &str)` | Append a string to an explicit I/O slot |

---

## Stream slots

Append-only record channels used to pass data between pipeline stages.

| Method | Returns | Description |
|---|---|---|
| `ShmApi::append_stream_data(slot: u32, payload: &[u8])` | `()` | Append a record to `slot` |
| `ShmApi::read_all_stream_records(slot: u32)` | `Vec<(u32, Vec<u8>)>` | Read all records from `slot` |
| `ShmApi::read_next_stream_record(slot: u32)` | `Option<(u32, Vec<u8>)>` | Read the next unconsumed record; advances a per-slot cursor |
| `ShmApi::read_stream_range(slot: u32, offset: u32, length: usize)` | `Option<Vec<u8>>` | Read `length` bytes from `slot` at an absolute byte offset |
| `ShmApi::write_stream_range(slot: u32, offset: u32, data: &[u8])` | `bool` | Overwrite bytes in-place; returns `false` if range is out of bounds |
| `ShmApi::lseek_stream(slot: u32, current_pos: u32, whence: SeekFrom)` | `Option<u32>` | Compute a new byte position within `slot` (POSIX-style seek) |

`read_next_stream_record` is cursor-based: successive calls return successive records.
Use `read_all_stream_records` when you need all records at once.

---

## I/O slots

General-purpose host↔guest byte channels, separate from the fixed input/output slots.

| Method | Returns | Description |
|---|---|---|
| `ShmApi::append_io_data(slot: u32, payload: &[u8])` | `()` | Append a record to I/O `slot` |
| `ShmApi::read_all_io_records(slot: u32)` | `Vec<(u32, Vec<u8>)>` | Read all records from I/O `slot` |
| `ShmApi::read_next_io_record(slot: u32)` | `Option<(u32, Vec<u8>)>` | Read the next unconsumed record; advances a per-slot cursor |

---

## Named atomics

Shared `AtomicU64` counters registered by name in a host-visible registry.
Useful for cross-worker coordination and reduction accumulators.

| Method | Returns | Description |
|---|---|---|
| `ShmApi::get_named_atomic(name: &str)` | `&'static AtomicU64` | Look up (or register) an atomic by name; cached after first access |
| `ShmApi::get_atomic(index: usize)` | `&'static AtomicU64` | Access an atomic by its raw registry index |

```rust
let counter = ShmApi::get_named_atomic("word_count");
counter.fetch_add(42, Ordering::Relaxed);
```

---

## Shared state

Manager-resolved consensus storage.  Multiple workers write competing values;
the host `BucketOrganizer` picks a winner and commits it to the registry.

| Method | Returns | Description |
|---|---|---|
| `ShmApi::write_shared_state(task: &str, writer_id: u32, data: &[u8])` | `()` | Submit a value for `task`; Manager picks the winner after all writers finish |
| `ShmApi::write_shared_state_range(task: &str, writer_id: u32, data: &[u8], offset: usize, length: usize)` | `bool` | Submit a byte-range slice of `data`; returns `false` if range is out of bounds |
| `ShmApi::read_shared_state(task: &str)` | `Option<Vec<u8>>` | Read the Manager-committed winning value for `task` |
| `ShmApi::read_shared_state_range(task: &str, offset: usize, length: usize)` | `Option<Vec<u8>>` | Read a sub-range of committed state without copying the full payload |
| `ShmApi::lseek_shared_state(task: &str, current_pos: u32, whence: SeekFrom)` | `Option<u32>` | Compute a new byte position within committed state (POSIX-style seek) |
| `ShmApi::insert_shared_data(key_hash: u32, writer_id: u32, data: &[u8])` | `()` | Insert directly into the lock-free shared hash map (low-level) |
| `ShmApi::read_shared_chain(key_hash: u32)` | `Vec<(u32, Vec<u8>)>` | Read all values stored under `key_hash` |

---

## Fan-out

| Method | Description |
|---|---|
| `ShmApi::fan_out(slots: &[u32], data: &[u8])` | Append the same record to every slot in `slots` in one call |

---

## Utilities

| Method / Type | Description |
|---|---|
| `ShmApi::try_allocate_page() -> u32` | Allocate a raw 4 KiB SHM page; returns the byte offset from SHM base |
| `ShmApi::append_log(msg: &str)` | Append a debug message to the shared log arena |
| `SeekFrom::Start(u32)` | Seek to an absolute byte offset |
| `SeekFrom::Current(i32)` | Seek relative to the current position |
| `SeekFrom::End(i32)` | Seek relative to the end (use negative values to seek backward) |
