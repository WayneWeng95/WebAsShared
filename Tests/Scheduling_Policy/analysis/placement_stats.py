#!/usr/bin/env python3
"""placement_stats.py — static placement metrics for a pre-partitioned ClusterDag.

Reads a ClusterDag (the `partition` binary's output, with a `node_dags` map) from
a file argument or stdin and reports, as one JSON object:

    hosts            number of nodes the work is partitioned across
    compute_per_host list of compute-sandbox counts, host 0..N-1
    busy_hosts       how many hosts got at least one compute sandbox
    imbalance        max(compute_per_host) - min(compute_per_host)
    cross_node_edges number of RemoteSend/RemoteRecv endpoints (RDMA wiring)

These quantify the SCHEDULING DECISION itself — the half of the story that is
deterministic and independent of the live cluster. The runner pairs them with the
measured end-to-end latency to show how the decision drives performance.

Compute kinds counted: WasmVoid, Aggregate, PyFunc, Reduce (the actual work).
RemoteSend/RemoteRecv are the cross-host RDMA edges the placement induced.
"""
import json
import sys

COMPUTE_KINDS = {"WasmVoid", "Aggregate", "PyFunc", "Reduce"}
REMOTE_KINDS = {"RemoteSend", "RemoteRecv"}


def kind_of(node: dict):
    k = node.get("kind")
    if isinstance(k, dict):
        return next(iter(k), None)
    return k


def summarize(cluster_dag: dict) -> dict:
    nd = cluster_dag.get("node_dags", {})
    # node_dags is a map { "0": [nodes...], "1": [...], ... }.
    per_host = {}
    remote = 0
    for nid, nodes in nd.items():
        c = 0
        for n in nodes:
            k = kind_of(n)
            if k in REMOTE_KINDS:
                remote += 1
            elif k in COMPUTE_KINDS:
                c += 1
        per_host[int(nid)] = c
    order = sorted(per_host)
    busy = [per_host[i] for i in order]
    return {
        "hosts": len(busy),
        "compute_per_host": busy,
        "busy_hosts": sum(1 for b in busy if b > 0),
        "imbalance": (max(busy) - min(busy)) if busy else 0,
        "cross_node_edges": remote,
    }


def main() -> int:
    src = open(sys.argv[1]) if len(sys.argv) > 1 else sys.stdin
    with src:
        d = json.load(src)
    print(json.dumps(summarize(d)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
