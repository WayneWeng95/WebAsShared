#!/usr/bin/env python3
"""Parallel-throughput ESTIMATE for RTSFaaS: run the working single-node config
INDEPENDENTLY on all 4 nodes concurrently (no cross-node coordination), sum the
per-node client throughput. This is the embarrassingly-parallel upper bound — what
RTSFaaS would deliver if its workload partitioned perfectly with zero cross-node
state. Drives each node via its faasm agent (9600, to stage a per-node Env) + the
RTSFaaS rt-agent (9700, to launch the 4 roles). No ssh, no Redis (RTSFaaS can't use it)."""
import json, sys, time, urllib.request, threading

SINGLE = "/opt/myapp/WebAsShared/Tests/Streaming Application_Benchmark/intra-node/baseline/RTSFaaS/single_node"
RTDIR = "/tmp/rt_indep"
APP = sys.argv[1] if len(sys.argv) > 1 else "MediaReview"
# ip -> RoCE reverse-DNS name (each node runs a SELF-contained single-node instance)
NODES = [("10.10.1.2","node-0-link-0"), ("10.10.1.1","node-1-link-0"),
         ("10.10.1.3","node-3-link-0"), ("10.10.1.4","node-2-link-0"),
         ("10.10.1.5","node-7-link-0"), ("10.10.1.6","node-6-link-0"),
         ("10.10.1.7","node-5-link-0"), ("10.10.1.8","node-4-link-0"),
         ("10.10.1.9","node-8-link-0")]

def faasm(ip, cmd, wait=60):
    h = json.loads(urllib.request.urlopen(urllib.request.Request(
        f"http://{ip}:9600/launch", data=json.dumps({"cmd":["bash","-lc",cmd],"tag":"indep"}).encode()),timeout=20).read())["handle"]
    for _ in range(wait*2):
        s = json.loads(urllib.request.urlopen(f"http://{ip}:9600/status/{h}",timeout=20).read())
        if not s["running"]: return s
        time.sleep(0.5)
    return s

def rt(ip, path, obj=None):
    data = json.dumps(obj).encode() if obj is not None else None
    req = urllib.request.Request(f"http://{ip}:9700{path}", data=data, method="POST" if obj is not None else "GET")
    return json.loads(urllib.request.urlopen(req, timeout=120).read())

def stage(ip, name):
    # build a self-contained rtdir on the node: copy single_node Env+DockerFiles,
    # then point Cluster.env's driver/worker host at THIS node's RoCE name (workerNum=1).
    cmd = (f'rm -rf {RTDIR} && mkdir -p {RTDIR} && '
           f'cp -r "{SINGLE}/Env" "{SINGLE}/DockerFiles" {RTDIR}/ && '
           f'chmod +x {RTDIR}/DockerFiles/*.sh && '
           f"sed -i 's/^driverHost=.*/driverHost={name}/; s/^workerHosts=.*/workerHosts={name}/' {RTDIR}/Env/Cluster.env && "
           f"sed -i 's/^databaseHost=.*/databaseHost={name}/' {RTDIR}/Env/Database.env && "
           f"grep -hE '^(driverHost|workerHosts|databaseHost|workerNum)=' {RTDIR}/Env/Cluster.env {RTDIR}/Env/Database.env | tr '\\n' ' '")
    s = faasm(ip, cmd)
    return s.get("stdout_tail","").strip()

RESULT = {}
def run_node(ip, name):
    rt(ip, "/rm", {})
    spec = lambda role: {"role":role, "worker_id":0, "app":APP, "rtdir":RTDIR, "image":"rtfaas:1.0", "docker":"sudo docker"}
    rt(ip, "/role", spec("driver"));   time.sleep(10)
    rt(ip, "/role", spec("database")); time.sleep(12)
    rt(ip, "/role", spec("worker"));   time.sleep(12)
    rt(ip, "/role", spec("client"))
    # poll client to completion (single-node finishes in ~20-40s)
    for _ in range(60):
        st = rt(ip, "/status/rt-client")
        if not st["running"] and st["status"] in ("exited","absent"): break
        time.sleep(3)
    log = rt(ip, "/logs/rt-client")["log"]
    thr = None
    for line in log.splitlines():
        if "Throughput:" in line:
            import re; m = re.search(r"Throughput:\s*([0-9.]+)", line)
            if m: thr = float(m.group(1))
    RESULT[name] = thr
    rt(ip, "/rm", {})

print(f"=== staging self-contained single-node config on each node ({APP}) ===", flush=True)
for ip, name in NODES:
    print(f"  {name} ({ip}): {stage(ip, name)}", flush=True)

print(f"=== launching {len(NODES)} INDEPENDENT single-node {APP} runs CONCURRENTLY ===", flush=True)
ths = [threading.Thread(target=run_node, args=(ip,name)) for ip,name in NODES]
[t.start() for t in ths]; [t.join() for t in ths]

print(f"\n=== per-node {APP} throughput (records/sec) ===", flush=True)
tot = 0.0; ok = 0
for ip, name in NODES:
    v = RESULT.get(name)
    print(f"  {name} ({ip}): {v}")
    if v: tot += v; ok += 1
print(f"\nPARALLEL ESTIMATE ({APP}, {ok}/{len(NODES)} nodes): sum = {tot:.1f} records/sec  (embarrassingly-parallel upper bound)")
