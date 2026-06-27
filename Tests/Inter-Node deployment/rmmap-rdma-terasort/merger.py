#!/usr/bin/env python3
# merger.py — worker-side RMMap (RDMA) TeraSort MERGER (the shuffle gather + sort).
# Runs on whichever node the driver SSHes into. It gathers its owner column by ONE-SIDED
# RDMA READ straight out of each partitioner's published bucket memory (no KVS, no
# serialize): for owner j it reads bucket[j] from every partitioner i = `rdma_ts read
# <ip_i> <port_i> <j>`, then SORTS its key range with the SAME ts_ops.ts_merge as the
# Cloudburst/Faasm bars (only the transfer differs).
#
# Args: <j> <N> <ip0:port0> <ip1:port1> ... <ip{N-1}:port{N-1}>
# Out (stdout, one line): "RESULT idx=j busy_ms=.. transfer_ms=.. sort_ms=.. records=..
#                          keysum=.. sorted=0|1 first=<hex> last=<hex> len=.."
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ts_ops

RDMA_TS = os.environ.get('RDMA_TS', '/tmp/rdma_ts')


def rdma_read(ip, port, idx, tries=5):
    """RDMA-READ bucket[idx] from a partitioner server, retrying briefly in case the
    server's bind+listen is still coming up. Returns raw bytes (b'' for an empty bucket)."""
    last = b''
    for _ in range(tries):
        p = subprocess.run([RDMA_TS, 'read', ip, str(port), str(idx)], capture_output=True)
        if p.returncode == 0:
            return p.stdout
        last = p.stderr
        time.sleep(0.3)
    sys.stderr.write('merger read from %s:%s idx=%s failed: %s\n'
                     % (ip, port, idx, last.decode('utf-8', 'ignore')))
    sys.exit(1)


def main():
    j, n = int(sys.argv[1]), int(sys.argv[2])
    endpoints = [ep.split(':') for ep in sys.argv[3:3 + n]]

    # @transfer: RDMA-READ this owner's column — bucket[j] from each of the N partitioners.
    t0 = time.time()
    cols = [rdma_read(ip, port, j) for ip, port in endpoints]
    transfer_ms = (time.time() - t0) * 1000.0

    # @execute: identical merge (gather + sort) body to the Cloudburst/Faasm bars.
    t1 = time.time()
    s = ts_ops.ts_merge(cols)
    sort_ms = (time.time() - t1) * 1000.0

    busy_ms = transfer_ms + sort_ms
    sys.stdout.write('RESULT idx=%d busy_ms=%.3f transfer_ms=%.3f sort_ms=%.3f records=%d '
                     'keysum=%d sorted=%d first=%s last=%s len=%d\n' %
                     (j, busy_ms, transfer_ms, sort_ms, s['records'], s['keysum'],
                      1 if s['sorted'] else 0, s['first'].hex(), s['last'].hex(),
                      sum(len(c) for c in cols)))


if __name__ == '__main__':
    main()
