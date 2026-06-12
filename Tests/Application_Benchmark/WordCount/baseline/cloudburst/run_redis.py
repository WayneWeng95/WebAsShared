#!/usr/bin/env python3
# run_redis.py — Cloudburst WordCount baseline on Redis (single dev box).
#
# Drives the REAL ported wordcount.py (split -> mapper x N -> reducer) through
# the Redis-backed Cloudburst runner installed in the compare_system tree
# (cloudburst/server/benchmarks/redis_runner.py + cloudburst/shared/redis_kvs.py).
# Every inter-stage value (corpus chunks, partial {word:count} dicts) is
# cloudpickled and round-tripped through Redis — the serialized state transfer
# that WebAsShared's zero-copy page-chain avoids.
#
# Runs ONE mapper count N (a fresh process per N keeps peak RSS clean — see
# run.sh, which loops N and concatenates rows). Prints one CSV row to stdout:
#
#   size_mb,workers,topo,e2e_ms_median,throughput_mb_s,words_per_s,
#   peak_mem_mb,reps,kvs_put_mb,kvs_get_mb,total_occurrences
#
# CAVEAT (parallelism): this runner executes the DAG in a single process, so the
# N mappers run sequentially, NOT on concurrent executors like real Cloudburst.
# The SERIALIZED-TRANSFER metrics (kvs_put_mb/kvs_get_mb, and the e2e latency
# that includes cloudpickle ser/deser through Redis) are faithful; the
# throughput-vs-N *speedup slope* is not — treat per-N absolute numbers and the
# KVS byte volume as the comparison, not the parallel scaling.
#
# Usage:
#   redis-server must be running on 127.0.0.1:6379.
#   ./run_redis.py CORPUS_PATH N [NUM_REQUESTS] [--header]
import importlib
import os
import resource
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
# WebAsShared/Tests/Application_Benchmark/WordCount/baseline/cloudburst -> repo roots
WAS_ROOT = os.path.abspath(os.path.join(HERE, '..', '..', '..', '..', '..'))
CB_ROOT = os.path.abspath(os.path.join(
    WAS_ROOT, '..', 'compare_system', 'cloudburst'))
sys.path.insert(0, CB_ROOT)

from cloudburst.server.benchmarks.redis_runner import RedisCloudburst  # noqa
from cloudburst.shared.redis_kvs import RedisKvsClient                 # noqa
from cloudburst.server.benchmarks import utils                         # noqa

HEADER = ('size_mb,workers,topo,e2e_ms_median,throughput_mb_s,words_per_s,'
          'peak_mem_mb,reps,kvs_put_mb,kvs_get_mb,total_occurrences')


def median(xs):
    xs = sorted(xs)
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    corpus = args[0] if len(args) > 0 else \
        os.path.join(WAS_ROOT, 'TestData', 'corpus_large.txt')
    N = args[1] if len(args) > 1 else '4'
    num_requests = int(args[2]) if len(args) > 2 else 3

    size_mb = round(os.path.getsize(corpus) / (1024 * 1024))

    os.environ['WC_NUM_MAPPERS'] = N
    os.environ['WC_CORPUS'] = corpus
    # Re-import wordcount so its module-level NUM_MAPPERS/CORPUS pick up env.
    import cloudburst.server.benchmarks.wordcount as wc
    importlib.reload(wc)

    kvs = RedisKvsClient()
    kvs.flushdb()
    kvs.reset_stats()
    client = RedisCloudburst(kvs=kvs)

    # wordcount.run() prints progress to stdout; keep stdout clean (CSV only) by
    # routing the benchmark's chatter to stderr.
    _real_stdout = sys.stdout
    sys.stdout = sys.stderr
    try:
        total_time, _, _, _ = wc.run(client, num_requests, None)
    finally:
        sys.stdout = _real_stdout

    # total_occurrences from the merged reducer output (validation/consistency).
    merged = client.last_result or {}
    occ = sum(merged.values())

    e2e_ms = [t * 1000 for t in total_time]
    med_ms = median(e2e_ms)
    stats = kvs.transfer_stats()
    put_mb = stats['bytes_put'] / (1024 * 1024)
    get_mb = stats['bytes_get'] / (1024 * 1024)
    # ru_maxrss is in KB on Linux; clean per-N because this is a fresh process.
    peak_mb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024.0
    mbps = size_mb / (med_ms / 1000.0)
    wps = occ / (med_ms / 1000.0)

    if want_header:
        print(HEADER)
    print('%d,%s,redis-kvs,%.1f,%.2f,%.0f,%.1f,%d,%.2f,%.2f,%d' %
          (size_mb, N, med_ms, mbps, wps, peak_mb, num_requests,
           put_mb, get_mb, occ))


if __name__ == '__main__':
    main()
