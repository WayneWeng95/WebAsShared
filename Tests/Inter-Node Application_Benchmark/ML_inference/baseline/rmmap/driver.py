#!/usr/bin/env python3
# driver.py — RMMap (dmerge) ES MNIST inference, single-box runner.
#
# RMMap's digital-minist is the suite's primary ML-inference baseline (its
# closest twin: RDMA, serialization-free state). We run the ES protocol
# (Redis + pickle) — no MITOSIS kernel module (§5.7) — with each predict worker
# a SEPARATE PROCESS (≈ Knative pod): the splitter pickles the model + each test
# shard into Redis; every worker pod re-`redis_get`s its shard + the model, runs
# the integer forward pass (infer_core, identical to the guest), and writes its
# partial counts back; the host sums them. Every byte crosses Redis serialized —
# the transfer WebAsShared's zero-copy page-chain reports as 0.
#
# Env: MLI_MODEL, MLI_DATA, REDIS_HOST/PORT. Args: SIZE_MB N_SAMPLES W [--header].
# Prints one CSV row:
#   size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,kvs_ser_mb,checksum,accuracy
import os
import pickle
import resource
import sys
import time
from multiprocessing import Pool

import numpy as np
import redis

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, '..'))   # infer_core
import infer_core as core

REDIS_HOST = os.environ.get('REDIS_HOST', '127.0.0.1')
REDIS_PORT = int(os.environ.get('REDIS_PORT', '6379'))
HEADER = ('size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,'
          'kvs_ser_mb,checksum,accuracy')
_P = pickle.HIGHEST_PROTOCOL


def _worker(task):
    """One ES pod: re-fetch this shard + the model from Redis, predict, write
    partial (correct,total,predsum) back. Returns serialized-byte count."""
    uid, i = task
    r = redis.Redis(host=REDIS_HOST, port=REDIS_PORT)
    sx = r.get('%s_x_%d' % (uid, i)); sy = r.get('%s_y_%d' % (uid, i)); mw = r.get('%s_model' % uid)
    X = pickle.loads(sx); y = pickle.loads(sy); W = pickle.loads(mw)
    corr, tot, psum = core.evaluate(X, y, W)
    out = pickle.dumps((corr, tot, psum), protocol=_P)
    r.set('%s_r_%d' % (uid, i), out)
    return len(sx) + len(sy) + len(mw) + len(out)


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    size_mb = float(args[0]); n_samples = int(args[1]); W = int(args[2])

    X, y = core.load_csv(os.environ['MLI_DATA'])
    _, _, Wt = core.load_model(os.environ['MLI_MODEL'])
    N = X.shape[0]
    r = redis.Redis(host=REDIS_HOST, port=REDIS_PORT)
    uid = 'inf_%d_%d' % (n_samples, W)

    bounds = [(k * N // W, (k + 1) * N // W) for k in range(W)]
    # Timer starts AFTER the raw file load (staging) but BEFORE data distribution
    # + worker startup — matching WasMem's TOTAL compute (which counts its split
    # + per-worker process spawns). Only the disk read of the test file is excluded.
    ser = 0
    t0 = time.time()
    mbuf = pickle.dumps(Wt, protocol=_P); r.set('%s_model' % uid, mbuf); ser += len(mbuf)
    for i, (lo, hi) in enumerate(bounds):
        bx = pickle.dumps(np.ascontiguousarray(X[lo:hi]), protocol=_P)
        by = pickle.dumps(np.ascontiguousarray(y[lo:hi]), protocol=_P)
        r.set('%s_x_%d' % (uid, i), bx); r.set('%s_y_%d' % (uid, i), by)
        ser += len(bx) + len(by)
    pool = Pool(processes=W)                          # worker-pod startup (counted)
    ser += sum(pool.map(_worker, [(uid, i) for i in range(W)]))
    correct = total = predsum = 0
    for i in range(W):
        rb = r.get('%s_r_%d' % (uid, i)); ser += len(rb)
        c2, t2, p2 = pickle.loads(rb); correct += c2; total += t2; predsum += p2
    compute_ms = (time.time() - t0) * 1000.0
    pool.close(); pool.join()

    acc = 100.0 * correct / total if total else 0.0
    peak_mb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024.0
    sps = n_samples / (compute_ms / 1000.0) if compute_ms else 0
    if want_header:
        print(HEADER)
    print('%.1f,%d,redis-es-parallel,%.1f,%.0f,%.1f,%.1f,%d,%.2f' %
          (size_mb, W, compute_ms, sps, peak_mb, ser / (1024 * 1024), predsum, acc))


if __name__ == '__main__':
    main()
