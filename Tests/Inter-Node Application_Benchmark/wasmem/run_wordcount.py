#!/usr/bin/env python3
"""run_wordcount.py — WasMem (auto-placement) inter-node WordCount driver (node 0).

The counterpart to ../faasm/run_wordcount.py, measured the SAME way so the two are
comparable. Per (corpus, fanout):

  1. OFFLINE, UNTIMED — build the symbolic DAG from
     DAGs/symbolic_dag/word_count_auto_placement.json with map_n.fanout=W (the map
     fan width, W/nodes per node) and the Input path rewritten to the chosen corpus,
     under the `balanced` policy; then `partition --nodes N` it into a ClusterDag so
     the embedded balanced placement is authoritative (a live cluster's SCX hints
     would otherwise override it). Generation + partition are the scheduler's job and
     are explicitly OUTSIDE the timed window.

  2. TIMED (per rep) — `node-agent submit` the ClusterDag. The coordinator reports a
     Job Summary whose `total wall time` is job_start→all-workers-complete
     (coordinator.rs) — i.e. pure job execution (input RDMA-staging + compute +
     cross-node aggregate + reduce + the save Output node), with the partitioner and
     the client-side dispatch excluded. That is the makespan we record. Summing the
     per-node executor durations gives total_job_ms (aggregate work across nodes),
     mirroring the Faasm Σ-per-Faaslet column — but see README on the granularity
     difference (one executor/node here vs one process/mapper in Faasm).

The DAG's `save` Output node writes TestOutput/wc_auto_placement_result.txt; we gate
total_occurrences (fan-out-invariant) across reps.

Usage:
  ./run_wordcount.py --corpus TestData/corpus_xlarge.txt --fanout 40 --nodes 4 --reps 3
"""
import argparse
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
SRC_DAG = os.path.join(ROOT, "DAGs", "symbolic_dag", "word_count_auto_placement.json")
PART = os.path.join(ROOT, "Partitioner", "target", "release", "partition")
NODE_AGENT = os.path.join(ROOT, "node-agent")
AGENT_TOML = os.path.join(ROOT, "NodeAgent", "agent.toml")
RESULT = os.path.join(ROOT, "TestOutput", "wc_auto_placement_result.txt")


def mean_std(xs):
    """(mean, sample-stdev); stdev=0 for <2 samples. Headline summary for N reps."""
    m = sum(xs) / len(xs)
    if len(xs) < 2:
        return m, 0.0
    return m, (sum((x - m) ** 2 for x in xs) / (len(xs) - 1)) ** 0.5


def build_sym(corpus_rel, fanout, nodes, policy, out_path):
    """word_count control DAG with map_n widened to `fanout` and Input → corpus_rel.
    map_n's --fanout CLI knob is pinned in gen_variants (it's the policy control), so
    we set the node's `fanout` field directly — the offline partitioner then spreads
    the fan per node under `policy` (a string like "balanced", or a weighted-policy
    dict {"type":"weighted","weights":[...]} that apportions both the map count AND
    the Input slice per host — used to offload node 0, which also runs the reduce +
    coordinator)."""
    d = json.load(open(SRC_DAG))
    d.pop("_comment", None)
    d["total_nodes"] = nodes
    d["placement_policy"] = policy
    touched = 0
    for n in d["nodes"]:
        if n.get("id") == "map_n":
            n["fanout"] = fanout
        k = n.get("kind", {})
        if isinstance(k, dict) and "Input" in k and "path" in k["Input"]:
            k["Input"]["path"] = corpus_rel
            touched += 1
    if touched == 0:
        sys.exit("[wc] no Input node with a path to override in word_count DAG")
    json.dump(d, open(out_path, "w"))


def partition(sym_path, nodes, cdag_path):
    """OFFLINE partition (untimed) — embedded balanced placement authoritative."""
    with open(cdag_path, "w") as f:
        r = subprocess.run([PART, sym_path, "--nodes", str(nodes)], stdout=f,
                           stderr=subprocess.PIPE, text=True)
    if r.returncode != 0:
        sys.exit(f"[wc] partition failed: {r.stderr[:800]}")


def submit(cdag_path):
    """TIMED — submit to the running coordinator; parse its Job Summary.
    Returns (makespan_ms, total_job_ms, per_node, ok)."""
    cmd = [NODE_AGENT, "submit", "--config", AGENT_TOML, "--dag", cdag_path, "--aot"]
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    out = r.stdout + "\n" + r.stderr
    if r.returncode != 0:
        sys.exit(f"[wc] submit failed (rc={r.returncode}):\n{out[-1500:]}")
    # Summary lines: "node 0 (local): <ms>ms", "node N (worker): <ms>ms",
    #                "total wall time: <ms>ms"
    makespan = None
    m = re.search(r"total wall time:\s*(\d+)\s*ms", out)
    if m:
        makespan = int(m.group(1))
    per_node = [int(x) for x in re.findall(r"node \d+ \((?:local|worker)\):\s*(\d+)\s*ms", out)]
    ok = ("Success: true" in out) or ("Success: True" in out)
    if makespan is None:
        sys.exit(f"[wc] could not parse 'total wall time' from submit output:\n{out[-1500:]}")
    return makespan, sum(per_node), per_node, ok


