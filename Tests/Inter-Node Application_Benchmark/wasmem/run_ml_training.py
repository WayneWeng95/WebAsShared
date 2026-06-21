#!/usr/bin/env python3
"""run_ml_training.py — WasMem (auto-placement) inter-node SGD-training driver (node 0).

Counterpart to ../faasm/run_ml_training.py, measured the SAME way. Training is the SGD
pipeline: `gen_variants.py ml_training --fanout W --input <sgd>` builds W `sgd_grad` workers
(each computes the gradient over a disjoint 1/W shard), gathered (per-machine local-combine,
one transfer/peer) into `sgd_update` (one weight step), then `sgd_validate`. The gate is
weight_checksum (Σ of all model weights after the epoch) — exact and fan-out-invariant
(associative integer gradient sum).

  1. OFFLINE, UNTIMED — gen_variants ml_training → partition --nodes N → ClusterDag.
  2. TIMED — node-agent submit; makespan = coordinator `total wall time`. weight_checksum /
     accuracy read from the `save` Output node (TestOutput/ml_training_ap_result.txt).

Makespan = mean ± sample-std over the reps (15 for headline). Training data RDMA-staged.

Usage:
  ./run_ml_training.py --data TestData/ml/sgd_600000.csv --fanout 60 --nodes 4 --reps 15
"""
import argparse
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
GEN = os.path.join(ROOT, "Tests", "Inter-Node Application_Benchmark", "scripts", "gen_variants.py")
PART = os.path.join(ROOT, "Partitioner", "target", "release", "partition")
NODE_AGENT = os.path.join(ROOT, "node-agent")
AGENT_TOML = os.path.join(ROOT, "NodeAgent", "agent.toml")
RESULT = os.path.join(ROOT, "TestOutput", "ml_training_ap_result.txt")


def mean_std(xs):
    """(mean, sample-stdev); stdev=0 for <2 samples. Headline summary for N reps."""
    m = sum(xs) / len(xs)
    if len(xs) < 2:
        return m, 0.0
    return m, (sum((x - m) ** 2 for x in xs) / (len(xs) - 1)) ** 0.5


def gen_sym(data_rel, fanout, nodes, policy, pack_cap, out_path):
    """OFFLINE (untimed): gen_variants ml_training SGD fan, Input → data_rel."""
    cmd = [sys.executable, GEN, "ml_training", policy, "--nodes", str(nodes),
           "--fanout", str(fanout), "--pack-cap", str(pack_cap),
           "--input", data_rel, "--out", out_path]
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"[ml_training] gen_variants failed: {(r.stderr + r.stdout)[:800]}")


def partition(sym_path, nodes, cdag_path):
    with open(cdag_path, "w") as f:
        r = subprocess.run([PART, sym_path, "--nodes", str(nodes)], stdout=f,
                           stderr=subprocess.PIPE, text=True)
    if r.returncode != 0:
        sys.exit(f"[ml_training] partition failed: {r.stderr[:800]}")


def submit(cdag_path):
    """TIMED — submit to the running coordinator; parse Job Summary."""
    cmd = [NODE_AGENT, "submit", "--config", AGENT_TOML, "--dag", cdag_path, "--aot"]
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    out = r.stdout + "\n" + r.stderr
    if r.returncode != 0:
        sys.exit(f"[ml_training] submit failed (rc={r.returncode}):\n{out[-1500:]}")
    m = re.search(r"total wall time:\s*(\d+)\s*ms", out)
    if not m:
        sys.exit(f"[ml_training] could not parse 'total wall time':\n{out[-1500:]}")
    makespan = int(m.group(1))
    per_node = [int(x) for x in re.findall(r"node \d+ \((?:local|worker)\):\s*(\d+)\s*ms", out)]
    ok = ("Success: true" in out) or ("Success: True" in out)
    return makespan, sum(per_node), per_node, ok


