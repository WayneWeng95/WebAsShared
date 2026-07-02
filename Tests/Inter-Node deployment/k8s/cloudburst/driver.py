#!/usr/bin/env python3
# driver.py — Cloudburst inter-node WordCount driver (runs on node 0).
#
# Distributed analogue of the Cloudburst port's run(): per rep it
#   1. reads the corpus and shards it into `fanout` newline-aligned chunks,
#      SETting each into Redis (the SHARDED upload — every value < 512 MB, so a
#      4 GB corpus fits with no single oversized key);
#   2. enqueues one wc_mapper task per chunk onto the shared executor queue (the
#      pool of executor pods, spread across the 4 nodes, runs them in parallel);
#   3. waits for all N partials, reads them back, and reduces (wc_reduce).
#
# Two metrics, matching ../../../Inter-Node Application_Benchmark/{wasmem,faasm}/
# results_wordcount.csv so the bars drop straight into plot_inter_bars.py:
#   makespan_ms   = end-to-end wall time of the whole KVS-routed DAG (the bar base)
#   total_job_ms  = Σ busy node-seconds = split(node0) + Σ mapper busy + reduce(node0)
#                   (the full bar; analogue of wasmem's Σ per-node executor durations)
# occurrences = total word count (the fan-out-invariant gate; equals wasmem/faasm
# on the identical corpus).
#
# Usage:
#   ./driver.py --corpus TestData/corpus_4gb.txt --fanout 60 --reps 3 \
#       --redis-host 10.10.1.2 --redis-port 30679 \
#       --csv "Tests/Inter-Node Application_Benchmark/cloudburst/results_wordcount.csv"
import argparse
import os
import statistics
import sys
import time
import uuid

import redis
import cloudpickle as cp

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wc_ops
from mem_probe import MemSampler
import exec_pool

# Map-stage robustness knobs. A normal rep drains all N done signals in seconds, so
# DONE_TIMEOUT only ever fires when a task is genuinely lost (its pod was evicted /
# died after BRPOP but before writing a result — leaving no result and no done
# signal, which is exactly what once hung the run). On each no-progress timeout the
# driver re-dispatches the tasks whose result key is still absent; MAX_REDISPATCH
# rounds bound it so a deterministically-failing chunk can't loop forever.
DONE_TIMEOUT = 60
MAX_REDISPATCH = 8


def task_json(uid, i, done_key):
    return ('{"op":"wc_mapper","idx":%d,"chunk_key":"%s_chunk_%d",'
            '"result_key":"%s_res_%d","done_key":"%s"}') % (i, uid, i, uid, i, done_key)


