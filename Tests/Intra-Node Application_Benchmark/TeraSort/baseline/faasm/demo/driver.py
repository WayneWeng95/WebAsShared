#!/usr/bin/env python3
# driver.py — Faasm-like TeraSort demo (Faaslet-lite), single box.
#
# Abstracts Faasm's mechanism (real Faasm can't be stood up here — see the
# WordCount ../README.md): each partition/merge stage runs as a fresh **wasmtime**
# instance of ts.cwasm (a wasm32-wasip1 module = the Faaslet WASM isolation,
# AOT-compiled like Faasm's cached machine code); the host does the KV I/O — the
# splitter writes `chunk_<i>`, each partition Faaslet's tagged output is demuxed
# into per-owner `bucket_<i>_<j>` keys, and each merge Faaslet reads its column —
# i.e. the WHOLE dataset crosses the shuffle SERIALIZED through Redis, exactly
# the transfer our zero-copy SHM page-chain shuffle avoids.
#
# Faithful: WASM isolation, serialized KV state, the all-to-all shuffle shape,
# lightweight (streaming) Faaslet footprint. Dropped (honest scope): Faasm's
# scheduler, Proto-Faaslet snapshots, runtime specifics. A Faasm-*like* demo.
#
# Env: TS_RECORDS, TS_NUM_WORKERS, REDIS_HOST/PORT/DB. Prints one CSV row (+ --header).
import os
import subprocess
import sys
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor

import redis

HERE = os.path.dirname(os.path.abspath(__file__))
WASM = os.path.join(HERE, 'ts.cwasm')
WASMTIME = os.path.expanduser('~/.wasmtime/bin/wasmtime')

KEY_LEN = 10


def _tree_rss_kb(root_pid):
    """Sum VmRSS (kB) over root_pid and all descendants — the same whole-tree
    accounting WasMem's run.sh applies, so both systems are measured identically."""
    children, rss = {}, {}
    for entry in os.listdir('/proc'):
        if not entry.isdigit():
            continue
        pid = int(entry)
        try:
            with open('/proc/%d/status' % pid) as f:
                ppid = vmrss = 0
                for ln in f:
                    if ln.startswith('PPid:'):
                        ppid = int(ln.split()[1])
                    elif ln.startswith('VmRSS:'):
                        vmrss = int(ln.split()[1])
        except (OSError, ValueError):
            continue
        children.setdefault(ppid, []).append(pid)
        rss[pid] = vmrss
    total, seen, stack = 0, set(), [root_pid]
    while stack:
        p = stack.pop()
        if p in seen:
            continue
        seen.add(p)
        total += rss.get(p, 0)
        stack.extend(children.get(p, []))
    return total


class PeakSampler(threading.Thread):
    def __init__(self, root_pid, interval=0.05):
        super().__init__(daemon=True)
        self.root_pid, self.interval = root_pid, interval
        self.peak_kb = 0
        self._stop = threading.Event()

    def run(self):
        while not self._stop.is_set():
            cur = _tree_rss_kb(self.root_pid)
            if cur > self.peak_kb:
                self.peak_kb = cur
            self._stop.wait(self.interval)

    def stop(self):
        self._stop.set()
        self.join()


def run_faaslet(args, data):
    """One fresh WASM instance (Faaslet): feed `data` on stdin, return stdout."""
    p = subprocess.Popen([WASMTIME, 'run', '--allow-precompiled', WASM] + args,
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL)
    out, _ = p.communicate(input=data)
    return out


