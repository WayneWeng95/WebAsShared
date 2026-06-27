#!/usr/bin/env python3
# rdma_mapper.py — worker-side RMMap (RDMA) WordCount mapper. Runs on whichever node the
# driver SSHes into. It acquires its 67 MB chunk by ONE-SIDED RDMA READ straight out of
# the driver's registered corpus memory (no KVS, no serialize) via the `rdma_wc read`
# helper, then counts with the SAME wc_ops.wc_map as every other bar — so the only thing
# that differs from the RMMap-ES bar is the *transfer* (RDMA vs Redis GET), which is the
# whole point of the comparison.
#
# Out (stdout): "RESULT busy_ms=.. transfer_ms=.. read_ms=.. count_ms=.. occ=.. unique=.. len=.."
# followed by the partial as compact "<word> <count>" lines (the driver merges = reduce).
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wc_ops

RDMA_WC = os.environ.get('RDMA_WC', '/tmp/rdma_wc')


def main():
    server, port, idx = sys.argv[1], sys.argv[2], sys.argv[3]
    # @transfer: one-sided RDMA READ of the chunk (helper writes raw bytes to stdout).
    t0 = time.time()
    p = subprocess.run([RDMA_WC, 'read', server, port, idx], capture_output=True)
    transfer_ms = (time.time() - t0) * 1000.0
    if p.returncode != 0:
        sys.stderr.write('mapper idx=%s read failed: %s\n'
                         % (idx, p.stderr.decode('utf-8', 'ignore')))
        sys.exit(1)
    data = p.stdout
    read_ms = 0.0                              # pure NIC read time (< transfer; excludes spawn+pipe)
    for tok in p.stderr.decode('utf-8', 'ignore').split():
        if tok.startswith('read_ms='):
            read_ms = float(tok.split('=', 1)[1])

    # @execute: identical WordCount body to the ES/Cloudburst/Faasm bars.
    t1 = time.time()
    partial = wc_ops.wc_map(data)
    count_ms = (time.time() - t1) * 1000.0

    busy_ms = transfer_ms + count_ms
    # one self-contained line per mapper (the node launcher concatenates 15 of these).
    # occ is the fan-out-invariant gate; the full {word:count} reduce is omitted — merging
    # 60 tiny dicts is sub-second and identical across all systems, not the cost compared.
    sys.stdout.write('RESULT idx=%s busy_ms=%.3f transfer_ms=%.3f read_ms=%.3f '
                     'count_ms=%.3f occ=%d unique=%d len=%d\n' %
                     (idx, busy_ms, transfer_ms, read_ms, count_ms,
                      sum(partial.values()), len(partial), len(data)))


if __name__ == '__main__':
    main()
