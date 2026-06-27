#!/usr/bin/env python3
# cold_start_runner.py — COLD-START variant of the inter-node Matrix / ML experiments for
# the two systems that have an infra cold start, Cloudburst and RMMap. Each rep starts the
# workers from NOTHING, times that cold start, runs ONE workload execution, then tears the
# workers down — so the reported time includes a fresh cold start every run.
#
#   Cloudburst : per rep — scale the executor Deployment 0 → N pods, wait until all N are
#                Running (= the cold start; the image is kept cached on every node so this
#                measures pod scheduling+startup, NOT an image pull), run the driver for 1
#                rep, scale back to 0. total = cold_start + workload makespan.
#   RMMap      : per rep — the driver freshly starts its RDMA server and reg_mr's the corpus
#                (the publish = RMMap's infra cold start, since it has no pods), runs 1 rep,
#                tears the server down. total = publish(warmup) + workload makespan.
#
# Same inputs as the warm inter-node runs (Matrix 4096²/64 workers, ML 6M/60 workers).
# Output: results_cold_start.csv (+ printed table).
import argparse
import re
import statistics
import subprocess
import time

TD = "/opt/myapp/WebAsShared/TestData"
DEP_DIR = "/opt/myapp/WebAsShared/Tests/Inter-Node deployment"
NS, DEP = "baselines", "cloudburst-executor"
TAR = "/tmp/cb-exec.tar"                              # cb-exec.tar on workers
TAR_LOCAL = DEP_DIR + "/k8s/cloudburst/cb-exec.tar"  # cb-exec.tar on node-0
WORKERS = ["node-1", "node-2", "node-3"]


def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True)


def pods_running():
    return int(sh(f"kubectl get pods -n {NS} -l app={DEP} --no-headers 2>/dev/null "
                  "| grep -c ' Running '").stdout.strip() or 0)


def pods_total():
    return int(sh(f"kubectl get pods -n {NS} -l app={DEP} --no-headers 2>/dev/null "
                  "| wc -l").stdout.strip() or 0)


def scale(n):
    sh(f"kubectl -n {NS} scale deploy/{DEP} --replicas={n}")


def wait_running(n, timeout=240):
    t = time.time()
    while time.time() - t < timeout:
        if pods_running() >= n:
            return True
        time.sleep(1)
    return False


def wait_gone(timeout=150):
    t = time.time()
    while time.time() - t < timeout:
        if pods_total() == 0:
            return True
        time.sleep(1)
    return False


def ensure_image():
    # UNTIMED: re-import on EVERY node so the cold start measures pod startup, not an image
    # pull. Any node's containerd can GC the image while the Deployment sits at 0 (the known
    # gotcha — seen on both node-0 and node-3), so refresh all four before each scale-up.
    sh(f"sudo k3s ctr images import '{TAR_LOCAL}' >/dev/null 2>&1")          # node-0
    for n in WORKERS:
        sh(f"ssh {n} 'sudo k3s ctr images import {TAR}' >/dev/null 2>&1")


def grab(pat, text, default=0.0):
    m = re.search(pat, text)
    return float(m.group(1)) if m else default


