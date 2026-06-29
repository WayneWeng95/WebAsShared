#!/usr/bin/env python3
# cold_mem_runner.py — add COLD-START + MEMORY metrics to the 9-node Cloudburst vs
# RMMap comparison, for all 6 inter-node workloads.
#
# COLD START (same definition as analysis/cold_start/cold_start_runner.py):
#   cloudburst : per rep scale the executor Deployment 0 → N pods, time until all N are
#                Running (image is cached on every node → measures pod sched+startup, not a
#                pull), run 1 rep, scale back to 0.
#   rmmap      : per rep the driver freshly registers the input as an RDMA MR (the "publish"
#                = RMMap's infra cold start, it has no pods), runs 1 rep, tears down.
#                Reported as the driver's `warmup=` ms.
#
# MEMORY (uniform, fair across both systems): whole-cluster MemAvailable delta. Snapshot
# /proc/meminfo MemAvailable on all 9 nodes before the run, poll every ~1 s during it, and
# take the low-water mark per node. peak_used[node] = baseline - min_avail.
#   total_mem_mb = Σ_nodes peak_used   (aggregate working set the job added, all 9 nodes)
#   max_mem_mb   = max_nodes peak_used (the most-loaded single node's peak)
# Page cache is reclaimable → excluded by MemAvailable, so reading the corpus doesn't count;
# Cloudburst's Redis/pod anon memory and RMMap's *pinned* (reg_mr/mlock) MR both DO count.
#   redis_peak_mb = Redis used_memory high-water during the run (Cloudburst KVS sub-metric).
import argparse, os, re, statistics, subprocess, threading, time
from concurrent.futures import ThreadPoolExecutor

ROOT = "/opt/myapp/WebAsShared"
TD   = ROOT + "/TestData"
DEP_DIR = ROOT + "/Tests/Inter-Node deployment"
CB   = DEP_DIR + "/k8s/cloudburst"
NS, DEP = "baselines", "cloudburst-executor"
TAR_W = "/tmp/cb-exec.tar"

# node-0 + 8 workers. SSH/host name (for /proc/meminfo) ; node-0 read locally.
WORKER_SSH = ["10.10.1.1","10.10.1.3","10.10.1.4","10.10.1.5",
              "10.10.1.6","10.10.1.7","10.10.1.8","10.10.1.9"]
ALL_SSH    = ["self"] + WORKER_SSH                      # 'self' = node-0 (local)
# RMMap node lists: IPs for wc/finra/ml/matrix; hostnames for terasort (NODE_IP map).
RM_NODES_IP = "10.10.1.2,10.10.1.1,10.10.1.3,10.10.1.4,10.10.1.5,10.10.1.6,10.10.1.7,10.10.1.8,10.10.1.9"
RM_NODES_HN = "node-0,node-1,node-2,node-3,node-4,node-5,node-6,node-7,node-8"


def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True)

# ---- memory sampling ---------------------------------------------------------
def mem_avail(target):
    """MemAvailable in kB for one node ('self' = local node-0)."""
    if target == "self":
        out = open("/proc/meminfo").read()
    else:
        out = sh(f"ssh -o BatchMode=yes -o ConnectTimeout=4 {target} cat /proc/meminfo").stdout
    m = re.search(r"MemAvailable:\s+(\d+) kB", out)
    return int(m.group(1)) if m else None

def sweep():
    with ThreadPoolExecutor(max_workers=len(ALL_SSH)) as ex:
        return dict(zip(ALL_SSH, ex.map(mem_avail, ALL_SSH)))

def redis_used_mb():
    try:
        import redis
        return redis.Redis(host="10.10.1.2", port=30679).info("memory")["used_memory"]/1e6
    except Exception:
        return 0.0

class MemSampler(threading.Thread):
    def __init__(self):
        super().__init__(daemon=True)
        self.stop = threading.Event(); self.base = sweep()
        self.lo = dict(self.base); self.redis_peak = redis_used_mb()
    def run(self):
        while not self.stop.is_set():
            cur = sweep()
            for n,v in cur.items():
                if v is not None and (self.lo[n] is None or v < self.lo[n]): self.lo[n]=v
            rp = redis_used_mb()
            if rp > self.redis_peak: self.redis_peak = rp
            time.sleep(1.0)
    def result(self):
        per = {n: max(0.0,(self.base[n]-self.lo[n])/1024.0) for n in ALL_SSH
               if self.base[n] is not None and self.lo[n] is not None}
        total = sum(per.values()); mx = max(per.values()) if per else 0.0
        return total, mx, self.redis_peak

