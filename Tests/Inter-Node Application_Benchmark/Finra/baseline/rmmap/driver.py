#!/usr/bin/env python3
# driver.py — RMMap (dmerge) FINRA baseline, ES protocol, single box.
#
# RMMap's ES transport = Redis + pickle. We run the frozen 8-rule FINRA spec
# (finra_rules.py, == finra.rs) over it: a `fetch` stage writes the parsed trades
# to Redis, then the 8 audit rules run as **parallel processes** (≈ RMMap's
# Knative pods), each `redis_get`-ing the WHOLE trades blob (the broadcast — 8×
# the trades through Redis) and writing its violation count back; a merge sums
# them. The serialized broadcast is the inter-stage transfer our zero-copy
# page-chain avoids. No kernel module (ES path); same spec → same violations.
#
# Env: WC_CORPUS (trades csv), REDIS_HOST/PORT/PASSWORD. Prints one CSV row.
import os
import pickle
import resource
import sys
import time
from multiprocessing import get_context

import redis

import finra_rules as fr

HEADER = ('size_trades,topo,e2e_ms,throughput_trades_s,peak_mem_mb,'
          'kvs_put_mb,kvs_get_mb,total_violations')


def _redis():
    return redis.Redis(host=os.environ.get('REDIS_HOST', '127.0.0.1'),
                       port=int(os.environ.get('REDIS_PORT', '6379')),
                       password=os.environ.get('REDIS_PASSWORD'))


def _rule_worker(args):
    """A separate process (≈ a pod): read the whole trades blob from Redis,
    run one audit rule, write the count back. Returns (bytes_read, bytes_written)."""
    rid, key, out_key = args
    r = _redis()
    blob = r.get(key)                      # ES read — the broadcast
    trades = pickle.loads(blob)
    v = fr.audit_rule(trades, rid)
    out = pickle.dumps(v)
    r.set(out_key, out)
    return len(blob), len(out)


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    corpus = os.environ.get('WC_CORPUS', '/data/trades.csv')
    with open(corpus) as f:
        size_trades = sum(1 for _ in f) - 1

    r = _redis()
    r.flushdb()
    uid = os.urandom(4).hex()
    trades_key = f'finra_{uid}_trades'

    t0 = time.time()
    # fetch: parse + write the trades blob (the broadcast source)
    with open(corpus) as f:
        trades = fr.parse_trades(f.read())
    blob = pickle.dumps(trades)
    r.set(trades_key, blob)
    put_bytes = len(blob)

    # 8 audit rules as parallel processes, each reads the whole blob (broadcast)
    payloads = [(i, trades_key, f'finra_{uid}_rule_{i}') for i in range(8)]
    with get_context('spawn').Pool(8) as pool:
        io = pool.map(_rule_worker, payloads)
    get_bytes = sum(rd for rd, _ in io)
    put_bytes += sum(wr for _, wr in io)

    # merge: read the 8 counts, sum
    total = 0
    for i in range(8):
        c = r.get(f'finra_{uid}_rule_{i}')
        get_bytes += len(c)
        total += pickle.loads(c)
    e2e_ms = (time.time() - t0) * 1000.0

    peak_mb = (resource.getrusage(resource.RUSAGE_SELF).ru_maxrss +
               resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss) / 1024.0
    tps = size_trades / (e2e_ms / 1000.0) if e2e_ms else 0
    if want_header:
        print(HEADER)
    print('%d,redis-es,%.1f,%.0f,%.1f,%.2f,%.2f,%d' %
          (size_trades, e2e_ms, tps, peak_mb,
           put_bytes / (1024 * 1024), get_bytes / (1024 * 1024), total))


if __name__ == '__main__':
    main()
