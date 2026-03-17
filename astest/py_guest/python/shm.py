"""SHM module for Python workloads — WASI-compatible file I/O edition.

Implements the same page-chain protocol as the Rust guest, reading the
shared-memory file that the host has formatted via format_shared_memory().

Uses plain seek + read/write (buffering=0) instead of mmap so that the
module works inside python.wasm under WASI, which has no mmap(2) support.

Layout constants must stay in sync with common/src/lib.rs.
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

# Superblock field byte offsets (all fields are u32 / AtomicU32, repr(C))
#   magic(0) bump(4) global_cap(8) log_off(12) reg_lock(16)
#   next_atomic(20) shared_map(24)
#   free_list_heads[16]  → offset 28, size 64
#   writer_heads[2048]   → offset 92, size 8192
#   writer_tails[2048]   → offset 8284, size 8192
#   io_heads[512]        → offset 16476, size 2048
#   io_tails[512]        → offset 18524, size 2048
_SB_BUMP         = 4
_SB_WRITER_HEADS = 92
_SB_WRITER_TAILS = 8284
_SB_IO_HEADS     = 16476
_SB_IO_TAILS     = 18524

# Page layout (repr(C, align(4096))):
#   next_offset u32 @ 0
#   cursor      u32 @ 4
#   data[4088]      @ 8
_PAGE_DATA_OFFSET = 8
_PAGE_DATA_SIZE   = 4088   # PAGE_SIZE - 8

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
            written = _ru32(page_off + 4)          # page.cursor
            avail = written - cursor_in_page
            if avail == 0:
                page_off = _ru32(page_off)         # page.next_offset
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
        ob = read_n(4)
        if ob is None:
            break
        origin = struct.unpack("<I", ob)[0]
        payload = read_n(plen)
        if payload is None:
            break
        records.append((origin, payload))

    return records


# ── Page allocator ────────────────────────────────────────────────────────────

def _alloc_page() -> int:
    """Bump-allocate one page; returns its byte offset in the SHM."""
    bump = _ru32(_SB_BUMP)
    _wu32(_SB_BUMP, bump + PAGE_SIZE)
    _wu32(bump,     0)   # next_offset = 0
    _wu32(bump + 4, 0)   # cursor = 0
    return bump


# ── Page-chain writer ─────────────────────────────────────────────────────────

def _append(head_off: int, tail_off: int, origin: int, payload: bytes) -> None:
    """Append [4-byte len][4-byte origin][payload] to the chain at (head_off, tail_off)."""
    head = _ru32(head_off)
    tail = _ru32(tail_off)
    record = struct.pack("<II", len(payload), origin) + payload
    rem = memoryview(record)

    while rem:
        if tail == 0:
            new = _alloc_page()
            head = new
            tail = new
            _wu32(head_off, head)
            _wu32(tail_off, tail)

        cursor = _ru32(tail + 4)
        space  = _PAGE_DATA_SIZE - cursor
        if space == 0:
            new = _alloc_page()
            _wu32(tail, new)          # chain: current.next_offset = new
            tail = new
            _wu32(tail_off, tail)
            continue

        take = min(space, len(rem))
        dst = tail + _PAGE_DATA_OFFSET + cursor
        _write_bytes(dst, bytes(rem[:take]))
        _wu32(tail + 4, cursor + take)
        rem = rem[take:]

    _wu32(head_off, head)
    _wu32(tail_off, tail)


# ── Public API (mirrors shm_module.c / Rust ShmApi) ──────────────────────────

def read_all_stream_records(slot: int) -> list:
    _init()
    head = _ru32(_SB_WRITER_HEADS + slot * 4)
    return _read_chain(head)


def read_all_inputs() -> list:
    return read_all_inputs_from(INPUT_IO_SLOT)


def read_all_inputs_from(io_slot: int) -> list:
    _init()
    head = _ru32(_SB_IO_HEADS + io_slot * 4)
    return _read_chain(head)


def append_stream_data(slot: int, data: bytes) -> None:
    _init()
    _append(
        _SB_WRITER_HEADS + slot * 4,
        _SB_WRITER_TAILS + slot * 4,
        slot, data,
    )


def write_output(data: bytes) -> None:
    _init()
    _append(
        _SB_IO_HEADS + OUTPUT_IO_SLOT * 4,
        _SB_IO_TAILS + OUTPUT_IO_SLOT * 4,
        OUTPUT_IO_SLOT, data,
    )


def write_output_str(s: str) -> None:
    write_output(s.encode("utf-8"))


def write_io(io_slot: int, data: bytes) -> None:
    """Append `data` to an explicit I/O slot (pipeline-mode counterpart of write_output)."""
    _init()
    _append(
        _SB_IO_HEADS + io_slot * 4,
        _SB_IO_TAILS + io_slot * 4,
        io_slot, data,
    )


# ── Per-slot read cursors (module-level, in-process state) ────────────────────
# Used by read_next_io_record so a persistent stage worker can consume one
# record per round without re-reading previously processed records.
# Only valid inside PyPipeline workers where the process stays alive across rounds.

_io_cursors: dict = {}   # io_slot -> next record index


def read_next_io_record(io_slot: int):
    """Return the next unread (origin, payload) tuple from `io_slot`, or None.

    Maintains a per-slot cursor in module-level state.  Works correctly inside
    a PyPipeline stage worker because the process is kept alive across rounds,
    so the cursor advances with each call.
    """
    _init()
    head = _ru32(_SB_IO_HEADS + io_slot * 4)
    records = _read_chain(head)
    idx = _io_cursors.get(io_slot, 0)
    if idx < len(records):
        _io_cursors[io_slot] = idx + 1
        return records[idx]
    return None
