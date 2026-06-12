#!/usr/bin/env python3
# driver.py — Faasm-like FINRA demo (Faaslet-lite), single box.
#
# Each audit rule is a fresh wasmtime instance running finra.cwasm (a
# wasm32-wasip1 module = a Faaslet); the host (this driver) does the KV I/O:
# `fetch` writes the trades to Redis, then the 8 rules run concurrently, each
# fed the trades on stdin (read from Redis — the serialized broadcast), and a
# `merge` sums their counts. Same 8-rule spec as finra.rs → identical violations.
#
# Env: WC_CORPUS (trades csv), REDIS_HOST/PORT, FINRA_WASM (default finra.cwasm).
import os
import resource
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor

import redis

HERE = os.path.dirname(os.path.abspath(__file__))
WASM = os.environ.get('FINRA_WASM', os.path.join(HERE, 'finra.cwasm'))
WASMTIME = os.path.expanduser('~/.wasmtime/bin/wasmtime')
HEADER = ('size_trades,topo,e2e_ms,throughput_trades_s,peak_mem_mb,'
          'state_kv_mb,total_violations')


def run_faaslet(rule_id, data):
    """One fresh WASM instance: feed trades on stdin, return (count, rss_kb)."""
    tf = tempfile.NamedTemporaryFile(delete=False)
    tf.close()
    p = subprocess.run(['/usr/bin/time', '-v', '-o', tf.name,
                        WASMTIME, 'run', '--allow-precompiled', WASM, str(rule_id)],
                       input=data, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    rss = 0
    try:
        for ln in open(tf.name):
            if 'Maximum resident set size' in ln:
                rss = int(ln.rsplit(':', 1)[1]); break
    finally:
        os.unlink(tf.name)
    return int(p.stdout.strip() or 0), rss


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    corpus = os.environ.get('WC_CORPUS', '/data/trades.csv')
    with open(corpus, 'rb') as f:
        trades = f.read()
    size_trades = trades.count(b'\n') - 1

    r = redis.Redis(host=os.environ.get('REDIS_HOST', '127.0.0.1'),
                    port=int(os.environ.get('REDIS_PORT', '6379')))
    key = 'finra_trades_%s' % os.urandom(4).hex()

    t0 = time.time()
    r.set(key, trades)                       # fetch: write trades to the KV
    state_bytes = len(trades)

    def rule(i):
        blob = r.get(key)                    # read trades (broadcast)
        cnt, rss = run_faaslet(i, blob)
        return cnt, len(blob), rss

    with ThreadPoolExecutor(max_workers=8) as ex:
        res = list(ex.map(rule, range(8)))
    total = sum(c for c, _, _ in res)
    state_bytes += sum(b for _, b, _ in res)   # 8× broadcast reads
    peak_rss = max(rss for _, _, rss in res)
    e2e_ms = (time.time() - t0) * 1000.0

    peak_mb = max(peak_rss, resource.getrusage(resource.RUSAGE_SELF).ru_maxrss) / 1024.0
    tps = size_trades / (e2e_ms / 1000.0) if e2e_ms else 0
    r.delete(key)
    if want_header:
        print(HEADER)
    print('%d,wasm-redis,%.1f,%.0f,%.1f,%.2f,%d' %
          (size_trades, e2e_ms, tps, peak_mb, state_bytes / (1024 * 1024), total))


if __name__ == '__main__':
    main()