def run_once(R, corpus_path, n, task_q, cold=False):
    uid = uuid.uuid4().hex
    done_key = 'cb:done:' + uid
    R.delete(done_key)

    t0 = time.time()
    # --- split stage (node 0 busy): read, shard, upload chunks, enqueue tasks ---
    s0 = time.time()
    with open(corpus_path, 'rb') as f:
        data = f.read()
    chunks = wc_ops.shard_bytes(data, n)
    # SET each chunk individually (NOT pipelined): pipelining all N would buffer the
    # whole corpus in Redis's query buffer and trip client-query-buffer-limit (1 GB).
    for i, ch in enumerate(chunks):
        R.set('%s_chunk_%d' % (uid, i), ch)
    if cold:                                  # cold start: launch executors ON the map wave
        exec_pool.launch_on_wave(n)           # (its latency is inside t0..t1 → cold makespan)
    for i in range(n):
        R.lpush(task_q, task_json(uid, i, done_key))
    split_ms = (time.time() - s0) * 1000.0

    # --- map stage (executor pods): collect done signals + busy times, re-dispatching
    # any task whose result never lands. Correctness is verified by RESULT PRESENCE,
    # not signal count: a done signal carries "idx:ms", busy[idx] keeps the first ms
    # seen (idempotent if a re-dispatched task also completes), and a task only leaves
    # `pending` once its result key actually exists. ---
    pending = set(range(n))                # tasks whose result is not yet present
    busy = {}                              # idx -> busy ms (first completion wins)
    rounds = 0
    while pending:
        item = R.blpop(done_key, timeout=DONE_TIMEOUT)
        if item is not None:
            idx_s, ms_s = item[1].decode().split(':')
            idx = int(idx_s)
            busy.setdefault(idx, float(ms_s))
            if idx in pending and R.exists('%s_res_%d' % (uid, idx)):
                pending.discard(idx)
                rounds = 0
            continue
        # no-progress timeout: reconcile by result presence, then re-dispatch the rest.
        pending = {i for i in pending if not R.exists('%s_res_%d' % (uid, i))}
        if not pending:
            break
        rounds += 1
        if rounds > MAX_REDISPATCH:
            raise TimeoutError('mappers stuck after %d re-dispatch rounds: %s'
                               % (rounds, sorted(pending)))
        print('[cloudburst-wc]   re-dispatching %d lost task(s): %s'
              % (len(pending), sorted(pending)), flush=True)
        for i in sorted(pending):
            R.lpush(task_q, task_json(uid, i, done_key))
    sum_map_ms = sum(busy.values())

    # --- reduce stage (node 0 busy): gather partials + merge ---
    r0 = time.time()
    partials = []
    for i in range(n):
        v = R.get('%s_res_%d' % (uid, i))
        partials.append(cp.loads(v) if v else {})
    total = wc_ops.wc_reduce(partials)
    reduce_ms = (time.time() - r0) * 1000.0

    t1 = time.time()
    keys = (['%s_chunk_%d' % (uid, i) for i in range(n)] +
            ['%s_res_%d' % (uid, i) for i in range(n)] + [done_key])
    R.delete(*keys)
    makespan_ms = (t1 - t0) * 1000.0
    total_job_ms = split_ms + sum_map_ms + reduce_ms
    return makespan_ms, total_job_ms, sum(total.values()), len(total)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--corpus', default='TestData/corpus_4gb.txt')
    ap.add_argument('--fanout', type=int, default=60)
    ap.add_argument('--redis-host', default=os.environ.get('REDIS_HOST', '10.10.1.2'))
    ap.add_argument('--redis-port', type=int, default=int(os.environ.get('REDIS_PORT', '30679')))
    ap.add_argument('--task-queue', default='cb:tasks')
    ap.add_argument('--expect', type=int, default=None, help='expected occurrences (gate)')
    ap.add_argument('--warm-reps', type=int, default=12, help='warm reps (pool pre-launched)')
    ap.add_argument('--cold-reps', type=int, default=3, help='cold reps (executors launched on-wave)')
    ap.add_argument('--reps', type=int, default=None, help='ignored (kept for back-compat)')
    ap.add_argument('--csv')
    a = ap.parse_args()

    if not os.path.exists(a.corpus):
        sys.exit('corpus not found: %s' % a.corpus)
    R = redis.Redis(host=a.redis_host, port=a.redis_port, socket_timeout=600)
    try:
        R.ping()
    except Exception as e:
        sys.exit('redis unreachable at %s:%d (%r)' % (a.redis_host, a.redis_port, e))

    size_mb = round(os.path.getsize(a.corpus) / (1024 * 1024))
    print('[cloudburst-wc] corpus=%s (%d MB) fanout=%d warm=%d cold=%d redis=%s:%d' %
          (a.corpus, size_mb, a.fanout, a.warm_reps, a.cold_reps, a.redis_host, a.redis_port), flush=True)

    mk_warm, mk_cold, tj_ms = [], [], []
    occ = uniq = None
    success = True
    mem = MemSampler(a.redis_host, a.redis_port); mem.start()   # peak-memory-cost sampler

    def _rep(r, cold):
        nonlocal occ, uniq, success
        m, tj, tot, u = run_once(R, a.corpus, a.fanout, a.task_queue, cold=cold)
        tj_ms.append(tj)
        if occ is None:
            occ, uniq = tot, u
        elif tot != occ:
            print('[cloudburst-wc] GATE FAIL %s rep%d: occurrences %d != %d' %
                  ('cold' if cold else 'warm', r, tot, occ)); success = False
        print('[cloudburst-wc] %s rep%d makespan=%.0f ms total_job=%.0f ms occ=%d unique=%d' %
              ('cold' if cold else 'warm', r, m, tj, tot, u), flush=True)
        return m

    # COLD reps: pool starts at 0, executors launched on the map wave (launch in makespan).
    for r in range(a.cold_reps):
        exec_pool.teardown()
        mk_cold.append(_rep(r, True))
    # WARM reps: bring the pool up once (not timed), then reuse it.
    exec_pool.launch_on_wave(a.fanout)
    for r in range(a.warm_reps):
        mk_warm.append(_rep(r, False))
    exec_pool.teardown()             # leave pool at 0 for the next workload's cold reps

    mem.stop.set(); mem.join()
    mem_total_mb, mem_max_mb, redis_peak_mb = mem.result()

    if a.expect is not None and occ != a.expect:
        print('[cloudburst-wc] GATE FAIL: occurrences %d != expected %d' % (occ, a.expect))
        success = False

    mk_mean = statistics.mean(mk_warm) if mk_warm else 0.0
    mk_std = statistics.pstdev(mk_warm) if len(mk_warm) > 1 else 0.0
    cold_mean = statistics.mean(mk_cold) if mk_cold else 0.0
    cold_std = statistics.pstdev(mk_cold) if len(mk_cold) > 1 else 0.0
    tj_mean = statistics.mean(tj_ms)
    print('[cloudburst-wc] === size=%d MB fanout=%d warm_makespan=%.0f ± %.1f ms '
          'cold_makespan=%.0f ± %.1f ms total_job=%.0f ms occ=%d %s ===' %
          (size_mb, a.fanout, mk_mean, mk_std, cold_mean, cold_std, tj_mean, occ,
           'OK' if success else 'GATE-FAIL'))

    if a.csv:
        os.makedirs(os.path.dirname(os.path.abspath(a.csv)), exist_ok=True)
        new = not os.path.exists(a.csv)
        with open(a.csv, 'a') as f:
            if new:
                f.write('size_mb,mappers,nodes_used,makespan_mean_ms,makespan_std_ms,'
                        'total_job_mean_ms,occurrences,expect,success,reps,'
                        'mem_max_mb,mem_total_mb,redis_peak_mb,'
                        'cold_makespan_mean_ms,cold_makespan_std_ms\n')
            f.write('%d,%d,8,%.0f,%.1f,%.0f,%d,%s,%s,%d,%.0f,%.0f,%.0f,%.0f,%.1f\n' %
                    (size_mb, a.fanout, mk_mean, mk_std, tj_mean, occ,
                     a.expect if a.expect is not None else occ, success, a.warm_reps,
                     mem_max_mb, mem_total_mb, redis_peak_mb, cold_mean, cold_std))
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
