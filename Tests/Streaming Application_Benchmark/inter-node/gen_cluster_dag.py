#!/usr/bin/env python3
# gen_cluster_dag.py — emit a multi-node streaming ClusterDag (node_dags format)
# for `node-agent submit`. First cluster version: data-parallel + RDMA merge,
# matching the proven DAGs/cluster_dag/{word_count,finra}.json shape.
#
# Each node loads its OWN disjoint event slice, seeds the password table, parses
# the slice BY KEY into `parts` local stream slots (BASE+key%parts), applies the
# per-event ops against keyed SHM state, and aggregates its per-partition partial
# tallies into slot 200. Non-merger nodes RemoteSend slot 200 to the merger (the
# last node); the merger RemoteRecv's every peer, aggregates all partials (its own
# slot 200 + the received slots) into slot 300, runs `*_summary` (the deterministic
# gate), and writes Output. The cross-node movement is the RDMA partial-tally merge.
#
# (The richer key-owning-node variant — where an event's key is owned by a remote
# node and the state access itself crosses the network — is the follow-up in
# EXPERIMENT_PLAN.md §2; this version proves the end-to-end cluster pipeline first.)
#
# Uses the same guest funcs as ../intra-node (mr_*/sn_*), so NO rebuild is needed.
# Slot map matches ../intra-node/gen_dag.py: BASE=10, partial=+100 (=110), agg=200/300.
import argparse
import json

PREFIX = {"mediareview": "mr", "socialnetwork": "sn"}

ap = argparse.ArgumentParser()
ap.add_argument("workload", choices=list(PREFIX))
ap.add_argument("--nodes", type=int, default=2, help="number of cluster nodes (>=2)")
ap.add_argument("--parts", type=int, default=4, help="local partitions per node")
ap.add_argument("--users", type=int, default=10000, help="seed-table size (all nodes seed it)")
ap.add_argument("--events-prefix", default="TestData/stream_app/cluster/events_node",
                help="per-node input path prefix; node i reads <prefix><i>.csv (relative to executor work dir)")
ap.add_argument("--shm-prefix", default="/dev/shm/rdma_stream")
ap.add_argument("--out", default="TestOutput/rdma_stream_result.txt",
                help="gate output path on the MERGER node (last node)")
ap.add_argument("--persist", choices=["none", "async", "sync"], default="none",
                help="FT Persist offload PER NODE (snapshots that node's apply working set). "
                     "async = background leaf joined at run end; "
                     "sync = fsync barrier between apply and aggregate_local.")
ap.add_argument("--persist-dir", default="TestOutput/persist",
                help="per-node directory the Persist node writes its snapshot to")
ap.add_argument("--merge", choices=["flat", "tree"], default="tree",
                help="partial-tally merge topology. flat = single-merger N-way fan-in "
                     "(original; the merger does N-1 sequential RemoteRecvs in one wave). "
                     "tree = hierarchical k-ary reduction: fan-in is only `branch` per "
                     "level and depth is ~log_branch(N), so the merge cost grows as log N "
                     "instead of N — the cure for the single-merger weak-scaling bottleneck.")
ap.add_argument("--branch", type=int, default=2,
                help="tree fan-in (children merged per node per level); 2 = binary tree.")
ap.add_argument("--batches", type=int, default=1,
                help="process each node's slice in B SEQUENTIAL batches instead of one shot. "
                     "Each batch loads <prefix><i>_b<k>.csv, parses+applies, then frees the "
                     "stream slots before the next batch — peak SHM = one batch, not the whole "
                     "slice. The keyed state and the appended per-batch tallies accumulate, so "
                     "the result is identical. Lets a large fixed total fit on few nodes (more "
                     "batches) and strong-scale (fewer batches as nodes grow).")
args = ap.parse_args()

p = PREFIX[args.workload]
n = max(1, args.parts)
N = max(1, args.nodes)   # N=1 → single-node pipeline (no RemoteSend/Recv), for the 1-node baseline
B = max(1, args.batches) # sequential per-node batches (1 = one-shot, original)
BASE, OUT, LOCAL_AGG, GLOBAL_AGG = 10, 110, 200, 300
merger = N - 1


