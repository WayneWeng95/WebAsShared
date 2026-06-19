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
args = ap.parse_args()

p = PREFIX[args.workload]
n = max(1, args.parts)
N = max(2, args.nodes)
BASE, OUT, LOCAL_AGG, GLOBAL_AGG = 10, 110, 200, 300
merger = N - 1


def node_dag(i):
    nodes = [
        {"id": "load", "deps": [],
         "kind": {"Input": {"path": f"{args.events_prefix}{i}.csv", "slot": 0, "prefetch": True}}},
        {"id": "seed", "deps": [],
         "kind": {"Func": {"func": f"{p}_seed", "arg": 0, "wasm_arg": args.users}}},
        {"id": "parse", "deps": ["load"],
         "kind": {"Func": {"func": f"{p}_parse", "arg": n, "arg2": BASE}}},
    ]
    applies = []
    for j in range(n):
        aid = f"apply_{j}"
        applies.append(aid)
        nodes.append({"id": aid, "deps": ["parse", "seed"],
                      "kind": {"Func": {"func": f"{p}_apply", "arg": BASE + j, "arg2": OUT + j}}})
    nodes.append({"id": "aggregate_local", "deps": applies,
                  "kind": {"Aggregate": {"upstream": [OUT + j for j in range(n)], "downstream": LOCAL_AGG}}})

    if i != merger:
        # send this node's partial tally to the merger over RDMA
        nodes.append({"id": "send", "deps": ["aggregate_local"],
                      "kind": {"RemoteSend": {"slot": LOCAL_AGG, "slot_kind": "Stream", "peer": merger}}})
    else:
        recv_ids, upstream = [], [LOCAL_AGG]
        for k in range(N):
            if k == merger:
                continue
            rid, slot = f"recv_{k}", LOCAL_AGG + 1 + k   # 201, 202, ...
            # recv must sit at the SAME wave depth as the senders' RemoteSend
            # (both after their aggregate_local) so the cross-node rendezvous
            # pairs up — an early recv (e.g. deps=[parse]) silently drops the
            # sender's data (merge yields only the merger's own slice).
            nodes.append({"id": rid, "deps": ["aggregate_local"],
                          "kind": {"RemoteRecv": {"slot": slot, "slot_kind": "Stream", "peer": k}}})
            recv_ids.append(rid)
            upstream.append(slot)
        nodes.append({"id": "aggregate_global", "deps": ["aggregate_local"] + recv_ids,
                      "kind": {"Aggregate": {"upstream": upstream, "downstream": GLOBAL_AGG}}})
        nodes.append({"id": "summary", "deps": ["aggregate_global"],
                      "kind": {"Func": {"func": f"{p}_summary", "arg": GLOBAL_AGG}}})
        nodes.append({"id": "save", "deps": ["summary"],
                      "kind": {"Output": {"path": args.out}}})
    return nodes


dag = {
    "_comment": f"ClusterDag: {args.workload} streaming across {N} nodes "
                f"(data-parallel + RDMA partial-tally merge on node {merger}).",
    "shm_path_prefix": args.shm_prefix,
    "log_level": "info",
    "transfer": True,
    # The coordinator STAGES these per-node slices to the workers before execution
    # (the worker resolves Input from the staged copy, not the shared FS — without
    # this, the worker's load() fails and the job reports success=false). All
    # slices live on the coordinator (node 0), which writes them in run-cluster.sh.
    "shared_inputs": [
        {"path": f"{args.events_prefix}{i}.csv", "source_node": 0} for i in range(N)
    ],
    "node_dags": {str(i): node_dag(i) for i in range(N)},
}
print(json.dumps(dag, indent=2))
