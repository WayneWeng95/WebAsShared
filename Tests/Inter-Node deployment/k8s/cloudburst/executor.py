#!/usr/bin/env python3
# executor.py — Cloudburst "executor" worker, one per pod, spread across the 4 nodes.
#
# The Cloudburst port (redis_runner.py) is a single-process stand-in; this is its
# distributed analogue: a pool of executor pods that pull function tasks off a Redis
# queue and run the REAL stage bodies (wc_ops), routing every inter-stage value
# through Redis. That reproduces exactly what the comparison measures — state moved
# between DAG stages, serialized through an external KVS — but now ACROSS nodes.
#
# Protocol (all via Redis):
#   task  = JSON {op, idx, ...keys, result_key, done_key}    popped from CB_TASK_QUEUE
#   result = cloudpickle(state)  SET at result_key           (the inter-stage state)
#   done   = LPUSH "idx:ms" -> done_key                      (driver waits on this)
#
# Ops:
#   wc_mapper    — WordCount map: read chunk_key → wc_ops.wc_map → SET result_key.
#   ts_partition — TeraSort scatter: read chunk_key → ts_partition into n owner buckets →
#                  SET each bucket_prefix_<idx>_<j>; SET result_key as a tiny done marker.
#   ts_merge     — TeraSort gather: read this owner's column bucket_prefix_<i>_<idx> for
#                  i in 0..n → ts_merge (sort) → SET result_key = cloudpickle(summary).
import json
import os
import socket
import struct
import sys
import time
import traceback

import redis
import cloudpickle as cp
import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wc_ops
import ts_ops
import finra_rules
import sgd_core
import infer_core


def _parse_ml_csv(data):
    """CSV-text bytes → (X int64 [n,F], y int64 [n]) — same parse as sgd_core/infer_core
    load_csv, but from a byte shard (no header in the ML data)."""
    rows = [[int(v) for v in ln.split(',')]
            for ln in data.decode('ascii', 'ignore').splitlines()
            if ln and not ln.startswith('label')]
    arr = np.asarray(rows, dtype=np.int64)
    return arr[:, 1:].copy(), arr[:, 0].copy()

R = redis.Redis(host=os.environ.get('REDIS_HOST', 'redis.baselines.svc.cluster.local'),
                port=int(os.environ.get('REDIS_PORT', '6379')))
TASK_Q = os.environ.get('CB_TASK_QUEUE', 'cb:tasks')
NAME = socket.gethostname()


def handle(task):
    # Time the worker's whole busy span (KVS read + compute + KVS write) — this
    # per-mapper duration is summed by the driver into total_job_ms (Σ busy
    # node-seconds), the analogue of wasmem's per-node executor durations.
    t0 = time.time()
    op = task['op']
    if op == 'wc_mapper':
        chunk = R.get(task['chunk_key'])
        partial = wc_ops.wc_map(chunk if chunk is not None else b'')
        R.set(task['result_key'], cp.dumps(partial))     # inter-stage state via KVS
    elif op == 'ts_partition':
        i, n, pfx = task['idx'], task['n'], task['bucket_prefix']
        chunk = R.get(task['chunk_key'])
        buckets = ts_ops.ts_partition(chunk if chunk is not None else b'', n)
        for j in range(n):                               # scatter: one Redis key per owner
            R.set('%s_%d_%d' % (pfx, i, j), buckets[j])
        R.set(task['result_key'], b'1')                  # tiny done marker (presence-checked)
    elif op == 'ts_merge':
        j, n, pfx = task['idx'], task['n'], task['bucket_prefix']
        cols = [R.get('%s_%d_%d' % (pfx, i, j)) for i in range(n)]   # gather this column
        summary = ts_ops.ts_merge(cols)
        R.set(task['result_key'], cp.dumps(summary))     # inter-stage state via KVS
    elif op == 'finra_rule':
        # One FINRA audit rule over an input (a header-prefixed shard for the 5 stateless
        # rules, the full trades for the 3 stateful ones). The inter-stage state is just the
        # violation COUNT — like WordCount; the heavy cost is the per-worker trades read+parse.
        data = R.get(task['input_key'])
        trades = finra_rules.parse_trades(data if data is not None else b'')
        v = finra_rules.audit_rule(trades, task['rule'])
        R.set(task['result_key'], str(v).encode())       # count via KVS
    elif op == 'mat_block':
        # One SUMMA block C_ij = A_i · B_j. The inter-stage state is the f64 matrix BLOCKS
        # (A_i/B_j panels in, C_ij out) — the object-heavy transfer Cloudburst serializes
        # through Redis. NAIVE O(N^3) einsum(optimize=False), NOT BLAS, so the kernel matches
        # the WASM ikj bars (fairness §5 — the comparison is the data substrate, not the kernel).
        n, br, bc = task['n'], task['br'], task['bc']
        a = np.frombuffer(R.get(task['a_key']), dtype=np.float64).reshape(br, n)
        b = np.frombuffer(R.get(task['b_key']), dtype=np.float64).reshape(n, bc)
        c = np.einsum('ik,kj->ij', a, b, optimize=False)
        R.set(task['result_key'], c.tobytes())            # C block via KVS
    elif op == 'ml_train':
        # One SGD shard: parse its samples, compute the integer gradient SUM (W starts at
        # zeros, one epoch) — the inter-stage state is the tiny [C,F] gradient; the heavy
        # cost is the per-worker CSV parse, like FINRA. Reuses the shared sgd_core kernel.
        X, y = _parse_ml_csv(R.get(task['shard_key']))
        g = sgd_core.grad_sum(X, y, sgd_core.init_weights(X.shape[1]))
        R.set(task['result_key'], g.tobytes())            # [C,F] i64 gradient via KVS
    elif op == 'ml_infer':
        # One predict shard: parse samples, integer forward pass with the shared model →
        # (correct, total, predsum). Reuses infer_core (same kernel as the wasm bars).
        X, y = _parse_ml_csv(R.get(task['shard_key']))
        c2, f2 = task['c'], task['f']
        W = np.frombuffer(R.get(task['model_key']), dtype=np.int64).reshape(c2, f2)
        correct, total, predsum = infer_core.evaluate(X, y, W)
        R.set(task['result_key'], struct.pack('<qqq', correct, total, predsum))
    else:
        R.set(task['result_key'], cp.dumps({'__error__': 'unknown op %s' % op}))
    elapsed_ms = (time.time() - t0) * 1000.0
    # Signal done ONLY after the result is committed, and tag it with the task idx:
    # the driver keys busy-time accounting by idx (each counted once) so a re-dispatched
    # task can't double-count, and it knows exactly which mappers finished. A task that
    # throws (or whose pod dies) emits NO done here — the driver's timeout + re-dispatch
    # recovers it, so the run can never hang on a lost in-flight task.
    R.lpush(task['done_key'], '%d:%.3f' % (task['idx'], elapsed_ms))


def main():
    print('[executor %s] up — BRPOP %s' % (NAME, TASK_Q), flush=True)
    while True:
        item = R.brpop(TASK_Q, timeout=5)
        if item is None:
            continue
        try:
            handle(json.loads(item[1]))
        except Exception:                                 # never kill the worker loop
            print('[executor %s] task error:' % NAME, flush=True)
            traceback.print_exc()


if __name__ == '__main__':
    main()
