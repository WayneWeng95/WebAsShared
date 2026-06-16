#!/usr/bin/env python3
# wordcount.py — BARE-METAL reference for the WordCount workload.
#
# A single-process, multi-threaded word count: NO framework, NO IPC, NO KVS, NO
# WASM, NO shared-memory page chain — just the raw machine doing the same work,
# so the suite's distributed/zero-copy solutions can be read against the floor a
# plain program hits on one box.
#
# Shape mirrors the other systems' splitter→mapper×N→reducer:
#   * split   : cut the corpus into N contiguous, newline-aligned chunks (in RAM,
#               zero-copy via memoryview — the bare-metal analogue of
#               wc_distribute's contiguous split).
#   * map ×N  : a ThreadPoolExecutor of N threads, each counting [A-Za-z]+ runs
#               (ASCII-lowercased) in its own chunk into a Counter.
#   * reduce  : merge the N partial Counters.
#
# Tokenization matches the guest and the Cloudburst baseline exactly: maximal
# [a-z]+ runs over lowercased text → total_occurrences is identical across every
# system and every N for a given corpus (the gate: 8,940,339 @50MB,
# 89,403,388 @500MB, 179,060,000 @1GB).
#
# >> The GIL caveat (this is the point of the experiment, not a bug):
#    Python threads cannot run CPU-bound bytecode in parallel — `re` and
#    `bytes.lower()` hold the GIL throughout — so the map threads SERIALIZE.
#    N>1 does NOT speed up the count; N=1 is the true single-thread reference.
#    The flat N-curve is exactly why the other systems fan out across PROCESSES
#    (RMMap pods, Faasm Faaslets, WebAsShared subprocess workers) instead of
#    threads. The single-process number is the bare-metal baseline; the curve's
#    flatness is the finding.
#
# This runs ONE (corpus, N) measurement and prints the parse-able result line
#   compute_ms=<f> total_occurrences=<n> unique_words=<n>
# to stdout. run.sh is the sweep harness (it also samples peak RSS out-of-band,
# the same way the WebAsShared run.sh samples the host tree).
#
# Usage: wordcount.py CORPUS_PATH N_THREADS
import re
import sys
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor

WORD_RE = re.compile(rb'[A-Za-z]+')   # ASCII letters; .lower() per word → [a-z]+

# Each map thread scans its chunk in newline-aligned WINDOW-byte slices. The
# count itself is C-level (re.findall builds the token list, Counter counts it),
# the idiom a competent single-box program uses — but windowing caps the live
# token list to ~WINDOW/avg_word_len entries instead of the whole chunk, so a
# 1 GB corpus doesn't blow up to one Python object per token (≈180 M of them).
WINDOW = 8 << 20   # 8 MiB


def newline_aligned_chunks(data, n):
    """Cut `data` (bytes) into <=n contiguous (start, end) byte ranges, each
    ending on a '\\n' so no word is split across chunks — the in-RAM analogue of
    the guest's split_input_contiguous. Ranges index the SAME `data` object (no
    per-thread copy of the corpus)."""
    total = len(data)
    ranges = []
    start = 0
    for i in range(n):
        if start >= total:
            break
        if i == n - 1:
            end = total
        else:
            approx = (total * (i + 1)) // n
            nl = data.find(b'\n', approx)
            end = (nl + 1) if nl != -1 else total
        ranges.append((start, end))
        start = end
    return ranges


def count_range(data, start, end):
    """Count [a-z]+ occurrences in data[start:end] → Counter(word -> count),
    scanning in newline-aligned WINDOW slices (C-level findall + Counter)."""
    counts = Counter()
    pos = start
    while pos < end:
        win_end = min(pos + WINDOW, end)
        if win_end < end:                       # extend to the next newline so
            nl = data.find(b'\n', win_end)      # a word never straddles a window
            win_end = (nl + 1) if (0 <= nl < end) else end
        window = data[pos:win_end].lower()      # ≤ WINDOW bytes, transient
        counts.update(WORD_RE.findall(window))
        pos = win_end
    return counts


def main():
    corpus_path = sys.argv[1]
    n = int(sys.argv[2])

    # Read the corpus OUTSIDE the timer — input staging is excluded for every
    # system in this suite (the timer covers distribute + map + reduce only).
    with open(corpus_path, 'rb') as fh:
        data = fh.read()

    t0 = time.perf_counter()

    ranges = newline_aligned_chunks(data, n)
    if n == 1 or len(ranges) == 1:
        partials = [count_range(data, s, e) for (s, e) in ranges]
    else:
        with ThreadPoolExecutor(max_workers=n) as pool:
            partials = list(pool.map(lambda r: count_range(data, r[0], r[1]), ranges))

    total = Counter()
    for p in partials:
        total.update(p)

    elapsed_ms = (time.perf_counter() - t0) * 1000.0

    total_occurrences = sum(total.values())
    unique_words = len(total)
    print("compute_ms=%.2f total_occurrences=%d unique_words=%d"
          % (elapsed_ms, total_occurrences, unique_words))


if __name__ == '__main__':
    main()
