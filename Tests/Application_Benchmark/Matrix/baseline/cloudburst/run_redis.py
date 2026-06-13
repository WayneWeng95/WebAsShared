#!/usr/bin/env python3
# run_redis.py — Cloudburst SUMMA matrix-multiply baseline on Redis (single box).
#
# Drives the local summa.py (mat_tile -> mat_block × r·c -> mat_reduce) through
# the Redis-backed Cloudburst runner (cloudburst/server/benchmarks/redis_runner.py
# + cloudburst/shared/redis_kvs.py). Every inter-stage value (A/B panels, C
# blocks) is cloudpickled and round-tripped through Redis — the serialized state
# transfer WebAsShared's zero-copy page-chain avoids. Mirrors ../../../WordCount/
# baseline/cloudburst/run_redis.py.
#
# One mapper grid W per process (fresh process per W → clean peak RSS); run.sh
# loops W and concatenates rows. Prints one CSV row:
#   size_n,workers,topo,e2e_ms_median,gflops,peak_mem_mb,reps,kvs_put_mb,kvs_get_mb,checksum
#
# Usage (redis on 127.0.0.1:6379):  ./run_redis.py N W [NUM_REQUESTS] [--header]
import importlib
import os
import resource
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
# WebAsShared/Tests/Application_Benchmark/Matrix/baseline/cloudburst -> repo roots
WAS_ROOT = os.path.abspath(os.path.join(HERE, '..', '..', '..', '..', '..'))
CB_ROOT = os.path.abspath(os.path.join(
    WAS_ROOT, '..', 'compare_system', 'cloudburst'))
sys.path.insert(0, CB_ROOT)
sys.path.insert(0, HERE)   # local summa.py

from cloudburst.server.benchmarks.redis_runner import RedisCloudburst  # noqa
from cloudburst.shared.redis_kvs import RedisKvsClient                 # noqa

HEADER = ('size_n,workers,topo,e2e_ms_median,gflops,peak_mem_mb,reps,'
          'kvs_put_mb,kvs_get_mb,checksum')


def median(xs):
    xs = sorted(xs)
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    N = int(args[0])
    W = int(args[1]) if len(args) > 1 else 4
    num_requests = int(args[2]) if len(args) > 2 else 3

    A = os.path.join(WAS_ROOT, 'TestData', 'matrix', 'A_%d.bin' % N)
    B = os.path.join(WAS_ROOT, 'TestData', 'matrix', 'B_%d.bin' % N)
    os.environ['MAT_N'] = str(N)
    os.environ['MAT_WORKERS'] = str(W)
    os.environ['MAT_A'] = A
    os.environ['MAT_B'] = B

    import summa
    importlib.reload(summa)   # pick up MAT_N / MAT_WORKERS

    kvs = RedisKvsClient()
    kvs.flushdb()
    kvs.reset_stats()
    client = RedisCloudburst(kvs=kvs)

    _real_stdout = sys.stdout
    sys.stdout = sys.stderr   # keep stdout clean (CSV only)
    try:
        total_time, _, _, _ = summa.run(client, num_requests, None)
    finally:
        sys.stdout = _real_stdout

    checksum = int(client.last_result or 0)
    e2e_ms = [t * 1000 for t in total_time]
    med_ms = median(e2e_ms)
    stats = kvs.transfer_stats()
    put_mb = stats['bytes_put'] / (1024 * 1024)
    get_mb = stats['bytes_get'] / (1024 * 1024)
    peak_mb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024.0
    gflops = (2.0 * N * N * N / (med_ms / 1000.0) / 1e9) if med_ms else 0

    if want_header:
        print(HEADER)
    print('%d,%d,redis-kvs,%.1f,%.2f,%.1f,%d,%.2f,%.2f,%d' %
          (N, W, med_ms, gflops, peak_mb, num_requests, put_mb, get_mb, checksum))


if __name__ == '__main__':
    main()