def read_result():
    """(weight_checksum, accuracy_str) from the Output file; None if absent."""
    cks, acc = None, None
    try:
        for line in open(RESULT):
            m = re.match(r"weight_checksum=(-?\d+)", line.strip())
            if m:
                cks = int(m.group(1))
            m2 = re.match(r"accuracy=([\d.]+)%", line.strip())
            if m2:
                acc = float(m2.group(1))
    except FileNotFoundError:
        return None
    if cks is None:
        return None
    return cks, acc


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", default="TestData/ml/sgd_600000.csv",
                    help="SGD training-set path RELATIVE to repo root (RDMA-staged from node 0)")
    ap.add_argument("--fanout", type=int, default=60, help="sgd_grad fan width")
    ap.add_argument("--nodes", type=int, default=4)
    ap.add_argument("--reps", type=int, default=15)
    ap.add_argument("--policy", default="balanced")
    ap.add_argument("--pack-cap", type=int, default=16)
    ap.add_argument("--expect", type=int, default=None, help="gate weight_checksum against this")
    ap.add_argument("--csv", default=os.path.join(HERE, "results_ml_training.csv"))
    args = ap.parse_args()

    data_abs = os.path.join(ROOT, args.data)
    if not os.path.exists(data_abs):
        sys.exit(f"[ml_training] data not found on node 0: {data_abs}")
    n_samples = max(0, sum(1 for _ in open(data_abs)))
    for binp in (PART, NODE_AGENT, GEN):
        if not os.path.exists(binp):
            sys.exit(f"[ml_training] missing: {binp}")

    dags = os.path.join(HERE, "dags")
    os.makedirs(dags, exist_ok=True)
    sym = os.path.join(dags, f"ml_training_{n_samples}_f{args.fanout}.sym.json")
    cdag = os.path.join(dags, f"ml_training_{n_samples}_f{args.fanout}.cdag.json")
    gen_sym(args.data, args.fanout, args.nodes, args.policy, args.pack_cap, sym)
    partition(sym, args.nodes, cdag)

    new = not os.path.exists(args.csv)
    fcsv = open(args.csv, "a")
    if new:
        fcsv.write("samples,fanout,nodes_used,makespan_mean_ms,makespan_std_ms,total_job_mean_ms,"
                   "accuracy_pct,weight_checksum,expect,success,reps\n")

    print(f"[ml_training] samples={n_samples} fanout={args.fanout} nodes={args.nodes} "
          f"policy={args.policy} reps={args.reps} (partition offline/untimed)")
    ms_list, job_list, cks_seen, acc_seen, ok_all = [], [], None, None, True
    for rep in range(1, args.reps + 1):
        try:
            os.remove(RESULT)
        except FileNotFoundError:
            pass
        makespan, total_job, per_node, ok = submit(cdag)
        res = read_result()
        cks = res[0] if res else None
        if res and res[1] is not None:
            acc_seen = res[1]
        gate = args.expect if args.expect is not None else (cks_seen if cks_seen is not None else cks)
        cks_seen = cks
        success = ok and cks is not None and cks == gate
        ok_all = ok_all and success
        ms_list.append(makespan)
        job_list.append(total_job)
        print(f"[ml_training] rep {rep}: makespan={makespan}ms total_job={total_job}ms "
              f"per_node={per_node} weight_checksum={cks} gate={gate} ok={success}")

    mean_ms, std_ms = mean_std(ms_list)
    mean_ms, std_ms = int(mean_ms), round(std_ms, 1)
    job_mean = int(sum(job_list) / len(job_list))
    gate = args.expect if args.expect is not None else cks_seen
    fcsv.write(f"{n_samples},{args.fanout},{args.nodes},{mean_ms},{std_ms},{job_mean},"
               f"{acc_seen},{cks_seen},{gate},{ok_all},{args.reps}\n")
    fcsv.close()
    print(f"[ml_training] mean makespan={mean_ms}±{std_ms}ms total_job={job_mean}ms "
          f"accuracy={acc_seen}% weight_checksum={cks_seen} success={ok_all} → {args.csv}")
    print(f"RESULT weight_checksum={cks_seen} makespan_ms={mean_ms} std={std_ms} success={ok_all}")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
