#!/usr/bin/env python3
"""rt-coordinator.py — place RTSFaaS roles across the cluster via the rt-agents and
read the client's end-to-end throughput + latency. Dedicated RTSFaaS driver
(replaces scripts/FaaS/run-application.sh); the WasMem counterpart is
../../run-cluster.sh + the node-agent.

Placement (mirrors single_node/run-single-node.sh ordering + RDMA-handshake delays,
lifted to N nodes): driver -> sleep 10 -> database -> sleep 12 -> worker i on
WORKER_IPS[i] for all i -> sleep 12 -> client. driver+database+client run on the
coordinator (node 0); each worker on its own node. All on the in-memory RDMA store
(Database.env isTiKV=0). After the client exits, scrape:
  - client end-to-end Throughput (records/s) + Latency  -> the headline number
  - driver "finished with throughput" / "average latency" / "99th latency"
  - worker per-executor engine throughput (in-cache rate; reported, not headline)

Usage:
  ./rt-coordinator.py MediaReview --reps 15 --csv ../../results_rtsfaas_cluster.csv
  ./rt-coordinator.py SocialNetwork --reps 3        # quick smoke
"""
import argparse
import json
import os
import re
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
APPS = ("MediaReview", "SocialNetwork")
DRIVER_BOOT, DB_BOOT, WORKER_BOOT = 10, 12, 12      # RDMA-handshake delays (single-node values)
WORKER_STAGGER = 10                                 # gap between worker launches so each higher-index
#   worker finishes its (slow, multi-GB) RDMA buffer alloc + binds its accept listener BEFORE lower-index
#   peers connect to it. The mesh (RdmaWorkerManager:127) has worker i connect only to workers >i, so we
#   MUST launch high->low (3,2,1,0); launching low-first/simultaneously => RDMA_CM_EVENT_REJECTED -> hang.


def load_env():
    env = {}
    with open(os.path.join(HERE, "cluster.env")) as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if "=" in line:
                k, v = line.split("=", 1)
                env[k.strip()] = v.strip().strip('"')
    coord = env["COORD_IP"]
    workers = env["WORKER_IPS"].split()             # workerId order
    port = int(env.get("AGENT_PORT", "9700"))
    rtdir = f"{env['REPO']}/Tests/Streaming Application_Benchmark/inter-node/baseline/RTSFaaS/cluster"
    return env, coord, workers, port, rtdir


