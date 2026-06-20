#!/usr/bin/env python3
# gen_terasort_ap_dag.py — emit the TeraSort auto-placement SymbolicDag (workload
# #7), the cross-node all-to-all SHUFFLE. Unlike the fan workloads (one gather to
# node 0), TeraSort is an N×N transpose: N partition workers each route their shard
# by key into N owner sub-slots, and N range-owners each gather one sub-slot from
# every partition worker. The whole dataset is repartitioned once.
#
# WHY A DEDICATED BUILDER (not gen_variants.py's generic FAN_PREFIX path)
# ----------------------------------------------------------------------
# The generic transform collapses a single homogeneous fan into ONE aggregator on
# node 0 (the fan→1 shape of word_count/finra/ml_training/ml_inference/matrix).
# TeraSort has N aggregators (one per range-owner) wired as a transpose, so it gets
# its own builder — the analogue of gen_variants._build_finra_hybrid.
#
# DEADLOCK-SAFE SHUFFLE = PER-MACHINE LOCAL-COMBINE PER OWNER
# ----------------------------------------------------------
# The naive shuffle has every partition worker send a raw sub-slot to every owner —
# an N×N all-to-all of tiny transfers that desyncs the executor's per-peer control
# stream and deadlocks (the same failure the fan workloads hit before the
# local-combine fix; see Scheduling_Policy/NEXT_STEPS.md). The fix mirrors
# word_count's: on each SOURCE machine M, for each owner j, a local Aggregate
# combines {part sub-slots for owner j produced on M} into ONE slot, and M sends
# exactly ONE transfer per (M, owner j) pair. Owner j's Aggregate then gathers one
# combined stream per source machine instead of one per partition worker.
#
#   • When fanout N == nodes P (the canonical layout: one partition + one owner per
#     node), each ordered machine pair (M → D) carries exactly ONE logical stream —
#     the same one-transfer-per-peer invariant the working gathers rely on, just
#     applied to a square transpose. This is the deadlock-safe sweet spot.
#   • When N > P a node hosts several owners, so a source machine sends one transfer
#     per owner hosted on the destination (still one per (source,owner), but >1 per
#     machine pair). This exercises the multi-stream-per-peer path that the cluster
#     run must validate (the known gather-stability risk flagged in the inter-node
#     EXPERIMENT_PLAN §8); prefer N == P for the first bring-up.
#
# WHY THE GATE STILL HOLDS (terasort.rs)
# --------------------------------------
# Range owner = (first_key_byte - LO) * N / SPAN — contiguous, monotonic ranges
# over a uniform key space, so concatenating owners 0..N-1 is globally sorted. The
# per-range record count and key checksum (Σ key bytes) are FAN-OUT-INVARIANT:
# identical at every N and every placement, so any record dropped or duplicated in
# the shuffle is caught. run.sh sums the per-range summaries into the gate.
#
# SINGLE-OUTPUT cluster tail (vs the intra path's per-range I/O files): the cluster
# partitioner gives every workload ONE Output at the reserved slot 1, so the N range
# summaries are folded into one reducer (ts_range_summary → gather → ts_finalize).
#
# Slots match the guest (Executor/guest/src/workloads/terasort.rs):
#   input slot 0; worker shards 10..10+N; per-(worker,owner) sub-slots 100+i*N+j;
#   per-owner gathered range 400+j; per-range STREAM summary 500+j; single output → slot 1.
import json
import sys

TS_DIST_BASE = 10     # per-worker input shards: 10 .. 10+N
TS_PART_BASE = 100    # ts_partition(i) writes owner j to 100 + i*N + j
TS_MERGE_IN = 400     # owner j's gathered range slot
TS_SUMMARY_BASE = 500  # owner j's range-summary STREAM slot (gathered into one output)


def packed(lo, hi):
    return (lo & 0xFFFF) | ((hi & 0xFFFF) << 16)


