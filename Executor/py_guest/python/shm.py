"""SHM module for Python workloads — WASI-compatible file I/O edition.

Implements the same page-chain protocol as the Rust guest, reading the
shared-memory file that the host has formatted via format_shared_memory().

Uses plain seek + read/write (buffering=0) instead of mmap so that the
module works inside python.wasm under WASI, which has no mmap(2) support.

Layout constants must stay in sync with common/src/lib.rs.

Public API
----------
  read_all_stream_records(slot)     -> list[(origin, data)]
  read_next_stream_record(slot)     -> (origin, data) | None
  reset_stream_cursor(slot)         -> None   # reset cursor for rdma_recv slots
  count_stream_records(slot)        -> int
  read_all_inputs()                 -> list[(origin, data)]
  read_all_inputs_from(io_slot)     -> list[(origin, data)]
  read_next_io_record(io_slot)      -> (origin, data) | None
  reset_io_cursor(io_slot)          -> None   # reset cursor for rdma_recv slots
  count_io_records(io_slot)         -> int
  append_stream_data(slot, data)    -> None
  write_output(data)                -> None
  write_output_str(s)               -> None
  write_io(io_slot, data)           -> None
  write_fanout(io_slots, data)      -> None
"""

import os
import struct

# ── Constants (must match common/src/lib.rs) ─────────────────────────────────

PAGE_SIZE          = 4096
STREAM_SLOT_COUNT  = 2048
IO_SLOT_COUNT      = 512
FREE_LIST_SHARDS   = 16
OUTPUT_IO_SLOT     = 1    # common::OUTPUT_IO_SLOT
INPUT_IO_SLOT      = 0    # common::INPUT_IO_SLOT

# Superblock field byte offsets (repr(C)).
# bump_allocator/global_cap/log_off/registry_lock/next_atomic/shared_map_base
# are all AtomicU32 (magic @ 0, bump @ 4, ...).  Slot arrays below are
# AtomicU64 (AtomicPageId) — 8-byte stride.  Layout is asserted from the
# Rust side in `common/src/lib.rs`; if those asserts fire, update these too.
#   magic u32 @ 0
#   bump  u32 @ 4
#   ... [u32 fields to 28]
#   (+4 pad to 8-align)
#   free_list_heads[16]  AtomicU64 @ 32,    size 128
#   writer_heads[2048]   AtomicU64 @ 160,   size 16384
#   writer_tails[2048]   AtomicU64 @ 16544, size 16384
#   io_heads[512]        AtomicU64 @ 32928, size 4096
#   io_tails[512]        AtomicU64 @ 37024, size 4096
#   barriers[64]         AtomicU32 @ 41120, size 256
_SB_BUMP         = 4
_SB_WRITER_HEADS = 160
_SB_WRITER_TAILS = 16544
_SB_IO_HEADS     = 32928
_SB_IO_TAILS     = 37024
_SLOT_STRIDE     = 8       # bytes per slot atomic (was 4 before widening to u64)

# Page layout (repr(C, align(4096))):
#   next_offset u64 @ 0   (PageId — widened from u32 for extended-pool support)
#   cursor      u32 @ 8
#   data[4084]      @ 12
_PAGE_NEXT_OFF    = 0
_PAGE_CURSOR_OFF  = 8
_PAGE_DATA_OFFSET = 12
_PAGE_DATA_SIZE   = 4084   # PAGE_SIZE - 12

# ── Module state ──────────────────────────────────────────────────────────────

_f = None   # raw binary file handle (buffering=0)


def _init():
    global _f
    if _f is not None:
        return
    path = os.environ.get("SHM_PATH", "")
    if not path:
        raise RuntimeError("SHM_PATH environment variable not set")
    # buffering=0 → raw unbuffered I/O; avoids stale-buffer issues when we
    # interleave reads and writes at arbitrary offsets.
    _f = open(path, "r+b", buffering=0)


