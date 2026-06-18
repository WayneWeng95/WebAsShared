#!/usr/bin/env python3
# driver.py — RMMap (dmerge) TeraSort, ES protocol, single-box runner.
#
# RMMap ships no sort workload, but TeraSort's map->shuffle->merge plumbing is the
# same shape as RMMap's WordCount ES app (splitter -> mapper x N -> reducer over
# Redis with pickle ser/deser). RMMap's real Knative+RDMA deployment isn't stood
# up here and DMERGE needs the MITOSIS kernel module (parked), so this driver
# replicates RMMap's ES data path directly: the same `redis_put`/`redis_get`
# (Redis SET/GET) + `pickle` round-trips, with RMMap's profile buckets
#   execute_time  — the map/partition/sort compute,
#   es_time       — Redis round-trip (external store),
#   sd_time       — pickle serialize+deserialize,
#   sd_bytes_len  — bytes pushed through Redis.
# sd_time IS the serialization cost our zero-copy page-chain shuffle reports as 0;
# es_time is the external-store round-trip it avoids. For TeraSort the WHOLE
# dataset crosses the shuffle, so both scale with the full input (vs WordCount's
# smaller partial maps) — the maximal-data-movement contrast.
#
# The DAG (all-to-all shuffle through Redis):
#   splitter  : cut records into N contiguous chunks → chunk_<i> in Redis.
#   mapper_i  : read chunk_<i>, RANGE-PARTITION by key-prefix into N owner blobs,
#               pickle+put each as bucket_<i>_<j>.
#   reducer_j : read its column {bucket_<i>_<j> : i}, unpickle, sort its range.
#
# Env: MAPPER_NUM=N, TS_RECORDS=<path>, REDIS_HOST/PORT/PASSWORD.
# Prints one CSV row (and --header) aligned with the other systems.
import os
import pickle
import resource
import sys
import time

import redis

KEY_LEN = 10
KEY_LO = 33          # must match gen_records.py / the guest
KEY_SPAN = 64

REDIS = redis.Redis(
    host=os.environ.get('REDIS_HOST', '127.0.0.1'),
    port=int(os.environ.get('REDIS_PORT', '6379')),
    password=os.environ.get('REDIS_PASSWORD') or None,
    # Separate logical DB so a concurrently-running baseline's flushdb (on db 0)
    # cannot wipe our keys mid-shuffle. flushdb() below only clears THIS db.
    db=int(os.environ.get('REDIS_DB', '2')),
)

# RMMap-style profile buckets (mirrors util.reduce_profile / functions profiling).
PROF = {'execute_time': 0.0, 'es_time': 0.0, 'sd_time': 0.0, 'sd_bytes_len': 0}


def _owner(first_byte, n):
    b = min(max(first_byte, KEY_LO), KEY_LO + KEY_SPAN - 1) - KEY_LO
    o = b * n // KEY_SPAN
    return n - 1 if o >= n else o


def es_put(key, obj):
    """pickle (sd) + Redis SET (es) — RMMap's ES external-store write."""
    t = time.time()
    blob = pickle.dumps(obj)
    PROF['sd_time'] += time.time() - t
    PROF['sd_bytes_len'] += len(blob)
    t = time.time()
    REDIS.set(key, blob)
    PROF['es_time'] += time.time() - t


def es_get(key):
    """Redis GET (es) + unpickle (sd) — RMMap's ES external-store read."""
    t = time.time()
    blob = REDIS.get(key)
    PROF['es_time'] += time.time() - t
    if blob is None:
        return None
    t = time.time()
    obj = pickle.loads(blob)
    PROF['sd_time'] += time.time() - t
    PROF['sd_bytes_len'] += len(blob)
    return obj


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    N = int(args[0]) if args else int(os.environ.get('MAPPER_NUM', '4'))
    records_path = os.environ.get('TS_RECORDS', '/data/terasort.dat')
    size_mb = round(os.path.getsize(records_path) / (1024 * 1024))

    REDIS.flushdb()
    t0 = time.time()

    # ── splitter: cut records into N contiguous chunks → Redis ──────────────────
    te = time.time()
    with open(records_path, 'r', encoding='latin-1') as f:
        lines = [ln for ln in f.read().split('\n') if ln]
    per = (len(lines) + N - 1) // N
    PROF['execute_time'] += time.time() - te
    for i in range(N):
        es_put('chunk_%d' % i, lines[i * per:(i + 1) * per])

    # ── mappers (fan-out): range-partition each chunk into N owner buckets ──────
    for i in range(N):
        chunk = es_get('chunk_%d' % i) or []
        te = time.time()
        buckets = [[] for _ in range(N)]
        for ln in chunk:
            buckets[_owner(ord(ln[0]), N)].append(ln)
        PROF['execute_time'] += time.time() - te
        for j in range(N):
            es_put('bucket_%d_%d' % (i, j), buckets[j])

    # ── reducers (fan-in per owner): gather column, sort the range ─────────────
    records = keysum = 0
    allsorted = True
    ordered = True
    prev_last = None
    for j in range(N):
        recs = []
        for i in range(N):
            b = es_get('bucket_%d_%d' % (i, j))
            if b:
                recs.extend(b)
        te = time.time()
        recs.sort(key=lambda r: r[:KEY_LEN])
        PROF['execute_time'] += time.time() - te
        records += len(recs)
        keysum += sum(ord(c) for r in recs for c in r[:KEY_LEN])
        if any(recs[k][:KEY_LEN] > recs[k + 1][:KEY_LEN]
               for k in range(len(recs) - 1)):
            allsorted = False
        if recs:
            if prev_last is not None and recs[0][:KEY_LEN] < prev_last:
                ordered = False
            prev_last = recs[-1][:KEY_LEN]

    e2e_ms = (time.time() - t0) * 1000.0
    if not allsorted:
        sys.stderr.write('WARN N=%d: a range is NOT sorted\n' % N)
    if not ordered:
        sys.stderr.write('WARN N=%d: ranges NOT globally ordered\n' % N)

    checksum = '%d:%d' % (records, keysum)
    kvs_mb = PROF['sd_bytes_len'] / (1024 * 1024)
    peak_mb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024.0
    e2e_s = e2e_ms / 1000.0
    mbps = size_mb / e2e_s if e2e_s else 0
    rps = records / e2e_s if e2e_s else 0

    header = ('size_mb,workers,topo,e2e_ms,throughput_mb_s,records_per_s,'
              'peak_mem_mb,exec_ms,sd_ms,es_ms,kvs_ser_mb,checksum')
    row = ('%d,%d,redis-es,%.1f,%.2f,%.0f,%.1f,%.1f,%.1f,%.1f,%.2f,%s' %
           (size_mb, N, e2e_ms, mbps, rps, peak_mb,
            PROF['execute_time'] * 1000, PROF['sd_time'] * 1000,
            PROF['es_time'] * 1000, kvs_mb, checksum))
    if want_header:
        print(header)
    print(row)


if __name__ == '__main__':
    main()
