#!/usr/bin/env python3
# driver.py — RMMap (ES/Redis) inter-node WordCount driver (runs on node 0).
#
# Option-A harness: the staged DAG is driven from node 0, but the *map* fan-out is a
# pool of Knative Service pods (`wc-mapper`) invoked over HTTP — the track-C analogue
# of the Cloudburst track-B executor pool. Per rep:
#   1. split (node 0 busy): read corpus, shard into `fanout` newline-aligned chunks,
#      SET each into Redis (sharded upload, every value < 512 MB);
#   2. map  (ksvc pods):    fan out `fanout` concurrent POSTs to the mapper Service;
#      each pod reads its chunk from Redis, counts, writes its partial back to Redis
#      (RMMap ES protocol), and returns its per-invocation busy time;
#   3. reduce (node 0 busy): GET the N partials, merge.
#
# Metrics match ../../cloudburst + ../../../Inter-Node Application_Benchmark/{wasmem,
# faasm}/results_wordcount.csv so the RMMap bar drops straight into plot_inter_bars.py:
#   makespan_ms  = end-to-end wall time of the whole KVS-routed DAG (the bar base)
#   total_job_ms = split(node0) + Σ mapper busy_ms + reduce(node0)  (the full bar)
#   occurrences  = total word count (fan-out-invariant gate; == the Cloudburst run's
#                  660,848,961 on the same corpus + tokenizer).
#
# The mapper Service is reached through the Kourier gateway (an external LB IP on the
# node subnet) with the ksvc's route as the Host header — node 0 is on the cluster
# subnet but outside the pod mesh, so it cannot use the in-cluster service DNS.
#
# Usage (see run.sh, which fills --gateway/--host from `kubectl get ksvc`):
#   ./driver.py --corpus TestData/corpus_4gb.txt --fanout 60 --reps 15 \
#       --gateway http://10.10.1.2 --host wc-mapper.baselines.svc.cluster.local \
#       --redis-host 10.10.1.2 --redis-port 30679 --variant warm \
#       --csv "Tests/Inter-Node Application_Benchmark/rmmap/results_wordcount.csv"
import argparse
import os
import pickle
import statistics
import subprocess
import sys
import time
import uuid
from concurrent.futures import ThreadPoolExecutor, as_completed

import redis
import requests

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wc_ops

NS = 'baselines'
KSVC = 'wc-mapper'


def invoke(session, gateway, host, payload):
    """One mapper invocation: POST through the Kourier gateway, routed by Host header."""
    r = session.post(gateway + '/', json=payload,
                     headers={'Host': host, 'Content-Type': 'application/json'},
                     timeout=600)
    r.raise_for_status()
    return r.json()


def n_pods():
    """Current number of (non-terminating) mapper pods, via the k8s API."""
    out = subprocess.run(
        ['kubectl', '-n', NS, 'get', 'pods', '-l',
         'serving.knative.dev/service=%s' % KSVC, '--no-headers'],
        capture_output=True, text=True).stdout
    return sum(1 for ln in out.splitlines() if ln.strip() and 'Terminating' not in ln)


def wait_zero(timeout=180):
    """Block until the mapper pool has scaled to zero (strict cold-start setup)."""
    t0 = time.time()
    while time.time() - t0 < timeout:
        if n_pods() == 0:
            return True
        time.sleep(3)
    return False


