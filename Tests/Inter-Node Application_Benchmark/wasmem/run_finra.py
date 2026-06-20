#!/usr/bin/env python3
"""run_finra.py — WasMem (auto-placement) inter-node FINRA driver (node 0).

Counterpart to ../faasm/run_finra.py, measured the same way. FINRA's parallelism is
the hybrid-sharded audit fan: `gen_variants.py finra --fanout F` shards the 5 stateless
per-record rules into S = round((F-3)/5) shards each (+3 stateful) → ~F audit workers,
spread by the balanced policy. Each shard scans a disjoint 1/S of the trades; the
aggregator sums per-(rule,shard) partials. Gate is total_violations (placement-invariant).

  1. OFFLINE, UNTIMED — gen_variants finra → partition --nodes N → ClusterDag.
  2. TIMED — node-agent submit; makespan = coordinator `total wall time` (excludes the
     partitioner). Σ per-node durations = total_job_ms. Violations read from the `save`
     Output node (TestOutput/finra_ap_result.txt).

Usage:
  ./run_finra.py --trades TestData/finra/trades_10000000.csv --fanout 60 --nodes 4 --reps 3
"""
import argparse
import json
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
RESULT = os.path.join(ROOT, "TestOutput", "finra_ap_result.txt")


def median(xs):
    xs = sorted(xs)
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def gen_sym(trades_rel, fanout, nodes, policy, pack_cap, out_path):
    """OFFLINE (untimed): gen_variants finra hybrid-shard at fanout, Input → trades_rel."""
    cmd = [sys.executable, GEN, "finra", policy, "--nodes", str(nodes),
           "--fanout", str(fanout), "--pack-cap", str(pack_cap),
           "--input", trades_rel, "--out", out_path]
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"[finra] gen_variants failed: {(r.stderr + r.stdout)[:800]}")


def partition(sym_path, nodes, cdag_path):
    with open(cdag_path, "w") as f:
        r = subprocess.run([PART, sym_path, "--nodes", str(nodes)], stdout=f,
                           stderr=subprocess.PIPE, text=True)
    if r.returncode != 0:
        sys.exit(f"[finra] partition failed: {r.stderr[:800]}")


def submit(cdag_path):
    """TIMED — submit to the running coordinator; parse Job Summary."""
    cmd = [NODE_AGENT, "submit", "--config", AGENT_TOML, "--dag", cdag_path, "--aot"]
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    out = r.stdout + "\n" + r.stderr
    if r.returncode != 0:
        sys.exit(f"[finra] submit failed (rc={r.returncode}):\n{out[-1500:]}")
    m = re.search(r"total wall time:\s*(\d+)\s*ms", out)
    if not m:
        sys.exit(f"[finra] could not parse 'total wall time':\n{out[-1500:]}")
    makespan = int(m.group(1))
    per_node = [int(x) for x in re.findall(r"node \d+ \((?:local|worker)\):\s*(\d+)\s*ms", out)]
    ok = ("Success: true" in out) or ("Success: True" in out)
    return makespan, sum(per_node), per_node, ok


def read_violations():
    try:
        for line in open(RESULT):
            m = re.match(r"total_violations=(\d+)", line.strip())
            if m:
                return int(m.group(1))
    except FileNotFoundError:
        pass
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--trades", default="TestData/finra/trades_10000000.csv",
                    help="trades path RELATIVE to repo root (RDMA-staged from node 0)")
    ap.add_argument("--fanout", type=int, default=60, help="audit-rule fan width (~F workers)")
    ap.add_argument("--nodes", type=int, default=4)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--policy", default="balanced")
    ap.add_argument("--pack-cap", type=int, default=16)
    ap.add_argument("--expect", type=int, default=None, help="gate violations against this count")
    ap.add_argument("--csv", default=os.path.join(HERE, "results_finra.csv"))
    args = ap.parse_args()

    trades_abs = os.path.join(ROOT, args.trades)
    if not os.path.exists(trades_abs):
        sys.exit(f"[finra] trades not found on node 0: {trades_abs}")
    size_mb = round(os.path.getsize(trades_abs) / (1024 * 1024))
    n_trades = max(0, sum(1 for _ in open(trades_abs)) - 1)
    for binp in (PART, NODE_AGENT, GEN):
        if not os.path.exists(binp):
            sys.exit(f"[finra] missing: {binp}")

    # OFFLINE gen + partition (untimed).
    dags = os.path.join(HERE, "dags")
    os.makedirs(dags, exist_ok=True)
    sym = os.path.join(dags, f"finra_{n_trades}_f{args.fanout}.sym.json")
    cdag = os.path.join(dags, f"finra_{n_trades}_f{args.fanout}.cdag.json")
    gen_sym(args.trades, args.fanout, args.nodes, args.policy, args.pack_cap, sym)
    partition(sym, args.nodes, cdag)

    new = not os.path.exists(args.csv)
    fcsv = open(args.csv, "a")
    if new:
        fcsv.write("trades,fanout,nodes_used,makespan_ms_median,total_job_ms_median,"
                   "violations,expect,success,reps\n")

    print(f"[finra] trades={n_trades} ({size_mb}MB) fanout={args.fanout} nodes={args.nodes} "
          f"policy={args.policy} reps={args.reps} (partition offline/untimed)")
    ms_list, job_list, viol_seen, ok_all = [], [], None, True
    for rep in range(1, args.reps + 1):
        makespan, total_job, per_node, ok = submit(cdag)
        viol = read_violations()
        gate = args.expect if args.expect is not None else (viol_seen if viol_seen is not None else viol)
        viol_seen = viol
        success = ok and viol == gate
        ok_all = ok_all and success
        ms_list.append(makespan)
        job_list.append(total_job)
        print(f"[finra] rep {rep}: makespan={makespan}ms total_job={total_job}ms "
              f"per_node={per_node} violations={viol} gate={gate} ok={success}")

    med = int(median(ms_list))
    job_med = int(median(job_list))
    gate = args.expect if args.expect is not None else viol_seen
    fcsv.write(f"{n_trades},{args.fanout},{args.nodes},{med},{job_med},"
               f"{viol_seen},{gate},{ok_all},{args.reps}\n")
    fcsv.close()
    print(f"[finra] median makespan={med}ms total_job={job_med}ms violations={viol_seen} "
          f"success={ok_all} → {args.csv}")
    print(f"RESULT viol={viol_seen} makespan_ms={med} success={ok_all}")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
