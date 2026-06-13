#!/usr/bin/env python3
# gen_dag.py — emit a native single-node SUMMA-style matrix-multiply DAG with an
# r×c block grid (r·c workers).
#
#   load_A (slot 0) ┐
#   load_B (slot 2) ┴► mat_tile(r,c) ─► mat_block_(0,0) ─┐
#                                       ├─ mat_block_(i,j) ├─► aggregate(→600) ─► mat_assemble ─► save
#                                       └─ mat_block_(r-1,c-1) ┘   (r·c workers)
#
# A is tiled into r contiguous block-rows, B into c gathered block-cols; each
# block worker (i,j) computes C_ij = A_i*·B_*j. Panels move through the SHM page
# chain with zero serialization (the headline this benchmark measures). The C
# blocks are aggregated and folded into a checksum (the correctness gate).
#
# Workers → balanced r×c grid: 1→1x1, 2→1x2, 4→2x2, 8→2x4, 16→4x4.
#
# Usage: gen_dag.py WORKERS N A_PATH B_PATH OUT_PATH SHM_PATH
# Env: MAT_WASM overrides the guest module path (e.g. a precompiled .cwasm for AOT).
import json
import os
import sys

WORKERS = int(sys.argv[1])
N = int(sys.argv[2])
a_path = sys.argv[3]
b_path = sys.argv[4]
out_path = sys.argv[5]
shm_path = sys.argv[6]
WASM = os.environ.get('MAT_WASM',
                      'Executor/target/wasm32-unknown-unknown/release/guest.wasm')

# Slot bases — must match Executor/guest/src/workloads/matrix.rs.
A_INPUT, B_INPUT = 0, 2
A_BASE, B_BASE, C_BASE = 10, 40, 100
AGG = 600


def grid(workers):
    """Balanced r×c factorization (r ≤ c, r·c == workers)."""
    r = 1
    k = 1
    while k * k <= workers:
        if workers % k == 0:
            r = k
        k += 1
    return r, workers // r


R, C = grid(WORKERS)
assert R * C == WORKERS, f"{WORKERS} not factorable"
assert N % R == 0 and N % C == 0, f"N={N} not divisible by grid {R}x{C}"
assert N < (1 << 13) and R < 8 and C < 8, "params exceed the packed-arg layout"

# The Func DAG node passes a single u32 — pack everything (matches matrix.rs):
#   N: bits 0..12, c: 13..15, r: 16..18, j: 19..23, i: 24..28
rcn = (R << 16) | (C << 13) | N

nodes = [
    {"id": "load_a", "deps": [],
     "kind": {"Input": {"path": a_path, "slot": A_INPUT, "binary": True, "prefetch": True}}},
    {"id": "load_b", "deps": [],
     "kind": {"Input": {"path": b_path, "slot": B_INPUT, "binary": True, "prefetch": True}}},
    {"id": "tile", "deps": ["load_a", "load_b"],
     "kind": {"Func": {"func": "mat_tile", "arg": rcn}}},
]

block_ids = []
c_slots = []
for i in range(R):
    for j in range(C):
        bid = f"block_{i}_{j}"
        block_ids.append(bid)
        c_slots.append(C_BASE + i * C + j)
        nodes.append({"id": bid, "deps": ["tile"],
                      "kind": {"Func": {"func": "mat_block",
                                        "arg": (i << 24) | (j << 19) | rcn}}})

nodes += [
    # NOTE: we deliberately do NOT FreeSlots the A/B panel slots here. FreeSlots
    # on these multi-MB binary panel records corrupts the SHM page allocator (the
    # next allocation — the OUTPUT slot — gets a bad page, so the result is lost).
    # word_count's free_input is safe because its freed records are tiny; large
    # binary records trip it. Leaving the panels allocated raises peak SHM by ≈1×
    # input but keeps results correct (a documented framework limitation, not a
    # benchmark concern). See Matrix/README.md.
    {"id": "aggregate", "deps": block_ids,
     "kind": {"Aggregate": {"upstream": c_slots, "downstream": AGG}}},
    {"id": "assemble", "deps": ["aggregate"],
     "kind": {"Func": {"func": "mat_assemble", "arg": AGG}}},
    {"id": "save", "deps": ["assemble"],
     "kind": {"Output": {"path": out_path}}},
]

print(json.dumps({
    "shm_path": shm_path,
    "wasm_path": WASM,
    "log_level": "warn",
    "nodes": nodes,
}, indent=2))
