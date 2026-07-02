#!/usr/bin/env python3
"""run_finra.py — Faasm inter-node FINRA experiment driver (runs on node 0).

Hybrid-shard audit fan, mirroring the WasMem side (`gen_variants finra --fanout F`):
the 5 STATELESS per-record rules (0,1,5,6,7) are each sharded into S disjoint trade
slices (per-shard violation counts SUM to the full-data count — exact), while the 3
STATEFUL rules (2,3,4 = wash/spoofing/concentration) stay single full-data Faaslets
(their cross-(account,symbol) state can't be reconstructed from a slice). Effective
fan = 3 + 5·S, so a requested fanout F → S = round((F-3)/5) (F=60 → S=11 → 58 workers).

Each Faaslet is a fresh `finra.cwasm` (wasm32-wasip1, reused from the intra baseline)
fed its trades CSV via the generic faaslet_run.py; the driver sums all partials. The
gate is total_violations (fan-out/placement-invariant). `finra.cwasm` skips the FIRST
input line as the header, so each shard gets the header PREPENDED (else its first data
line would be dropped). State crosses nodes through Redis — the serialized transfer
WasMem moves zero-copy; per the methodology the trades upload is Faasm's cost (timed).

Topology/Redis from ../../Inter-Node deployment/faasm/cluster.env. finra.cwasm +
faaslet_run.py are git-synced to every node.

Usage:
  ./run_finra.py --trades TestData/finra/trades_5000000.csv --fanout 60 --nodes 4 --reps 3
  ./run_finra.py --trades …trades_100000.csv --fanout 60 --expect 49763
"""
import argparse
import os
import sys
import time
import uuid

import redis

import faasm_lib as fl

HERE = os.path.dirname(os.path.abspath(__file__))
FINRA_STATEFUL = (2, 3, 4)            # WASH_TRADE, SPOOFING, CONCENTRATION — need all records
FINRA_STATELESS = (0, 1, 5, 6, 7)     # PRICE_OUTLIER, LARGE_ORDER, AFTER_HOURS, PENNY_STOCK, ROUND_LOT


def split_shards(trades, s):
    """Split the trades CSV into `s` newline-aligned shards, each = header + 1/s of
    the data rows. The header is prepended so finra.cwasm's header-skip lands on it
    (not a real trade). Disjoint + complete over data rows → per-record counts sum."""
    nl = trades.find(b"\n")
    header, data = trades[:nl + 1], trades[nl + 1:]
    total, approx, out, start = len(data), len(data) // max(1, s), [], 0
    for i in range(s):
        if i == s - 1:
            end = total
        else:
            end = start + approx
            while end < total and data[end] != 0x0A:
                end += 1
            if end < total:
                end += 1
        out.append(header + data[start:end])
        start = end
    return out


def main():
    env, nodes, root = fl.load_env()
    ap = argparse.ArgumentParser()
    ap.add_argument("--trades", default=os.path.join(root, "TestData", "finra", "trades_100000.csv"))
    ap.add_argument("--fanout", type=int, default=60, help="audit-rule fan width (~F workers; F=60→58)")
    ap.add_argument("--nodes", type=int, default=4)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--expect", type=int, default=None, help="gate violations against this count")
    ap.add_argument("--csv", default=os.path.join(HERE, "results_finra.csv"))
    args = ap.parse_args()

    nodes = fl.map_nodes(nodes, args.nodes)   # compute faaslets on WORKER nodes only (node-0 = coordinator)
    S = max(1, round((args.fanout - len(FINRA_STATEFUL)) / len(FINRA_STATELESS)))
    n_workers = len(FINRA_STATELESS) * S + len(FINRA_STATEFUL)
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
        fcsv.write("trades,fanout,nodes_used,makespan_mean_ms,makespan_std_ms,total_job_mean_ms,"
                   "violations,expect,success,reps\n")

    print(f"[finra] trades={n_trades} fanout={args.fanout} shards/rule={S} workers={n_workers} "
          f"(5 stateless×{S} + 3 stateful) nodes={len(nodes)} reps={args.reps}")
    ms_list, job_list, viol_seen, ok_all = [], [], None, True
    for rep in range(1, args.reps + 1):
        uid = uuid.uuid4().hex
        r.flushdb()                                  # cold Redis — no warm state across reps (untimed)
        t0 = time.time()                             # end-to-end clock incl. the trades upload (Faasm's KV cost)
        r.set(f"{uid}_trades", trades)               # full set for the 3 stateful rules
        for s, blob in enumerate(split_shards(trades, S)):
            r.set(f"{uid}_shard_{s}", blob)          # header-prefixed slice for the stateless shards

        specs = []
        # Stateless rules: one Faaslet per (rule, shard) — scans its 1/S slice.
        for rule in FINRA_STATELESS:
            for s in range(S):
                specs.append({"node": nodes[len(specs) % len(nodes)], "tag": f"r{rule}_s{s}",
                              "cmd": ["python3", wrap, module, f"{uid}_shard_{s}",
                                      f"{uid}_count_{rule}_{s}", "--", str(rule)], "env": fenv})
        # Stateful rules: one full-data Faaslet each.
        for rule in FINRA_STATEFUL:
            specs.append({"node": nodes[len(specs) % len(nodes)], "tag": f"r{rule}_full",
                          "cmd": ["python3", wrap, module, f"{uid}_trades",
                                  f"{uid}_count_{rule}_full", "--", str(rule)], "env": fenv})

        _, ok, per = fl.run_faaslets(specs, port, t0=t0)
        total = 0                                    # sum every partial (stateless shards + stateful full)
        for rule in FINRA_STATELESS:
            for s in range(S):
                total += int((r.get(f"{uid}_count_{rule}_{s}") or b"0").strip() or 0)
        for rule in FINRA_STATEFUL:
            total += int((r.get(f"{uid}_count_{rule}_full") or b"0").strip() or 0)
        makespan = int((time.time() - t0) * 1000)    # clock stops after the aggregate (mirrors WasMem reduce)
        job_ms = sum(per.values())                   # aggregate per-node Faaslet execution time (cluster compute)

        gate = args.expect if args.expect is not None else (viol_seen if viol_seen is not None else total)
        viol_seen = total
        success = ok and total == gate
        ok_all = ok_all and success
        ms_list.append(makespan)
        job_list.append(job_ms)
        print(f"[finra] rep {rep}: makespan={makespan}ms total_job={job_ms}ms "
              f"violations={total} gate={gate} ok={success}")

    mean_ms, std_ms = fl.mean_std(ms_list)
    mean_ms, std_ms = int(mean_ms), round(std_ms, 1)
    job_mean = int(sum(job_list) / len(job_list))
    gate = args.expect if args.expect is not None else viol_seen
    fcsv.write(f"{n_trades},{n_workers},{len(nodes)},{mean_ms},{std_ms},{job_mean},"
               f"{viol_seen},{gate},{ok_all},{args.reps}\n")
    fcsv.close()
    print(f"[finra] mean makespan={mean_ms}±{std_ms}ms total_job={job_mean}ms "
          f"violations={viol_seen} success={ok_all} → {args.csv}")
    print(f"RESULT viol={viol_seen} makespan_ms={mean_ms} std={std_ms} success={ok_all}")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
