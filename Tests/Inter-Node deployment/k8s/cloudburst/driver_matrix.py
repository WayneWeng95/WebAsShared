#!/usr/bin/env python3
# driver_matrix.py — Cloudburst inter-node Matrix (SUMMA) driver (runs on node 0).
#
# C = A·B as an R×C block grid over the same executor pool, all inter-stage state through
# Redis (the object-heavy serialized transfer the comparison is about). One wave:
#
#   1. tile (node 0)    : A → R row-panels A_i (BR×N), B → C col-panels B_j (N×BC) → Redis
#                         {uid}_a_{i} / {uid}_b_{j}  (the SUMMA broadcast; read R·C times).
#   2. block wave        : R·C mat_block tasks — each reads its A_i + B_j panels, computes
#                         C_ij = A_i·B_j with NAIVE einsum(optimize=False) (matches the WASM
#                         ikj bars, fairness §5), writes C_ij → {uid}_c_{i}_{j}.
#   3. assemble (node 0) : read every C block, fold the f64 checksum (Σ of all entries) —
#                         the fan-out-invariant gate (= 1,391,095,867,672 for 4096²).
#
# grid(W): balanced R×C (R≤C, R·C==W) — W=64 → 8×8, BR=BC=512. Metrics match the
# wasmem/faasm matrix CSV: makespan (incl. panel upload — Cloudburst's KVS cost), total_job
# = tile + Σ block busy + assemble, gflops = 2N³/makespan.
#
# Usage:
#   ./driver_matrix.py --a TestData/matrix_a_4096.bin --b TestData/matrix_b_4096.bin \
#       --matrix-n 4096 --workers 64 --reps 15 --expect 1391095867672 \
#       --csv "Tests/Inter-Node Application_Benchmark/cloudburst/results_matrix.csv"
import argparse
import json
import os
import statistics
import sys
import time
import uuid

import numpy as np
import redis

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mem_probe import MemSampler
import exec_pool

DONE_TIMEOUT = 300
MAX_REDISPATCH = 8


def grid(w):
    """Balanced R×C factorization (R≤C, R·C==w) — matches the faasm/wasmem grid()."""
    r, k = 1, 1
    while k * k <= w:
        if w % k == 0:
            r = k
        k += 1
    return r, w // r


def run_wave(R, task_q, tasks, done_key, label):
    R.delete(done_key)
    by_idx = {idx: (rk, tj) for idx, rk, tj in tasks}
    for _, _, tj in tasks:
        R.lpush(task_q, tj)
    pending = set(by_idx)
    busy, rounds = {}, 0
    while pending:
        item = R.blpop(done_key, timeout=DONE_TIMEOUT)
        if item is not None:
            idx_s, ms_s = item[1].decode().split(':')
            idx = int(idx_s)
            busy.setdefault(idx, float(ms_s))
            if idx in pending and R.exists(by_idx[idx][0]):
                pending.discard(idx)
                rounds = 0
            continue
        pending = {i for i in pending if not R.exists(by_idx[i][0])}
        if not pending:
            break
        rounds += 1
        if rounds > MAX_REDISPATCH:
            raise TimeoutError('%s stuck after %d rounds: %s' % (label, rounds, sorted(pending)))
        print('[cloudburst-matrix]   re-dispatching %d lost: %s'
              % (len(pending), sorted(pending)), flush=True)
        for i in sorted(pending):
            R.lpush(task_q, by_idx[i][1])
    return busy


