#!/usr/bin/env python3
# driver.py — RMMap (dmerge) SUMMA matrix-multiply, ES protocol, single-box runner.
#
# RMMap ships no native matrix-multiply (unlike wordcount/finra), so this is the
# ES port (per ../../../EXPERIMENT_RUNBOOK.md §4.3): the r×c block decomposition
# expressed in RMMap's ES (Redis + pickle) data path, with each block worker a
# SEPARATE PROCESS — mirroring RMMap's per-function Knative pods (the parallelism
# that distinguishes it from Cloudburst's single-process runner). The MITOSIS
# kernel module is NOT used: ES needs no RDMA/sopen syscalls (DMERGE would — it
# stays parked, §5.7).
#
# Faithful: the ES data path — every A/B panel and C block is pickle-serialized
# and round-tripped through Redis (the serialized inter-stage transfer our
# zero-copy page-chain reports as 0), with parallel per-block workers. Dropped:
# the Knative HTTP/CloudEvent envelope and placement (not the serialization cost
# being compared) and the dmerge Cython bindings (only DMERGE needs them).
#
# Env: MAT_A, MAT_B, REDIS_HOST/PORT. Args: N W [--header]. Prints one CSV row.
import os
import pickle
import resource
import sys
import time
import uuid
from multiprocessing import Pool

import numpy as np
import redis

REDIS_HOST = os.environ.get('REDIS_HOST', '127.0.0.1')
REDIS_PORT = int(os.environ.get('REDIS_PORT', '6379'))


def _grid(w):
    r = 1
    k = 1
    while k * k <= w:
        if w % k == 0:
            r = k
        k += 1
    return r, w // r


def _worker(task):
    """One block worker = one Knative-pod-equivalent process: read A_i / B_j
    panels from Redis (pickle), compute C_ij (numpy BLAS), write C_ij back."""
    uid, i, j = task
    r = redis.Redis(host=REDIS_HOST, port=REDIS_PORT)
    a_raw = r.get('%s_a_%d' % (uid, i))
    b_raw = r.get('%s_b_%d' % (uid, j))
    a = pickle.loads(a_raw)        # BR × N
    b = pickle.loads(b_raw)        # N × BC
    # NAIVE O(N^3) contraction (no BLAS) — np.einsum(optimize=False) runs numpy's
    # nested-loop kernel, NOT a tuned GEMM, matching the WASM baselines' naive ikj
    # kernel so the comparison isolates the data substrate, not kernel speed.
    c = np.einsum('ik,kj->ij', a, b, optimize=False)   # BR × BC
    c_raw = pickle.dumps(c, protocol=pickle.HIGHEST_PROTOCOL)
    r.set('%s_c_%d_%d' % (uid, i, j), c_raw)
    ser_bytes = len(a_raw) + len(b_raw) + len(c_raw)  # ES gets + put
    rss_kb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return int(c.sum()), ser_bytes, rss_kb


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    N = int(args[0])
    W = int(args[1]) if len(args) > 1 else 4
    R, C = _grid(W)
    assert N % R == 0 and N % C == 0, f"N={N} not divisible by grid {R}x{C}"
    BR, BC = N // R, N // C

    A = np.fromfile(os.environ['MAT_A']).reshape(N, N)
    B = np.fromfile(os.environ['MAT_B']).reshape(N, N)

    r = redis.Redis(host=REDIS_HOST, port=REDIS_PORT)
    uid = uuid.uuid4().hex

    t0 = time.time()
    # ── tile: pickle each A block-row / B block-col into Redis (ES state) ──────
    ser_bytes = 0
    for i in range(R):
        buf = pickle.dumps(np.ascontiguousarray(A[i * BR:(i + 1) * BR, :]),
                           protocol=pickle.HIGHEST_PROTOCOL)
        r.set('%s_a_%d' % (uid, i), buf)
        ser_bytes += len(buf)
    for j in range(C):
        buf = pickle.dumps(np.ascontiguousarray(B[:, j * BC:(j + 1) * BC]),
                           protocol=pickle.HIGHEST_PROTOCOL)
        r.set('%s_b_%d' % (uid, j), buf)
        ser_bytes += len(buf)

    # ── block workers in parallel (separate processes ≈ Knative pods) ─────────
    tasks = [(uid, i, j) for i in range(R) for j in range(C)]
    with Pool(processes=W) as pool:
        results = pool.map(_worker, tasks)
    checksum = sum(s for s, _, _ in results)
    ser_bytes += sum(sb for _, sb, _ in results)
    peak_rss_kb = max((rss for _, _, rss in results), default=0)
    e2e_s = time.time() - t0

    r.delete(*['%s_a_%d' % (uid, i) for i in range(R)],
             *['%s_b_%d' % (uid, j) for j in range(C)],
             *['%s_c_%d_%d' % (uid, i, j) for i in range(R) for j in range(C)])

    gflops = (2.0 * N * N * N / e2e_s / 1e9) if e2e_s else 0
    e2e_ms = e2e_s * 1000.0
    peak_mb = peak_rss_kb / 1024.0
    ser_mb = ser_bytes / (1024 * 1024)

    header = 'size_n,workers,topo,e2e_ms,gflops,peak_mem_mb,kvs_ser_mb,checksum'
    row = ('%d,%d,rmmap-es,%.1f,%.2f,%.1f,%.2f,%d' %
           (N, W, e2e_ms, gflops, peak_mb, ser_mb, checksum))
    if want_header:
        print(header)
    print(row)


if __name__ == '__main__':
    main()