# ── Low-level helpers ─────────────────────────────────────────────────────────

def _ru32(off: int) -> int:
    _f.seek(off)
    return struct.unpack("<I", _f.read(4))[0]


def _wu32(off: int, v: int) -> None:
    _f.seek(off)
    _f.write(struct.pack("<I", v))


def _ru64(off: int) -> int:
    _f.seek(off)
    return struct.unpack("<Q", _f.read(8))[0]


def _wu64(off: int, v: int) -> None:
    _f.seek(off)
    _f.write(struct.pack("<Q", v))


def _read_bytes(off: int, n: int) -> bytes:
    _f.seek(off)
    return _f.read(n)


def _write_bytes(off: int, data: bytes) -> None:
    _f.seek(off)
    _f.write(data)


# ── Page-chain reader ─────────────────────────────────────────────────────────

def _read_chain(head: int) -> list:
    """Walk a page chain starting at byte offset `head`.
    Returns list of (origin: int, payload: bytes) tuples.
    """
    records = []
    page_off = head
    cursor_in_page = 0

    def read_n(n):
        nonlocal page_off, cursor_in_page
        buf = bytearray()
        while len(buf) < n:
            if page_off == 0:
                return None
            written = _ru32(page_off + _PAGE_CURSOR_OFF)   # page.cursor (u32 @ 8)
            avail = written - cursor_in_page
            if avail == 0:
                page_off = _ru64(page_off + _PAGE_NEXT_OFF) # page.next_offset (u64 @ 0)
                cursor_in_page = 0
                continue
            take = min(avail, n - len(buf))
            start = page_off + _PAGE_DATA_OFFSET + cursor_in_page
            buf += _read_bytes(start, take)
            cursor_in_page += take
        return bytes(buf)

    while True:
        lb = read_n(4)
        if lb is None:
            break
        plen = struct.unpack("<I", lb)[0]
        if plen > 64 * 1024 * 1024:  # sanity: >64 MiB is corrupted
            break
        ob = read_n(4)
        if ob is None:
            break
        origin = struct.unpack("<I", ob)[0]
        payload = read_n(plen)
        if payload is None:
            break
        records.append((origin, payload))

    return records


# ── Page-chain counter ────────────────────────────────────────────────────────

def _count_chain(head: int) -> int:
    """Count records in a page chain without allocating payloads.

    Reads only the 4-byte length and 4-byte origin headers per record, then
    skips `plen` bytes of payload without reading them into Python memory.
    Saves one file-read syscall per payload byte relative to _read_chain.
    """
    # Use a list as a mutable container so nested helpers can update state.
    state = [head, 0]   # [page_off, cursor_in_page]

    def read4():
        """Read the next 4 bytes from the chain, advancing state."""
        buf = bytearray()
        while len(buf) < 4:
            if state[0] == 0:
                return None
            avail = _ru32(state[0] + _PAGE_CURSOR_OFF) - state[1]
            if avail == 0:
                nxt = _ru64(state[0] + _PAGE_NEXT_OFF)
                if nxt == 0:
                    return None
                state[0], state[1] = nxt, 0
                continue
            take = min(avail, 4 - len(buf))
            buf += _read_bytes(state[0] + _PAGE_DATA_OFFSET + state[1], take)
            state[1] += take
        return struct.unpack("<I", bytes(buf))[0]

    def skip(n):
        """Advance the chain position by `n` bytes without reading data."""
        rem = n
        while rem > 0:
            if state[0] == 0:
                return False
            avail = _ru32(state[0] + _PAGE_CURSOR_OFF) - state[1]
            if avail == 0:
                nxt = _ru64(state[0] + _PAGE_NEXT_OFF)
                if nxt == 0:
                    return False
                state[0], state[1] = nxt, 0
                continue
            take = min(avail, rem)
            state[1] += take
            rem -= take
        return True

    count = 0
    while True:
        plen = read4()
        if plen is None:
            break
        if not skip(4 + plen):   # skip origin (4 bytes) + payload (plen bytes)
            break
        count += 1
    return count


