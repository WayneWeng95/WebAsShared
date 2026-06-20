#!/usr/bin/env python3
"""run_finra.py — Faasm inter-node FINRA experiment driver (runs on node 0).

FINRA is a fixed 8-rule fan: the trades are broadcast to Redis once, then 8 audit
rules run across the cluster — each a fresh finra.cwasm Faaslet fed the trades
(via the generic faaslet_run.py), emitting its violation count — and the driver
sums them. The gate is total_violations (fan-out/placement-invariant: each rule
scans the full trades independently, so the sum is identical at any placement).

Topology/Redis from ../../Inter-Node deployment/faasm/cluster.env. finra.cwasm +
faaslet_run.py are git-synced to every node.

Usage:
  ./run_finra.py --nodes 4 --reps 3                 # distributed (8 rules / 4 nodes)
  ./run_finra.py --nodes 1                          # ground truth (all rules on node 0)
  ./run_finra.py --trades TestData/finra/trades_100000.csv --expect 1952
"""
import argparse
import os
import sys
import uuid

import redis

import faasm_lib as fl

HERE = os.path.dirname(os.path.abspath(__file__))
N_RULES = 8


def main():
    env, nodes, root = fl.load_env()
    ap = argparse.ArgumentParser()
    ap.add_argument("--trades", default=os.path.join(root, "TestData", "finra", "trades_100000.csv"))
    ap.add_argument("--nodes", type=int, default=4)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--expect", type=int, default=None, help="gate violations against this count")
    ap.add_argument("--csv", default=os.path.join(HERE, "results_finra.csv"))
    args = ap.parse_args()

    nodes = nodes[:max(1, min(args.nodes, len(nodes)))]
    port = int(env.get("AGENT_PORT", "9600"))
    rh, rp = env["REDIS_HOST"], int(env.get("REDIS_PORT", "6379"))
    module = os.path.join(root, "Tests", "Intra-Node Application_Benchmark", "Finra",
                          "baseline", "faasm", "demo", "finra.cwasm")
    wrap = fl.generic_wrapper()

    r = redis.Redis(host=rh, port=rp)
    try:
        r.ping()
    except Exception as ex:
        sys.exit(f"[finra] Redis unreachable at {rh}:{rp} ({ex})")
    fl.health_check(nodes, port)

    trades = open(args.trades, "rb").read()
    n_trades = max(0, trades.count(b"\n") - 1)
    fenv = {"REDIS_HOST": rh, "REDIS_PORT": str(rp), "WASMTIME": env.get("WASMTIME", "wasmtime")}

    new = not os.path.exists(args.csv)
    fcsv = open(args.csv, "a")
    if new:
        fcsv.write("trades,nodes_used,makespan_ms_median,violations,expect,success,reps\n")

    print(f"[finra] trades={n_trades} rules={N_RULES} nodes={len(nodes)} reps={args.reps}")
    ms_list, viol_seen, ok_all = [], None, True
    for rep in range(1, args.reps + 1):
        uid = uuid.uuid4().hex
        r.flushdb()                      # cold Redis — no warm state carried across reps (untimed)
        t0 = time.time()                 # end-to-end clock: input upload → dispatch → compute
        r.set(f"{uid}_trades", trades)   # broadcast once (now inside the timed window)
        specs = [{"node": nodes[i % len(nodes)], "tag": f"rule_{i}",
                  "cmd": ["python3", wrap, module, f"{uid}_trades", f"{uid}_count_{i}", "--", str(i)],
                  "env": fenv} for i in range(N_RULES)]
        makespan, ok, _ = fl.run_faaslets(specs, port, t0=t0)
        total = sum(int((r.get(f"{uid}_count_{i}") or b"0").strip() or 0) for i in range(N_RULES))
        gate = args.expect if args.expect is not None else (viol_seen if viol_seen is not None else total)
        viol_seen = total
        success = ok and total == gate
        ok_all = ok_all and success
        ms_list.append(makespan)
        print(f"[finra] rep {rep}: makespan={makespan}ms violations={total} gate={gate} ok={success}")

    med = int(fl.median(ms_list))
    gate = args.expect if args.expect is not None else viol_seen
    fcsv.write(f"{n_trades},{min(N_RULES, len(nodes))},{med},{viol_seen},{gate},{ok_all},{args.reps}\n")
    fcsv.close()
    print(f"[finra] median makespan={med}ms violations={viol_seen} success={ok_all} → {args.csv}")
    print(f"RESULT viol={viol_seen} makespan_ms={med} success={ok_all}")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
