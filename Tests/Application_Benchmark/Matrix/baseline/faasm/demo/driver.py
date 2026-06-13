#!/usr/bin/env python3
# driver.py — Faasm-like matrix-multiply demo (Faaslet-lite), single box.
#
# Mirrors ../../../WordCount/baseline/faasm/demo/driver.py. SUMMA-style r×c block
# grid: A tiled into r block-rows, B into c block-cols; each block C_ij is one
# fresh wasmtime instance (a Faaslet) running matblock.cwasm. The host (this
# driver) does the KV I/O — panels written to Redis, each block worker reads its
# A_i / B_j panels and writes C_ij — i.e. **state serialized through the KV**, the
# transfer our zero-copy SHM page-chain avoids. Block workers run concurrently
# (a Faaslet per function). Same naive ikj kernel as the WebAsShared guest, so
# it's a true WASM-vs-WASM comparison differing only in the data path.
#
# Env: MAT_A, MAT_B (.bin paths), REDIS_HOST/PORT. Args: N WORKERS [--header].
import os
import struct
import subprocess
import sys
import tempfile
import time
import uuid
from concurrent.futures import ThreadPoolExecutor

import numpy as np
import redis

HERE = os.path.dirname(os.path.abspath(__file__))
WASM = os.path.join(HERE, 'matblock.cwasm')
WASMTIME = os.path.expanduser('~/.wasmtime/bin/wasmtime')


def grid(workers):
    r = 1
    k = 1
    while k * k <= workers:
        if workers % k == 0:
            r = k
        k += 1
    return r, workers // r


def run_faaslet(frame):
    """One fresh WASM instance (Faaslet): feed `frame` on stdin, return
    (stdout_bytes, peak_rss_kb)."""
    tf = tempfile.NamedTemporaryFile(delete=False)
    tf.close()
    p = subprocess.run(['/usr/bin/time', '-v', '-o', tf.name,
                        WASMTIME, 'run', '--allow-precompiled', WASM],
                       input=frame, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    rss_kb = 0
    try:
        with open(tf.name) as f:
            for ln in f:
                if 'Maximum resident set size' in ln:
                    rss_kb = int(ln.rsplit(':', 1)[1])
                    break
    finally:
        os.unlink(tf.name)
    return p.stdout, rss_kb


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    N = int(args[0])
    W = int(args[1]) if len(args) > 1 else int(os.environ.get('MAPPER_NUM', '4'))
    a_path = os.environ['MAT_A']
    b_path = os.environ['MAT_B']

    R, C = grid(W)
    assert N % R == 0 and N % C == 0, f"N={N} not divisible by grid {R}x{C}"
    br, bc = N // R, N // C

    A = np.fromfile(a_path).reshape(N, N)
    B = np.fromfile(b_path).reshape(N, N)

    r = redis.Redis(host=os.environ.get('REDIS_HOST', '127.0.0.1'),
                    port=int(os.environ.get('REDIS_PORT', '6379')))
    uid = uuid.uuid4().hex

    t0 = time.time()
    # ── tile: write the r A block-rows and c B block-cols to the KV ───────────
    state_bytes = 0
    for i in range(R):
        panel = np.ascontiguousarray(A[i * br:(i + 1) * br, :])  # br × N
        buf = panel.tobytes()
        r.set('%s_a_%d' % (uid, i), buf)
        state_bytes += len(buf)
    for j in range(C):
        panel = np.ascontiguousarray(B[:, j * bc:(j + 1) * bc])  # N × bc
        buf = panel.tobytes()
        r.set('%s_b_%d' % (uid, j), buf)
        state_bytes += len(buf)

    hdr = struct.pack('<III', br, bc, N)

    # ── block workers (Faaslet per (i,j), concurrent): A_i,B_j -> C_ij ────────
    def worker(ij):
        i, j = ij
        a_buf = r.get('%s_a_%d' % (uid, i))
        b_buf = r.get('%s_b_%d' % (uid, j))
        out, rss = run_faaslet(hdr + a_buf + b_buf)
        r.set('%s_c_%d_%d' % (uid, i, j), out)
        return (i, j), len(a_buf) + len(b_buf) + len(out), rss

    cells = [(i, j) for i in range(R) for j in range(C)]
    with ThreadPoolExecutor(max_workers=W) as ex:
        results = list(ex.map(worker, cells))
    state_bytes += sum(sb for _, sb, _ in results)
    peak_rss_kb = max((rss for _, _, rss in results), default=0)

    # ── assemble: read C blocks, fold checksum (Σ of all entries) ─────────────
    checksum = 0
    for (i, j) in cells:
        cbuf = r.get('%s_c_%d_%d' % (uid, i, j))
        block = np.frombuffer(cbuf, dtype=np.float64)
        checksum += int(block.sum())
    e2e_s = time.time() - t0

    r.delete(*[k for (i, j) in cells
               for k in ('%s_c_%d_%d' % (uid, i, j),)],
             *['%s_a_%d' % (uid, i) for i in range(R)],
             *['%s_b_%d' % (uid, j) for j in range(C)])

    gflops = (2.0 * N * N * N / e2e_s / 1e9) if e2e_s else 0
    e2e_ms = e2e_s * 1000.0
    peak_mb = peak_rss_kb / 1024.0
    state_mb = state_bytes / (1024 * 1024)

    header = ('size_n,workers,topo,e2e_ms,gflops,peak_mem_mb,state_kv_mb,checksum')
    row = ('%d,%d,wasm-redis,%.1f,%.2f,%.1f,%.2f,%d' %
           (N, W, e2e_ms, gflops, peak_mb, state_mb, checksum))
    if want_header:
        print(header)
    print(row)


if __name__ == '__main__':
    main()