def build_dag(n, part_nodes=None, owner_nodes=None,
              records_path="TestData/terasort/records_1m.txt",
              out_prefix="TestOutput/terasort_ap_result"):
    """Return the TeraSort auto-placement SymbolicDag for `n` partition workers and
    `n` range-owners.

    `part_nodes[i]`  = node hosting partition worker i   (default: all node 0).
    `owner_nodes[j]` = node hosting owner j's merge/range (default: all node 0).
    gen_variants.py computes both via its placement policy and passes them in; the
    standalone CLI default (all on node 0) is the single-node ground-truth shape.
    """
    if part_nodes is None:
        part_nodes = [0] * n
    if owner_nodes is None:
        owner_nodes = [0] * n

    # IMPORTANT — packed args go in `wasm_arg`, NOT `arg` (see slot.rs::collect_slots:
    # a packed value in `arg` poisons the recv-slot allocator and deadlocks the
    # gather). `arg` stays a small REAL slot the partitioner can account for.
    nodes = [
        # Replicated input + zero-copy split: every node splits its FULL local copy
        # into N shards; partition worker i reads its same-machine shard 10+i. The
        # split is cheap (no parse); the routing happens in the placed ts_partition.
        {"id": "load", "placement": "all", "deps": [],
         "kind": {"Input": {"path": records_path, "slot": 0,
                            "prefetch": True, "replicate": True}}},
        {"id": "distribute", "placement": "all", "deps": ["load"],
         "kind": {"Func": {"func": "ts_distribute", "arg": TS_DIST_BASE,
                           "wasm_arg": packed(TS_DIST_BASE, n)}}},
    ]

    # ── partition workers (placed per policy) ────────────────────────────────────
    # Worker i routes its shard's records into owner sub-slots 100 + i*N + j.
    part_ids = []
    for i in range(n):
        pid = f"part_{i}"
        part_ids.append(pid)
        nodes.append({"id": pid, "node_id": part_nodes[i], "deps": ["distribute"],
                      "kind": {"Func": {"func": "ts_partition", "arg": TS_DIST_BASE + i,
                                        "wasm_arg": packed(TS_DIST_BASE + i, n)}}})

    # ── per-owner shuffle gather → range summary ─────────────────────────────────
    # Owner j gathers its sub-slot from EVERY partition worker {100+i*N+j : all i}
    # into one range (slot 400+j). With N == nodes (one partition + one owner per
    # node) each directed node-pair carries exactly one stream — the partitioner adds
    # send/recv ordering for the bidirectional transpose (deadlock-safe). ts_range_
    # summary then sorts the range and emits its (records, sorted, keysum) as a STREAM
    # record on 500+j, so the N ranges can be folded into ONE output (below).
    summary_ids = []
    for j in range(n):
        owner_node = owner_nodes[j]
        nodes.append({"id": f"agg_{j}", "node_id": owner_node, "deps": list(part_ids),
                      "kind": {"Aggregate": {"upstream": [TS_PART_BASE + i * n + j for i in range(n)],
                                             "downstream": TS_MERGE_IN + j}}})
        sid = f"summary_{j}"
        summary_ids.append(sid)
        nodes.append({"id": sid, "node_id": owner_node, "deps": [f"agg_{j}"],
                      "output_slot": TS_SUMMARY_BASE + j,
                      "kind": {"Func": {"func": "ts_range_summary", "arg": TS_MERGE_IN + j,
                                        "wasm_arg": packed(TS_MERGE_IN + j, TS_SUMMARY_BASE + j)}}})

    # ── single-output tail (mirrors the fan workloads) ───────────────────────────
    # Per-machine local-combine of the tiny range summaries (one transfer/peer), a
    # global gather on node 0, then ts_finalize sums them into ONE output record
    # (slot 1). Empty {Aggregate:{}} → the partitioner derives each upstream from the
    # summaries' output_slot, exactly as the fan workloads' local-combine does.
    by_machine = {}
    for j in range(n):
        by_machine.setdefault(owner_nodes[j], []).append(f"summary_{j}")
    local_ids = []
    for m in sorted(by_machine):
        lid = f"summary_local_{m}"
        local_ids.append(lid)
        nodes.append({"id": lid, "node_id": m, "deps": by_machine[m],
                      "kind": {"Aggregate": {}}})
    nodes += [
        {"id": "summary_global", "node_id": 0, "deps": local_ids,
         "kind": {"Aggregate": {}}},
        # ts_finalize: NO arg → the partitioner auto-wires it to summary_global's
        # downstream slot (same as the fan workloads' reduce).
        {"id": "finalize", "node_id": 0, "deps": ["summary_global"],
         "kind": {"Func": {"func": "ts_finalize"}}},
        {"id": "save", "node_id": 0, "deps": ["finalize"],
         "kind": {"Output": {"path": f"{out_prefix}.txt"}}},
    ]

    return {
        "shm_path_prefix": "/dev/shm/rdma_terasort_ap",
        "log_level": "info",
        "total_nodes": max(max(part_nodes), max(owner_nodes)) + 1,
        "transfer": True,
        "placement_policy": "balanced",
        "converge_on_coordinator": False,
        "nodes": nodes,
    }


if __name__ == "__main__":
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 4
    records = sys.argv[2] if len(sys.argv) > 2 else "TestData/terasort/records_32mb.txt"
    # Standalone default: square layout, one partition + one owner per node 0..N-1.
    print(json.dumps(build_dag(n, list(range(n)), list(range(n)), records), indent=2))
