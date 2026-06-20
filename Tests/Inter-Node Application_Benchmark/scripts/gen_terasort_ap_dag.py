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
# Slots match Tests/Intra-Node Application_Benchmark/TeraSort/gen_dag.py and the
# guest (Executor/guest/src/workloads/terasort.rs):
#   input slot 0; worker shards 10..10+N; per-(worker,owner) sub-slots 100+i*N+j;
#   per-owner gathered range 400+j; per-range summary I/O slot 2+j.
import json
import sys

TS_DIST_BASE = 10     # per-worker input shards: 10 .. 10+N
TS_PART_BASE = 100    # ts_partition(i) writes owner j to 100 + i*N + j
TS_MERGE_IN = 400     # owner j's gathered range slot
TS_OUT_BASE = 2       # owner j's summary I/O slot (0=input, 1=default output)


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

    # ── per-owner shuffle: per-machine local-combine → owner gather → merge → save ─
    for j in range(n):
        owner_node = owner_nodes[j]
        # Group this owner's source sub-slots {100+i*N+j} by the machine that
        # PRODUCED them (= where partition worker i was placed).
        by_machine = {}
        for i in range(n):
            by_machine.setdefault(part_nodes[i], []).append(TS_PART_BASE + i * n + j)

        # One local Aggregate per source machine: combine that machine's owner-j
        # sub-slots into a single stream → exactly ONE transfer per (machine, owner).
        local_ids = []
        for m in sorted(by_machine):
            lid = f"shuf_{j}_local_{m}"
            local_ids.append(lid)
            nodes.append({"id": lid, "node_id": m,
                          "deps": [f"part_{i}" for i in range(n) if part_nodes[i] == m],
                          "kind": {"Aggregate": {"upstream": by_machine[m]}}})

        # Owner j's gather: collect one combined stream per source machine (the
        # partitioner emits RemoteSend/Recv for locals not on owner_node) → 400+j.
        nodes.append({"id": f"agg_{j}", "node_id": owner_node, "deps": local_ids,
                      "kind": {"Aggregate": {"upstream_nodes": local_ids,
                                             "downstream": TS_MERGE_IN + j}}})
        # Merge: owner j sorts its range and writes a summary to I/O slot 2+j.
        nodes.append({"id": f"merge_{j}", "node_id": owner_node, "deps": [f"agg_{j}"],
                      "kind": {"Func": {"func": "ts_merge", "arg": TS_MERGE_IN + j,
                                        "wasm_arg": packed(TS_MERGE_IN + j, j)}}})
        nodes.append({"id": f"save_{j}", "node_id": owner_node, "deps": [f"merge_{j}"],
                      "kind": {"Output": {"path": f"{out_prefix}.part{j}",
                                          "slot": TS_OUT_BASE + j}}})

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
    records = sys.argv[2] if len(sys.argv) > 2 else "TestData/terasort/records_1m.txt"
    # Standalone default: square layout, one partition + one owner per node 0..N-1.
    print(json.dumps(build_dag(n, list(range(n)), list(range(n)), records), indent=2))
