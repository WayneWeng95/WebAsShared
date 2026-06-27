#!/usr/bin/env python3
# driver_ml_training.py — Cloudburst inter-node SGD-training driver (runs on node 0).
#
# Synchronous SGD, ONE epoch (the weight_checksum gate), as a W-wide gradient fan over the
# executor pool, all state through Redis. MAP-only like FINRA: the inter-stage state is the
# tiny [C,F] gradient per worker; the heavy cost is the per-worker CSV parse.
#
#   1. (untimed) load X,y once on node 0 (for accuracy + N) and pre-split the raw CSV into
#      W newline-aligned shards (disjoint complete rows; Σ grad_sum over shards == full).
#   2. (timed) upload each shard's raw bytes → Redis; dispatch W ml_train tasks — each parses
#      its shard and computes grad_sum (W=zeros) with the shared sgd_core kernel.
#   3. (timed) aggregate the W gradients → ONE central integer apply_update → weight_checksum
#      (the fan-out-invariant gate = 1232) + accuracy.
#
# Metrics match wasmem/faasm results_ml_training.csv. Usage:
#   ./driver_ml_training.py --data TestData/ml_training_6m.csv --workers 60 --reps 15 \
#       --expect 1232 --csv ".../cloudburst/results_ml_training.csv"
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
import sgd_core

DONE_TIMEOUT = 300
MAX_REDISPATCH = 8


def split_aligned(data, n):
    """n contiguous newline-aligned shards of the raw CSV bytes (complete rows each)."""
    total = len(data)
    bounds = [i * total // n for i in range(n + 1)]
    for i in range(1, n):
        nl = data.find(b'\n', bounds[i])
        bounds[i] = (nl + 1) if nl != -1 else total
    return [data[bounds[i]:bounds[i + 1]] for i in range(n)]


def run_wave(R, task_q, tasks, done_key, label):
    R.delete(done_key)
    by_idx = {idx: (rk, tj) for idx, rk, tj in tasks}
    for _, _, tj in tasks:
        R.lpush(task_q, tj)
    pending, busy, rounds = set(by_idx), {}, 0
    while pending:
        item = R.blpop(done_key, timeout=DONE_TIMEOUT)
        if item is not None:
            idx_s, ms_s = item[1].decode().split(':')
            idx = int(idx_s)
            busy.setdefault(idx, float(ms_s))
            if idx in pending and R.exists(by_idx[idx][0]):
                pending.discard(idx); rounds = 0
            continue
        pending = {i for i in pending if not R.exists(by_idx[i][0])}
        if not pending:
            break
        rounds += 1
        if rounds > MAX_REDISPATCH:
            raise TimeoutError('%s stuck: %s' % (label, sorted(pending)))
        for i in sorted(pending):
            R.lpush(task_q, by_idx[i][1])
    return busy


def run_once(R, shards, F, N, W, task_q, X, y):
    uid = uuid.uuid4().hex
    done = 'cb:mltrain:' + uid
    t0 = time.time()
    up0 = time.time()
    for i, sh in enumerate(shards):
        R.set('%s_shard_%d' % (uid, i), sh)
    up_ms = (time.time() - up0) * 1000.0
    tasks = []
    for i in range(W):
        rk = '%s_g_%d' % (uid, i)
        tasks.append((i, rk, json.dumps(
            {'op': 'ml_train', 'idx': i, 'shard_key': '%s_shard_%d' % (uid, i),
             'result_key': rk, 'done_key': done})))
    busy = run_wave(R, task_q, tasks, done, 'ml_train')
    a0 = time.time()
    gsum = np.zeros((sgd_core.N_CLASSES, F), dtype=np.int64)
    for i in range(W):
        gsum += np.frombuffer(R.get('%s_g_%d' % (uid, i)), dtype=np.int64).reshape(sgd_core.N_CLASSES, F)
    Wt = sgd_core.apply_update(sgd_core.init_weights(F), gsum, N * sgd_core.SGD_LR_K)
    cks = int(sgd_core.checksum(Wt))
    correct, total = sgd_core.accuracy(Wt, X, y)
    agg_ms = (time.time() - a0) * 1000.0
    t1 = time.time()
    R.delete(*(['%s_shard_%d' % (uid, i) for i in range(W)] +
               ['%s_g_%d' % (uid, i) for i in range(W)] + [done]))
    return (t1 - t0) * 1000.0, up_ms + sum(busy.values()) + agg_ms, cks, 100.0 * correct / total


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--data', default='TestData/ml_training_6m.csv')
    ap.add_argument('--workers', type=int, default=60)
    ap.add_argument('--reps', type=int, default=15)
    ap.add_argument('--redis-host', default=os.environ.get('REDIS_HOST', '10.10.1.2'))
    ap.add_argument('--redis-port', type=int, default=int(os.environ.get('REDIS_PORT', '30679')))
    ap.add_argument('--task-queue', default='cb:tasks')
    ap.add_argument('--expect', type=int, default=None)
    ap.add_argument('--csv')
    a = ap.parse_args()

    if not os.path.exists(a.data):
        sys.exit('data not found: %s' % a.data)
    R = redis.Redis(host=a.redis_host, port=a.redis_port, socket_timeout=600)
    R.ping()
    X, y, F = sgd_core.load_csv(a.data)
    N = X.shape[0]
    with open(a.data, 'rb') as f:
        shards = split_aligned(f.read(), a.workers)
    print('[cloudburst-mltrain] samples=%d F=%d workers=%d reps=%d' % (N, F, a.workers, a.reps), flush=True)

    mk, tj = [], []
    cks = acc = None
    success = True
    for r in range(a.reps):
        m, j, c, ac = run_once(R, shards, F, N, a.workers, a.task_queue, X, y)
        mk.append(m); tj.append(j)
        if cks is None:
            cks, acc = c, ac
        elif c != cks:
            print('[cloudburst-mltrain] GATE FAIL rep%d: %d != %d' % (r, c, cks)); success = False
        print('[cloudburst-mltrain] rep%d makespan=%.0f ms total_job=%.0f ms weight_checksum=%d acc=%.2f%%' %
              (r, m, j, c, ac), flush=True)
    if a.expect is not None and cks != a.expect:
        print('[cloudburst-mltrain] GATE FAIL: %d != expected %d' % (cks, a.expect)); success = False
    mk_m = statistics.mean(mk); mk_s = statistics.pstdev(mk) if len(mk) > 1 else 0.0
    tj_m = statistics.mean(tj)
    print('[cloudburst-mltrain] === samples=%d workers=%d makespan=%.0f ± %.1f ms total_job=%.0f ms '
          'acc=%.2f%% weight_checksum=%d %s ===' %
          (N, a.workers, mk_m, mk_s, tj_m, acc, cks, 'OK' if success else 'GATE-FAIL'))
    if a.csv:
        os.makedirs(os.path.dirname(os.path.abspath(a.csv)), exist_ok=True)
        new = not os.path.exists(a.csv)
        with open(a.csv, 'a') as f:
            if new:
                f.write('samples,workers,nodes_used,makespan_mean_ms,makespan_std_ms,'
                        'total_job_mean_ms,accuracy_pct,weight_checksum,expect,success,reps\n')
            f.write('%d,%d,4,%.0f,%.1f,%.0f,%.2f,%d,%d,%s,%d\n' %
                    (N, a.workers, mk_m, mk_s, tj_m, round(acc, 2), cks,
                     a.expect if a.expect is not None else cks, success, a.reps))
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