# workload → (cloudburst cmd --reps 1, rmmap cmd --reps 1, gate-regex)
def cmds(wl):
    cb = DEP_DIR + "/k8s/cloudburst"
    if wl == "matrix":
        c = (f"cd '{cb}' && python3 driver_matrix.py --a {TD}/matrix_a_4096.bin "
             f"--b {TD}/matrix_b_4096.bin --matrix-n 4096 --workers 64 --reps 1 "
             "--redis-host 10.10.1.2 --redis-port 30679 --task-queue cb:tasks "
             "--expect 1391095867672")
        r = (f"cd '{DEP_DIR}/rmmap-rdma-matrix' && python3 driver.py --a {TD}/matrix_a_4096.bin "
             f"--b {TD}/matrix_b_4096.bin --matrix-n 4096 --workers 64 --reps 1 "
             "--server-ip 10.10.1.2 --serve-port 18620 --expect 1391095867672")
        gate = r"checksum=(\d+)"
    elif wl == "ml_training":
        c = (f"cd '{cb}' && python3 driver_ml_training.py --data {TD}/ml_training_6m.csv "
             "--workers 60 --reps 1 --redis-host 10.10.1.2 --redis-port 30679 --expect 1232")
        r = (f"cd '{DEP_DIR}/rmmap-rdma-ml' && python3 driver.py --mode train "
             f"--data {TD}/ml_training_6m.csv --fanout 60 --reps 1 --server-ip 10.10.1.2 "
             "--port 18515 --expect 1232")
        gate = r"weight_checksum=(\d+)"
    elif wl == "ml_inference":
        c = (f"cd '{cb}' && python3 driver_ml_inference.py --data {TD}/ml_inference_6m.csv "
             f"--model {TD}/ml_inference_model.csv --workers 60 --reps 1 "
             "--redis-host 10.10.1.2 --redis-port 30679 --expect 18633154")
        r = (f"cd '{DEP_DIR}/rmmap-rdma-ml' && python3 driver.py --mode infer "
             f"--data {TD}/ml_inference_6m.csv --model {TD}/ml_inference_model.csv --fanout 60 "
             "--reps 1 --server-ip 10.10.1.2 --port 18515 --expect 18633154")
        gate = r"prediction_checksum=(\d+)"
    elif wl == "wordcount":
        c = (f"cd '{cb}' && python3 driver.py --corpus {TD}/corpus_4gb.txt --fanout 60 --reps 1 "
             "--redis-host 10.10.1.2 --redis-port 30679 --task-queue cb:tasks --expect 660848961")
        r = (f"cd '{DEP_DIR}/rmmap-rdma-wordcount' && python3 driver.py --corpus {TD}/corpus_4gb.txt "
             "--fanout 60 --reps 1 --server-ip 10.10.1.2 --port 18515 --expect 660848961")
        gate = r"occ=(\d+)"
    elif wl == "terasort":
        c = (f"cd '{cb}' && python3 driver_terasort.py --records {TD}/terasort_1.2gb.txt --fanout 4 "
             "--reps 1 --redis-host 10.10.1.2 --redis-port 30679 --task-queue cb:tasks "
             "--expect 12884902,8310813852,1")
        r = (f"cd '{DEP_DIR}/rmmap-rdma-terasort' && python3 driver.py --records {TD}/terasort_1.2gb.txt "
             "--fanout 4 --reps 1 --server-ip 10.10.1.2 --input-port 18515 --serve-base 18601 "
             "--expect 12884902,8310813852,1")
        gate = r"records=(\d+)"
    elif wl == "finra":
        c = (f"cd '{cb}' && python3 driver_finra.py --trades {TD}/finra_5m.csv --fanout 60 --reps 1 "
             "--redis-host 10.10.1.2 --redis-port 30679 --task-queue cb:tasks --expect 2271415")
        r = (f"cd '{DEP_DIR}/rmmap-rdma-finra' && python3 driver.py --trades {TD}/finra_5m.csv "
             "--fanout 60 --reps 1 --server-ip 10.10.1.2 --port 18515 --expect 2271415")
        gate = r"violations=(\d+)"
    return c, r, gate