def read_occurrences():
    try:
        for line in open(RESULT):
            m = re.match(r"total_occurrences=(\d+)", line.strip())
            if m:
                return int(m.group(1))
    except FileNotFoundError:
        pass
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", default="TestData/corpus_xlarge.txt",
                    help="corpus path RELATIVE to repo root (staged from node 0 via RDMA)")
    ap.add_argument("--fanout", type=int, default=40, help="map_n fan width (fanout/nodes per node)")
    ap.add_argument("--nodes", type=int, default=4)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--policy", default="balanced")
    ap.add_argument("--weights", default=None,
                    help="comma-separated per-node capacity weights → weighted policy "
                         "(e.g. 6,11,11,12 gives node 0 fewer maps + smaller slice)")
    ap.add_argument("--expect", type=int, default=None, help="gate occurrences against this count")
    ap.add_argument("--csv", default=os.path.join(HERE, "results_wordcount.csv"))
    args = ap.parse_args()

    corpus_abs = os.path.join(ROOT, args.corpus)
    if not os.path.exists(corpus_abs):
        sys.exit(f"[wc] corpus not found on node 0: {corpus_abs}")
    size_mb = round(os.path.getsize(corpus_abs) / (1024 * 1024))
    for binp in (PART, NODE_AGENT):
        if not os.path.exists(binp):
            sys.exit(f"[wc] missing binary: {binp} — run ./build.sh")

    # Policy: weighted (per-node capacity) if --weights given, else the named policy.
    if args.weights:
        weights = [float(w) for w in args.weights.split(",")]
        policy = {"type": "weighted", "weights": weights}
        tag = "w" + "_".join(args.weights.split(","))
    else:
        policy = args.policy
        tag = args.policy

    # OFFLINE gen + partition (untimed) — done once per (corpus, fanout, policy).
    dags = os.path.join(HERE, "dags")
    os.makedirs(dags, exist_ok=True)
    sym = os.path.join(dags, f"wc_{size_mb}mb_f{args.fanout}_{tag}.sym.json")
    cdag = os.path.join(dags, f"wc_{size_mb}mb_f{args.fanout}_{tag}.cdag.json")
    build_sym(args.corpus, args.fanout, args.nodes, policy, sym)
    partition(sym, args.nodes, cdag)

    new = not os.path.exists(args.csv)
    fcsv = open(args.csv, "a")
    if new:
        fcsv.write("size_mb,mappers,nodes_used,makespan_mean_ms,makespan_std_ms,total_job_mean_ms,"
                   "occurrences,expect,success,reps\n")

    print(f"[wc] corpus={size_mb}MB fanout={args.fanout} nodes={args.nodes} "
          f"policy={tag} reps={args.reps} (partition offline/untimed)")
    ms_list, job_list, occ_seen, ok_all = [], [], None, True
    for rep in range(1, args.reps + 1):
        makespan, total_job, per_node, ok = submit(cdag)
        occ = read_occurrences()
        gate = args.expect if args.expect is not None else (occ_seen if occ_seen is not None else occ)
        occ_seen = occ
        success = ok and occ == gate
        ok_all = ok_all and success
        ms_list.append(makespan)
        job_list.append(total_job)
        print(f"[wc] rep {rep}: makespan={makespan}ms total_job={total_job}ms "
              f"per_node={per_node} occurrences={occ} gate={gate} ok={success}")

    mean_ms, std_ms = mean_std(ms_list)
    mean_ms, std_ms = int(mean_ms), round(std_ms, 1)
    job_mean = int(sum(job_list) / len(job_list))
    gate = args.expect if args.expect is not None else occ_seen
    fcsv.write(f"{size_mb},{args.fanout},{args.nodes},{mean_ms},{std_ms},{job_mean},"
               f"{occ_seen},{gate},{ok_all},{args.reps}\n")
    fcsv.close()
    print(f"[wc] mean makespan={mean_ms}±{std_ms}ms total_job={job_mean}ms occurrences={occ_seen} "
          f"success={ok_all} → {args.csv}")
    print(f"RESULT occ={occ_seen} makespan_ms={mean_ms} std={std_ms} success={ok_all}")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
