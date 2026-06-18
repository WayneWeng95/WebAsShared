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
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor

import redis

HERE = os.path.dirname(os.path.abspath(__file__))
WASM = os.environ.get('FINRA_WASM', os.path.join(HERE, 'finra.cwasm'))
WASMTIME = os.path.expanduser('~/.wasmtime/bin/wasmtime')
HEADER = ('size_trades,topo,e2e_ms,throughput_trades_s,peak_mem_mb,'
          'state_kv_mb,total_violations')


def _read_smaps(pid):
    """(private_kb, shared_kb) of `pid` from /proc/<pid>/smaps_rollup.
    Private = Private_Clean+Private_Dirty; Shared = Shared_* (common runtime/lib
    pages, shared across the co-resident Faaslets)."""
    priv = shared = 0
    try:
        with open('/proc/%d/smaps_rollup' % pid) as f:
            for ln in f:
                key = ln.split(':', 1)[0]
                if key in ('Private_Clean', 'Private_Dirty'):
                    priv += int(ln.split()[1])
                elif key in ('Shared_Clean', 'Shared_Dirty'):
                    shared += int(ln.split()[1])
    except OSError:
        pass
    return priv, shared


def run_faaslet(rule_id, data):
    """One fresh WASM instance: feed trades on stdin, return (count,
    peak_private_kb, peak_shared_kb). The peak RSS is split private vs shared by
    sampling the child's smaps_rollup at ~2 ms while it runs, so the concurrent
    footprint = Σ private + shared-once (matches the WasMem metric)."""
    p = subprocess.Popen([WASMTIME, 'run', '--allow-precompiled', WASM, str(rule_id)],
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL)
    box = {}

    def _io():
        box['out'] = p.communicate(input=data)[0]

    t = threading.Thread(target=_io)
    t.start()
    peak_priv = peak_shared = 0
    while t.is_alive():
        pr, sh = _read_smaps(p.pid)
        if pr + sh > peak_priv + peak_shared:
            peak_priv, peak_shared = pr, sh
        time.sleep(0.002)
    t.join()
    return int((box.get('out') or b'').strip() or 0), peak_priv, peak_shared


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
        cnt, priv, shared = run_faaslet(i, blob)
        return cnt, len(blob), priv, shared

    with ThreadPoolExecutor(max_workers=8) as ex:
        res = list(ex.map(rule, range(8)))
    total = sum(c for c, _, _, _ in res)
    state_bytes += sum(b for _, b, _, _ in res)   # 8× broadcast reads
    # The 8 rule-checkers run CONCURRENTLY (a Faaslet each, broadcast trades), so
    # the footprint is Σ private RSS + the shared runtime once (max), not the max
    # of one, nor Σ full RSS (which would count the shared wasmtime runtime 8×).
    peak_rss = (sum(pr for _, _, pr, _ in res)
                + max((sh for _, _, _, sh in res), default=0))
    e2e_ms = (time.time() - t0) * 1000.0

    peak_mb = peak_rss / 1024.0
    tps = size_trades / (e2e_ms / 1000.0) if e2e_ms else 0
    r.delete(key)
    if want_header:
        print(HEADER)
    print('%d,wasm-redis,%.1f,%.0f,%.1f,%.2f,%d' %
          (size_trades, e2e_ms, tps, peak_mb, state_bytes / (1024 * 1024), total))


if __name__ == '__main__':
    main()