# ---- cloudburst pod lifecycle ------------------------------------------------
def pods_running(): return int(sh(f"kubectl get pods -n {NS} -l app={DEP} --no-headers 2>/dev/null|grep -c ' Running '").stdout.strip() or 0)
def pods_total():   return int(sh(f"kubectl get pods -n {NS} -l app={DEP} --no-headers 2>/dev/null|wc -l").stdout.strip() or 0)
def scale(n):       sh(f"kubectl -n {NS} scale deploy/{DEP} --replicas={n}")
def wait_running(n,t=240):
    s=time.time()
    while time.time()-s<t:
        if pods_running()>=n: return True
        time.sleep(1)
    return False
def wait_gone(t=150):
    s=time.time()
    while time.time()-s<t:
        if pods_total()==0: return True
        time.sleep(1)
    return False
def ensure_image():
    sh(f"sudo k3s ctr images import '{TAR_W}' >/dev/null 2>&1")
    with ThreadPoolExecutor(max_workers=8) as ex:
        list(ex.map(lambda n: sh(f"ssh {n} 'sudo k3s ctr images import {TAR_W}' >/dev/null 2>&1"), WORKER_SSH))

def grab(pat,t,d=0.0):
    m=re.search(pat,t); return float(m.group(1)) if m else d

# workload → (cloudburst driver cmd, rmmap driver cmd, gate regex). --reps 1.
def cmds(wl):
    rm_common = f"--server-ip 10.10.1.2 --self-host 10.10.1.2 --nodes {RM_NODES_IP} --reps 1"
    if wl=="wordcount":
        c=f"cd '{CB}' && python3 driver.py --corpus {TD}/corpus_4gb.txt --fanout 60 --reps 1 --redis-host 10.10.1.2 --redis-port 30679"
        r=f"cd '{DEP_DIR}/rmmap-rdma-wordcount' && python3 driver.py --corpus {TD}/corpus_4gb.txt --fanout 60 {rm_common}"
        g=r"occ=(\d+)"
    elif wl=="finra":
        c=f"cd '{CB}' && python3 driver_finra.py --trades {TD}/finra_5m.csv --fanout 60 --reps 1 --redis-host 10.10.1.2 --redis-port 30679"
        r=f"cd '{DEP_DIR}/rmmap-rdma-finra' && python3 driver.py --trades {TD}/finra_5m.csv --fanout 60 {rm_common}"
        g=r"violations=(\d+)"
    elif wl=="ml_training":
        c=f"cd '{CB}' && python3 driver_ml_training.py --data {TD}/ml_training_6m.csv --workers 60 --reps 1 --redis-host 10.10.1.2 --redis-port 30679"
        r=f"cd '{DEP_DIR}/rmmap-rdma-ml' && python3 driver.py --mode train --data {TD}/ml_training_6m.csv --fanout 60 {rm_common}"
        g=r"weight_checksum=(\d+)"
    elif wl=="ml_inference":
        c=f"cd '{CB}' && python3 driver_ml_inference.py --data {TD}/ml_inference_6m.csv --model {TD}/ml_inference_model.csv --workers 60 --reps 1 --redis-host 10.10.1.2 --redis-port 30679"
        r=f"cd '{DEP_DIR}/rmmap-rdma-ml' && python3 driver.py --mode infer --data {TD}/ml_inference_6m.csv --model {TD}/ml_inference_model.csv --fanout 60 {rm_common}"
        g=r"prediction_checksum=(\d+)"
    elif wl=="matrix":
        c=f"cd '{CB}' && python3 driver_matrix.py --a {TD}/matrix_a_4096.bin --b {TD}/matrix_b_4096.bin --matrix-n 4096 --workers 64 --reps 1 --redis-host 10.10.1.2 --redis-port 30679"
        r=f"cd '{DEP_DIR}/rmmap-rdma-matrix' && python3 driver.py --a {TD}/matrix_a_4096.bin --b {TD}/matrix_b_4096.bin --matrix-n 4096 --workers 64 {rm_common}"
        g=r"checksum=(\d+)"
    elif wl=="terasort":
        c=f"cd '{CB}' && python3 driver_terasort.py --records {TD}/terasort_1.2gb.txt --fanout 4 --reps 1 --redis-host 10.10.1.2 --redis-port 30679"
        r=(f"cd '{DEP_DIR}/rmmap-rdma-terasort' && python3 driver.py --records {TD}/terasort_1.2gb.txt --fanout 4 "
           f"--server-ip 10.10.1.2 --self-host node-0 --nodes {RM_NODES_HN} --reps 1")
        g=r"records=(\d+)"
    return c,r,g

