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
import tempfile
import time
import uuid
from concurrent.futures import ThreadPoolExecutor

import redis

HERE = os.path.dirname(os.path.abspath(__file__))
WASM = os.path.join(HERE, 'wc.cwasm')
WASMTIME = os.path.expanduser('~/.wasmtime/bin/wasmtime')


def run_faaslet(mode, data):
    """One fresh WASM instance (Faaslet): feed `data` on stdin, return
    (stdout, peak_rss_kb). RSS is measured per-instance with /usr/bin/time so the
    lightweight per-Faaslet footprint is captured accurately."""
    tf = tempfile.NamedTemporaryFile(delete=False)
    tf.close()
    p = subprocess.run(['/usr/bin/time', '-v', '-o', tf.name,
                        WASMTIME, 'run', '--allow-precompiled', WASM, mode],
                       input=data, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    rss_kb = 0
    try:
        with open(tf.name) as f:
            for ln in f:
                if 'Maximum resident set size' in ln:
                    rss_kb = int(ln.rsplit(':', 1)[1])
                    break
    finally:
        os.unlink(tf.name)
    return p.stdout, rss_kb


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
        partial, rss = run_faaslet('map', ch)
        r.set('%s_partial_%d' % (uid, i), partial)
        return len(partial), rss

    with ThreadPoolExecutor(max_workers=N) as ex:
        results = list(ex.map(mapper, range(N)))
    partial_lens = [pl for pl, _ in results]
    peak_rss_kb = max(rss for _, rss in results) if results else 0
    state_bytes += sum(partial_lens)             # writes
    state_bytes += sum(len(c) for c in chunks)   # mapper reads (partitioned: 1× corpus)
    state_bytes += sum(partial_lens)             # reducer reads

    # ── reducer: merge the N partials in one Faaslet ──────────────────────────
    partials = b''.join(r.get('%s_partial_%d' % (uid, i)) for i in range(N))
    merged, red_rss = run_faaslet('reduce', partials)
    peak_rss_kb = max(peak_rss_kb, red_rss)
    e2e_s = time.time() - t0

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

    # Faaslet footprint: largest single WASM-instance RSS (Faasm's headline is a
    # small per-function footprint vs containers — ~constant here thanks to the
    # streaming map, independent of corpus size).
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
