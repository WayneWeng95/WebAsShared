#!/usr/bin/env python3
# driver_terasort.py — Cloudburst inter-node TeraSort driver (runs on node 0).
#
# Distributed analogue of the Cloudburst port's terasort run(): an all-to-all shuffle
# routed entirely through Redis (the KVS-serialized transfer the comparison is about),
# run as two waves over the same executor pool that serves WordCount:
#
#   1. split (node 0)   : read records, cut into N newline-aligned chunks → SET each
#                         <uid>_chunk_<i>  (the same sharded upload as WordCount).
#   2. partition wave   : N ts_partition tasks — each executor reads its chunk, range-
#                         partitions every record into N owner buckets, SETs each
#                         <uid>_bucket_<i>_<j>  (the shuffle SCATTER: ~1× input put).
#   3. merge wave        : N ts_merge tasks — each executor gathers its owner column
#                         {<uid>_bucket_<i>_<j> : i}, SORTS it, SETs a summary
#                         <uid>_summary_<j>   (the shuffle GATHER: ~1× input got).
#   4. collect (node 0)  : sum records + keysum, check every range sorted & globally
#                         ordered → the fan-out-invariant gate.
#
# Metrics match ../../../Inter-Node Application_Benchmark/{wasmem,faasm}/results_terasort.csv:
#   makespan_ms   = end-to-end wall of split → partition → merge → collect (the bar base).
#   total_job_ms  = Σ busy node-seconds = split(node0) + Σ partition busy + Σ merge busy
#                   + collect(node0)  (analogue of wasmem's Σ per-node executor durations).
# gate = (records, keysum, sorted), fan-out-invariant, self-consistent across reps.
#
# Usage:
#   ./driver_terasort.py --records TestData/terasort_1.2gb.txt --fanout 4 --reps 15 \
#       --redis-host 10.10.1.2 --redis-port 30679 --expect 12884902,8310813852,1 \
#       --csv "Tests/Inter-Node Application_Benchmark/cloudburst/results_terasort.csv"
import argparse
import json
import os
import statistics
import sys
import time
import uuid

import redis
import cloudpickle as cp

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ts_ops
from mem_probe import MemSampler
import exec_pool

# Same map-stage robustness knobs as driver.py, but DONE_TIMEOUT is much larger here:
# TeraSort runs at fanout=4 (vs WordCount's 60), so each task moves ~1/4 of the dataset
# (a ~300 MB chunk GET + ~300 MB of bucket SETs) through the single Redis under contention
# — legitimately tens of seconds. A 60 s timeout would wrongly re-dispatch a live task and
# cascade. DONE_TIMEOUT only fires when a task is genuinely lost (pod evicted/died after
# BRPOP, before writing its result); the driver then reconciles by RESULT-KEY PRESENCE and
# re-dispatches the rest, with MAX_REDISPATCH bounding the loop.
DONE_TIMEOUT = 300
MAX_REDISPATCH = 8


def run_wave(R, task_q, tasks, done_key, label):
    """Dispatch `tasks` (list of (idx, result_key, task_json)) and block until every
    result key exists, re-dispatching any task whose result never lands. Returns
    {idx: busy_ms} (first completion wins, so re-dispatch can't double-count)."""
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
            raise TimeoutError('%s stuck after %d re-dispatch rounds: %s'
                               % (label, rounds, sorted(pending)))
        print('[cloudburst-ts]   re-dispatching %d lost %s task(s): %s'
              % (len(pending), label, sorted(pending)), flush=True)
        for i in sorted(pending):
            R.lpush(task_q, by_idx[i][1])
    return busy


