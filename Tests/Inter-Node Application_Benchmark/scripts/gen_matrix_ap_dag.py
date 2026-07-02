#!/usr/bin/env python3
# gen_matrix_ap_dag.py — emit the SUMMA matrix-multiply auto-placement SymbolicDag
# (workload #4), the cross-node analogue of gen_sgd_ap_dag.py.
#
# WHY THIS EXISTS
# ---------------
# The inter-node application benchmark drives our platform through
# gen_variants.py → partition → submit. SUMMA's block workers form a homogeneous
# r·c-wide fan (block_i_j, each computing C_ij = A_i*·B_*j), gathered into a
# checksum — the SAME fan→aggregate shape as ml_training, so it reuses
# gen_variants.py's generic fan transform (per-policy placement + per-machine
# local-combine gather, one transfer/peer, deadlock-safe). This generator emits the
# grid variant; gen_variants imports build_dag so `--fanout W --matrix-n N`
# regenerates the grid at W = r·c blocks over an N×N product.
#
# WHY THE GATE STILL HOLDS (Matrix/README.md)
# -------------------------------------------
# A,B have small integer entries, so every C entry is an exact integer < 2^53; the
# f64 checksum (Σ of all entries) is therefore EXACT and ORDER-INDEPENDENT —
# identical regardless of how the r·c blocks are placed across nodes. It matches
# gen_matrix.py's numpy int64 sum bit-for-bit.
#
# STRUCTURE (mirrors ml_training_auto_placement.json so gen_variants.py works UNCHANGED)
# -------------------------------------------------------------------------------
#   load_a (all, replicate) Input slot 0  (binary)
#   load_b (all, replicate) Input slot 2  (binary)
#   tile   (all) mat_tile wasm_arg=rcn    → A row-panels 10..10+r, B col-panels 40..40+c
#   block_i_j ← FAN (all) mat_block wasm_arg=(i<<26)|(j<<21)|rcn  → C block 100+i*c+j
#                   gen_variants strips placement and places these per policy; each
#                   reads its same-machine A_i and B_j panels (tile is placement:"all",
#                   so every node holds every panel — only the C blocks cross nodes).
#   aggregate (all) Aggregate upstream=[100..100+r*c] downstream=600  ← AGGREGATOR
#                   gen_variants rewrites this into the per-machine local-combine +
#                   global gather on node 0 (1 transfer/peer, deadlock-safe).
#   aggregate_global  Aggregate {}                                   (auto → node 0)
#   assemble          mat_assemble  (NO arg → partitioner auto-wires it to
#                     aggregate_global's downstream slot, like sgd_update)  (auto → node 0)
#   save              Output → TestOutput/matrix_ap_result.txt
#
# Slots match Tests/Intra-Node Application_Benchmark/Matrix/gen_dag.py and the guest
# (Executor/guest/src/workloads/matrix.rs): A=slot 0, B=slot 2, A panels 10.., B
# panels 40.., C blocks 100.., aggregate 600.
#
# Usage:  ./gen_matrix_ap_dag.py [WORKERS] [N] [A_PATH] [B_PATH] \
#             > DAGs/symbolic_dag/matrix_auto_placement.json
import json
import sys

# Slot bases — must match Executor/guest/src/workloads/matrix.rs.
A_INPUT, B_INPUT = 0, 2
A_BASE, B_BASE, C_BASE = 10, 40, 100
AGG = 600


def grid(workers):
    """Balanced r×c factorization (r ≤ c, r·c == workers): 1→1x1, 2→1x2, 4→2x2,
    8→2x4, 16→4x4. Matches Matrix/gen_dag.py."""
    r = 1
    k = 1
    while k * k <= workers:
        if workers % k == 0:
            r = k
        k += 1
    return r, workers // r


