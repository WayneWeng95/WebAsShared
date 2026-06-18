#!/usr/bin/env python3
# driver.py — RMMap (dmerge) ES synchronous-SGD, single-box runner.
#
# RMMap's ml-pipeline is the suite's primary ML-training baseline (its closest
# twin: RDMA, serialization-free state). We run the **ES** protocol (Redis +
# pickle) — no MITOSIS kernel module (ES needs none, §5.7) — with each gradient
# worker a SEPARATE PROCESS, mirroring RMMap's per-function Knative pods (the
# parallelism that distinguishes it from Cloudburst's single-process runner).
#
# Faithful to the ES data path: the splitter pickles each data shard into Redis;
# then EVERY EPOCH each worker pod re-`redis_get`s its shard + the current model,
# computes the integer gradient SUM, and `redis_set`s it; the aggregator gets the
# W gradients, sums them, takes one central step, and writes the new model back.
# Every per-iteration shard/model/gradient byte is serialized through Redis — the
# transfer WebAsShared's zero-copy page-chain reports as 0. Same integer kernel
# (sgd_core) as the guest → identical weight-checksum gate.
#
# Dropped (not the serialization cost being measured): the Knative HTTP/CloudEvent
# envelope + placement, and the dmerge Cython bindings (only DMERGE needs them).
#
# Env: ML_DATA (csv), ML_EPOCHS, REDIS_HOST/PORT. Args: SIZE_MB N_SAMPLES W [--header].
# Prints one CSV row:
#   size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,kvs_ser_mb,checksum,accuracy
import os
import pickle
import resource
import sys
import time

import numpy as np
import redis

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, '..'))   # sgd_core
import sgd_core as core
from multiprocessing import Pool

REDIS_HOST = os.environ.get('REDIS_HOST', '127.0.0.1')
REDIS_PORT = int(os.environ.get('REDIS_PORT', '6379'))
HEADER = ('size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,'
          'kvs_ser_mb,checksum,accuracy')

_P = pickle.HIGHEST_PROTOCOL


def _smaps_kb():
    """(private_kb, shared_kb) of THIS process from /proc/self/smaps_rollup.
    Private = Private_Clean+Private_Dirty (per-process); Shared = Shared_* (common
    library/runtime pages). Point-in-time; call at the memory high-water."""
    priv = shared = 0
    try:
        with open('/proc/self/smaps_rollup') as f:
            for ln in f:
                key = ln.split(':', 1)[0]
                if key in ('Private_Clean', 'Private_Dirty'):
                    priv += int(ln.split()[1])
                elif key in ('Shared_Clean', 'Shared_Dirty'):
                    shared += int(ln.split()[1])
    except OSError:
        pass
    return priv, shared


def _worker(task):
    """One ES pod: re-fetch this shard + the current model from Redis, compute
    the integer gradient sum, write it back. Returns (serialized-byte count,
    private_kb, shared_kb) — the last two for the concurrent-footprint sum."""
    uid, i = task
    r = redis.Redis(host=REDIS_HOST, port=REDIS_PORT)
    sx = r.get('%s_x_%d' % (uid, i))
    sy = r.get('%s_y_%d' % (uid, i))
    mw = r.get('%s_model' % uid)
    X = pickle.loads(sx); y = pickle.loads(sy); W = pickle.loads(mw)
    g = core.grad_sum(X, y, W)
    gbuf = pickle.dumps(g, protocol=_P)
    priv_kb, shared_kb = _smaps_kb()   # high-water: shard X/y, model, gradient
    r.set('%s_g_%d' % (uid, i), gbuf)
    return len(sx) + len(sy) + len(mw) + len(gbuf), priv_kb, shared_kb


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
    uid = 'sgd_%d_%d' % (n_samples, W)
    # Timer starts AFTER the raw file load (staging) but BEFORE splitter +
    # worker startup — matching WasMem's TOTAL compute (counts partition/encode
    # + per-worker spawns). Only the disk read of the training file is excluded.
    bounds = [(k * N // W, (k + 1) * N // W) for k in range(W)]
    ser_bytes = 0
    Wt = core.init_weights(F)
    t0 = time.time()
    # ── splitter: pickle each contiguous shard into Redis (ES state) ──────────
    for i, (lo, hi) in enumerate(bounds):
        bx = pickle.dumps(np.ascontiguousarray(X[lo:hi]), protocol=_P)
        by = pickle.dumps(np.ascontiguousarray(y[lo:hi]), protocol=_P)
        r.set('%s_x_%d' % (uid, i), bx); r.set('%s_y_%d' % (uid, i), by)
        ser_bytes += len(bx) + len(by)
    pool = Pool(processes=W)                          # worker-pod startup (counted)
    peak_kb = 0   # peak concurrent worker footprint over epochs
    for _e in range(epochs):
        mbuf = pickle.dumps(Wt, protocol=_P)
        r.set('%s_model' % uid, mbuf)
        # parallel pods re-fetch shard+model, emit gradient
        results = pool.map(_worker, [(uid, i) for i in range(W)])
        ser_bytes += sum(b for b, _, _ in results)
        # Concurrent footprint: W separate pods each hold a PRIVATE shard+model,
        # so sum their private RSS + the shared runtime once (max). Track the
        # per-epoch peak. (Was parent RUSAGE_SELF only — the pods were uncounted.)
        ep_kb = (sum(p for _, p, _ in results)
                 + max((s for _, _, s in results), default=0))
        peak_kb = max(peak_kb, ep_kb)
        # aggregator: get the W gradients, sum, one central step
        gsum = np.zeros((core.N_CLASSES, F), dtype=np.int64)
        for i in range(W):
            gb = r.get('%s_g_%d' % (uid, i))
            ser_bytes += len(gb)
            gsum += pickle.loads(gb)
        Wt = core.apply_update(Wt, gsum, lr_den)
    compute_ms = (time.time() - t0) * 1000.0
    pool.close(); pool.join()

    correct, total = core.accuracy(Wt, X, y)
    acc = 100.0 * correct / total if total else 0.0
    ck = core.checksum(Wt)
    peak_mb = peak_kb / 1024.0
    sps = n_samples * epochs / (compute_ms / 1000.0) if compute_ms else 0
    ser_mb = ser_bytes / (1024 * 1024)

    if want_header:
        print(HEADER)
    print('%.1f,%d,redis-es-parallel,%.1f,%.0f,%.1f,%.1f,%d,%.2f' %
          (size_mb, W, compute_ms, sps, peak_mb, ser_mb, ck, acc))


if __name__ == '__main__':
    main()