def run_cb(wl,reps,N):
    c,_,g=cmds(wl); scale(0); wait_gone()
    cold,mk,tot,mx,rp,gate=[],[],[],[],[],None
    for i in range(reps):
        ensure_image()
        # Baseline memory at 0 pods, BEFORE scale-up, so the 60-pod pool's full footprint
        # is counted — the fair counterpart to RMMap counting its freshly-spawned mappers.
        ms=MemSampler(); ms.start()
        t0=time.time(); scale(N)
        if not wait_running(N): ms.stop.set(); ms.join(); raise RuntimeError(f"cb {wl} rep{i}: pods not Running")
        cold_ms=(time.time()-t0)*1000.0
        out=sh(c).stdout; ms.stop.set(); ms.join()
        T,M,R=ms.result()
        mk_ms=grab(r"makespan=([0-9.]+)",out); gate=int(grab(g,out))
        scale(0); wait_gone()
        cold.append(cold_ms); mk.append(mk_ms); tot.append(T); mx.append(M); rp.append(R)
        print(f"  [cb {wl}] rep{i}: cold={cold_ms:.0f}ms make={mk_ms:.0f}ms mem_total={T:.0f}MB mem_max={M:.0f}MB redis_peak={R:.0f}MB gate={gate}",flush=True)
    return cold,mk,tot,mx,rp,gate

def run_rm(wl,reps):
    _,r,g=cmds(wl)
    cold,mk,tot,mx,rp,gate=[],[],[],[],[],None
    for i in range(reps):
        ms=MemSampler(); ms.start(); out=sh(r).stdout; ms.stop.set(); ms.join()
        T,M,R=ms.result()
        cold_ms=grab(r"warmup=([0-9.]+)",out); mk_ms=grab(r"makespan=([0-9.]+)",out); gate=int(grab(g,out))
        cold.append(cold_ms); mk.append(mk_ms); tot.append(T); mx.append(M); rp.append(R)
        print(f"  [rm {wl}] rep{i}: cold={cold_ms:.0f}ms make={mk_ms:.0f}ms mem_total={T:.0f}MB mem_max={M:.0f}MB gate={gate}",flush=True)
    return cold,mk,tot,mx,rp,gate

def summarize(f,wl,sysn,cold,mk,tot,mx,rp,gate,reps):
    cm=statistics.mean(cold); cs=statistics.pstdev(cold) if len(cold)>1 else 0.0
    row=(f"{sysn},{wl},{reps},{cm:.0f},{cs:.0f},{statistics.mean(mk):.0f},"
         f"{statistics.mean(tot):.0f},{max(mx):.0f},{statistics.mean(rp):.0f},{gate}")
    print(f"=== {sysn} {wl}: cold_start={cm:.0f}±{cs:.0f}ms mem_total={statistics.mean(tot):.0f}MB "
          f"mem_max={max(mx):.0f}MB redis_peak={statistics.mean(rp):.0f}MB gate={gate} ===",flush=True)
    f.write(row+"\n"); f.flush()

def main():
    ap=argparse.ArgumentParser()
    ap.add_argument("--reps",type=int,default=2)
    ap.add_argument("--workers",type=int,default=60)
    ap.add_argument("--workloads",default="wordcount,finra,ml_training,ml_inference,matrix,terasort")
    ap.add_argument("--systems",default="cloudburst,rmmap")
    ap.add_argument("--csv",default=ROOT+"/Tests/9_cluster_results/cold_mem_results.csv")
    a=ap.parse_args(); wls=a.workloads.split(","); systems=a.systems.split(",")
    new=not os.path.exists(a.csv)
    with open(a.csv,"a") as f:
        if new: f.write("system,workload,reps,cold_start_mean_ms,cold_start_std_ms,makespan_mean_ms,mem_total_mean_mb,mem_max_mb,redis_peak_mean_mb,gate\n")
        if "cloudburst" in systems:
            for wl in wls:
                print(f"### COLD+MEM Cloudburst {wl} ###",flush=True)
                try: summarize(f,wl,"cloudburst",*run_cb(wl,a.reps,a.workers),a.reps)
                except Exception as e: print(f"  [cb {wl}] FAILED: {e}",flush=True)
            scale(0); wait_gone()
        if "rmmap" in systems:
            scale(0); wait_gone()
            for wl in wls:
                print(f"### COLD+MEM RMMap {wl} ###",flush=True)
                try: summarize(f,wl,"rmmap",*run_rm(wl,a.reps),a.reps)
                except Exception as e: print(f"  [rm {wl}] FAILED: {e}",flush=True)
    print(f"\nwrote {a.csv}",flush=True)

if __name__=="__main__":
    main()