def call(host, port, path, obj=None, timeout=900):
    data = json.dumps(obj).encode() if obj is not None else None
    req = urllib.request.Request(f"http://{host}:{port}{path}", data=data,
                                 method="POST" if obj is not None else "GET",
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def mean_std(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return None, None
    m = sum(xs) / len(xs)
    if len(xs) < 2:
        return m, 0.0
    return m, (sum((x - m) ** 2 for x in xs) / (len(xs) - 1)) ** 0.5


def _num_after(log, *keywords):
    """First float on a line containing any keyword (case-insensitive)."""
    for line in log.splitlines():
        low = line.lower()
        if any(k.lower() in low for k in keywords):
            m = re.search(r"[-+]?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?", line)
            if m:
                return float(m.group())
    return None


def one_run(app, env, coord, workers, port, rtdir, reps_idx, total, deadline_s=600):
    image, docker = env.get("IMAGE", "rtfaas:1.0"), env.get("DOCKER", "sudo docker")
    nodes = sorted(set([coord] + workers))
    for n in nodes:                                  # clean slate everywhere
        call(n, port, "/rm", {})
    spec = lambda role, wid: {"role": role, "worker_id": wid, "app": app,
                              "rtdir": rtdir, "image": image, "docker": docker}
    print(f"  [{reps_idx}/{total}] {app}: driver", flush=True)
    call(coord, port, "/role", spec("driver", 0));   time.sleep(DRIVER_BOOT)
    print(f"  [{reps_idx}/{total}] {app}: database", flush=True)
    call(coord, port, "/role", spec("database", 0)); time.sleep(DB_BOOT)
    for wid, wip in reversed(list(enumerate(workers))):     # high->low: peers a worker connects to must be up first
        print(f"  [{reps_idx}/{total}] {app}: worker {wid} -> {wip}", flush=True)
        call(wip, port, "/role", spec("worker", wid))
        time.sleep(WORKER_STAGGER)
    time.sleep(WORKER_BOOT)
    print(f"  [{reps_idx}/{total}] {app}: client", flush=True)
    call(coord, port, "/role", spec("client", 0))

    # poll the client to completion
    deadline = time.time() + deadline_s
    while time.time() < deadline:
        st = call(coord, port, "/status/rt-client")
        if not st["running"] and st["status"] in ("exited", "absent"):
            break
        time.sleep(3)
    client_log = call(coord, port, "/logs/rt-client")["log"]
    driver_log = call(coord, port, "/logs/rt-driver")["log"]
    # capture worker logs too (one per worker node) before teardown — essential for debugging
    worker_logs = {}
    for wid, wip in enumerate(workers):
        try:
            worker_logs[wid] = call(wip, port, f"/logs/rt-worker{wid}")["log"]
        except Exception as ex:
            worker_logs[wid] = f"(could not fetch: {ex})"

    res = {
        "throughput_req_s": _num_after(client_log, "Throughput"),
        "avg_ms": _num_after(driver_log, "average latency"),
        "p99_ms": _num_after(driver_log, "99th latency"),
    }
    for n in nodes:
        call(n, port, "/rm", {})
    return res, client_log, driver_log, worker_logs


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("app", choices=APPS, nargs="?")
    ap.add_argument("--both", action="store_true", help="run both apps")
    ap.add_argument("--reps", type=int, default=15)
    ap.add_argument("--csv", default=os.path.join(HERE, "..", "..", "results_rtsfaas_cluster.csv"))
    ap.add_argument("--logdir", default="/tmp/rt_cluster_logs")
    ap.add_argument("--deadline", type=int, default=600, help="seconds to wait for the client to finish per rep")
    args = ap.parse_args()
    if not args.app and not args.both:
        ap.error("give an app or --both")

    env, coord, workers, port, rtdir = load_env()
    apps = list(APPS) if args.both else [args.app]
    os.makedirs(args.logdir, exist_ok=True)

    # health-check every agent up front
    for n in sorted(set([coord] + workers)):
        try:
            h = call(n, port, "/health", timeout=8)
            assert h["ok"], h
        except Exception as ex:
            sys.exit(f"[rt-coord] agent down/unhealthy on {n}:{port} — "
                     f"start rt-agent.py there (deploy.sh start). ({ex})")

    rows = []
    for app in apps:
        thr, avg, p99 = [], [], []
        for r in range(1, args.reps + 1):
            res, clog, dlog, wlogs = one_run(app, env, coord, workers, port, rtdir, r, args.reps, args.deadline)
            open(f"{args.logdir}/{app}_rep{r}_client.log", "w").write(clog)
            open(f"{args.logdir}/{app}_rep{r}_driver.log", "w").write(dlog)
            for wid, wl in wlogs.items():
                open(f"{args.logdir}/{app}_rep{r}_worker{wid}.log", "w").write(wl)
            print(f"    -> {res}", flush=True)
            thr.append(res["throughput_req_s"]); avg.append(res["avg_ms"]); p99.append(res["p99_ms"])
        tm, ts = mean_std(thr); am, _ = mean_std(avg); pm, _ = mean_std(p99)
        rows.append({"system": "rtsfaas", "workload": app.lower(), "nodes": len(workers),
                     "throughput_req_s_mean": round(tm) if tm else None,
                     "throughput_req_s_std": round(ts) if ts else None,
                     "avg_ms": round(am, 2) if am else None,
                     "p99_ms": round(pm, 2) if pm else None, "reps": args.reps})

    cols = ["system", "workload", "nodes", "throughput_req_s_mean",
            "throughput_req_s_std", "avg_ms", "p99_ms", "reps"]
    new = not os.path.exists(args.csv)
    with open(args.csv, "a") as f:
        if new:
            f.write(",".join(cols) + "\n")
        for row in rows:
            f.write(",".join(str(row[c]) for c in cols) + "\n")
    print(f"[rt-coord] wrote {len(rows)} row(s) -> {os.path.abspath(args.csv)}")
    for row in rows:
        print("  ", row)


if __name__ == "__main__":
    main()
