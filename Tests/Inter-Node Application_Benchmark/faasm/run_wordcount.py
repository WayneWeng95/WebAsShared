#!/usr/bin/env python3
"""run_wordcount.py — Faasm inter-node WordCount experiment driver (runs on node 0).

Splits the corpus into N newline-aligned chunks → Redis (Faasm KV), launches one
map Faaslet per chunk ACROSS the cluster via each node's agent, plus a reduce
Faaslet on node 0, times the run, reads the merged result, computes
total_occurrences (the fan-out-invariant correctness gate), and appends a
results.csv row. State crosses nodes through Redis — the serialized transfer
WasMem moves zero-copy.

Topology + agent port + Redis come from ../../Inter-Node deployment/faasm/cluster.env.
The wc.cwasm module + this wrapper are git-synced to every node (same abs path).

Usage:
  ./run_wordcount.py --corpus TestData/corpus_large.txt --mappers 8 --nodes 4 --reps 5
  ./run_wordcount.py --mappers 1 --nodes 1            # ground truth (all on node 0)
  ./run_wordcount.py --mappers 8 --expect 8940339     # gate against a known count
"""
import argparse
import json
import os
import sys
import time
import urllib.request
import uuid

import redis

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DEPLOY = os.path.join(ROOT, "Tests", "Inter-Node deployment", "faasm")
WC_CWASM = os.path.join(ROOT, "Tests", "Intra-Node Application_Benchmark",
                        "WordCount", "baseline", "faasm", "demo", "wc.cwasm")
WRAPPER = os.path.join(HERE, "wc_faaslet.py")


def load_env():
    env = {}
    with open(os.path.join(DEPLOY, "cluster.env")) as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if "=" in line:
                k, v = line.split("=", 1)
                env[k.strip()] = v.strip().strip('"')
    nodes = [env["COORD_IP"]] + env.get("WORKER_IPS", "").split()
    return env, nodes


