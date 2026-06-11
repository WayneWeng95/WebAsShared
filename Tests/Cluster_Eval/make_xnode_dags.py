#!/usr/bin/env python3
"""Partition a SymbolicDag and emit ready-to-run per-node DAGs for a real cluster.

Runs the partitioner, then splits the resulting ClusterDag into one
`node_<id>.json` per machine — each a complete executor DAG with its own
`shm_path` and an `rdma` block carrying the cluster IPs.  Copy `node_<id>.json`
to machine <id> and run `host dag node_<id>.json` there.

Usage (from project root):
  python3 Tests/Cluster_Eval/make_xnode_dags.py \
      DAGs/symbolic_dag/word_count_auto_placement.json \
      --ips 10.10.1.2,10.10.1.1 --out Tests/Cluster_Eval/out

  # local 2-node loopback smoke test (both on this machine):
  python3 Tests/Cluster_Eval/make_xnode_dags.py <dag> --ips 127.0.0.1,127.0.0.1 --out /tmp/xn
"""
import os, sys, json, argparse, subprocess

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
PART = os.path.join(ROOT, "Partitioner", "target", "release", "partition")
WASM = os.path.join("Executor", "target", "wasm32-unknown-unknown", "release", "guest.wasm")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dag")
    ap.add_argument("--ips", required=True, help="comma-separated per-node IPs, index = node_id")
    ap.add_argument("--hints", help="optional PlacementHints JSON path")
    ap.add_argument("--out", default="Tests/Cluster_Eval/out")
    args = ap.parse_args()

    ips = args.ips.split(",")
    total = len(ips)
    cmd = [PART, args.dag, "--nodes", str(total)]
    if args.hints:
        cmd += ["--hints", args.hints]
    res = subprocess.run(cmd, capture_output=True, text=True, cwd=ROOT)
    if res.returncode != 0:
        raise SystemExit(f"partition failed: {res.stderr.strip()}")
    cluster = json.loads(res.stdout)

    shm_prefix = cluster.get("shm_path_prefix", "/dev/shm/xnode_eval")
    log_level  = cluster.get("log_level", "info")
    transfer   = cluster.get("transfer", False)

    out_dir = os.path.join(ROOT, args.out)
    os.makedirs(out_dir, exist_ok=True)

    print(f"partitioned across {total} node(s); IPs={ips}\n")
    for host_str, nodes in sorted(cluster["node_dags"].items(), key=lambda x: int(x[0])):
        nid = int(host_str)
        has_remote = any("Remote" in json.dumps(n.get("kind", "")) for n in nodes)
        per_node = {"shm_path": f"{shm_prefix}_n{nid}", "wasm_path": WASM, "log_level": log_level}
        if transfer or has_remote:
            per_node["rdma"] = {"node_id": nid, "total": total, "ips": ips, "transfer": transfer}
        per_node["nodes"] = nodes
        path = os.path.join(out_dir, f"node_{nid}.json")
        with open(path, "w") as f:
            json.dump(per_node, f, indent=2)

        outputs = [n["id"] for n in nodes if "Output" in json.dumps(n.get("kind", ""))]
        infra = sum(1 for n in nodes if n["id"].startswith(("rs_", "rr_")))
        print(f"  node {nid} ({ips[nid]}): {len(nodes)} nodes ({infra} rdma infra) "
              f"→ {path}" + (f"   [writes Output: {outputs}]" if outputs else ""))

    print("\nRun on each machine (from the project root, same commit + ./build.sh on both):")
    for nid, ip in enumerate(ips):
        print(f"  [{ip}]  Executor/target/release/host dag {args.out}/node_{nid}.json")


if __name__ == "__main__":
    main()
