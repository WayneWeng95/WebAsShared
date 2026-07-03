#!/usr/bin/env python3
"""mem_wrap.py — run a command with the cluster-wide MemAvailable-delta sampler
(the same mem_probe.MemSampler the Cloudburst drivers use), so RMMap/Faasm runs
get a peak-memory number comparable to Cloudburst's.

Usage: mem_wrap.py <tag> <redis_host|none> <redis_port> -- <command...>
Prints: MEMRESULT <tag> total=<MB> max=<MB> redis=<MB>
"""
import sys, subprocess, os
CB = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                  "..", "Inter-Node deployment", "k8s", "cloudburst")
sys.path.insert(0, os.path.abspath(CB))
from mem_probe import MemSampler

tag = sys.argv[1]
rh = None if sys.argv[2] == "none" else sys.argv[2]
rp = int(sys.argv[3])
assert sys.argv[4] == "--"
cmd = sys.argv[5:]

ms = MemSampler(rh, rp, interval=0.2)
ms.start()
rc = subprocess.run(cmd).returncode
ms.stop.set()
ms.join()
total, mx, redis = ms.result()
print("MEMRESULT %s total=%.0f max=%.0f redis=%.0f rc=%d" % (tag, total, mx, redis, rc))
