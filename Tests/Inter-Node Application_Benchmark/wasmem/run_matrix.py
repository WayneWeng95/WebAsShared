#!/usr/bin/env python3
"""run_matrix.py — WasMem (auto-placement) inter-node Matrix (SUMMA) driver (node 0).

Counterpart to ../faasm/run_matrix.py, measured the SAME way. Matrix multiply C = A·B
is a homogeneous r·c-wide block fan: `gen_variants.py matrix --fanout W --matrix-n N`
builds W = r·c block workers (block_i_j computes C_ij = A_i*·B_*j), placed per policy and
gathered (per-machine local-combine, one transfer/peer) into a global checksum. The gate
is the f64 checksum (Σ of all C entries) — EXACT (integer entries < 2^53) and
order-independent, so it's identical at every fanout/placement.

  1. OFFLINE, UNTIMED — gen_variants matrix → partition --nodes N → ClusterDag.
  2. TIMED — node-agent submit; makespan = coordinator `total wall time` (excludes the
     partitioner). Σ per-node durations = total_job_ms. checksum read from the `save`
     Output node (TestOutput/matrix_ap_result.txt).

A_<N>.bin / B_<N>.bin (binary, replicated, RDMA-staged from node 0) must exist for the
chosen N. The compute is N³-bound (naive, not BLAS — fairness §5), so the inputs are tiny
and the SHM/replicate limits that cap TeraSort do not apply here.

Usage:
  ./run_matrix.py --matrix-n 512 --fanout 4 --nodes 4 --reps 3
"""
import argparse
import math
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
RESULT = os.path.join(ROOT, "TestOutput", "matrix_ap_result.txt")


def mean_std(xs):
    """(mean, sample-stdev); stdev=0 for <2 samples. Headline summary for N reps."""
    m = sum(xs) / len(xs)
    if len(xs) < 2:
        return m, 0.0
    return m, (sum((x - m) ** 2 for x in xs) / (len(xs) - 1)) ** 0.5


def gen_sym(matrix_n, fanout, nodes, policy, pack_cap, out_path):
    """OFFLINE (untimed): gen_variants matrix SUMMA grid (A/B paths derive from N)."""
    cmd = [sys.executable, GEN, "matrix", policy, "--nodes", str(nodes),
           "--fanout", str(fanout), "--pack-cap", str(pack_cap),
           "--matrix-n", str(matrix_n), "--out", out_path]
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"[matrix] gen_variants failed: {(r.stderr + r.stdout)[:800]}")


def partition(sym_path, nodes, cdag_path):
    with open(cdag_path, "w") as f:
        r = subprocess.run([PART, sym_path, "--nodes", str(nodes)], stdout=f,
                           stderr=subprocess.PIPE, text=True)
    if r.returncode != 0:
        sys.exit(f"[matrix] partition failed: {r.stderr[:800]}")


def submit(cdag_path):
    """TIMED — submit to the running coordinator; parse Job Summary."""
    cmd = [NODE_AGENT, "submit", "--config", AGENT_TOML, "--dag", cdag_path, "--aot"]
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    out = r.stdout + "\n" + r.stderr
    if r.returncode != 0:
        sys.exit(f"[matrix] submit failed (rc={r.returncode}):\n{out[-1500:]}")
    m = re.search(r"total wall time:\s*(\d+)\s*ms", out)
    if not m:
        sys.exit(f"[matrix] could not parse 'total wall time':\n{out[-1500:]}")
    makespan = int(m.group(1))
    per_node = [int(x) for x in re.findall(r"node \d+ \((?:local|worker)\):\s*(\d+)\s*ms", out)]
    ok = ("Success: true" in out) or ("Success: True" in out)
    return makespan, sum(per_node), per_node, ok


def read_checksum():
    try:
        for line in open(RESULT):
            m = re.search(r"checksum=(-?\d+)", line.strip())
            if m:
                return int(m.group(1))
    except FileNotFoundError:
        pass
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--matrix-n", type=int, default=512, help="N×N product dimension (needs A_<N>.bin/B_<N>.bin)")
    ap.add_argument("--fanout", type=int, default=4, help="block count r·c (1,2,4,8,16 → balanced grid)")
    ap.add_argument("--nodes", type=int, default=4)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--policy", default="balanced")
    ap.add_argument("--pack-cap", type=int, default=16)
    ap.add_argument("--expect", type=int, default=None, help="gate checksum against this value")
    ap.add_argument("--csv", default=os.path.join(HERE, "results_matrix.csv"))
    args = ap.parse_args()

    n = args.matrix_n
    a_bin = os.path.join(ROOT, "TestData", "matrix", f"A_{n}.bin")
    b_bin = os.path.join(ROOT, "TestData", "matrix", f"B_{n}.bin")
    for f in (a_bin, b_bin):
        if not os.path.exists(f):
            sys.exit(f"[matrix] missing input on node 0: {f}")
    for binp in (PART, NODE_AGENT, GEN):
        if not os.path.exists(binp):
            sys.exit(f"[matrix] missing: {binp}")

    # OFFLINE gen + partition (untimed).
    dags = os.path.join(HERE, "dags")
    os.makedirs(dags, exist_ok=True)
    sym = os.path.join(dags, f"matrix_n{n}_f{args.fanout}.sym.json")
    cdag = os.path.join(dags, f"matrix_n{n}_f{args.fanout}.cdag.json")
    gen_sym(n, args.fanout, args.nodes, args.policy, args.pack_cap, sym)
    partition(sym, args.nodes, cdag)

    new = not os.path.exists(args.csv)
    fcsv = open(args.csv, "a")
    if new:
        fcsv.write("matrix_n,fanout,nodes_used,makespan_mean_ms,makespan_std_ms,total_job_mean_ms,"
                   "gflops,checksum,expect,success,reps\n")

    print(f"[matrix] N={n} fanout={args.fanout} nodes={args.nodes} policy={args.policy} "
          f"reps={args.reps} (partition offline/untimed)")
    ms_list, job_list, cks_seen, ok_all = [], [], None, True
    for rep in range(1, args.reps + 1):
        try:
            os.remove(RESULT)
        except FileNotFoundError:
            pass
        makespan, total_job, per_node, ok = submit(cdag)
        cks = read_checksum()
        gate = args.expect if args.expect is not None else (cks_seen if cks_seen is not None else cks)
        cks_seen = cks
        success = ok and cks is not None and cks == gate
        ok_all = ok_all and success
        ms_list.append(makespan)
        job_list.append(total_job)
        print(f"[matrix] rep {rep}: makespan={makespan}ms total_job={total_job}ms "
              f"per_node={per_node} checksum={cks} gate={gate} ok={success}")

    mean_ms, std_ms = mean_std(ms_list)
    mean_ms, std_ms = int(mean_ms), round(std_ms, 1)
    job_mean = int(sum(job_list) / len(job_list))
    gate = args.expect if args.expect is not None else cks_seen
    gflops = round(2.0 * n ** 3 / (mean_ms / 1000.0) / 1e9, 3) if mean_ms > 0 else 0  # 2N³ flops / s
    fcsv.write(f"{n},{args.fanout},{args.nodes},{mean_ms},{std_ms},{job_mean},"
               f"{gflops},{cks_seen},{gate},{ok_all},{args.reps}\n")
    fcsv.close()
    print(f"[matrix] mean makespan={mean_ms}±{std_ms}ms total_job={job_mean}ms gflops={gflops} "
          f"checksum={cks_seen} success={ok_all} → {args.csv}")
    print(f"RESULT checksum={cks_seen} makespan_ms={mean_ms} std={std_ms} gflops={gflops} success={ok_all}")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
