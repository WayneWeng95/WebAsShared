#!/usr/bin/env python3
"""coordinator.py — Faasm inter-node driver (track D of EXPERIMENT_PLAN §5).

Runs on node 0. Reads a JSON job spec, STAGES each Faaslet module/input to the
nodes that need it, LAUNCHES the Faaslets on each node via its agent.py, polls to
completion, and writes a results.csv row in the shared column shape. State moves
through the shared Redis (Faasm KV); this driver only places + times, mirroring how
Knative/k8s place the other baselines.

Job spec (see job-example.json):
{
  "workload": "word_count",
  "stage":    [ {"src": "<local path>", "dst": "wc.cwasm", "nodes": ["10.10.1.1", ...] } ],
  "faaslets": [ {"node": "10.10.1.1", "tag": "map_0",
                 "cmd": ["wasmtime","/tmp/faasm_stage/wc.cwasm","map","0"],
                 "env": {"REDIS_HOST":"10.10.1.5"} }, ... ]
}
"nodes":"all" stages to every node in cluster.env. Metrics: makespan_ms (max
faaslet wall), total_exec_ms (Σ over working nodes), per-node rss.

Usage:  ./coordinator.py job-example.json [--reps 5] [--csv results.csv]
"""
import argparse
import base64
import json
import os
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))


def _env():
    """Parse cluster.env for AGENT_PORT and the node list."""
    env = {}
    with open(os.path.join(HERE, "cluster.env")) as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if "=" in line:
                k, v = line.split("=", 1)
                env[k.strip()] = v.strip().strip('"')
    nodes = [env["COORD_IP"]] + env.get("WORKER_IPS", "").split()
    return env, nodes


def _call(host, port, path, obj=None, timeout=300):
    url = f"http://{host}:{port}{path}"
    data = json.dumps(obj).encode() if obj is not None else None
    req = urllib.request.Request(url, data=data, method="POST" if obj is not None else "GET",
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("spec")
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--csv", default=os.path.join(HERE, "results.csv"))
    args = ap.parse_args()

    env, nodes = _env()
    port = int(env.get("AGENT_PORT", "9600"))
    spec = json.load(open(args.spec))
    wl = spec.get("workload", "unknown")

    # ── health check every node's agent ──────────────────────────────────────
    for n in nodes:
        try:
            h = _call(n, port, "/health", timeout=5)
            print(f"[coord] agent up: {n} ({h['node']})")
        except Exception as ex:
            sys.exit(f"[coord] agent unreachable on {n}:{port} — run deploy.sh first ({ex})")

    # ── stage modules/inputs (once; staging excluded from compute timing) ─────
    for s in spec.get("stage", []):
        raw = open(s["src"], "rb").read()
        b64 = base64.b64encode(raw).decode()
        targets = nodes if s.get("nodes") == "all" else s["nodes"]
        for n in targets:
            r = _call(n, port, "/stage", {"path": s["dst"], "b64": b64})
            print(f"[coord] staged {s['dst']} → {n} ({r['bytes']} B)")

    # ── timed reps ───────────────────────────────────────────────────────────
    new = not os.path.exists(args.csv)
    fcsv = open(args.csv, "a")
    if new:
        fcsv.write("workload,nodes_used,faaslets,makespan_ms,total_exec_ms,success,rep\n")

    for rep in range(1, args.reps + 1):
        handles = []  # (node, handle)
        t0 = time.time()
        for fa in spec["faaslets"]:
            n = fa["node"]
            r = _call(n, port, "/launch",
                      {"cmd": fa["cmd"], "env": fa.get("env", {}),
                       "cwd": fa.get("cwd", env.get("STAGE_DIR", "/tmp/faasm_stage")),
                       "tag": fa.get("tag", "faaslet")})
            handles.append((n, r["handle"]))

        # poll all to completion
        done, per_ms, ok = {}, {}, True
        while len(done) < len(handles):
            time.sleep(0.1)
            for n, h in handles:
                if h in done:
                    continue
                st = _call(n, port, f"/status/{h}")
                if not st["running"]:
                    done[h] = True
                    per_ms.setdefault(n, 0)
                    per_ms[n] += st["ms"]
                    if st["exit_code"] not in (0, None):
                        ok = False
                        print(f"[coord] FAASLET FAILED {n}/{h} exit={st['exit_code']}\n{st['stderr_tail']}")
        makespan = int((time.time() - t0) * 1000)
        total_exec = sum(per_ms.values())
        used = len(per_ms)
        print(f"[coord] {wl} rep {rep}: makespan={makespan}ms total_exec={total_exec}ms "
              f"nodes={used} ok={ok}")
        fcsv.write(f"{wl},{used},{len(spec['faaslets'])},{makespan},{total_exec},{ok},{rep}\n")
        fcsv.flush()

    fcsv.close()
    print(f"[coord] wrote {args.csv}")


if __name__ == "__main__":
    main()
