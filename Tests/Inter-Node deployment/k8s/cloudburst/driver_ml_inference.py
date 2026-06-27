#!/usr/bin/env python3
# driver_ml_inference.py — Cloudburst inter-node MNIST-inference driver (runs on node 0).
#
# Homogeneous W-wide predict fan over the executor pool, state through Redis. MAP-only:
# the inter-stage state is a tiny (correct,total,predsum) triple per worker; the heavy cost
# is the per-worker CSV parse.
#
#   1. (untimed) load model (infer_core) + pre-split the raw test CSV into W newline-aligned
#      shards.
#   2. (timed) upload the model + each shard's raw bytes → Redis; dispatch W ml_infer tasks —
#      each parses its shard and runs the integer forward pass (infer_core.evaluate).
#   3. (timed) aggregate → accuracy + prediction_checksum (Σ predicted labels; the gate).
#
# gate prediction_checksum is self-consistent: with the REGENERATED model (ml_inference_model
# .csv) it is 18,633,154 — differs from the wasm bars' 18,623,474 (original model), the same
# regenerated-data split WordCount has. Metrics match wasmem/faasm results_ml_inference.csv.
import argparse
import json
import os
import statistics
import struct
import sys
import time
import uuid

import numpy as np
import redis

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import infer_core

DONE_TIMEOUT = 300
MAX_REDISPATCH = 8


def split_aligned(data, n):
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


def run_once(R, shards, C, F, Wm, W, task_q):
    uid = uuid.uuid4().hex
    done = 'cb:mlinfer:' + uid
    t0 = time.time()
    up0 = time.time()
    R.set('%s_model' % uid, Wm.astype(np.int64).tobytes())
    for i, sh in enumerate(shards):
        R.set('%s_shard_%d' % (uid, i), sh)
    up_ms = (time.time() - up0) * 1000.0
    tasks = []
    for i in range(W):
        rk = '%s_r_%d' % (uid, i)
        tasks.append((i, rk, json.dumps(
            {'op': 'ml_infer', 'idx': i, 'shard_key': '%s_shard_%d' % (uid, i),
             'model_key': '%s_model' % uid, 'c': C, 'f': F,
             'result_key': rk, 'done_key': done})))
    busy = run_wave(R, task_q, tasks, done, 'ml_infer')
    a0 = time.time()
    correct = total = predsum = 0
    for i in range(W):
        c2, t2, p2 = struct.unpack('<qqq', R.get('%s_r_%d' % (uid, i))[:24])
        correct += c2; total += t2; predsum += p2
    agg_ms = (time.time() - a0) * 1000.0
    t1 = time.time()
    R.delete(*(['%s_shard_%d' % (uid, i) for i in range(W)] +
               ['%s_r_%d' % (uid, i) for i in range(W)] + ['%s_model' % uid, done]))
    return (t1 - t0) * 1000.0, up_ms + sum(busy.values()) + agg_ms, predsum, 100.0 * correct / total


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--data', default='TestData/ml_inference_6m.csv')
    ap.add_argument('--model', default='TestData/ml_inference_model.csv')
    ap.add_argument('--workers', type=int, default=60)
    ap.add_argument('--reps', type=int, default=15)
    ap.add_argument('--redis-host', default=os.environ.get('REDIS_HOST', '10.10.1.2'))
    ap.add_argument('--redis-port', type=int, default=int(os.environ.get('REDIS_PORT', '30679')))
    ap.add_argument('--task-queue', default='cb:tasks')
    ap.add_argument('--expect', type=int, default=None)
    ap.add_argument('--csv')
    a = ap.parse_args()

    for f in (a.data, a.model):
        if not os.path.exists(f):
            sys.exit('missing: %s' % f)
    R = redis.Redis(host=a.redis_host, port=a.redis_port, socket_timeout=600)
    R.ping()
    C, F, Wm = infer_core.load_model(a.model)
    with open(a.data, 'rb') as f:
        raw = f.read()
    N = raw.count(b'\n') + (0 if raw.endswith(b'\n') else 1)
    shards = split_aligned(raw, a.workers)
    print('[cloudburst-mlinfer] samples=%d C=%d F=%d workers=%d reps=%d' % (N, C, F, a.workers, a.reps), flush=True)

    mk, tj = [], []
    cks = acc = None
    success = True
    for r in range(a.reps):
        m, j, p, ac = run_once(R, shards, C, F, Wm, a.workers, a.task_queue)
        mk.append(m); tj.append(j)
        if cks is None:
            cks, acc = p, ac
        elif p != cks:
            print('[cloudburst-mlinfer] GATE FAIL rep%d: %d != %d' % (r, p, cks)); success = False
        print('[cloudburst-mlinfer] rep%d makespan=%.0f ms total_job=%.0f ms prediction_checksum=%d acc=%.2f%%' %
              (r, m, j, p, ac), flush=True)
    if a.expect is not None and cks != a.expect:
        print('[cloudburst-mlinfer] GATE FAIL: %d != expected %d' % (cks, a.expect)); success = False
    mk_m = statistics.mean(mk); mk_s = statistics.pstdev(mk) if len(mk) > 1 else 0.0
    tj_m = statistics.mean(tj)
    print('[cloudburst-mlinfer] === samples=%d workers=%d makespan=%.0f ± %.1f ms total_job=%.0f ms '
          'acc=%.2f%% prediction_checksum=%d %s ===' %
          (N, a.workers, mk_m, mk_s, tj_m, acc, cks, 'OK' if success else 'GATE-FAIL'))
    if a.csv:
        os.makedirs(os.path.dirname(os.path.abspath(a.csv)), exist_ok=True)
        new = not os.path.exists(a.csv)
        with open(a.csv, 'a') as f:
            if new:
                f.write('samples,workers,nodes_used,makespan_mean_ms,makespan_std_ms,'
                        'total_job_mean_ms,accuracy_pct,prediction_checksum,expect,success,reps\n')
            f.write('%d,%d,4,%.0f,%.1f,%.0f,%.2f,%d,%d,%s,%d\n' %
                    (N, a.workers, mk_m, mk_s, tj_m, round(acc, 2), cks,
                     a.expect if a.expect is not None else cks, success, a.reps))
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