def run_once(Rds, A, B, N, Rg, Cg, task_q, cold=False):
    uid = uuid.uuid4().hex
    done = 'cb:matrix:' + uid
    br, bc = N // Rg, N // Cg

    t0 = time.time()
    # --- tile (node 0): A row-panels + B col-panels → Redis (the broadcast upload) ---
    s0 = time.time()
    for i in range(Rg):
        Rds.set('%s_a_%d' % (uid, i), np.ascontiguousarray(A[i * br:(i + 1) * br, :]).tobytes())
    for j in range(Cg):
        Rds.set('%s_b_%d' % (uid, j), np.ascontiguousarray(B[:, j * bc:(j + 1) * bc]).tobytes())
    tile_ms = (time.time() - s0) * 1000.0

    # --- block wave: R·C mat_block tasks ---
    cells = [(i, j) for i in range(Rg) for j in range(Cg)]
    tasks = []
    for idx, (i, j) in enumerate(cells):
        rk = '%s_c_%d_%d' % (uid, i, j)
        tasks.append((idx, rk, json.dumps(
            {'op': 'mat_block', 'idx': idx, 'n': N, 'br': br, 'bc': bc,
             'a_key': '%s_a_%d' % (uid, i), 'b_key': '%s_b_%d' % (uid, j),
             'result_key': rk, 'done_key': done})))
    if cold:                                  # cold start: launch executors ON the block wave
        exec_pool.launch_on_wave(len(tasks))
    busy = run_wave(Rds, task_q, tasks, done, 'block')

    # --- assemble (node 0): fold the checksum over all C blocks ---
    a0 = time.time()
    checksum = 0
    for (i, j) in cells:
        cbuf = Rds.get('%s_c_%d_%d' % (uid, i, j))
        checksum += int(np.frombuffer(cbuf, dtype=np.float64).sum())
    assemble_ms = (time.time() - a0) * 1000.0

    t1 = time.time()
    cleanup = (['%s_a_%d' % (uid, i) for i in range(Rg)] +
               ['%s_b_%d' % (uid, j) for j in range(Cg)] +
               ['%s_c_%d_%d' % (uid, i, j) for (i, j) in cells] + [done])
    Rds.delete(*cleanup)
    makespan_ms = (t1 - t0) * 1000.0
    total_job_ms = tile_ms + sum(busy.values()) + assemble_ms
    return makespan_ms, total_job_ms, checksum


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--a', default='TestData/matrix_a_4096.bin')
    ap.add_argument('--b', default='TestData/matrix_b_4096.bin')
    ap.add_argument('--matrix-n', type=int, default=4096)
    ap.add_argument('--workers', type=int, default=64, help='block count R·C')
    ap.add_argument('--warm-reps', type=int, default=12, help='warm reps (pool pre-launched)')
    ap.add_argument('--cold-reps', type=int, default=3, help='cold reps (executors launched on-wave)')
    ap.add_argument('--reps', type=int, default=None, help='ignored (kept for back-compat)')
    ap.add_argument('--redis-host', default=os.environ.get('REDIS_HOST', '10.10.1.2'))
    ap.add_argument('--redis-port', type=int, default=int(os.environ.get('REDIS_PORT', '30679')))
    ap.add_argument('--task-queue', default='cb:tasks')
    ap.add_argument('--expect', type=int, default=None)
    ap.add_argument('--csv')
    a = ap.parse_args()

    N, W = a.matrix_n, a.workers
    Rg, Cg = grid(W)
    if N % Rg or N % Cg:
        sys.exit('N=%d not divisible by grid %dx%d' % (N, Rg, Cg))
    for f in (a.a, a.b):
        if not os.path.exists(f):
            sys.exit('missing: %s' % f)
    Rds = redis.Redis(host=a.redis_host, port=a.redis_port, socket_timeout=600)
    try:
        Rds.ping()
    except Exception as e:
        sys.exit('redis unreachable at %s:%d (%r)' % (a.redis_host, a.redis_port, e))

    A = np.fromfile(a.a).reshape(N, N)
    B = np.fromfile(a.b).reshape(N, N)
    print('[cloudburst-matrix] N=%d grid=%dx%d workers=%d warm=%d cold=%d' %
          (N, Rg, Cg, W, a.warm_reps, a.cold_reps), flush=True)

    mk_warm, mk_cold, tj_ms = [], [], []
    cks = None
    success = True
    mem = MemSampler(a.redis_host, a.redis_port); mem.start()   # peak-memory-cost sampler

    def _rep(r, cold):
        nonlocal cks, success
        m, tj, c = run_once(Rds, A, B, N, Rg, Cg, a.task_queue, cold=cold)
        tj_ms.append(tj)
        if cks is None:
            cks = c
        elif c != cks:
            print('[cloudburst-matrix] GATE FAIL %s rep%d: checksum %d != %d' %
                  ('cold' if cold else 'warm', r, c, cks)); success = False
        print('[cloudburst-matrix] %s rep%d makespan=%.0f ms total_job=%.0f ms checksum=%d' %
              ('cold' if cold else 'warm', r, m, tj, c), flush=True)
        return m

    for r in range(a.cold_reps):
        exec_pool.teardown()
        mk_cold.append(_rep(r, True))
    exec_pool.launch_on_wave(W)
    for r in range(a.warm_reps):
        mk_warm.append(_rep(r, False))
    exec_pool.teardown()

    mem.stop.set(); mem.join()
    mem_total_mb, mem_max_mb, redis_peak_mb = mem.result()

    if a.expect is not None and cks != a.expect:
        print('[cloudburst-matrix] GATE FAIL: checksum %d != expected %d' % (cks, a.expect))
        success = False

    mk_mean = statistics.mean(mk_warm) if mk_warm else 0.0
    mk_std = statistics.pstdev(mk_warm) if len(mk_warm) > 1 else 0.0
    cold_mean = statistics.mean(mk_cold) if mk_cold else 0.0
    cold_std = statistics.pstdev(mk_cold) if len(mk_cold) > 1 else 0.0
    tj_mean = statistics.mean(tj_ms)
    gflops = round(2.0 * N ** 3 / (mk_mean / 1000.0) / 1e9, 3) if mk_mean > 0 else 0
    print('[cloudburst-matrix] === N=%d workers=%d warm_makespan=%.0f ± %.1f ms '
          'cold_makespan=%.0f ± %.1f ms total_job=%.0f ms gflops=%.3f checksum=%d %s ===' %
          (N, W, mk_mean, mk_std, cold_mean, cold_std, tj_mean, gflops, cks,
           'OK' if success else 'GATE-FAIL'))

    if a.csv:
        os.makedirs(os.path.dirname(os.path.abspath(a.csv)), exist_ok=True)
        new = not os.path.exists(a.csv)
        with open(a.csv, 'a') as f:
            if new:
                f.write('matrix_n,workers,nodes_used,makespan_mean_ms,makespan_std_ms,'
                        'total_job_mean_ms,gflops,checksum,expect,success,reps,'
                        'mem_max_mb,mem_total_mb,redis_peak_mb,'
                        'cold_makespan_mean_ms,cold_makespan_std_ms\n')
            f.write('%d,%d,8,%.0f,%.1f,%.0f,%.3f,%d,%d,%s,%d,%.0f,%.0f,%.0f,%.0f,%.1f\n' %
                    (N, W, mk_mean, mk_std, tj_mean, gflops, cks,
                     a.expect if a.expect is not None else cks, success, a.warm_reps,
                     mem_max_mb, mem_total_mb, redis_peak_mb, cold_mean, cold_std))
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
