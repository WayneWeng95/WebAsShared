#!/usr/bin/env python3
# infer.py — Cloudburst MNIST inference baseline on Redis (single box).
#
# Anna won't stand up here, so (as in WordCount/Matrix) we run a Redis-backed
# runner: the real split→predict→aggregate dataflow, model + chunks + partial
# results routed THROUGH Redis (cloudpickle), SINGLE process (predict functions
# sequential — Cloudburst's local runner has no per-function parallelism, curve
# ~flat in W). Same integer forward pass (infer_core) as the guest → identical
# prediction-checksum gate.
#
# Env: MLI_MODEL, MLI_DATA, REDIS_HOST/PORT. Args: SIZE_MB N_SAMPLES W [--header].
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
sys.path.insert(0, os.path.join(HERE, '..'))   # infer_core
import infer_core as core

REDIS_HOST = os.environ.get('REDIS_HOST', '127.0.0.1')
REDIS_PORT = int(os.environ.get('REDIS_PORT', '6379'))
HEADER = ('size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,'
          'kvs_ser_mb,checksum,accuracy')


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    size_mb = float(args[0]); n_samples = int(args[1]); W = int(args[2])

    X, y = core.load_csv(os.environ['MLI_DATA'])
    _, _, Wt = core.load_model(os.environ['MLI_MODEL'])
    N = X.shape[0]
    r = redis.Redis(host=REDIS_HOST, port=REDIS_PORT); r.flushdb()
    uid = 'cbi_%d_%d' % (n_samples, W)
    bounds = [(k * N // W, (k + 1) * N // W) for k in range(W)]

    correct = total = predsum = 0
    ser = 0
    # Timer starts AFTER the raw file load (staging) but BEFORE distribution +
    # compute — matching WasMem's TOTAL compute (which counts its split stage).
    t0 = time.time()
    mbuf = cp.dumps(Wt); r.set('%s_model' % uid, mbuf); ser += len(mbuf)
    for i, (lo, hi) in enumerate(bounds):
        bx = cp.dumps(np.ascontiguousarray(X[lo:hi])); by = cp.dumps(np.ascontiguousarray(y[lo:hi]))
        r.set('%s_x_%d' % (uid, i), bx); r.set('%s_y_%d' % (uid, i), by); ser += len(bx) + len(by)
    # SINGLE PROCESS: predict functions run sequentially, each routing its chunk
    # + model + result through Redis (the serialized state path).
    for i in range(W):
        sx = r.get('%s_x_%d' % (uid, i)); sy = r.get('%s_y_%d' % (uid, i)); mw = r.get('%s_model' % uid)
        c2, t2, p2 = core.evaluate(cp.loads(sx), cp.loads(sy), cp.loads(mw))
        rb = cp.dumps((c2, t2, p2)); r.set('%s_r_%d' % (uid, i), rb)
        ser += len(sx) + len(sy) + len(mw) + len(rb)
    for i in range(W):
        rb = r.get('%s_r_%d' % (uid, i)); ser += len(rb)
        c2, t2, p2 = cp.loads(rb); correct += c2; total += t2; predsum += p2
    compute_ms = (time.time() - t0) * 1000.0

    acc = 100.0 * correct / total if total else 0.0
    peak_mb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024.0
    sps = n_samples / (compute_ms / 1000.0) if compute_ms else 0
    if want_header:
        print(HEADER)
    print('%.1f,%d,redis-kvs-seq,%.1f,%.0f,%.1f,%.1f,%d,%.2f' %
          (size_mb, W, compute_ms, sps, peak_mb, ser / (1024 * 1024), predsum, acc))


if __name__ == '__main__':
    main()
