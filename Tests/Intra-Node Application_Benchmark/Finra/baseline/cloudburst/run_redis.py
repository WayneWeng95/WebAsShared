#!/usr/bin/env python3
# run_redis.py — Cloudburst FINRA baseline on Redis (one trades file per process).
#
# Drives the real finra.py DAG (fetch → 8 rules → merge) through the Redis-backed
# Cloudburst runner. The trades cross fetch → each rule serialized through Redis
# 8× (the broadcast). Same 8-rule spec as our native system → identical
# total_violations.
#
# Usage: ./run_redis.py TRADES.csv [NUM_REQUESTS] [--header]
import importlib
import os
import resource
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
FINRA_ROOT = os.path.abspath(os.path.join(HERE, '..', '..'))
WAS_ROOT = os.path.abspath(os.path.join(FINRA_ROOT, '..', '..', '..'))
CB_ROOT = os.path.abspath(os.path.join(WAS_ROOT, '..', 'compare_system', 'cloudburst'))
sys.path.insert(0, CB_ROOT)
sys.path.insert(0, FINRA_ROOT)
sys.path.insert(0, HERE)

from cloudburst.server.benchmarks.redis_runner import RedisCloudburst  # noqa
from cloudburst.shared.redis_kvs import RedisKvsClient                 # noqa

HEADER = ('size_trades,topo,e2e_ms,throughput_trades_s,peak_mem_mb,'
          'kvs_put_mb,kvs_get_mb,total_violations')


def median(xs):
    xs = sorted(xs)
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    trades = args[0]
    num_requests = int(args[1]) if len(args) > 1 else 3
    with open(trades) as f:
        size_trades = sum(1 for _ in f) - 1

    os.environ['WC_CORPUS'] = trades
    import finra  # the Cloudburst FINRA workload (finra.py)
    importlib.reload(finra)

    kvs = RedisKvsClient()
    kvs.flushdb()
    kvs.reset_stats()
    client = RedisCloudburst(kvs=kvs)

    _real = sys.stdout
    sys.stdout = sys.stderr
    try:
        total_time, _, _, _ = finra.run(client, num_requests, None)
    finally:
        sys.stdout = _real

    occ = client.last_result or 0
    med_ms = median([t * 1000 for t in total_time])
    stats = kvs.transfer_stats()
    put_mb = stats['bytes_put'] / (1024 * 1024)
    get_mb = stats['bytes_get'] / (1024 * 1024)
    peak_mb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024.0
    tps = size_trades / (med_ms / 1000.0) if med_ms else 0

    if want_header:
        print(HEADER)
    print('%d,redis-kvs,%.1f,%.0f,%.1f,%.2f,%.2f,%d' %
          (size_trades, med_ms, tps, peak_mb, put_mb, get_mb, occ))


if __name__ == '__main__':
    main()
