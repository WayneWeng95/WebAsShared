#!/usr/bin/env python3
# sgd.py — Cloudburst synchronous-SGD baseline on Redis (single box).
#
# Cloudburst's state plane is the Anna KVS; the full Anna control plane won't
# stand up here (2019/py3.6 stack), so — as in the WordCount/Matrix baselines —
# we run a **Redis-backed** runner: the real split→gradient→aggregate dataflow,
# every chunk / model / gradient routed THROUGH Redis (cloudpickle), in-process.
# That Redis round-trip is the serialized state transfer WebAsShared's zero-copy
# page-chain avoids.
#
# One FRESH process per (size,W) cell (clean peak RSS). SINGLE-PROCESS: the W
# gradient "functions" run sequentially (Cloudburst's local runner has no
# per-function parallelism — its curve is ~flat in W; noted as a caveat, §4.4-style).
# Re-reads each shard + the model from Redis EVERY EPOCH (stateless functions).
# Same integer kernel (sgd_core) as the guest → identical weight-checksum gate.
#
# Env: ML_DATA (csv), ML_EPOCHS, REDIS_HOST/PORT. Args: SIZE_MB N_SAMPLES W [--header].
# Prints one CSV row:
#   size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,kvs_ser_mb,checksum,accuracy
import os
import resource
import sys
import time

import numpy as np
import redis
try:
    import cloudpickle as cp
except ImportError:
    import pickle as cp

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, '..'))   # sgd_core
import sgd_core as core

REDIS_HOST = os.environ.get('REDIS_HOST', '127.0.0.1')
REDIS_PORT = int(os.environ.get('REDIS_PORT', '6379'))
HEADER = ('size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,'
          'kvs_ser_mb,checksum,accuracy')


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    size_mb = float(args[0]); n_samples = int(args[1]); W = int(args[2])
    epochs = int(os.environ.get('ML_EPOCHS', '10'))
    data = os.environ['ML_DATA']

    X, y, F = core.load_csv(data)
    N = X.shape[0]
    lr_den = N * core.SGD_LR_K

    r = redis.Redis(host=REDIS_HOST, port=REDIS_PORT)
    r.flushdb()
    uid = 'cb_%d_%d' % (n_samples, W)
    bounds = [(k * N // W, (k + 1) * N // W) for k in range(W)]
    ser_bytes = 0
    # split: each chunk put through the KVS once
    for i, (lo, hi) in enumerate(bounds):
        bx = cp.dumps(np.ascontiguousarray(X[lo:hi]))
        by = cp.dumps(np.ascontiguousarray(y[lo:hi]))
        r.set('%s_x_%d' % (uid, i), bx); r.set('%s_y_%d' % (uid, i), by)
        ser_bytes += len(bx) + len(by)

    Wt = core.init_weights(F)
    t0 = time.time()
    for _e in range(epochs):
        mbuf = cp.dumps(Wt)
        r.set('%s_model' % uid, mbuf)
        gsum = np.zeros((core.N_CLASSES, F), dtype=np.int64)
        # SINGLE PROCESS: gradient functions run sequentially, each routing its
        # chunk + model + gradient through Redis (the serialized state path).
        for i in range(W):
            sx = r.get('%s_x_%d' % (uid, i)); sy = r.get('%s_y_%d' % (uid, i))
            mw = r.get('%s_model' % uid)
            Xi = cp.loads(sx); yi = cp.loads(sy); Wi = cp.loads(mw)
            g = core.grad_sum(Xi, yi, Wi)
            gbuf = cp.dumps(g)
            r.set('%s_g_%d' % (uid, i), gbuf)
            ser_bytes += len(sx) + len(sy) + len(mw) + len(gbuf)
        for i in range(W):
            gb = r.get('%s_g_%d' % (uid, i))
            ser_bytes += len(gb)
            gsum += cp.loads(gb)
        Wt = core.apply_update(Wt, gsum, lr_den)
    compute_ms = (time.time() - t0) * 1000.0

    correct, total = core.accuracy(Wt, X, y)
    acc = 100.0 * correct / total if total else 0.0
    ck = core.checksum(Wt)
    peak_mb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024.0
    sps = n_samples * epochs / (compute_ms / 1000.0) if compute_ms else 0
    ser_mb = ser_bytes / (1024 * 1024)

    if want_header:
        print(HEADER)
    print('%.1f,%d,redis-kvs-seq,%.1f,%.0f,%.1f,%.1f,%d,%.2f' %
          (size_mb, W, compute_ms, sps, peak_mb, ser_mb, ck, acc))


if __name__ == '__main__':
    main()