def build_dag(workers, n, a_path=None, b_path=None):
    """Return the SUMMA auto-placement SymbolicDag dict for a `workers`-block grid
    over an N×N product. Imported by gen_variants.py so `--fanout W --matrix-n N`
    regenerates the grid at width W = r·c (the matrix analogue of ml_training's
    gen_sgd_ap_dag)."""
    R, C = grid(workers)
    if R * C != workers:
        sys.exit(f"[gen_matrix] {workers} not factorable into a balanced grid")
    if n % R != 0 or n % C != 0:
        sys.exit(f"[gen_matrix] N={n} not divisible by grid {R}x{C}")
    if not (n < (1 << 13) and R < 16 and C < 16):
        sys.exit("[gen_matrix] params exceed the packed-arg layout (N<8192, r,c<16)")
    a_path = a_path or f"TestData/matrix/A_{n}.bin"
    b_path = b_path or f"TestData/matrix/B_{n}.bin"

    # Packed (r, c, N) triple shared by mat_tile and mat_block:
    #   N: bits 0..12, c: 13..16 (4b), r: 17..20 (4b)  (+ j: 21..25, i: 26..30 for mat_block)
    # r,c are 4 bits (was 3) so grids up to 15×15 fit — needed for the 8×8 grid (64
    # blocks). Keep in sync with Executor/guest/src/workloads/matrix.rs::unpack_rcn.
    rcn = (R << 17) | (C << 13) | n

    # IMPORTANT — packed args go in `wasm_arg`, NOT `arg`.
    # rcn and the per-block (i<<24)|(j<<19)|rcn are HUGE integers (r<<16 ≥ 65536).
    # The partitioner's recv-slot allocator (slot.rs::collect_slots → splitter.rs
    # next_slot) scans every integer in the node kinds for the max slot in use; a
    # packed value in `arg` poisons that scan and pushes RemoteRecv slots out of
    # range, deadlocking the gather. collect_slots SKIPS `wasm_arg`, so the packed
    # guest parameter lives there; `arg` stays a small REAL slot (the C-block output
    # slot) the partitioner can safely account for.
    nodes = [
        {"id": "load_a", "placement": "all", "deps": [],
         "kind": {"Input": {"path": a_path, "slot": A_INPUT,
                            "binary": True, "prefetch": True, "replicate": True}}},
        {"id": "load_b", "placement": "all", "deps": [],
         "kind": {"Input": {"path": b_path, "slot": B_INPUT,
                            "binary": True, "prefetch": True, "replicate": True}}},
        {"id": "tile", "placement": "all", "deps": ["load_a", "load_b"],
         "kind": {"Func": {"func": "mat_tile", "arg": A_BASE, "wasm_arg": rcn}}},
    ]

    # The FAN: one worker per C block, emitted in i-major/j-minor order so the
    # collected fan order lines up with the aggregator's upstream slot list (the
    # generic fan transform zips them positionally). placement:"all" is a
    # placeholder — gen_variants strips it and assigns an explicit node_id per policy.
    c_slots = []
    for i in range(R):
        for j in range(C):
            out_slot = C_BASE + i * C + j
            c_slots.append(out_slot)
            nodes.append({"id": f"block_{i}_{j}", "placement": "all", "deps": ["tile"],
                          "kind": {"Func": {"func": "mat_block", "arg": out_slot,
                                            "wasm_arg": (i << 26) | (j << 21) | rcn}}})

    # AGGREGATOR: upstream declares each block's output slot (C_BASE+i*c+j) so
    # gen_variants/the partitioner know where the guest writes; downstream is the
    # gather slot the assembler reads.
    nodes.append({"id": "aggregate", "placement": "all",
                  "deps": [f"block_{i}_{j}" for i in range(R) for j in range(C)],
                  "kind": {"Aggregate": {"upstream": c_slots, "downstream": AGG}}})

    nodes += [
        {"id": "aggregate_global", "deps": ["aggregate"],
         "kind": {"Aggregate": {}}},
        # Assembler: fold the global checksum over every aggregated C block and write
        # it out (the gate). NO arg — the partitioner auto-wires it to
        # aggregate_global's downstream slot (same as sgd_update).
        {"id": "assemble", "deps": ["aggregate_global"],
         "kind": {"Func": {"func": "mat_assemble"}}},
        {"id": "save", "deps": ["assemble"],
         "kind": {"Output": {"path": "TestOutput/matrix_ap_result.txt"}}},
    ]

    return {
        "shm_path_prefix": "/dev/shm/rdma_matrix_ap",
        "log_level": "info",
        "total_nodes": 3,
        "transfer": True,
        "placement_policy": "balanced",
        "nodes": nodes,
    }


if __name__ == "__main__":
    workers = int(sys.argv[1]) if len(sys.argv) > 1 else 4
    n = int(sys.argv[2]) if len(sys.argv) > 2 else 512
    a = sys.argv[3] if len(sys.argv) > 3 else None
    b = sys.argv[4] if len(sys.argv) > 4 else None
    print(json.dumps(build_dag(workers, n, a, b), indent=2))
