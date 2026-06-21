#!/usr/bin/env python3
"""faasm_lib.py — shared helpers for the Faasm inter-node workload drivers.

Topology + agent port + Redis come from the deployment substrate's cluster.env
(../../Inter-Node deployment/faasm/cluster.env). Each per-workload driver does its
own input-prep + gate; the launch/poll/time/csv machinery lives here.
"""
import json
import os
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DEPLOY = os.path.join(ROOT, "Tests", "Inter-Node deployment", "faasm")


def load_env():
    """Returns (env_dict, nodes_list, repo_root). nodes[0] = coordinator (node 0)."""
    env = {}
    with open(os.path.join(DEPLOY, "cluster.env")) as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if "=" in line:
                k, v = line.split("=", 1)
                env[k.strip()] = v.strip().strip('"')
    nodes = [env["COORD_IP"]] + env.get("WORKER_IPS", "").split()
    return env, nodes, ROOT


def call(host, port, path, obj=None, timeout=3600):
    data = json.dumps(obj).encode() if obj is not None else None
    req = urllib.request.Request(f"http://{host}:{port}{path}", data=data,
                                 method="POST" if obj is not None else "GET",
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def health_check(nodes, port):
    for n in nodes:
        try:
            call(n, port, "/health", timeout=5)
        except Exception as ex:
            raise SystemExit(f"[faasm] agent down on {n}:{port} — "
                             f"deployment/faasm/deploy.sh start ({ex})")


def run_faaslets(specs, port, t0=None):
    """Launch every spec {node, cmd, env, tag} via its node's agent, poll to done.
    Returns (makespan_ms, ok, per_node_ms). Faaslets self-synchronize through Redis
    where needed (e.g. a reduce that waits for partials), so all are launched at once
    and makespan = wall from t0 to last completion. Pass t0 (a time.time() captured by
    the driver *before* it stages input into Redis) to make the window end-to-end —
    input upload + agent dispatch + Faaslet compute + Redis state I/O; if omitted the
    clock starts here (dispatch only, upload excluded)."""
    launched = []
    if t0 is None:
        t0 = time.time()
    for s in specs:
        h = call(s["node"], port, "/launch",
                 {"cmd": s["cmd"], "env": s.get("env", {}), "tag": s.get("tag", "faaslet")})["handle"]
        launched.append((s["node"], h, s.get("tag")))
    done, ok, per = set(), True, {}
    while len(done) < len(launched):
        time.sleep(0.1)
        for n, h, tag in launched:
            if h in done:
                continue
            st = call(n, port, f"/status/{h}")
            if not st["running"]:
                done.add(h)
                per[n] = per.get(n, 0) + st["ms"]
                if st["exit_code"] not in (0, None):
                    ok = False
                    print(f"[faasm] FAILED {n}/{h} ({tag}) exit={st['exit_code']}: {st['stderr_tail'][:300]}")
    return int((time.time() - t0) * 1000), ok, per


def median(xs):
    xs = sorted(xs)
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def mean_std(xs):
    """Return (mean, sample-stdev) over xs. stdev=0 for <2 samples. The headline
    summary for N-rep benchmarks (replaces median 2026-06-21)."""
    m = sum(xs) / len(xs)
    if len(xs) < 2:
        return m, 0.0
    var = sum((x - m) ** 2 for x in xs) / (len(xs) - 1)
    return m, var ** 0.5


def generic_wrapper():
    """Abs path to faaslet_run.py (the generic per-node runner), git-synced to all nodes."""
    return os.path.join(HERE, "faaslet_run.py")
