#!/usr/bin/env python3
# driver.py — Faasm-like synchronous-SGD demo (Faaslet-lite), single box.
#
# Each gradient computation is one fresh wasmtime instance (a Faaslet) running
# sgd_grad.cwasm; the host (this driver) does the KV I/O — every epoch it writes
# the model to Redis, each Faaslet's data shard + model are read from Redis and
# fed on stdin, the gradient comes back on stdout and is written to Redis, and
# the aggregator reads the W gradients and takes one central integer step
# (sgd_core, identical to the guest). State serialized through the KV = the
# transfer our zero-copy SHM page-chain avoids. Faaslets run CONCURRENTLY (a
# Faaslet per function). Same integer kernel as the guest → identical checksum.
#
# Env: ML_DATA (csv), ML_EPOCHS, REDIS_HOST/PORT. Args: SIZE_MB N_SAMPLES W [--header].
# Prints one CSV row:
#   size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,kvs_ser_mb,checksum,accuracy
import os
import resource
import struct
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

import numpy as np
import redis

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, '..', '..', '..'))  # ML_training/ for nothing; sgd_core is ../../
sys.path.insert(0, os.path.join(HERE, '..', '..'))        # baseline/ → sgd_core
import sgd_core as core

WASM = os.path.join(HERE, 'sgd_grad.cwasm')
WASMTIME = os.path.expanduser('~/.wasmtime/bin/wasmtime')
REDIS_HOST = os.environ.get('REDIS_HOST', '127.0.0.1')
REDIS_PORT = int(os.environ.get('REDIS_PORT', '6379'))
HEADER = ('size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,'
          'kvs_ser_mb,checksum,accuracy')


def make_frame(Xi, yi, W):
    """Build the Faaslet stdin frame: [n][f] model(C*f i64) then samples."""
    n, f = Xi.shape
    buf = bytearray()
    buf += struct.pack('<II', n, f)
    buf += W.astype('<i8').tobytes()                       # C*f i64
    rows = np.empty((n, f + 1), dtype='<i4')
    rows[:, 0] = yi.astype('<i4')
    rows[:, 1:] = Xi.astype('<i4')
    buf += rows.tobytes()
    return bytes(buf)


def run_faaslet(frame):
    """One fresh WASM instance (Faaslet): feed `frame` on stdin, return stdout."""
    p = subprocess.run([WASMTIME, 'run', '--allow-precompiled', WASM],
                       input=frame, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    return p.stdout


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    size_mb = float(args[0]); n_samples = int(args[1]); W = int(args[2])
    epochs = int(os.environ.get('ML_EPOCHS', '10'))
    data = os.environ['ML_DATA']

    X, y, F = core.load_csv(data)
    N = X.shape[0]
    lr_den = N * core.SGD_LR_K
    bounds = [(k * N // W, (k + 1) * N // W) for k in range(W)]

    r = redis.Redis(host=REDIS_HOST, port=REDIS_PORT)
    r.flushdb()
    uid = 'fa_%d_%d' % (n_samples, W)
    # KV state: each shard's raw bytes (the host re-reads them every epoch).
    ser_bytes = 0
    shard_bytes = []
    Wt = core.init_weights(F)
    pool = ThreadPoolExecutor(max_workers=W)
    # Timer starts after the raw file load (staging), before distribution +
    # Faaslet spawns — matching WasMem's TOTAL compute.
    t0 = time.time()
    for i, (lo, hi) in enumerate(bounds):
        sb = np.ascontiguousarray(X[lo:hi]).tobytes()
        yb = np.ascontiguousarray(y[lo:hi]).tobytes()
        r.set('%s_x_%d' % (uid, i), sb); r.set('%s_y_%d' % (uid, i), yb)
        shard_bytes.append((len(sb), len(yb), lo, hi))
    for _e in range(epochs):
        mbuf = Wt.astype('<i8').tobytes()
        r.set('%s_model' % uid, mbuf)

        def one(i):
            lo, hi = bounds[i]
            sx = r.get('%s_x_%d' % (uid, i)); sy = r.get('%s_y_%d' % (uid, i))
            mw = r.get('%s_model' % uid)
            Xi = np.frombuffer(sx, dtype=np.int64).reshape(hi - lo, F)
            yi = np.frombuffer(sy, dtype=np.int64)
            grad = run_faaslet(make_frame(Xi, yi, Wt))
            r.set('%s_g_%d' % (uid, i), grad)
            return len(sx) + len(sy) + len(mw) + len(grad)

        for b in pool.map(one, range(W)):
            ser_bytes += b
        gsum = np.zeros((core.N_CLASSES, F), dtype=np.int64)
        for i in range(W):
            gb = r.get('%s_g_%d' % (uid, i))
            ser_bytes += len(gb)
            gsum += np.frombuffer(gb, dtype=np.int64).reshape(core.N_CLASSES, F)
        Wt = core.apply_update(Wt, gsum, lr_den)
    compute_ms = (time.time() - t0) * 1000.0
    pool.shutdown()

    correct, total = core.accuracy(Wt, X, y)
    acc = 100.0 * correct / total if total else 0.0
    ck = core.checksum(Wt)
    peak_mb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024.0
    sps = n_samples * epochs / (compute_ms / 1000.0) if compute_ms else 0
    ser_mb = ser_bytes / (1024 * 1024)

    if want_header:
        print(HEADER)
    print('%.1f,%d,wasm-faaslet-redis,%.1f,%.0f,%.1f,%.1f,%d,%.2f' %
          (size_mb, W, compute_ms, sps, peak_mb, ser_mb, ck, acc))


if __name__ == '__main__':
    main()
