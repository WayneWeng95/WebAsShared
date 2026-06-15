#!/usr/bin/env python3
# driver.py — Faasm-like MNIST inference demo (Faaslet-lite), single box.
#
# Each shard's forward pass runs as a fresh wasmtime instance (Faaslet) on
# infer_predict.cwasm; the host moves the model + shard + result through Redis
# (serialized state — what our zero-copy SHM page-chain avoids). Faaslets run
# concurrently. SAME integer kernel as the guest → identical prediction checksum.
#
# Env: MLI_MODEL, MLI_DATA, REDIS_HOST/PORT. Args: SIZE_MB N_SAMPLES W [--header].
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
sys.path.insert(0, os.path.join(HERE, '..', '..'))   # baseline/ → infer_core
import infer_core as core

WASM = os.path.join(HERE, 'infer_predict.cwasm')
WASMTIME = os.path.expanduser('~/.wasmtime/bin/wasmtime')
REDIS_HOST = os.environ.get('REDIS_HOST', '127.0.0.1')
REDIS_PORT = int(os.environ.get('REDIS_PORT', '6379'))
HEADER = ('size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,'
          'kvs_ser_mb,checksum,accuracy')


def make_frame(Xi, yi, W):
    n, f = Xi.shape
    buf = bytearray(struct.pack('<II', n, f))
    buf += W.astype('<i8').tobytes()
    rows = np.empty((n, f + 1), dtype='<i4')
    rows[:, 0] = yi.astype('<i4'); rows[:, 1:] = Xi.astype('<i4')
    buf += rows.tobytes()
    return bytes(buf)


def run_faaslet(frame):
    p = subprocess.run([WASMTIME, 'run', '--allow-precompiled', WASM],
                       input=frame, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    return p.stdout


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    size_mb = float(args[0]); n_samples = int(args[1]); W = int(args[2])

    X, y = core.load_csv(os.environ['MLI_DATA'])
    _, F, Wt = core.load_model(os.environ['MLI_MODEL'])
    N = X.shape[0]
    bounds = [(k * N // W, (k + 1) * N // W) for k in range(W)]
    r = redis.Redis(host=REDIS_HOST, port=REDIS_PORT); r.flushdb()
    uid = 'fai_%d_%d' % (n_samples, W)

    ser = 0
    pool = ThreadPoolExecutor(max_workers=W)
    # Timer starts AFTER the raw file load (staging) but BEFORE distribution +
    # Faaslet spawns — matching WasMem's TOTAL compute. (Faaslet wasmtime spawns
    # already ran inside the timed region; now the splitter writes do too.)
    t0 = time.time()
    mbuf = Wt.astype('<i8').tobytes(); r.set('%s_model' % uid, mbuf)
    for i, (lo, hi) in enumerate(bounds):
        sb = np.ascontiguousarray(X[lo:hi]).tobytes(); yb = np.ascontiguousarray(y[lo:hi]).tobytes()
        r.set('%s_x_%d' % (uid, i), sb); r.set('%s_y_%d' % (uid, i), yb)

    def one(i):
        lo, hi = bounds[i]
        sx = r.get('%s_x_%d' % (uid, i)); sy = r.get('%s_y_%d' % (uid, i)); mw = r.get('%s_model' % uid)
        Xi = np.frombuffer(sx, dtype=np.int64).reshape(hi - lo, F)
        yi = np.frombuffer(sy, dtype=np.int64)
        Wm = np.frombuffer(mw, dtype=np.int64).reshape(Wt.shape)
        res = run_faaslet(make_frame(Xi, yi, Wm))
        r.set('%s_r_%d' % (uid, i), res)
        return len(sx) + len(sy) + len(mw) + len(res)

    ser += sum(pool.map(one, range(W)))
    correct = total = predsum = 0
    for i in range(W):
        rb = r.get('%s_r_%d' % (uid, i)); ser += len(rb)
        c2, t2, p2 = struct.unpack('<qqq', rb); correct += c2; total += t2; predsum += p2
    compute_ms = (time.time() - t0) * 1000.0
    pool.shutdown()

    acc = 100.0 * correct / total if total else 0.0
    peak_mb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024.0
    sps = n_samples / (compute_ms / 1000.0) if compute_ms else 0
    if want_header:
        print(HEADER)
    print('%.1f,%d,wasm-faaslet-redis,%.1f,%.0f,%.1f,%.1f,%d,%.2f' %
          (size_mb, W, compute_ms, sps, peak_mb, ser / (1024 * 1024), predsum, acc))


if __name__ == '__main__':
    main()
