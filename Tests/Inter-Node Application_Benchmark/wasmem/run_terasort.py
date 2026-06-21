#!/usr/bin/env python3
"""run_terasort.py — WasMem (auto-placement) inter-node TeraSort driver (node 0).

Counterpart to ../faasm/run_terasort.py, measured the SAME way. TeraSort's parallelism
is the all-to-all shuffle: `gen_variants.py terasort --nodes N --fanout F` builds F
partition workers (each scans a disjoint 1/F of the records and routes every record to
its range-owner) + F range-owners (each sorts the records that fall in its key range).
At the deadlock-safe sweet spot **F == nodes** every directed node-pair carries exactly
ONE stream (the partitioner orders the bidirectional transpose to avoid deadlock); F > N
puts several owners per node and needs the multi-stream-per-peer path (see STATUS §gaps).

NOTE (multi-node path): the executor's blocking RDMA transport deadlocks on a true
all-to-all transpose, so the cluster DAG uses a CENTRALIZED gather — workers route +
local-combine, send ONE transfer each, node 0 runs the final merge/sort. Distributed
partition, single-reducer merge.

  1. OFFLINE, UNTIMED — gen_variants terasort → partition --nodes N → ClusterDag.
  2. TIMED — node-agent submit; makespan = coordinator `total wall time` (excludes the
     partitioner). Σ per-node durations = total_job_ms. The `save` Output node writes
     TestOutput/terasort_ap_result.txt; we gate the composite (records, keysum, sorted),
     all fan-out-invariant.

Usage:
  ./run_terasort.py --records TestData/terasort/records_32mb.txt --nodes 4 --reps 3
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
RESULT = os.path.join(ROOT, "TestOutput", "terasort_ap_result.txt")


def median(xs):
    xs = sorted(xs)
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def gen_sym(records_rel, fanout, nodes, policy, pack_cap, out_path, shard_input=False):
    """OFFLINE (untimed): gen_variants terasort all-to-all shuffle, Input → records_rel."""
    cmd = [sys.executable, GEN, "terasort", policy, "--nodes", str(nodes),
           "--fanout", str(fanout), "--pack-cap", str(pack_cap),
           "--input", records_rel, "--out", out_path]
    if shard_input:
        cmd.append("--shard-input")
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"[terasort] gen_variants failed: {(r.stderr + r.stdout)[:800]}")


def partition(sym_path, nodes, cdag_path):
    with open(cdag_path, "w") as f:
        r = subprocess.run([PART, sym_path, "--nodes", str(nodes)], stdout=f,
                           stderr=subprocess.PIPE, text=True)
    if r.returncode != 0:
        sys.exit(f"[terasort] partition failed: {r.stderr[:800]}")


def submit(cdag_path):
    """TIMED — submit to the running coordinator; parse Job Summary."""
    cmd = [NODE_AGENT, "submit", "--config", AGENT_TOML, "--dag", cdag_path, "--aot"]
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    out = r.stdout + "\n" + r.stderr
    if r.returncode != 0:
        sys.exit(f"[terasort] submit failed (rc={r.returncode}):\n{out[-1500:]}")
    m = re.search(r"total wall time:\s*(\d+)\s*ms", out)
    if not m:
        sys.exit(f"[terasort] could not parse 'total wall time':\n{out[-1500:]}")
    makespan = int(m.group(1))
    per_node = [int(x) for x in re.findall(r"node \d+ \((?:local|worker)\):\s*(\d+)\s*ms", out)]
    ok = ("Success: true" in out) or ("Success: True" in out)
    return makespan, sum(per_node), per_node, ok


def read_gate():
    """Composite fan-out-invariant gate: (records, keysum, sorted). None if absent."""
    vals = {}
    try:
        for line in open(RESULT):
            m = re.match(r"(records|keysum|sorted)=(-?\d+)", line.strip())
            if m:
                vals[m.group(1)] = int(m.group(2))
    except FileNotFoundError:
        return None
    if not {"records", "keysum", "sorted"} <= vals.keys():
        return None
    return (vals["records"], vals["keysum"], vals["sorted"])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--records", default="TestData/terasort/records_32mb.txt",
                    help="records path RELATIVE to repo root (RDMA-staged from node 0)")
    ap.add_argument("--fanout", type=int, default=None,
                    help="partition/owner fan width (default = nodes, the deadlock-safe N==P)")
    ap.add_argument("--nodes", type=int, default=4)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--policy", default="balanced")
    ap.add_argument("--pack-cap", type=int, default=16)
    ap.add_argument("--expect", default=None,
                    help="gate against 'records,keysum,sorted' (e.g. 335544,216366291,1)")
    ap.add_argument("--shard-input", action="store_true",
                    help="slice input across nodes (1/P each) instead of replicate — "
                         "lifts the ~512MB SHM ceiling toward ~1.3GB (needs sharded guest)")
    ap.add_argument("--csv", default=os.path.join(HERE, "results_terasort.csv"))
    args = ap.parse_args()

    records_abs = os.path.join(ROOT, args.records)
    if not os.path.exists(records_abs):
        sys.exit(f"[terasort] records not found on node 0: {records_abs}")
    size_mb = round(os.path.getsize(records_abs) / (1024 * 1024))
    fanout = args.fanout if args.fanout is not None else args.nodes
    for binp in (PART, NODE_AGENT, GEN):
        if not os.path.exists(binp):
            sys.exit(f"[terasort] missing: {binp}")

    expect = None
    if args.expect is not None:
        try:
            expect = tuple(int(x) for x in args.expect.split(","))
            assert len(expect) == 3
        except (ValueError, AssertionError):
            sys.exit("[terasort] --expect must be 'records,keysum,sorted' (3 ints)")

    # OFFLINE gen + partition (untimed).
    dags = os.path.join(HERE, "dags")
    os.makedirs(dags, exist_ok=True)
    sym = os.path.join(dags, f"terasort_{size_mb}mb_f{fanout}.sym.json")
    cdag = os.path.join(dags, f"terasort_{size_mb}mb_f{fanout}.cdag.json")
    gen_sym(args.records, fanout, args.nodes, args.policy, args.pack_cap, sym,
            shard_input=args.shard_input)
    partition(sym, args.nodes, cdag)

    new = not os.path.exists(args.csv)
    fcsv = open(args.csv, "a")
    if new:
        fcsv.write("size_mb,fanout,nodes_used,makespan_ms_median,total_job_ms_median,"
                   "records,keysum,sorted,expect,success,reps\n")

    print(f"[terasort] records={size_mb}MB fanout={fanout} nodes={args.nodes} "
          f"policy={args.policy} reps={args.reps} (partition offline/untimed)")
    ms_list, job_list, gate_seen, ok_all = [], [], None, True
    for rep in range(1, args.reps + 1):
        # Clear the result so a stale read can't masquerade as success.
        try:
            os.remove(RESULT)
        except FileNotFoundError:
            pass
        makespan, total_job, per_node, ok = submit(cdag)
        g = read_gate()
        gate = expect if expect is not None else (gate_seen if gate_seen is not None else g)
        gate_seen = g
        success = ok and g is not None and g == gate and (g[2] == 1)  # sorted must be 1
        ok_all = ok_all and success
        ms_list.append(makespan)
        job_list.append(total_job)
        print(f"[terasort] rep {rep}: makespan={makespan}ms total_job={total_job}ms "
              f"per_node={per_node} gate={g} expect={gate} ok={success}")

    med = int(median(ms_list))
    job_med = int(median(job_list))
    gate = expect if expect is not None else gate_seen
    rec, ks, srt = (gate_seen if gate_seen is not None else (None, None, None))
    exp_str = ("|".join(str(x) for x in gate)) if gate is not None else ""
    fcsv.write(f"{size_mb},{fanout},{args.nodes},{med},{job_med},"
               f"{rec},{ks},{srt},{exp_str},{ok_all},{args.reps}\n")
    fcsv.close()
    print(f"[terasort] median makespan={med}ms total_job={job_med}ms gate={gate_seen} "
          f"success={ok_all} → {args.csv}")
    print(f"RESULT gate={gate_seen} makespan_ms={med} success={ok_all}")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