def call(host, port, path, obj=None, timeout=900):
    data = json.dumps(obj).encode() if obj is not None else None
    req = urllib.request.Request(f"http://{host}:{port}{path}", data=data,
                                 method="POST" if obj is not None else "GET",
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def split_aligned(corpus, n):
    """N contiguous newline-aligned chunks (matches the intra wordcount split)."""
    total, approx, out, s = len(corpus), len(corpus) // n, [], 0
    for i in range(n):
        if i == n - 1:
            e = total
        else:
            e = s + approx
            while e < total and corpus[e] != 0x0A:
                e += 1
            if e < total:
                e += 1
        out.append(corpus[s:e]); s = e
    return out


def parse_counts(merged):
    """merged `word\\x1fcount\\n…` blob → sorted [(word, count)], total occurrences."""
    counts, occ = [], 0
    for line in (merged or b"").split(b"\n"):
        sep = line.rfind(b"\x1f")
        if sep == -1:
            continue
        try:
            c = int(line[sep + 1:])
        except ValueError:
            continue
        counts.append((line[:sep].decode(errors="replace"), c))
        occ += c
    counts.sort(key=lambda wc: (-wc[1], wc[0]))
    return counts, occ


def occurrences(merged):
    return parse_counts(merged)[1]


def write_result(path, merged, size_mb, mappers, nodes_used):
    """Save the merged result back on node 0 in the TestOutput word_count format
    (mirrors the auto-placement `save` Output node, so the file write is part of the
    timed end-to-end window)."""
    counts, occ = parse_counts(merged)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write("=== word_count ===\n")
        f.write(f"size_mb={size_mb}\nmappers={mappers}\nnodes_used={nodes_used}\n")
        f.write(f"unique_words={len(counts)}\ntotal_occurrences={occ}\n")
        for w, c in counts:
            f.write(f"{w}: {c}\n")
    return occ


def median(xs):
    xs = sorted(xs)
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", default=os.path.join(ROOT, "TestData", "corpus_large.txt"))
    ap.add_argument("--mappers", type=int, default=8)
    ap.add_argument("--nodes", type=int, default=4, help="spread map Faaslets over the first K nodes")
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--expect", type=int, default=None, help="gate occurrences against this count")
    ap.add_argument("--csv", default=os.path.join(HERE, "results.csv"))
    ap.add_argument("--out", default=os.path.join(ROOT, "TestOutput", "wc_faasm_result.txt"),
                    help="where node 0 saves the merged result (in the timed window)")
    args = ap.parse_args()

    env, nodes = load_env()
    nodes = nodes[:max(1, min(args.nodes, len(nodes)))]
    port = int(env.get("AGENT_PORT", "9600"))
    rh, rp = env["REDIS_HOST"], int(env.get("REDIS_PORT", "6379"))
    r = redis.Redis(host=rh, port=rp)
    try:
        r.ping()
    except Exception as ex:
        sys.exit(f"[wc] Redis unreachable at {rh}:{rp} — open protected-mode + bind on node 0 ({ex})")
    for n in nodes:
        try:
            call(n, port, "/health", timeout=5)
        except Exception as ex:
            sys.exit(f"[wc] agent down on {n}:{port} — run deployment/faasm/deploy.sh start ({ex})")

    corpus = open(args.corpus, "rb").read()
    size_mb = round(len(corpus) / (1024 * 1024))
    N = args.mappers
    fenv = {"REDIS_HOST": rh, "REDIS_PORT": str(rp), "WC_CWASM": WC_CWASM,
            "WASMTIME": env.get("WASMTIME", "wasmtime")}

    new = not os.path.exists(args.csv)
    fcsv = open(args.csv, "a")
    if new:
        fcsv.write("size_mb,mappers,nodes_used,makespan_ms_median,total_job_ms_median,"
                   "occurrences,expect,success,reps\n")

    print(f"[wc] corpus={size_mb}MB mappers={N} nodes={[*range(len(nodes))]} reps={args.reps}")
    ms_list, job_list, occ_seen, ok_all = [], [], None, True
    for rep in range(1, args.reps + 1):
        uid = uuid.uuid4().hex
        r.flushdb()                                         # cold Redis — no warm state across reps (untimed)
        t0 = time.time()                                    # end-to-end clock: split/upload → dispatch → compute
        for i, ch in enumerate(split_aligned(corpus, N)):   # splitter → KV (now inside the timed window)
            r.set(f"{uid}_chunk_{i}", ch)

        launched = []
        for i in range(N):                                  # map Faaslets across nodes
            n = nodes[i % len(nodes)]
            h = call(n, port, "/launch", {"cmd": ["python3", WRAPPER, "map", uid, str(i)],
                                          "env": fenv, "tag": f"map_{i}"})["handle"]
            launched.append((n, h))
        h = call(nodes[0], port, "/launch", {"cmd": ["python3", WRAPPER, "reduce", uid, str(N)],
                                             "env": fenv, "tag": "reduce"})["handle"]
        launched.append((nodes[0], h))

        done, ok, job_ms = set(), True, 0
        while len(done) < len(launched):
            time.sleep(0.1)
            for n, h in launched:
                if h in done:
                    continue
                st = call(n, port, f"/status/{h}")
                if not st["running"]:
                    done.add(h)
                    job_ms += st["ms"]   # this Faaslet's own runtime; summed = total work across all jobs
                    if st["exit_code"] not in (0, None):
                        ok = False
                        print(f"[wc] FAILED {n}/{h} exit={st['exit_code']}: {st['stderr_tail'][:300]}")
        merged = r.get(f"{uid}_result")                              # result sent back to node 0
        occ = write_result(args.out, merged, size_mb, N, len(nodes)) # …and saved to TestOutput
        makespan = int((time.time() - t0) * 1000)                    # clock stops after delivery+save
        gate = args.expect if args.expect is not None else (occ_seen if occ_seen is not None else occ)
        occ_seen = occ
        success = ok and occ == gate
        ok_all = ok_all and success
        ms_list.append(makespan)
        job_list.append(job_ms)
        print(f"[wc] rep {rep}: makespan={makespan}ms total_job={job_ms}ms "
              f"occurrences={occ} gate={gate} ok={success}")
        # Redis is flushed at the top of each rep, so no per-rep key cleanup is needed.

    med = int(median(ms_list))
    job_med = int(median(job_list))
    gate = args.expect if args.expect is not None else occ_seen
    nodes_used = min(N, len(nodes))
    fcsv.write(f"{size_mb},{N},{nodes_used},{med},{job_med},{occ_seen},{gate},{ok_all},{args.reps}\n")
    fcsv.close()
    print(f"[wc] median makespan={med}ms total_job={job_med}ms occurrences={occ_seen} "
          f"success={ok_all} → {args.csv}")
    print(f"[wc] result saved → {args.out}")
    print(f"RESULT occ={occ_seen} makespan_ms={med} success={ok_all}")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
