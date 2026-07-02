#!/usr/bin/env python3
# mem_probe.py — whole-cluster peak-memory sampler shared by the 6 Cloudburst drivers, so
# every run records its MAXIMUM MEMORY COST alongside makespan/total_job in the same CSV.
#
# Same methodology as cold_mem_runner.py: snapshot /proc/meminfo MemAvailable on all 9 nodes
# (node-0 read locally as 'self', the 8 workers over SSH), poll every ~1 s for the span of the
# run, and take each node's low-water mark. peak_used[node] = baseline - min_avail.
#   mem_max_mb   = the most-loaded single node's peak used  → the "maximum memory cost"
#   mem_total_mb = Σ per-node peak (aggregate working set the run added, all 9 nodes)
#   redis_peak_mb = Redis used_memory high-water during the run (the KVS sub-cost)
# MemAvailable excludes reclaimable page cache, so reading the input file doesn't count;
# anon RSS in the executor pods and the Redis-staged copy DO. The sampler is started just
# before the reps loop (pool already at 128 idle pods), so the delta reflects the JOB's
# working set — the chunks staged in Redis + the per-pod task growth — not the idle pool.
import re
import subprocess
import threading
import time
from concurrent.futures import ThreadPoolExecutor

# node-0 = 'self' (10.10.1.2, coordinator/Redis); the 8 workers by IP.
WORKER_SSH = ["10.10.1.1", "10.10.1.3", "10.10.1.4", "10.10.1.5",
              "10.10.1.6", "10.10.1.7", "10.10.1.8", "10.10.1.9"]
ALL_SSH = ["self"] + WORKER_SSH


def _mem_avail(target):
    """MemAvailable in kB for one node ('self' = local node-0)."""
    if target == "self":
        out = open("/proc/meminfo").read()
    else:
        out = subprocess.run(
            "ssh -o BatchMode=yes -o ConnectTimeout=4 %s cat /proc/meminfo" % target,
            shell=True, capture_output=True, text=True).stdout
    m = re.search(r"MemAvailable:\s+(\d+) kB", out)
    return int(m.group(1)) if m else None


def _sweep():
    with ThreadPoolExecutor(max_workers=len(ALL_SSH)) as ex:
        return dict(zip(ALL_SSH, ex.map(_mem_avail, ALL_SSH)))


class MemSampler(threading.Thread):
    """Background whole-cluster MemAvailable low-water tracker + Redis peak.

    Usage:
        ms = MemSampler(redis_host, redis_port); ms.start()
        ... run the workload ...
        ms.stop.set(); ms.join()
        mem_total_mb, mem_max_mb, redis_peak_mb = ms.result()
    """

    def __init__(self, redis_host=None, redis_port=None, interval=1.0):
        super().__init__(daemon=True)
        self.stop = threading.Event()
        self.rh, self.rp, self.interval = redis_host, redis_port, interval
        self.base = _sweep()
        self.lo = dict(self.base)
        self.redis_peak = self._redis_used()

    def _redis_used(self):
        if not self.rh:
            return 0.0
        try:
            import redis
            return redis.Redis(host=self.rh, port=self.rp).info("memory")["used_memory"] / 1e6
        except Exception:
            return 0.0

    def run(self):
        while not self.stop.is_set():
            cur = _sweep()
            for n, v in cur.items():
                if v is not None and (self.lo[n] is None or v < self.lo[n]):
                    self.lo[n] = v
            rp = self._redis_used()
            if rp > self.redis_peak:
                self.redis_peak = rp
            time.sleep(self.interval)

    def result(self):
        per = {n: max(0.0, (self.base[n] - self.lo[n]) / 1024.0) for n in ALL_SSH
               if self.base[n] is not None and self.lo[n] is not None}
        total = sum(per.values())
        mx = max(per.values()) if per else 0.0
        return total, mx, self.redis_peak