# ── tree-merge reduction plan ────────────────────────────────────────────────
# Hierarchical reduction toward node 0. Each level groups the survivors into chunks
# of `B`; chunk[0] is the leader (receiver), the rest send their running tally to it.
# A trailing singleton is ABSORBED into the previous chunk so every survivor at a
# given level has done exactly that many recv-rounds — i.e. all senders/receivers at
# level L sit at the SAME wave depth, which is what makes the RDMA rendezvous pair up
# (a recv one wave off its send silently drops data). Returns:
#   recvs[node] = {level: [sender, ...]}   sends[node] = (level, receiver)   root
def reduction_plan(N, B):
    B = max(2, B)
    recvs, sends = {}, {}
    survivors, level = list(range(N)), 0
    while len(survivors) > 1:
        nxt, i, L = [], 0, len(survivors)
        while i < L:
            end = min(i + B, L)
            if L - end == 1:        # avoid leaving a lone survivor (free-rider → wave skew)
                end = L
            chunk = survivors[i:end]
            leader = chunk[0]; nxt.append(leader)
            for m in chunk[1:]:
                recvs.setdefault(leader, {}).setdefault(level, []).append(m)
                sends[m] = (level, leader)
            i = end
        survivors, level = nxt, level + 1
    return recvs, sends, (survivors[0] if survivors else 0)


RECVS, SENDS, ROOT = reduction_plan(N, args.branch)
# slot map for the tree tail (kept clear of 0/BASE/OUT/LOCAL_AGG=200/GLOBAL_AGG=300):
TREE_RECV_BASE, TREE_RECV_STRIDE, TREE_AGG_BASE = 400, 32, 360


def batch_path(i, k):
    return f"{args.events_prefix}{i}_b{k}.csv" if B > 1 else f"{args.events_prefix}{i}.csv"