# ── Page allocator ────────────────────────────────────────────────────────────

def _alloc_page() -> int:
    """Bump-allocate one page; returns its byte offset in the SHM."""
    bump = _ru32(_SB_BUMP)
    _wu32(_SB_BUMP, bump + PAGE_SIZE)
    _wu64(bump + _PAGE_NEXT_OFF,   0)   # next_offset (u64) = 0
    _wu32(bump + _PAGE_CURSOR_OFF, 0)   # cursor (u32) = 0
    return bump


# ── Page-chain writer ─────────────────────────────────────────────────────────

def _append(head_off: int, tail_off: int, origin: int, payload: bytes) -> None:
    """Append [4-byte len][4-byte origin][payload] to the chain at (head_off, tail_off).

    `head_off` / `tail_off` point at AtomicPageId (u64) slot atomics inside the
    Superblock, so reads/writes are 8 bytes wide.
    """
    head = _ru64(head_off)
    tail = _ru64(tail_off)
    record = struct.pack("<II", len(payload), origin) + payload
    rem = memoryview(record)

    while rem:
        if tail == 0:
            new = _alloc_page()
            head = new
            tail = new
            _wu64(head_off, head)
            _wu64(tail_off, tail)

        cursor = _ru32(tail + _PAGE_CURSOR_OFF)
        space  = _PAGE_DATA_SIZE - cursor
        if space == 0:
            new = _alloc_page()
            _wu64(tail + _PAGE_NEXT_OFF, new)   # chain: current.next_offset (u64) = new
            tail = new
            _wu64(tail_off, tail)
            continue

        take = min(space, len(rem))
        dst = tail + _PAGE_DATA_OFFSET + cursor
        _write_bytes(dst, bytes(rem[:take]))
        _wu32(tail + _PAGE_CURSOR_OFF, cursor + take)
        rem = rem[take:]

    _wu64(head_off, head)
    _wu64(tail_off, tail)


# ── Public API (mirrors shm_module.c / Rust ShmApi) ──────────────────────────

def read_all_stream_records(slot: int) -> list:
    _init()
    head = _ru64(_SB_WRITER_HEADS + slot * _SLOT_STRIDE)
    return _read_chain(head)


def read_all_inputs() -> list:
    return read_all_inputs_from(INPUT_IO_SLOT)


def read_all_inputs_from(io_slot: int) -> list:
    _init()
    head = _ru64(_SB_IO_HEADS + io_slot * _SLOT_STRIDE)
    return _read_chain(head)


def append_stream_data(slot: int, data: bytes) -> None:
    _init()
    _append(
        _SB_WRITER_HEADS + slot * _SLOT_STRIDE,
        _SB_WRITER_TAILS + slot * _SLOT_STRIDE,
        slot, data,
    )


def write_output(data: bytes) -> None:
    _init()
    _append(
        _SB_IO_HEADS + OUTPUT_IO_SLOT * _SLOT_STRIDE,
        _SB_IO_TAILS + OUTPUT_IO_SLOT * _SLOT_STRIDE,
        OUTPUT_IO_SLOT, data,
    )


def write_output_str(s: str) -> None:
    write_output(s.encode("utf-8"))


def write_io(io_slot: int, data: bytes) -> None:
    """Append `data` to an explicit I/O slot (pipeline-mode counterpart of write_output)."""
    _init()
    _append(
        _SB_IO_HEADS + io_slot * _SLOT_STRIDE,
        _SB_IO_TAILS + io_slot * _SLOT_STRIDE,
        io_slot, data,
    )


def write_fanout(io_slots, data: bytes) -> None:
    """Append `data` to every I/O slot in `io_slots` (broadcast / fan-out write)."""
    _init()
    for slot in io_slots:
        _append(
            _SB_IO_HEADS + slot * _SLOT_STRIDE,
            _SB_IO_TAILS + slot * _SLOT_STRIDE,
            slot, data,
        )