def run_once(R, records_path, n, task_q, cold=False):
    uid = uuid.uuid4().hex
    pfx = '%s_bucket' % uid
    done_p, done_m = 'cb:tsdone:p:' + uid, 'cb:tsdone:m:' + uid

    t0 = time.time()
    # --- split stage (node 0): read records, shard, upload chunks ---
    s0 = time.time()
    with open(records_path, 'rb') as f:
        data = f.read()
    for i, ch in enumerate(ts_ops.split_aligned(data, n)):
        R.set('%s_chunk_%d' % (uid, i), ch)
    split_ms = (time.time() - s0) * 1000.0

    # --- partition wave (scatter) ---
    ptasks = [(i, '%s_pdone_%d' % (uid, i), json.dumps(
        {'op': 'ts_partition', 'idx': i, 'n': n, 'chunk_key': '%s_chunk_%d' % (uid, i),
         'bucket_prefix': pfx, 'result_key': '%s_pdone_%d' % (uid, i),
         'done_key': done_p})) for i in range(n)]
    if cold:                                  # cold start: launch executors ON the first (partition) wave
        exec_pool.launch_on_wave(n)
    pbusy = run_wave(R, task_q, ptasks, done_p, 'partition')

    # --- merge wave (gather + sort) ---
    mtasks = [(j, '%s_summary_%d' % (uid, j), json.dumps(
        {'op': 'ts_merge', 'idx': j, 'n': n, 'bucket_prefix': pfx,
         'result_key': '%s_summary_%d' % (uid, j),
         'done_key': done_m})) for j in range(n)]
    mbusy = run_wave(R, task_q, mtasks, done_m, 'merge')

    # --- collect stage (node 0): aggregate summaries → gate ---
    c0 = time.time()
    summaries = {}
    for j in range(n):
        v = R.get('%s_summary_%d' % (uid, j))
        summaries[j] = cp.loads(v) if v else None
    records, keysum, srt = ts_ops.ts_collect(summaries)
    collect_ms = (time.time() - c0) * 1000.0

    t1 = time.time()
    keys = (['%s_chunk_%d' % (uid, i) for i in range(n)] +
            ['%s_pdone_%d' % (uid, i) for i in range(n)] +
            ['%s_summary_%d' % (uid, j) for j in range(n)] +
            ['%s_%d_%d' % (pfx, i, j) for i in range(n) for j in range(n)] +
            [done_p, done_m])
    R.delete(*keys)
    makespan_ms = (t1 - t0) * 1000.0
    total_job_ms = split_ms + sum(pbusy.values()) + sum(mbusy.values()) + collect_ms
    return makespan_ms, total_job_ms, records, keysum, 1 if srt else 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--records', default='TestData/terasort_1.2gb.txt')
    ap.add_argument('--fanout', type=int, default=4, help='shuffle width (N partitioners = N mergers)')
    ap.add_argument('--warm-reps', type=int, default=12, help='warm reps (pool pre-launched)')
    ap.add_argument('--cold-reps', type=int, default=3, help='cold reps (executors launched on-wave)')
    ap.add_argument('--reps', type=int, default=None, help='ignored (kept for back-compat)')
    ap.add_argument('--redis-host', default=os.environ.get('REDIS_HOST', '10.10.1.2'))
    ap.add_argument('--redis-port', type=int, default=int(os.environ.get('REDIS_PORT', '30679')))
    ap.add_argument('--task-queue', default='cb:tasks')
    ap.add_argument('--expect', default=None, help="gate 'records,keysum,sorted'")
    ap.add_argument('--csv')
    a = ap.parse_args()

    if not os.path.exists(a.records):
        sys.exit('records not found: %s' % a.records)
    expect = None
    if a.expect is not None:
        expect = tuple(int(x) for x in a.expect.split(','))
        if len(expect) != 3:
            sys.exit("--expect must be 'records,keysum,sorted'")
    R = redis.Redis(host=a.redis_host, port=a.redis_port, socket_timeout=600)
    try:
        R.ping()
    except Exception as e:
        sys.exit('redis unreachable at %s:%d (%r)' % (a.redis_host, a.redis_port, e))

    size_mb = round(os.path.getsize(a.records) / (1024 * 1024))
    print('[cloudburst-ts] records=%s (%d MB) fanout=%d warm=%d cold=%d redis=%s:%d' %
          (a.records, size_mb, a.fanout, a.warm_reps, a.cold_reps, a.redis_host, a.redis_port), flush=True)

    mk_warm, mk_cold, tj_ms = [], [], []
    gate_seen = None
    success = True
    mem = MemSampler(a.redis_host, a.redis_port); mem.start()   # peak-memory-cost sampler

    def _rep(r, cold):
        nonlocal gate_seen, success
        m, tj, rec, ks, srt = run_once(R, a.records, a.fanout, a.task_queue, cold=cold)
        tj_ms.append(tj)
        g = (rec, ks, srt)
        if gate_seen is None:
            gate_seen = g
        elif g != gate_seen:
            print('[cloudburst-ts] GATE FAIL %s rep%d: %s != %s' %
                  ('cold' if cold else 'warm', r, g, gate_seen)); success = False
        if srt != 1:
            print('[cloudburst-ts] SORT FAIL %s rep%d' % ('cold' if cold else 'warm', r)); success = False
        print('[cloudburst-ts] %s rep%d makespan=%.0f ms total_job=%.0f ms records=%d keysum=%d sorted=%d' %
              ('cold' if cold else 'warm', r, m, tj, rec, ks, srt), flush=True)
        return m

    for r in range(a.cold_reps):
        exec_pool.teardown()
        mk_cold.append(_rep(r, True))
    exec_pool.launch_on_wave(a.fanout)
    for r in range(a.warm_reps):
        mk_warm.append(_rep(r, False))
    exec_pool.teardown()

    mem.stop.set(); mem.join()
    mem_total_mb, mem_max_mb, redis_peak_mb = mem.result()

    if expect is not None and gate_seen != expect:
        print('[cloudburst-ts] GATE FAIL: %s != expected %s' % (gate_seen, expect))
        success = False

    mk_mean = statistics.mean(mk_warm) if mk_warm else 0.0
    mk_std = statistics.pstdev(mk_warm) if len(mk_warm) > 1 else 0.0
    cold_mean = statistics.mean(mk_cold) if mk_cold else 0.0
    cold_std = statistics.pstdev(mk_cold) if len(mk_cold) > 1 else 0.0
    tj_mean = statistics.mean(tj_ms)
    rec, ks, srt = gate_seen
    print('[cloudburst-ts] === size=%d MB fanout=%d warm_makespan=%.0f ± %.1f ms '
          'cold_makespan=%.0f ± %.1f ms total_job=%.0f ms records=%d keysum=%d sorted=%d %s ===' %
          (size_mb, a.fanout, mk_mean, mk_std, cold_mean, cold_std, tj_mean, rec, ks, srt,
           'OK' if success else 'GATE-FAIL'))

    if a.csv:
        os.makedirs(os.path.dirname(os.path.abspath(a.csv)), exist_ok=True)
        new = not os.path.exists(a.csv)
        exp_str = '|'.join(str(x) for x in (expect if expect is not None else gate_seen))
        with open(a.csv, 'a') as f:
            if new:
                f.write('size_mb,workers,nodes_used,makespan_mean_ms,makespan_std_ms,'
                        'total_job_mean_ms,records,keysum,sorted,expect,success,reps,'
                        'mem_max_mb,mem_total_mb,redis_peak_mb,'
                        'cold_makespan_mean_ms,cold_makespan_std_ms\n')
            f.write('%d,%d,8,%.0f,%.1f,%.0f,%d,%d,%d,%s,%s,%d,%.0f,%.0f,%.0f,%.0f,%.1f\n' %
                    (size_mb, a.fanout, mk_mean, mk_std, tj_mean, rec, ks, srt,
                     exp_str, success, a.warm_reps,
                     mem_max_mb, mem_total_mb, redis_peak_mb, cold_mean, cold_std))
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
