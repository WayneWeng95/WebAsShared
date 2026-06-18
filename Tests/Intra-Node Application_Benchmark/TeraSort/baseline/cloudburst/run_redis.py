#!/usr/bin/env python3
# run_redis.py — Cloudburst TeraSort baseline on Redis (single dev box).
#
# Drives the REAL ported terasort.py (split -> partition x N -> merge x N ->
# collect) through the Redis-backed Cloudburst runner installed in the
# compare_system tree (cloudburst/server/benchmarks/redis_runner.py +
# cloudburst/shared/redis_kvs.py). Every inter-stage value — the record chunks
# and the N x N shuffle buckets — is cloudpickled and round-tripped through
# Redis, so the WHOLE dataset is serialized through the KVS (~1x in, ~1x out):
# the maximal-data-movement transfer that WebAsShared's zero-copy page-chain
# shuffle avoids.
#
# Runs ONE worker count N (a fresh process per N keeps peak RSS clean — see
# run.sh, which loops N and concatenates rows). Prints one CSV row to stdout:
#
#   size_mb,workers,topo,e2e_ms,throughput_mb_s,records_per_s,
#   peak_mem_mb,reps,kvs_put_mb,kvs_get_mb,checksum
#
# CAVEAT (parallelism): this runner executes the DAG in a single process, so the
# N workers run sequentially, NOT on concurrent executors like real Cloudburst.
# The SERIALIZED-TRANSFER metrics (kvs_put_mb/kvs_get_mb, and the e2e latency
# that includes cloudpickle ser/deser through Redis) are faithful; the
# throughput-vs-N speedup slope is not — compare per-N absolute numbers and the
# KVS byte volume, not the parallel scaling slope.
#
# Usage:
#   redis-server must be running on 127.0.0.1:6379.
#   ./run_redis.py RECORDS_PATH N [NUM_REQUESTS] [--header]
import importlib
import os
import resource
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
# .../TeraSort/baseline/cloudburst -> repo roots
WAS_ROOT = os.path.abspath(os.path.join(HERE, '..', '..', '..', '..', '..'))
CB_ROOT = os.path.abspath(os.path.join(
    WAS_ROOT, '..', 'compare_system', 'cloudburst'))
sys.path.insert(0, CB_ROOT)

from cloudburst.server.benchmarks.redis_runner import RedisCloudburst  # noqa
from cloudburst.shared.redis_kvs import RedisKvsClient                 # noqa

HEADER = ('size_mb,workers,topo,e2e_ms,throughput_mb_s,records_per_s,'
          'peak_mem_mb,reps,kvs_put_mb,kvs_get_mb,checksum')

REC_LEN = 100  # bytes per record incl. newline


def median(xs):
    xs = sorted(xs)
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    records = args[0] if len(args) > 0 else \
        os.path.join(WAS_ROOT, 'TestData', 'terasort_50mb.dat')
    N = args[1] if len(args) > 1 else '4'
    num_requests = int(args[2]) if len(args) > 2 else 3

    size_mb = round(os.path.getsize(records) / (1024 * 1024))

    os.environ['TS_NUM_WORKERS'] = N
    os.environ['TS_RECORDS'] = records
    import cloudburst.server.benchmarks.terasort as ts
    importlib.reload(ts)

    kvs = RedisKvsClient()
    kvs.flushdb()
    kvs.reset_stats()
    client = RedisCloudburst(kvs=kvs)

    _real_stdout = sys.stdout
    sys.stdout = sys.stderr
    try:
        total_time, _, _, _ = ts.run(client, num_requests, None)
    finally:
        sys.stdout = _real_stdout

    res = client.last_result or {}
    n_records = res.get('records', 0)
    keysum = res.get('keysum', 0)
    checksum = '%d:%d' % (n_records, keysum)
    if not res.get('sorted', True):
        sys.stderr.write('WARN N=%s: a range is NOT sorted\n' % N)
    if not res.get('ordered', True):
        sys.stderr.write('WARN N=%s: ranges NOT globally ordered\n' % N)

    e2e_ms = [t * 1000 for t in total_time]
    med_ms = median(e2e_ms)
    stats = kvs.transfer_stats()
    put_mb = stats['bytes_put'] / (1024 * 1024)
    get_mb = stats['bytes_get'] / (1024 * 1024)
    peak_mb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024.0
    mbps = size_mb / (med_ms / 1000.0)
    rps = n_records / (med_ms / 1000.0)

    if want_header:
        print(HEADER)
    print('%d,%s,redis-kvs,%.1f,%.2f,%.0f,%.1f,%d,%.2f,%.2f,%s' %
          (size_mb, N, med_ms, mbps, rps, peak_mb, num_requests,
           put_mb, get_mb, checksum))


if __name__ == '__main__':
    main()
