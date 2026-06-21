#!/usr/bin/env python3
"""run_terasort.py — Faasm inter-node TeraSort experiment driver (runs on node 0).

The counterpart to ../wasmem/run_terasort.py, measured the SAME way so the two are
comparable. TeraSort is an all-to-all shuffle, run here as two Faaslet waves across
the cluster, with all state crossing nodes through Redis (the serialized transfer
WasMem moves zero-copy):

  1. splitter (node 0)  : N newline-aligned record chunks → Redis  {uid}_chunk_{i}
  2. partition wave     : N Faaslets across nodes — each reads its chunk, tags every
                          record with its range-owner, scatters into N Redis buckets
                          {uid}_bucket_{i}_{j}  (the shuffle scatter).
  3. merge wave         : N range-owner Faaslets across nodes — each gathers its
                          column {uid}_bucket_{*}_{j}, sorts the keys, writes a
                          summary {uid}_summary_{j}  (the shuffle gather).
  4. aggregate (node 0) : sum records + keysum, AND the per-range sorted flag → the
                          fan-out-invariant gate (records, keysum, sorted).

The driver barriers between the two waves (Redis is the sync point), so makespan =
t0 (before the chunk upload) → last merge complete + result saved — end to end, the
partitioner-free window the WasMem driver also reports.

ts.cwasm (reused from the intra-node TeraSort baseline) + ts_faaslet.py are git-synced
to every node at the same abs path. Topology/Redis from ../../Inter-Node deployment/
faasm/cluster.env.

Usage:
  ./run_terasort.py --records TestData/terasort/records_32mb.txt --workers 4 --nodes 4 --reps 3
  ./run_terasort.py --workers 1 --nodes 1                 # ground truth (all on node 0)
  ./run_terasort.py --expect 335544,216366291,1           # gate against a known triple
"""
import argparse
import os
import sys
import time
import uuid

import redis

import faasm_lib as fl

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
TS_CWASM = os.path.join(ROOT, "Tests", "Intra-Node Application_Benchmark", "TeraSort",
                        "baseline", "faasm", "demo", "ts.cwasm")
WRAPPER = os.path.join(HERE, "ts_faaslet.py")
RESULT = os.path.join(ROOT, "TestOutput", "terasort_faasm_result.txt")


def split_aligned(data, n):
    """N contiguous newline-aligned chunks (matches the intra/wordcount split)."""
    total, approx, out, s = len(data), len(data) // n, [], 0
    for i in range(n):
        if i == n - 1:
            e = total
        else:
            e = s + approx
            while e < total and data[e] != 0x0A:
                e += 1
            if e < total:
                e += 1
        out.append(data[s:e]); s = e
    return out


def parse_summary(blob):
    """ts.cwasm merge summary → dict of bytes k→v."""
    return dict(tok.split(b"=", 1) for tok in (blob or b"").split() if b"=" in tok)


def write_result(path, records, keysum, srt, ordered, size_mb, workers, nodes_used):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write("=== terasort ===\n")
        f.write(f"size_mb={size_mb}\nworkers={workers}\nnodes_used={nodes_used}\n")
        f.write(f"records={records}\nkeysum={keysum}\nsorted={srt}\nordered={int(ordered)}\n")