def split_newline_aligned(data, n):
    total = len(data)
    approx = total // n
    chunks, start = [], 0
    for i in range(n):
        if i == n - 1:
            end = total
        else:
            end = start + approx
            while end < total and data[end] != 0x0A:
                end += 1
            if end < total:
                end += 1
        chunks.append(data[start:end])
        start = end
    return chunks


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    N = int(args[0]) if args else int(os.environ.get('TS_NUM_WORKERS', '4'))
    records_path = os.environ.get('TS_RECORDS', '/data/terasort.dat')
    size_mb = round(os.path.getsize(records_path) / (1024 * 1024))

    r = redis.Redis(host=os.environ.get('REDIS_HOST', '127.0.0.1'),
                    port=int(os.environ.get('REDIS_PORT', '6379')),
                    db=int(os.environ.get('REDIS_DB', '3')))
    uid = uuid.uuid4().hex
    with open(records_path, 'rb') as f:
        data = f.read()

    sampler = PeakSampler(os.getpid())
    sampler.start()

    t0 = time.time()
    # ── splitter: write N contiguous chunks to the KV (serialized state) ───────
    chunks = split_newline_aligned(data, N)
    state_bytes = 0
    for i, ch in enumerate(chunks):
        r.set('%s_chunk_%d' % (uid, i), ch)
        state_bytes += len(ch)

    # ── partition Faaslets (concurrent): chunk_i -> tagged stream -> bucket_i_j ─
    def partition(i):
        ch = r.get('%s_chunk_%d' % (uid, i))
        tagged = run_faaslet(['partition', str(N)], ch)
        # Each tagged line is (128 + owner) + record; the +128 tag never collides
        # with '\n' or printable record bytes (owner 10 would == '\n' otherwise).
        buckets = [[] for _ in range(N)]
        for ln in tagged.split(b'\n'):
            if not ln:
                continue
            buckets[ln[0] - 128].append(ln[1:])
        written = 0
        for j in range(N):
            blob = b'\n'.join(buckets[j])
            r.set('%s_bucket_%d_%d' % (uid, i, j), blob)
            written += len(blob)
        return len(ch) + written  # chunk read + buckets written through KV

    with ThreadPoolExecutor(max_workers=N) as ex:
        part_bytes = list(ex.map(partition, range(N)))
    state_bytes += sum(part_bytes)

    # ── merge Faaslets (per owner): gather column, sort, summary ───────────────
    records = keysum = 0
    allsorted = ordered = True
    prev_last = None

    def merge(j):
        cols = [r.get('%s_bucket_%d_%d' % (uid, i, j)) for i in range(N)]
        blob = b'\n'.join(c for c in cols if c)
        summary = run_faaslet(['merge'], blob)
        kv = dict(tok.split(b'=', 1) for tok in summary.split() if b'=' in tok)
        return len(blob), kv

    with ThreadPoolExecutor(max_workers=N) as ex:
        merge_outs = list(ex.map(merge, range(N)))

    summaries = {}
    for read_bytes, kv in merge_outs:
        state_bytes += read_bytes
        n = int(kv.get(b'records', 0))
        records += n
        keysum += int(kv.get(b'keysum', 0))
        if kv.get(b'sorted') != b'1':
            allsorted = False
        if n > 0:
            summaries[kv[b'first']] = kv[b'last']
    # global-order check across ranges (by first key)
    for first in sorted(summaries):
        if prev_last is not None and first < prev_last:
            ordered = False
        prev_last = summaries[first]

    e2e_s = time.time() - t0
    sampler.stop()
    peak_mb = sampler.peak_kb / 1024.0

    if not allsorted:
        sys.stderr.write('WARN N=%d: a range is NOT sorted\n' % N)
    if not ordered:
        sys.stderr.write('WARN N=%d: ranges NOT globally ordered\n' % N)

    checksum = '%d:%d' % (records, keysum)
    e2e_ms = e2e_s * 1000.0
    mbps = size_mb / e2e_s if e2e_s else 0
    rps = records / e2e_s if e2e_s else 0
    state_mb = state_bytes / (1024 * 1024)

    # cleanup this run's keys
    keys = ['%s_chunk_%d' % (uid, i) for i in range(N)] + \
           ['%s_bucket_%d_%d' % (uid, i, j) for i in range(N) for j in range(N)]
    if keys:
        r.delete(*keys)

    header = ('size_mb,workers,topo,e2e_ms,throughput_mb_s,records_per_s,'
              'peak_mem_mb,state_kv_mb,checksum')
    row = ('%d,%d,wasm-redis,%.1f,%.2f,%.0f,%.1f,%.2f,%s' %
           (size_mb, N, e2e_ms, mbps, rps, peak_mb, state_mb, checksum))
    if want_header:
        print(header)
    print(row)


if __name__ == '__main__':
    main()
