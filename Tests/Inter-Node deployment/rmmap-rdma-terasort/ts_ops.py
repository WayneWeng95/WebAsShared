# ts_ops.py — the TeraSort stage bodies, the same logic as the Cloudburst port
# (port_files/cloudburst_benchmarks/terasort.py): split / partition / merge / collect,
# but operating on BYTES (memory-friendly for a 1.2 GB record file, and identical to the
# byte-wise split used by wc_ops). Shared by the driver (node 0) and the executor pods so
# the distributed run executes the REAL function bodies, only routed across nodes through
# Redis. TeraSort is the maximal-data-movement contrast to WordCount: the ENTIRE dataset
# crosses the all-to-all shuffle (every record put once by its partitioner, got once by
# its merger), serialized THROUGH the KVS — vs WasMem's zero-copy page-chain shuffle.
#
# Records are Gensort 100-byte lines (see TeraSort/gen_records.py): a 10-byte key over
# printable ASCII [33..96] + 89-byte payload + '\n'. The host strips the newline, so each
# record is the 99-byte body and its key is the first 10 bytes.

KEY_LEN = 10
KEY_LO = 33          # must match gen_records.py / the guest
KEY_SPAN = 64        # 64 key symbols → equal-width range splitters are balanced


def _owner(first_byte, n):
    """Range-owner of a record from its first key byte — equal-width over [LO, LO+SPAN)."""
    b = min(max(first_byte, KEY_LO), KEY_LO + KEY_SPAN - 1) - KEY_LO
    o = b * n // KEY_SPAN
    return n - 1 if o >= n else o


def split_aligned(data, n):
    """N contiguous, NEWLINE-ALIGNED chunks (byte-for-byte the same split as the Faasm/
    WasMem terasort drivers and wc_ops.shard_bytes): even split, push each interior cut
    forward to the next '\\n' so no record is ever cut across a chunk boundary."""
    total = len(data)
    bounds = [i * total // n for i in range(n + 1)]
    for i in range(1, n):
        nl = data.find(b'\n', bounds[i])
        bounds[i] = (nl + 1) if nl != -1 else total
    return [data[bounds[i]:bounds[i + 1]] for i in range(n)]


def ts_partition(chunk, n):
    """Range-partition one chunk's records into n owner buckets (the shuffle scatter).
    Returns a list of n byte-blobs, each `b'\\n'.join(records)` for that owner."""
    buckets = [[] for _ in range(n)]
    for ln in chunk.split(b'\n'):
        if not ln:
            continue
        buckets[_owner(ln[0], n)].append(ln)
    return [b'\n'.join(b) for b in buckets]


def ts_merge(cols):
    """Gather one owner's column of buckets {bucket_i_j : i} (the shuffle gather),
    concatenate, SORT its key range, and return a summary (the merger)."""
    recs = []
    for c in cols:
        if c:
            recs.extend(ln for ln in c.split(b'\n') if ln)
    recs.sort(key=lambda r: r[:KEY_LEN])
    keysum = sum(b for r in recs for b in r[:KEY_LEN])
    srt = all(recs[k][:KEY_LEN] <= recs[k + 1][:KEY_LEN] for k in range(len(recs) - 1))
    first = recs[0][:KEY_LEN] if recs else b''
    last = recs[-1][:KEY_LEN] if recs else b''
    return {'records': len(recs), 'keysum': keysum, 'sorted': srt,
            'first': first, 'last': last}


def ts_collect(summaries):
    """Aggregate the n per-owner summaries → the fan-out-invariant gate: sum records +
    keysum, AND every range sorted, AND globally ordered across ranges."""
    rng = {j: s for j, s in summaries.items() if s}
    records = sum(s['records'] for s in rng.values())
    keysum = sum(s['keysum'] for s in rng.values())
    allsorted = all(s['sorted'] for s in rng.values())
    ordered, prev_last = True, None
    for j in sorted(rng):
        s = rng[j]
        if s['records'] == 0:
            continue
        if prev_last is not None and s['first'] < prev_last:
            ordered = False
        prev_last = s['last']
    return records, keysum, (allsorted and ordered)