def main():
    env, nodes, root = fl.load_env()
    ap = argparse.ArgumentParser()
    ap.add_argument("--records", default=os.path.join(root, "TestData", "terasort", "records_32mb.txt"))
    ap.add_argument("--workers", type=int, default=None,
                    help="partition/owner fan width (default = nodes, the N==P sweet spot)")
    ap.add_argument("--nodes", type=int, default=4, help="spread Faaslets over the first K nodes")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--expect", default=None,
                    help="gate against 'records,keysum,sorted' (e.g. 335544,216366291,1)")
    ap.add_argument("--csv", default=os.path.join(HERE, "results_terasort.csv"))
    args = ap.parse_args()

    nodes = nodes[:max(1, min(args.nodes, len(nodes)))]
    N = args.workers if args.workers is not None else len(nodes)
    port = int(env.get("AGENT_PORT", "9600"))
    rh, rp = env["REDIS_HOST"], int(env.get("REDIS_PORT", "6379"))
    fenv = {"REDIS_HOST": rh, "REDIS_PORT": str(rp), "TS_CWASM": TS_CWASM,
            "WASMTIME": env.get("WASMTIME", "wasmtime")}

    expect = None
    if args.expect is not None:
        try:
            expect = tuple(int(x) for x in args.expect.split(","))
            assert len(expect) == 3
        except (ValueError, AssertionError):
            sys.exit("[terasort] --expect must be 'records,keysum,sorted' (3 ints)")

    r = redis.Redis(host=rh, port=rp)
    try:
        r.ping()
    except Exception as ex:
        sys.exit(f"[terasort] Redis unreachable at {rh}:{rp} ({ex})")
    fl.health_check(nodes, port)
    if not os.path.exists(TS_CWASM):
        sys.exit(f"[terasort] ts.cwasm missing on node 0: {TS_CWASM}")

    data = open(args.records, "rb").read()
    size_mb = round(len(data) / (1024 * 1024))

    new = not os.path.exists(args.csv)
    fcsv = open(args.csv, "a")
    if new:
        fcsv.write("size_mb,workers,nodes_used,makespan_mean_ms,makespan_std_ms,total_job_mean_ms,"
                   "records,keysum,sorted,expect,success,reps\n")

    print(f"[terasort] records={size_mb}MB workers={N} nodes={len(nodes)} reps={args.reps}")
    ms_list, job_list, gate_seen, ok_all = [], [], None, True
    for rep in range(1, args.reps + 1):
        uid = uuid.uuid4().hex
        r.flushdb()                                      # cold Redis — no warm state across reps (untimed)
        t0 = time.time()                                 # end-to-end clock: split/upload → shuffle → sort
        for i, ch in enumerate(split_aligned(data, N)):  # splitter → KV (inside the timed window)
            r.set(f"{uid}_chunk_{i}", ch)

        # Wave 1 — partition Faaslets across the cluster (scatter into Redis buckets).
        pspecs = [{"node": nodes[i % len(nodes)], "tag": f"part_{i}",
                   "cmd": ["python3", WRAPPER, "partition", uid, str(i), str(N)],
                   "env": fenv} for i in range(N)]
        _, ok1, per1 = fl.run_faaslets(pspecs, port, t0=t0)

        # Barrier: all buckets are now in Redis → Wave 2 merge Faaslets (gather+sort).
        mspecs = [{"node": nodes[j % len(nodes)], "tag": f"merge_{j}",
                   "cmd": ["python3", WRAPPER, "merge", uid, str(j), str(N)],
                   "env": fenv} for j in range(N)]
        _, ok2, per2 = fl.run_faaslets(mspecs, port, t0=t0)

        # Aggregate the per-owner summaries → the fan-out-invariant gate.
        records = keysum = 0
        allsorted, ordered, prev_last, firsts = True, True, None, {}
        for j in range(N):
            kv = parse_summary(r.get(f"{uid}_summary_{j}"))
            n = int(kv.get(b"records", 0))
            records += n
            keysum += int(kv.get(b"keysum", 0))
            if kv.get(b"sorted") != b"1":
                allsorted = False
            if n > 0:
                firsts[kv[b"first"]] = kv[b"last"]
        for first in sorted(firsts):                     # global cross-range order check
            if prev_last is not None and first < prev_last:
                ordered = False
            prev_last = firsts[first]

        srt = 1 if (allsorted and ordered) else 0
        write_result(RESULT, records, keysum, srt, ordered, size_mb, N, len(nodes))
        makespan = int((time.time() - t0) * 1000)        # clock stops after gather+save
        job_ms = sum(per1.values()) + sum(per2.values())

        g = (records, keysum, srt)
        gate = expect if expect is not None else (gate_seen if gate_seen is not None else g)
        gate_seen = g
        success = ok1 and ok2 and g == gate and srt == 1
        ok_all = ok_all and success
        ms_list.append(makespan)
        job_list.append(job_ms)
        print(f"[terasort] rep {rep}: makespan={makespan}ms total_job={job_ms}ms "
              f"gate={g} expect={gate} ok={success}")

    mean_ms, std_ms = fl.mean_std(ms_list)
    mean_ms, std_ms = int(mean_ms), round(std_ms, 1)
    job_mean = int(sum(job_list) / len(job_list))
    gate = expect if expect is not None else gate_seen
    rec, ks, srt = gate_seen if gate_seen is not None else (None, None, None)
    exp_str = ("|".join(str(x) for x in gate)) if gate is not None else ""
    fcsv.write(f"{size_mb},{N},{len(nodes)},{mean_ms},{std_ms},{job_mean},"
               f"{rec},{ks},{srt},{exp_str},{ok_all},{args.reps}\n")
    fcsv.close()
    print(f"[terasort] mean makespan={mean_ms}±{std_ms}ms total_job={job_mean}ms gate={gate_seen} "
          f"success={ok_all} → {args.csv}")
    print(f"[terasort] result saved → {RESULT}")
    print(f"RESULT gate={gate_seen} makespan_ms={mean_ms} std={std_ms} success={ok_all}")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
