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
              out_prefix="TestOutput/terasort_ap_result",
              shard_input=False):
    """Return the TeraSort auto-placement SymbolicDag for `n` partition workers and
    `n` range-owners.

    `part_nodes[i]` = node hosting partition worker i (default: all node 0). The
    merge is CENTRALIZED on node 0 (see below), so `owner_nodes` is accepted for
    call-compatibility with gen_variants.py but not used for placement. Both the
    single-node GT (all on node 0) and the distributed run use this same structure,
    so their gates match exactly.

    `shard_input` (cluster scale): when True, the input is SLICED across the P nodes
    instead of REPLICATED — each node loads only its 1/P shard (no `replicate`, no
    `ts_distribute`), and worker i reads its node-local slice directly via
    `ts_partition_local` (explicit worker index). This caps the per-node SHM
    footprint at ~1/P of the dataset (vs the whole file under replicate), lifting the
    ~512 MB replicate ceiling toward the centralized-gather limit (~1.3 GB). Requires
    one partition worker per node (the N==P square layout); the node-0 gather is still
    centralized, so >~1.3 GB still needs the true distributed all-to-all. Default
    False keeps the proven replicate path for small inputs / N>P.
    """
    if part_nodes is None:
        part_nodes = [0] * n
    if owner_nodes is None:
        owner_nodes = [0] * n

    # IMPORTANT — packed args go in `wasm_arg`, NOT `arg` (see slot.rs::collect_slots:
    # a packed value in `arg` poisons the recv-slot allocator and deadlocks the
    # gather). `arg` stays a small REAL slot the partitioner can account for.
    part_ids = []
    if shard_input:
        # SLICED input: the partitioner splits slot 0 across nodes (like WordCount's
        # corpus), so each node holds only its 1/P shard. Worker i reads that slice
        # DIRECTLY (slot 0) — no full-file replica, no contiguous re-split.
        nodes = [
            {"id": "load", "placement": "all", "deps": [],
             "kind": {"Input": {"path": records_path, "slot": 0, "prefetch": True}}},
        ]
        for i in range(n):
            pid = f"part_{i}"
            part_ids.append(pid)
            nodes.append({"id": pid, "node_id": part_nodes[i], "deps": ["load"],
                          "kind": {"Func": {"func": "ts_partition_local",
                                            "arg": 0, "wasm_arg": packed(i, n)}}})
    else:
        # Replicated input + zero-copy split: every node splits its FULL local copy
        # into N shards; partition worker i reads its same-machine shard 10+i. The
        # split is cheap (no parse); the routing happens in the placed ts_partition.
        nodes = [
            {"id": "load", "placement": "all", "deps": [],
             "kind": {"Input": {"path": records_path, "slot": 0,
                                "prefetch": True, "replicate": True}}},
            {"id": "distribute", "placement": "all", "deps": ["load"],
             "kind": {"Func": {"func": "ts_distribute", "arg": TS_DIST_BASE,
                               "wasm_arg": packed(TS_DIST_BASE, n)}}},
        ]
        # ── partition workers (placed per policy) ────────────────────────────────
        # Worker i routes its shard's records into owner sub-slots 100 + i*N + j.
        for i in range(n):
            pid = f"part_{i}"
            part_ids.append(pid)
            nodes.append({"id": pid, "node_id": part_nodes[i], "deps": ["distribute"],
                          "kind": {"Func": {"func": "ts_partition", "arg": TS_DIST_BASE + i,
                                            "wasm_arg": packed(TS_DIST_BASE + i, n)}}})

    # ── centralized gather (all-to-ONE) → sort on node 0 ─────────────────────────
    # WHY CENTRALIZED (not a true all-to-all transpose): the executor's blocking RDMA
    # transport deadlocks on the bidirectional all-to-all (every node sends to AND
    # receives from every other; the partitioner can't order all sends before all
    # recvs without a cycle — splitter.rs:443). So we use the proven UNIDIRECTIONAL
    # fan-gather instead: each worker routes its shard by owner (ts_partition — the
    # real partition compute stays distributed), combines its N sub-slots into ONE
    # declared stream, and sends exactly ONE transfer to node 0. Node 0 gathers the N
    # combined streams (one RemoteRecv per peer, like every working workload) and
    # sorts the whole dataset. The cross-node cost is the full shuffled dataset moving
    # to node 0 over RDMA; the merge is centralized (single reducer node).
    #
    # combine_i consumes part_i's raw sub-slots LOCALLY (same node), so the only slot
    # that crosses the network is its DECLARED downstream — which the partitioner can
    # route (the raw sub-slots have no output_slot and could not be routed directly).
    combine_ids = []
    for i in range(n):
        cid = f"combine_{i}"
        combine_ids.append(cid)
        nodes.append({"id": cid, "node_id": part_nodes[i], "deps": [f"part_{i}"],
                      "kind": {"Aggregate": {"upstream": [TS_PART_BASE + i * n + j for j in range(n)]}}})

    # Node 0 gathers every worker's combined stream → the full dataset in slot 400.
    nodes.append({"id": "gather", "node_id": 0, "deps": combine_ids,
                  "kind": {"Aggregate": {"upstream_nodes": combine_ids,
                                         "downstream": TS_MERGE_IN}}})
    # Sort the gathered dataset and emit its (records, sorted, keysum) summary, then
    # finalize into ONE output record (slot 1). ts_finalize takes NO arg → the
    # partitioner auto-wires it to summary_global's downstream (like the fan reduce).
    nodes += [
        {"id": "summary", "node_id": 0, "deps": ["gather"], "output_slot": TS_SUMMARY_BASE,
         "kind": {"Func": {"func": "ts_range_summary", "arg": TS_MERGE_IN,
                           "wasm_arg": packed(TS_MERGE_IN, TS_SUMMARY_BASE)}}},
        {"id": "summary_global", "node_id": 0, "deps": ["summary"],
         "kind": {"Aggregate": {}}},
        {"id": "finalize", "node_id": 0, "deps": ["summary_global"],
         "kind": {"Func": {"func": "ts_finalize"}}},
        {"id": "save", "node_id": 0, "deps": ["finalize"],
         "kind": {"Output": {"path": f"{out_prefix}.txt"}}},
    ]

    return {
        "shm_path_prefix": "/dev/shm/rdma_terasort_ap",
        "log_level": "info",
        "total_nodes": max(part_nodes) + 1,
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