def count_stream_records(slot: int) -> int:
    """Count records in a stream slot without allocating payloads.

    Faster than len(read_all_stream_records(slot)) for large payloads because
    it only reads the 8-byte per-record header and skips the payload bytes.
    """
    _init()
    head = _ru64(_SB_WRITER_HEADS + slot * _SLOT_STRIDE)
    return _count_chain(head)


def count_io_records(io_slot: int) -> int:
    """Count records in an I/O slot without allocating payloads."""
    _init()
    head = _ru64(_SB_IO_HEADS + io_slot * _SLOT_STRIDE)
    return _count_chain(head)


# ── Per-slot read cursors (module-level, in-process state) ────────────────────
# Used by read_next_*_record so a persistent stage worker can consume one
# record per round without re-reading previously processed records.
# Only valid inside PyPipeline workers where the process stays alive across rounds.

_io_cursors: dict     = {}   # io_slot     -> next record index
_stream_cursors: dict = {}   # stream_slot -> next record index


def read_next_io_record(io_slot: int):
    """Return the next unread (origin, payload) tuple from `io_slot`, or None.

    Maintains a per-slot cursor in module-level state.  Works correctly inside
    a PyPipeline stage worker because the process is kept alive across rounds,
    so the cursor advances with each call.

    NOTE: do NOT use this for slots that are refreshed via `rdma_recv` in a
    PyPipeline.  The host frees and repopulates those slots each round, so the
    cursor becomes stale.  Use `read_all_inputs_from(slot)` instead, or call
    `reset_io_cursor(slot)` at the start of each round before reading.
    """
    _init()
    head = _ru64(_SB_IO_HEADS + io_slot * _SLOT_STRIDE)
    records = _read_chain(head)
    idx = _io_cursors.get(io_slot, 0)
    if idx < len(records):
        _io_cursors[io_slot] = idx + 1
        return records[idx]
    return None


def read_next_stream_record(slot: int):
    """Return the next unread (origin, payload) tuple from stream `slot`, or None.

    Mirrors read_next_io_record for stream slots.  Maintains a per-slot cursor
    in module-level state so persistent PyPipeline stage workers advance through
    the chain one record per round without re-reading already-processed records.

    NOTE: do NOT use this for slots that are refreshed via `rdma_recv` in a
    PyPipeline.  The host frees and repopulates those slots each round, so the
    cursor becomes stale.  Use `read_all_stream_records(slot)` instead, or call
    `reset_stream_cursor(slot)` at the start of each round before reading.
    """
    _init()
    head = _ru64(_SB_WRITER_HEADS + slot * _SLOT_STRIDE)
    records = _read_chain(head)
    idx = _stream_cursors.get(slot, 0)
    if idx < len(records):
        _stream_cursors[slot] = idx + 1
        return records[idx]
    return None


def reset_io_cursor(io_slot: int) -> None:
    """Reset the in-process read cursor for `io_slot` back to the beginning.

    Call this at the start of each round when reading a slot that is refreshed
    by the host on every pipeline round (e.g. a slot populated via `rdma_recv`
    in a PyPipeline).  Without a reset, `read_next_io_record` would skip all
    records because the cursor still points past the end of the previous round's
    (now-freed and replaced) chain.

    Not needed when using `read_all_inputs_from`, which always re-reads from
    the current chain head with no cursor state.
    """
    _io_cursors.pop(io_slot, None)


def reset_stream_cursor(slot: int) -> None:
    """Reset the in-process read cursor for stream `slot` back to the beginning.

    Mirrors `reset_io_cursor` for stream slots.  Call this at the start of each
    round in a PyPipeline stage worker when the input slot is replenished by the
    host via `rdma_recv` (free + replace pattern) rather than accumulated.
    """
    _stream_cursors.pop(slot, None)