def node_dag(i):
    # seed the keyed state ONCE; it (and the appended per-batch tallies) persist
    # across all batches, so B sequential batches give the same result as one shot.
    nodes = [{"id": "seed", "deps": [],
              "kind": {"Func": {"func": f"{p}_seed", "arg": 0, "wasm_arg": args.users}}}]
    prev_free, last_applies = None, []
    for k in range(B):
        lid, pid = f"load_{k}", f"parse_{k}"
        # Batches run STRICTLY sequentially: batch k doesn't even load until batch k-1's
        # partition slots are freed, so peak SHM ≈ ONE batch (not the whole slice and not
        # the ~2 batches a load/apply overlap would hold). This is what keeps a large fixed
        # total within the per-node SHM window on few nodes.
        nodes.append({"id": lid, "deps": ([prev_free] if prev_free else []),
                      "kind": {"Input": {"path": batch_path(i, k), "slot": 0, "prefetch": True}}})
        nodes.append({"id": pid, "deps": [lid],
                      "kind": {"Func": {"func": f"{p}_parse", "arg": n, "arg2": BASE}}})
        applies = []
        for j in range(n):
            aid = f"apply_{k}_{j}"
            applies.append(aid)
            # mr_apply APPENDS its tally to OUT+j, so batch tallies accumulate there.
            nodes.append({"id": aid, "deps": [pid, "seed"],
                          "kind": {"Func": {"func": f"{p}_apply", "arg": BASE + j, "arg2": OUT + j}}})
        last_applies = applies
        if k < B - 1:
            # recycle the partition slots (page chains + cursors) so the next batch's
            # parse starts fresh and apply reads only ITS batch (no carry-over/double-count).
            fid = f"free_{k}"
            nodes.append({"id": fid, "deps": applies,
                          "kind": {"FreeSlots": {"stream": [BASE + j for j in range(n)], "io": []}}})
            prev_free = fid
    # aggregate_local merges OUT+j (which now hold every batch's appended tallies);
    # deps = the LAST batch's applies (batches are chained, so this gates all of them).
    agg_local_deps = last_applies
    if args.persist == "sync":
        barrier_slots = [BASE + j for j in range(n)] + [OUT + j for j in range(n)]
        nodes.append({"id": "persist", "deps": last_applies,
                      "kind": {"Persist": {"output_dir": f"{args.persist_dir}/node{i}",
                                           "atomics": True, "stream_slots": barrier_slots,
                                           "shared_state": True, "sync": True}}})
        agg_local_deps = ["persist"]
    nodes.append({"id": "aggregate_local", "deps": agg_local_deps,
                  "kind": {"Aggregate": {"upstream": [OUT + j for j in range(n)], "downstream": LOCAL_AGG}}})
    if args.persist == "async":
        # async: background offload of this node's working set as a leaf, joined at run end.
        persist_slots = [0] + [BASE + j for j in range(n)] + [OUT + j for j in range(n)] + [LOCAL_AGG]
        nodes.append({"id": "persist", "deps": ["aggregate_local"],
                      "kind": {"Persist": {"output_dir": f"{args.persist_dir}/node{i}",
                                           "atomics": True, "stream_slots": persist_slots,
                                           "shared_state": True, "sync": False}}})

    if args.merge == "flat":
        # ── single-merger N-way fan-in (original): every worker sends to node N-1 ──
        if i != merger:
            nodes.append({"id": "send", "deps": ["aggregate_local"],
                          "kind": {"RemoteSend": {"slot": LOCAL_AGG, "slot_kind": "Stream", "peer": merger}}})
        else:
            recv_ids, upstream = [], [LOCAL_AGG]
            for k in range(N):
                if k == merger:
                    continue
                rid, slot = f"recv_{k}", LOCAL_AGG + 1 + k   # 201, 202, ...
                nodes.append({"id": rid, "deps": ["aggregate_local"],
                              "kind": {"RemoteRecv": {"slot": slot, "slot_kind": "Stream", "peer": k}}})
                recv_ids.append(rid); upstream.append(slot)
            nodes.append({"id": "aggregate_global", "deps": ["aggregate_local"] + recv_ids,
                          "kind": {"Aggregate": {"upstream": upstream, "downstream": GLOBAL_AGG}}})
            nodes.append({"id": "summary", "deps": ["aggregate_global"],
                          "kind": {"Func": {"func": f"{p}_summary", "arg": GLOBAL_AGG}}})
            nodes.append({"id": "save", "deps": ["summary"],
                          "kind": {"Output": {"path": args.out}}})
        return nodes

    # ── hierarchical tree merge ──
    # Walk this node's recv-levels in order: each level RemoteRecvs its children into
    # fresh slots and Aggregates [running tally + children] into a new accumulator.
    # Then, if this node is a non-root, RemoteSend the accumulator to its parent.
    # deps chain the levels so the recv at level L lands at wave-depth 2L+1, matching
    # every paired sender (which has also done L recv-rounds) — see reduction_plan.
    acc, last_dep = LOCAL_AGG, "aggregate_local"
    for lvl in sorted(RECVS.get(i, {})):
        recv_ids = []
        for j, s in enumerate(RECVS[i][lvl]):
            slot = TREE_RECV_BASE + lvl * TREE_RECV_STRIDE + j
            rid = f"recv_l{lvl}_{s}"
            nodes.append({"id": rid, "deps": [last_dep],
                          "kind": {"RemoteRecv": {"slot": slot, "slot_kind": "Stream", "peer": s}}})
            recv_ids.append((rid, slot))
        agg_out, aid = TREE_AGG_BASE + lvl, f"agg_l{lvl}"
        nodes.append({"id": aid, "deps": [last_dep] + [r for r, _ in recv_ids],
                      "kind": {"Aggregate": {"upstream": [acc] + [sl for _, sl in recv_ids],
                                             "downstream": agg_out}}})
        acc, last_dep = agg_out, aid
    if i in SENDS:
        _, parent = SENDS[i]
        nodes.append({"id": "send", "deps": [last_dep],
                      "kind": {"RemoteSend": {"slot": acc, "slot_kind": "Stream", "peer": parent}}})
    if i == ROOT:
        nodes.append({"id": "summary", "deps": [last_dep],
                      "kind": {"Func": {"func": f"{p}_summary", "arg": acc}}})
        nodes.append({"id": "save", "deps": ["summary"],
                      "kind": {"Output": {"path": args.out}}})
    return nodes


dag = {
    "_comment": f"ClusterDag: {args.workload} streaming across {N} nodes"
                + (f", {B} batches/node" if B > 1 else "")
                + f" (data-parallel + RDMA partial-tally {args.merge} merge"
                + (f", branch={args.branch}, root=node {ROOT}" if args.merge == "tree"
                   else f" on node {merger}") + ").",
    "shm_path_prefix": args.shm_prefix,
    "log_level": "info",
    "transfer": True,
    # The coordinator STAGES these per-node slices to the workers before execution
    # (the worker resolves Input from the staged copy, not the shared FS — without
    # this, the worker's load() fails and the job reports success=false). All
    # slices live on the coordinator (node 0), which writes them in run-cluster.sh.
    "shared_inputs": [
        {"path": batch_path(i, k), "source_node": 0}
        for i in range(N) for k in range(B)
    ],
    "node_dags": {str(i): node_dag(i) for i in range(N)},
}
print(json.dumps(dag, indent=2))
