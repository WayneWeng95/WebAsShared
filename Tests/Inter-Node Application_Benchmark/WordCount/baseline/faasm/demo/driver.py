#!/usr/bin/env python3
# driver.py — Faasm-like WordCount demo (Faaslet-lite), single box.
#
# Faasm runs WordCount as WASM functions (Faaslets) in a lightweight runtime,
# moving the corpus chunks and partial counts through a distributed KV
# (faasmReadState/WriteState, Redis-backed). Standing up real Faasm here is
# blocked (images gone — see ../README.md), so this abstracts the *mechanism*:
#
#   * each map/reduce is a fresh **wasmtime** instance running wc.cwasm (a
#     wasm32-wasip1 module) — the Faaslet WASM isolation, AOT-compiled like
#     Faasm's cached machine code;
#   * the host (this driver) does the KV I/O — splitter writes `chunk_<i>` to
#     Redis, each mapper reads its chunk and writes `partial_<i>`, the reducer
#     reads the partials — i.e. **state serialized through the KV**, exactly the
#     transfer our zero-copy SHM page-chain avoids;
#   * mappers run concurrently (a Faaslet per function).
#
# Faithful: WASM isolation + serialized KV state + chaining shape + the
# partitioned (not broadcast) read pattern of the real wordcount.cpp. Dropped:
# Faasm's scheduler, Proto-Faaslet snapshots, WAVM specifics, billable-memory
# accounting beyond peak RSS. Honestly a Faasm-*like* demo, not the runtime.
#
# Env: WC_CORPUS, MAPPER_NUM, REDIS_HOST/PORT. Prints one CSV row (+ --header).
import os
import subprocess
import sys
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor

import redis

HERE = os.path.dirname(os.path.abspath(__file__))
WASM = os.path.join(HERE, 'wc.cwasm')
WASMTIME = os.path.expanduser('~/.wasmtime/bin/wasmtime')


def _tree_rss_kb(root_pid):
    """Σ VmRSS (kB) over `root_pid` and all its descendants — the full resident
    footprint of the driver + its co-resident Faaslet subprocesses. This is the
    same accounting WasMem's run.sh applies to the host process tree (sum the
    whole tree, full RSS, no shared-page de-duplication), so the two systems are
    measured identically."""
    children = {}
    rss = {}
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
    """Out-of-band high-water sampler of the driver process tree's full RSS
    (driver + co-resident Faaslet subprocesses) at 50 ms — mirrors WasMem's
    run.sh sample_peak (Σ tree RSS) so both systems are measured the same way.
    No SHM term: Faasm's state substrate is Redis (reported as state_kv_mb)."""

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


def run_faaslet(mode, data):
    """One fresh WASM instance (Faaslet): feed `data` on stdin, return stdout.
    Memory is captured out-of-band by the process-tree PeakSampler, not here."""
    p = subprocess.Popen([WASMTIME, 'run', '--allow-precompiled', WASM, mode],
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL)
    out, _ = p.communicate(input=data)
    return out


def split_newline_aligned(corpus, n):
    """N contiguous, newline-aligned byte chunks (matches wordcount.cpp)."""
    total = len(corpus)
    approx = total // n
    chunks = []
    start = 0
    for i in range(n):
        if i == n - 1:
            end = total
        else:
            end = start + approx
            while end < total and corpus[end] != 0x0A:  # to next '\n'
                end += 1
            if end < total:
                end += 1
        chunks.append(corpus[start:end])
        start = end
    return chunks


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    N = int(args[0]) if args else int(os.environ.get('MAPPER_NUM', '4'))
    corpus_path = os.environ.get('WC_CORPUS', '/data/corpus.txt')
    size_mb = round(os.path.getsize(corpus_path) / (1024 * 1024))

    r = redis.Redis(host=os.environ.get('REDIS_HOST', '127.0.0.1'),
                    port=int(os.environ.get('REDIS_PORT', '6379')))
    uid = uuid.uuid4().hex
    with open(corpus_path, 'rb') as f:
        corpus = f.read()

    # Sample the whole driver process tree (driver + Faaslets) out-of-band for
    # the duration of the run, exactly as WasMem's run.sh samples its host tree.
    sampler = PeakSampler(os.getpid())
    sampler.start()

    t0 = time.time()
    # ── splitter: write N chunks to the KV (serialized state) ─────────────────
    chunks = split_newline_aligned(corpus, N)
    state_bytes = 0
    for i, ch in enumerate(chunks):
        r.set('%s_chunk_%d' % (uid, i), ch)
        state_bytes += len(ch)

    # ── mappers (Faaslet per function, concurrent): chunk_i -> partial_i ──────
    def mapper(i):
        ch = r.get('%s_chunk_%d' % (uid, i))
        partial = run_faaslet('map', ch)
        r.set('%s_partial_%d' % (uid, i), partial)
        return len(partial)

    with ThreadPoolExecutor(max_workers=N) as ex:
        partial_lens = list(ex.map(mapper, range(N)))
    state_bytes += sum(partial_lens)             # writes
    state_bytes += sum(len(c) for c in chunks)   # mapper reads (partitioned: 1× corpus)
    state_bytes += sum(partial_lens)             # reducer reads

    # ── reducer: merge the N partials in one Faaslet ──────────────────────────
    partials = b''.join(r.get('%s_partial_%d' % (uid, i)) for i in range(N))
    merged = run_faaslet('reduce', partials)
    e2e_s = time.time() - t0
    sampler.stop()
    peak_rss_kb = sampler.peak_kb

    # total_occurrences from the merged "word\x1fcount\n" output.
    occ = 0
    for line in merged.split(b'\n'):
        if not line:
            continue
        sep = line.rfind(b'\x1f')
        if sep != -1:
            try:
                occ += int(line[sep + 1:])
            except ValueError:
                pass

    # Footprint = high-water Σ RSS of the whole driver process tree (driver +
    # the N concurrent map Faaslets, then the reducer), the same whole-tree
    # accounting WasMem applies to its host process tree.
    peak_mb = peak_rss_kb / 1024.0
    e2e_ms = e2e_s * 1000.0
    mbps = size_mb / e2e_s if e2e_s else 0
    wps = occ / e2e_s if e2e_s else 0
    state_mb = state_bytes / (1024 * 1024)

    # cleanup
    r.delete(*[k for i in range(N)
               for k in ('%s_chunk_%d' % (uid, i), '%s_partial_%d' % (uid, i))])

    header = ('size_mb,workers,topo,e2e_ms,throughput_mb_s,words_per_s,'
              'peak_mem_mb,state_kv_mb,total_occurrences')
    row = ('%d,%d,wasm-redis,%.1f,%.2f,%.0f,%.1f,%.2f,%d' %
           (size_mb, N, e2e_ms, mbps, wps, peak_mb, state_mb, occ))
    if want_header:
        print(header)
    print(row)


if __name__ == '__main__':
    main()
