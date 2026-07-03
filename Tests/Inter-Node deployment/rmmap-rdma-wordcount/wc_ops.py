# wc_ops.py — the WordCount stage bodies, byte-for-byte the same logic as the
# Cloudburst port (port_files/cloudburst_benchmarks/wordcount.py): split / mapper /
# reducer. Shared by the driver (node 0) and the executor pods so the distributed
# run executes the REAL function bodies, only routed across nodes through Redis.
import re
from collections import Counter

WORD_RE = re.compile(r'[a-z]+')


def shard_bytes(data, n):
    """Cut `data` (bytes) into n contiguous, NEWLINE-ALIGNED chunks — the byte-wise
    analogue of wordcount.py's `split` (which split on '\\n'). Memory-friendly for a
    multi-GB corpus: slice on adjusted byte offsets, never build a giant line list."""
    total = len(data)
    bounds = [i * total // n for i in range(n + 1)]
    for i in range(1, n):                      # push each interior cut to the next '\n'
        nl = data.find(b'\n', bounds[i])
        bounds[i] = (nl + 1) if nl != -1 else total
    return [data[bounds[i]:bounds[i + 1]] for i in range(n)]


def wc_map(text):
    """Count words in one chunk → {word: count}. Same regex/lowercasing as the port."""
    if isinstance(text, (bytes, bytearray)):
        text = text.decode('utf-8', 'ignore')
    counts = Counter()
    for w in WORD_RE.findall(text.lower()):
        counts[w] += 1
    return dict(counts)


def wc_reduce(partials):
    """Merge the N mapper partials → {word: count} (the reducer)."""
    total = Counter()
    for p in partials:
        if p:
            total.update(p)
    return dict(total)


def wc_reduce_lines(lines):
    """Reducer over serialized partials: each mapper ships its {word:count} as
    "word\\tcount" records (the RDMA shuffle payload); merge them all into the final
    {word:count}. Equivalent to wc_reduce over the deserialized dicts, but merges the
    120 partials straight into one Counter without rebuilding intermediate dicts."""
    total = Counter()
    for ln in lines:
        tab = ln.rfind('\t')
        if tab <= 0:
            continue
        total[ln[:tab]] += int(ln[tab + 1:])
    return dict(total)
