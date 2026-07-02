#!/usr/bin/env python3
# driver_finra.py — Cloudburst inter-node FINRA driver (runs on node 0).
#
# The hybrid-shard audit fan, mirroring ../../../Inter-Node Application_Benchmark/{wasmem,
# faasm}/run_finra.py so the bars are comparable. FINRA is a MAP-only audit (no shuffle):
# the inter-stage state is just a violation COUNT per worker, but the input is read ~8×
# (each of the 8 rule-groups scans the trades), so it is input-read-heavy — the KVS upload
# + per-worker GET is exactly Cloudburst's cost the RDMA bar avoids.
#
#   - 5 STATELESS rules (0,1,5,6,7): each sharded into S disjoint trade slices → 5·S tasks,
#     each reads its header-prefixed shard, counts that rule (per-shard counts SUM exactly).
#   - 3 STATEFUL rules (2,3,4 = wash/spoofing/concentration): one FULL-data task each (their
#     cross-(account,symbol) state can't be reconstructed from a slice).
#   Effective fan = 3 + 5·S; a requested fanout F → S = round((F-3)/5) (F=60 → S=11 → 58).
#
# Each shard is the header PREPENDED to a newline-aligned 1/S slice, so parse_trades' header
# skip lands on the header (not a real trade) — disjoint + complete → counts sum to the gate.
#
# Metrics match the wasmem/faasm finra CSV schema:
#   makespan_ms   = end-to-end wall (incl. the trades + shard upload — Cloudburst's KVS cost).
#   total_job_ms  = split(node0) + Σ worker busy (GET + parse + audit) + aggregate(node0).
# gate = total violations (fan-out-invariant) = 2,271,415.
#
# Usage:
#   ./driver_finra.py --trades TestData/finra_5m.csv --fanout 60 --reps 15 \
#       --redis-host 10.10.1.2 --redis-port 30679 --expect 2271415 \
#       --csv "Tests/Inter-Node Application_Benchmark/cloudburst/results_finra.csv"
import argparse
import json
import os
import statistics
import sys
import time
import uuid

import redis

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import finra_rules
from mem_probe import MemSampler
import exec_pool

DONE_TIMEOUT = 300         # a stateful full-data task parses 5M trades (~30 s); 300 s headroom
MAX_REDISPATCH = 8


def split_shards(trades, s):
    """S newline-aligned shards, each = header + 1/s of the data rows (faasm-style)."""
    nl = trades.find(b'\n')
    header, data = trades[:nl + 1], trades[nl + 1:]
    total, approx, out, start = len(data), len(data) // max(1, s), [], 0
    for i in range(s):
        if i == s - 1:
            end = total
        else:
            end = start + approx
            while end < total and data[end] != 0x0A:
                end += 1
            if end < total:
                end += 1
        out.append(header + data[start:end])
        start = end
    return out


def run_wave(R, task_q, tasks, done_key, label):
    """Dispatch (idx, result_key, task_json) tasks; block until every result exists,
    re-dispatching any lost task (verified by result-key presence). Returns {idx: busy_ms}."""
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
        print('[cloudburst-finra]   re-dispatching %d lost task(s): %s'
              % (len(pending), sorted(pending)), flush=True)
        for i in sorted(pending):
            R.lpush(task_q, by_idx[i][1])
    return busy


