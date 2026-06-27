#!/usr/bin/env python3
# matrix_worker.py — worker-side RMMap (RDMA) SUMMA block. Runs on whichever node the
# driver SSHes into. It acquires its A_i row-panel and B_j col-panel by ONE-SIDED RDMA READ
# straight out of node 0's published A/B memory (no KVS, no serialize), then computes the
# block C_ij = A_i·B_j with the SAME naive einsum(optimize=False) kernel as the Cloudburst/
# Faasm bars — only the *transfer* differs.
#
# node 0 publishes ONE buffer = [A_row_panel_0..A_row_panel_{R-1}, B_col_panel_0..B_col_panel_{C-1}]
# via `rdma_ts serve_idx` (panels are contiguous byte ranges). Block (i,j) reads chunk[i]
# (= A_i, BR×N) and chunk[R+j] (= B_j, N×BC). The block's contribution to the global
# checksum (Σ of all C entries) is its partial sum int(C_ij.sum()) — reported back; shipping
# the 2 MB C block to a reducer (small vs the 32 MB/worker panel broadcast) is omitted, the
# same way the WordCount RDMA mappers report counts, not the word dicts.
#
# Args: <i> <j> <R> <BR> <BC> <N> <server_ip> <port>
# Out (stdout): "RESULT i=.. j=.. busy_ms=.. transfer_ms=.. compute_ms=.. partial=.."
import os
import subprocess
import sys
import time

import numpy as np

RDMA_TS = os.environ.get('RDMA_TS', '/tmp/rdma_ts')


def rdma_read(server, port, idx):
    p = subprocess.run([RDMA_TS, 'read', server, str(port), str(idx)], capture_output=True)
    if p.returncode != 0:
        sys.stderr.write('read idx=%s failed: %s\n' % (idx, p.stderr.decode('utf-8', 'ignore')))
        sys.exit(1)
    return p.stdout


def main():
    i, j, R, BR, BC, N, server, port = (int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]),
                                        int(sys.argv[4]), int(sys.argv[5]), int(sys.argv[6]),
                                        sys.argv[7], sys.argv[8])
    # @transfer: one-sided RDMA READ of the A_i row-panel (chunk i) and B_j col-panel (chunk R+j).
    t0 = time.time()
    a_buf = rdma_read(server, port, i)
    b_buf = rdma_read(server, port, R + j)
    transfer_ms = (time.time() - t0) * 1000.0

    # @execute: identical naive einsum block kernel to the Cloudburst/Faasm bars.
    t1 = time.time()
    a = np.frombuffer(a_buf, dtype=np.float64).reshape(BR, N)
    b = np.frombuffer(b_buf, dtype=np.float64).reshape(N, BC)
    c = np.einsum('ik,kj->ij', a, b, optimize=False)
    partial = int(c.sum())
    compute_ms = (time.time() - t1) * 1000.0

    busy_ms = transfer_ms + compute_ms
    sys.stdout.write('RESULT i=%d j=%d busy_ms=%.3f transfer_ms=%.3f compute_ms=%.3f partial=%d\n'
                     % (i, j, busy_ms, transfer_ms, compute_ms, partial))


if __name__ == '__main__':
    main()