def run_once(R, corpus_path, n, gateway, host):
    uid = uuid.uuid4().hex
    t0 = time.time()
    # --- split stage (node 0 busy): read, shard, upload chunks ---
    s0 = time.time()
    with open(corpus_path, 'rb') as f:
        data = f.read()
    chunks = wc_ops.shard_bytes(data, n)
    for i, ch in enumerate(chunks):             # SET each individually (see cloudburst README)
        R.set('%s_chunk_%d' % (uid, i), ch)
    split_ms = (time.time() - s0) * 1000.0

    # --- map stage (ksvc pods): concurrent HTTP fan-out, collect per-pod busy time ---
    payloads = [{'chunk_key': '%s_chunk_%d' % (uid, i),
                 'result_key': '%s_res_%d' % (uid, i)} for i in range(n)]
    sum_map_ms = 0.0
    with ThreadPoolExecutor(max_workers=n) as pool:
        sess = requests.Session()
        futs = [pool.submit(invoke, sess, gateway, host, p) for p in payloads]
        for fut in as_completed(futs):
            sum_map_ms += fut.result()['busy_ms']

    # --- reduce stage (node 0 busy): gather partials + merge ---
    r0 = time.time()
    partials = []
    for i in range(n):
        v = R.get('%s_res_%d' % (uid, i))
        partials.append(pickle.loads(v) if v else {})
    total = wc_ops.wc_reduce(partials)
    reduce_ms = (time.time() - r0) * 1000.0

    t1 = time.time()
    R.delete(*(['%s_chunk_%d' % (uid, i) for i in range(n)] +
               ['%s_res_%d' % (uid, i) for i in range(n)]))
    return ((t1 - t0) * 1000.0, split_ms + sum_map_ms + reduce_ms,
            sum(total.values()), len(total))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--corpus', default='TestData/corpus_4gb.txt')
    ap.add_argument('--fanout', type=int, default=60)
    ap.add_argument('--reps', type=int, default=15)
    ap.add_argument('--gateway', required=True, help='Kourier gateway, e.g. http://10.10.1.2')
    ap.add_argument('--host', required=True, help='ksvc route host (Host header)')
    ap.add_argument('--redis-host', default=os.environ.get('REDIS_HOST', '10.10.1.2'))
    ap.add_argument('--redis-port', type=int, default=int(os.environ.get('REDIS_PORT', '30679')))
    ap.add_argument('--variant', default='warm', choices=['warm', 'cold'])
    ap.add_argument('--ensure-zero', action='store_true',
                    help='wait for the pool to scale to 0 before each rep (cold-start)')
    ap.add_argument('--expect', type=int, default=None, help='expected occurrences (gate)')
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
    print('[rmmap-wc] corpus=%s (%d MB) fanout=%d reps=%d variant=%s gateway=%s host=%s' %
          (a.corpus, size_mb, a.fanout, a.reps, a.variant, a.gateway, a.host), flush=True)

    mk_ms, tj_ms = [], []
    occ = uniq = None
    success = True
    for r in range(a.reps):
        if a.ensure_zero:
            if not wait_zero():
                print('[rmmap-wc] WARN rep%d: pool did not reach 0 before timeout' % r, flush=True)
            else:
                print('[rmmap-wc] rep%d: pool at 0 — cold start' % r, flush=True)
        m, tj, tot, u = run_once(R, a.corpus, a.fanout, a.gateway, a.host)
        mk_ms.append(m)
        tj_ms.append(tj)
        if occ is None:
            occ, uniq = tot, u
        elif tot != occ:
            print('[rmmap-wc] GATE FAIL rep%d: occurrences %d != %d' % (r, tot, occ))
            success = False
        print('[rmmap-wc] rep%d makespan=%.0f ms total_job=%.0f ms occ=%d unique=%d' %
              (r, m, tj, tot, u), flush=True)

    if a.expect is not None and occ != a.expect:
        print('[rmmap-wc] GATE FAIL: occurrences %d != expected %d' % (occ, a.expect))
        success = False

    mk_mean = statistics.mean(mk_ms)
    mk_std = statistics.pstdev(mk_ms) if len(mk_ms) > 1 else 0.0
    tj_mean = statistics.mean(tj_ms)
    print('[rmmap-wc] === size=%d MB fanout=%d variant=%s makespan=%.0f ± %.1f ms '
          'total_job=%.0f ms occ=%d %s ===' %
          (size_mb, a.fanout, a.variant, mk_mean, mk_std, tj_mean, occ,
           'OK' if success else 'GATE-FAIL'))

    if a.csv:
        os.makedirs(os.path.dirname(os.path.abspath(a.csv)), exist_ok=True)
        new = not os.path.exists(a.csv)
        with open(a.csv, 'a') as f:
            if new:
                f.write('size_mb,mappers,nodes_used,variant,makespan_mean_ms,makespan_std_ms,'
                        'total_job_mean_ms,occurrences,expect,success,reps\n')
            f.write('%d,%d,4,%s,%.0f,%.1f,%.0f,%d,%s,%s,%d\n' %
                    (size_mb, a.fanout, a.variant, mk_mean, mk_std, tj_mean, occ,
                     a.expect if a.expect is not None else occ, success, a.reps))
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