def run_once(R, trades, n_shards, task_q, cold=False):
    uid = uuid.uuid4().hex
    done = 'cb:finra:' + uid

    t0 = time.time()
    # --- split + upload (node 0): full trades (for stateful) + S header-prefixed shards ---
    s0 = time.time()
    R.set('%s_trades' % uid, trades)
    for s, blob in enumerate(split_shards(trades, n_shards)):
        R.set('%s_shard_%d' % (uid, s), blob)
    split_ms = (time.time() - s0) * 1000.0

    # --- build the task list: idx → (op over an input_key) ---
    tasks, keymap = [], []
    idx = 0
    for rule in finra_rules.STATELESS:           # 5·S sharded tasks
        for s in range(n_shards):
            rk = '%s_count_%d_%d' % (uid, rule, s)
            tasks.append((idx, rk, json.dumps(
                {'op': 'finra_rule', 'idx': idx, 'rule': rule,
                 'input_key': '%s_shard_%d' % (uid, s), 'result_key': rk, 'done_key': done})))
            keymap.append(rk)
            idx += 1
    for rule in finra_rules.STATEFUL:            # 3 full-data tasks
        rk = '%s_count_%d_full' % (uid, rule)
        tasks.append((idx, rk, json.dumps(
            {'op': 'finra_rule', 'idx': idx, 'rule': rule,
             'input_key': '%s_trades' % uid, 'result_key': rk, 'done_key': done})))
        keymap.append(rk)
        idx += 1

    if cold:                                  # cold start: launch executors ON the audit wave
        exec_pool.launch_on_wave(len(tasks))
    busy = run_wave(R, task_q, tasks, done, 'finra')

    # --- aggregate (node 0): sum every worker's violation count ---
    a0 = time.time()
    total = sum(int(R.get(k) or b'0') for k in keymap)
    agg_ms = (time.time() - a0) * 1000.0

    t1 = time.time()
    cleanup = (['%s_trades' % uid] + ['%s_shard_%d' % (uid, s) for s in range(n_shards)] +
               keymap + [done])
    R.delete(*cleanup)
    makespan_ms = (t1 - t0) * 1000.0
    total_job_ms = split_ms + sum(busy.values()) + agg_ms
    return makespan_ms, total_job_ms, total, len(tasks)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--trades', default='TestData/finra_5m.csv')
    ap.add_argument('--fanout', type=int, default=60, help='audit-rule fan width (F → S=round((F-3)/5))')
    ap.add_argument('--warm-reps', type=int, default=12, help='warm reps (pool pre-launched)')
    ap.add_argument('--cold-reps', type=int, default=3, help='cold reps (executors launched on-wave)')
    ap.add_argument('--reps', type=int, default=None, help='ignored (kept for back-compat)')
    ap.add_argument('--redis-host', default=os.environ.get('REDIS_HOST', '10.10.1.2'))
    ap.add_argument('--redis-port', type=int, default=int(os.environ.get('REDIS_PORT', '30679')))
    ap.add_argument('--task-queue', default='cb:tasks')
    ap.add_argument('--expect', type=int, default=None)
    ap.add_argument('--csv')
    a = ap.parse_args()

    if not os.path.exists(a.trades):
        sys.exit('trades not found: %s' % a.trades)
    R = redis.Redis(host=a.redis_host, port=a.redis_port, socket_timeout=600)
    try:
        R.ping()
    except Exception as e:
        sys.exit('redis unreachable at %s:%d (%r)' % (a.redis_host, a.redis_port, e))

    with open(a.trades, 'rb') as f:
        trades = f.read()
    n_trades = max(0, trades.count(b'\n') - 1)
    S = max(1, round((a.fanout - len(finra_rules.STATEFUL)) / len(finra_rules.STATELESS)))
    n_workers = len(finra_rules.STATELESS) * S + len(finra_rules.STATEFUL)
    print('[cloudburst-finra] trades=%d fanout=%d shards/rule=%d workers=%d warm=%d cold=%d' %
          (n_trades, a.fanout, S, n_workers, a.warm_reps, a.cold_reps), flush=True)

    mk_warm, mk_cold, tj_ms = [], [], []
    viol = None
    success = True
    mem = MemSampler(a.redis_host, a.redis_port); mem.start()   # peak-memory-cost sampler

    def _rep(r, cold):
        nonlocal viol, success
        m, tj, v, w = run_once(R, trades, S, a.task_queue, cold=cold)
        tj_ms.append(tj)
        if viol is None:
            viol = v
        elif v != viol:
            print('[cloudburst-finra] GATE FAIL %s rep%d: violations %d != %d' %
                  ('cold' if cold else 'warm', r, v, viol)); success = False
        print('[cloudburst-finra] %s rep%d makespan=%.0f ms total_job=%.0f ms violations=%d workers=%d' %
              ('cold' if cold else 'warm', r, m, tj, v, w), flush=True)
        return m

    for r in range(a.cold_reps):
        exec_pool.teardown()
        mk_cold.append(_rep(r, True))
    exec_pool.launch_on_wave(n_workers)
    for r in range(a.warm_reps):
        mk_warm.append(_rep(r, False))
    exec_pool.teardown()

    mem.stop.set(); mem.join()
    mem_total_mb, mem_max_mb, redis_peak_mb = mem.result()

    if a.expect is not None and viol != a.expect:
        print('[cloudburst-finra] GATE FAIL: violations %d != expected %d' % (viol, a.expect))
        success = False

    mk_mean = statistics.mean(mk_warm) if mk_warm else 0.0
    mk_std = statistics.pstdev(mk_warm) if len(mk_warm) > 1 else 0.0
    cold_mean = statistics.mean(mk_cold) if mk_cold else 0.0
    cold_std = statistics.pstdev(mk_cold) if len(mk_cold) > 1 else 0.0
    tj_mean = statistics.mean(tj_ms)
    print('[cloudburst-finra] === trades=%d workers=%d warm_makespan=%.0f ± %.1f ms '
          'cold_makespan=%.0f ± %.1f ms total_job=%.0f ms violations=%d %s ===' %
          (n_trades, n_workers, mk_mean, mk_std, cold_mean, cold_std, tj_mean, viol,
           'OK' if success else 'GATE-FAIL'))

    if a.csv:
        os.makedirs(os.path.dirname(os.path.abspath(a.csv)), exist_ok=True)
        new = not os.path.exists(a.csv)
        with open(a.csv, 'a') as f:
            if new:
                f.write('trades,fanout,nodes_used,makespan_mean_ms,makespan_std_ms,'
                        'total_job_mean_ms,violations,expect,success,reps,'
                        'mem_max_mb,mem_total_mb,redis_peak_mb,'
                        'cold_makespan_mean_ms,cold_makespan_std_ms\n')
            f.write('%d,%d,8,%.0f,%.1f,%.0f,%d,%d,%s,%d,%.0f,%.0f,%.0f,%.0f,%.1f\n' %
                    (n_trades, n_workers, mk_mean, mk_std, tj_mean, viol,
                     a.expect if a.expect is not None else viol, success, a.warm_reps,
                     mem_max_mb, mem_total_mb, redis_peak_mb, cold_mean, cold_std))
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