def run_cloudburst(wl, reps, N):
    c, _, gate = cmds(wl)
    scale(0); wait_gone()
    cold, mk, tj, g = [], [], [], None
    for r in range(reps):
        ensure_image()
        t0 = time.time()
        scale(N)
        if not wait_running(N):
            raise RuntimeError(f"cloudburst {wl} rep{r}: pods not Running")
        cold_ms = (time.time() - t0) * 1000.0
        out = sh(c).stdout
        mk_ms = grab(r"makespan=([0-9.]+)", out)
        tj_ms = grab(r"total_job=([0-9.]+)", out)
        g = int(grab(gate, out))
        scale(0); wait_gone()
        cold.append(cold_ms); mk.append(mk_ms); tj.append(tj_ms)
        print(f"  [cloudburst {wl}] rep{r}: cold_start={cold_ms:.0f}ms makespan={mk_ms:.0f}ms "
              f"total={cold_ms+mk_ms:.0f}ms total_job={tj_ms:.0f}ms gate={g}", flush=True)
    return cold, mk, tj, g


def run_rmmap(wl, reps):
    _, rcmd, gate = cmds(wl)
    cold, mk, tj, g = [], [], [], None
    for r in range(reps):
        out = sh(rcmd).stdout                       # driver: fresh publish + 1 rep + teardown
        cold_ms = grab(r"warmup=([0-9.]+)", out)    # the RMMap publish = its cold start
        mk_ms = grab(r"makespan=([0-9.]+)", out)
        tj_ms = grab(r"total_job=([0-9.]+)", out)
        g = int(grab(gate, out))
        cold.append(cold_ms); mk.append(mk_ms); tj.append(tj_ms)
        print(f"  [rmmap {wl}] rep{r}: cold_start={cold_ms:.0f}ms makespan={mk_ms:.0f}ms "
              f"total={cold_ms+mk_ms:.0f}ms total_job={tj_ms:.0f}ms gate={g}", flush=True)
    return cold, mk, tj, g


def summarize(f, wl, system, cold, mk, tj, g, reps):
    cm, mm = statistics.mean(cold), statistics.mean(mk)
    cs = statistics.pstdev(cold) if len(cold) > 1 else 0.0
    tot = cm + mm
    tjm = statistics.mean(tj)
    print(f"=== {system} {wl}: cold_start={cm:.0f}±{cs:.0f}ms makespan={mm:.0f}ms "
          f"TOTAL={tot:.0f}ms total_job={tjm:.0f}ms gate={g} ===", flush=True)
    f.write(f"{wl},{system},{reps},{cm:.0f},{cs:.0f},{mm:.0f},{tot:.0f},{tjm:.0f},{g}\n")
    f.flush()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--workers", type=int, default=60, help="cloudburst pod count")
    ap.add_argument("--workloads", default="matrix,ml_training,ml_inference")
    ap.add_argument("--systems", default="cloudburst,rmmap")
    ap.add_argument("--csv", default="/opt/myapp/WebAsShared/Tests/Inter-Node Application_Benchmark/"
                                     "analysis/cold_start/results_cold_start.csv")
    a = ap.parse_args()
    wls = a.workloads.split(","); systems = a.systems.split(",")

    import os as _os
    new = not _os.path.exists(a.csv)
    with open(a.csv, "a") as f:
        if new:
            f.write("workload,system,reps,cold_start_mean_ms,cold_start_std_ms,"
                    "makespan_mean_ms,total_mean_ms,total_job_mean_ms,gate\n")
        # Cloudburst first (pods cycle 0↔N); then RMMap (pods stay at 0 → cores free).
        if "cloudburst" in systems:
            for wl in wls:
                print(f"### COLD-START Cloudburst {wl} ###", flush=True)
                cold, mk, tj, g = run_cloudburst(wl, a.reps, a.workers)
                summarize(f, wl, "cloudburst", cold, mk, tj, g, a.reps)
            scale(0); wait_gone()
        if "rmmap" in systems:
            scale(0); wait_gone()                   # ensure cores free for RMMap host procs
            for wl in wls:
                print(f"### COLD-START RMMap {wl} ###", flush=True)
                cold, mk, tj, g = run_rmmap(wl, a.reps)
                summarize(f, wl, "rmmap", cold, mk, tj, g, a.reps)
    print(f"\nwrote {a.csv}", flush=True)


if __name__ == "__main__":
    main()
