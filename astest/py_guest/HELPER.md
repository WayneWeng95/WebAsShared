# Python Guest API Reference

All functions are module-level in `shm.py`.  Import with:

```python
import shm
```

The module opens the SHM file on first use via the `SHM_PATH` environment
variable (set automatically by the host runner).

---

## Input

Host-loaded data read by workloads.  The default input slot is `INPUT_IO_SLOT` (0).

| Function | Returns | Description |
|---|---|---|
| `shm.read_all_inputs()` | `list[(int, bytes)]` | All records from the default input slot |
| `shm.read_all_inputs_from(io_slot: int)` | `list[(int, bytes)]` | All records from an explicit I/O slot |
| `shm.read_next_io_record(io_slot: int)` | `(int, bytes) \| None` | Next unconsumed record from an I/O slot; advances a per-slot in-process cursor |

Each returned tuple is `(origin_id, payload)`.

---

## Output

Results written by workloads and flushed to disk by the host `Output` node.
The default output slot is `OUTPUT_IO_SLOT` (1).

| Function | Description |
|---|---|
| `shm.write_output(data: bytes)` | Append bytes to the default output slot |
| `shm.write_output_str(s: str)` | Append a UTF-8 string to the default output slot |
| `shm.write_io(io_slot: int, data: bytes)` | Append bytes to an explicit I/O slot |

---

## Stream slots

Append-only record channels used to pass data between pipeline stages.

| Function | Returns | Description |
|---|---|---|
| `shm.append_stream_data(slot: int, data: bytes)` | `None` | Append a record to `slot` |
| `shm.read_all_stream_records(slot: int)` | `list[(int, bytes)]` | Read all records from `slot` |
| `shm.read_next_stream_record(slot: int)` | `(int, bytes) \| None` | Next unconsumed record from `slot`; advances a per-slot in-process cursor |

---

## Cursor management

`read_next_*_record` maintain per-slot cursors in module-level dicts.
These are **in-process only** — they work inside persistent `PyPipeline` stage
workers that stay alive across rounds.

| Function | Description |
|---|---|
| `shm.reset_stream_cursor(slot: int)` | Reset the read cursor for a stream slot to the beginning |
| `shm.reset_io_cursor(io_slot: int)` | Reset the read cursor for an I/O slot to the beginning |

> **When to call reset**: if the host replenishes a slot via `rdma_recv` each
> pipeline round (free + replace), the old cursor points past the end of the
> new chain.  Call `reset_*_cursor` at the start of each round before reading,
> or use `read_all_*` which always reads from the chain head with no cursor state.

---

## Fan-out

| Function | Description |
|---|---|
| `shm.write_fanout(io_slots: list[int], data: bytes)` | Append the same record to every slot in `io_slots` |

---

## Record counting

Faster than `len(read_all_*(...))` for large payloads — skips payload bytes,
reads only the 8-byte per-record header.

| Function | Returns | Description |
|---|---|---|
| `shm.count_stream_records(slot: int)` | `int` | Number of records in a stream slot |
| `shm.count_io_records(io_slot: int)` | `int` | Number of records in an I/O slot |

---

## Constants

| Name | Value | Description |
|---|---|---|
| `shm.INPUT_IO_SLOT` | `0` | Default slot for host-loaded input |
| `shm.OUTPUT_IO_SLOT` | `1` | Default slot for workload output |
| `shm.PAGE_SIZE` | `4096` | SHM page size in bytes |
| `shm.STREAM_SLOT_COUNT` | `2048` | Total number of stream slots |
| `shm.IO_SLOT_COUNT` | `512` | Total number of I/O slots |

---

## Notes

- **`read_all_*` vs `read_next_*`**: use `read_all_*` when you need every record
  at once (e.g. reduce step); use `read_next_*` inside a persistent pipeline worker
  that processes one new record per round.
- **Cursor scope**: cursors are process-local dicts — they are not stored in SHM
  and are not visible to other workers or to the Rust guest.  This differs from
  the Rust `read_next_*_record` which stores cursors as named SHM atomics.
- **WASI compatibility**: `shm.py` uses plain `seek`/`read`/`write` with no
  `mmap`, so it works inside `python.wasm` under WASI as well as native Python 3.
