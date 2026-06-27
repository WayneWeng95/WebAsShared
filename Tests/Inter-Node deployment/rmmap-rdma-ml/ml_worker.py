#!/usr/bin/env python3
# ml_worker.py — worker-side RMMap (RDMA) ML worker (SGD-training gradient OR inference
# predict). Runs on whichever node the driver SSHes into. It acquires its samples shard by
# ONE-SIDED RDMA READ straight out of node 0's published CSV memory (no KVS, no serialize),
# then runs the SAME integer kernel (sgd_core / infer_core) as the Cloudburst/Faasm bars —
# only the *transfer* differs.
#
#   train <idx> <S> <server> <port>            : grad_sum (W=zeros) → [C,F] i64 gradient
#                                                (base64 on the RESULT line; driver sums).
#   infer <idx> <S> <server> <port> <model>    : infer_core.evaluate → (correct,total,predsum).
#
# Out (stdout): "RESULT idx=.. busy_ms=.. transfer_ms=.. compute_ms=.. (grad=<b64> | correct=..
#               total=.. predsum=..)"
import base64
import os
import subprocess
import sys
import time

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sgd_core
import infer_core

RDMA_TS = os.environ.get('RDMA_TS', '/tmp/rdma_ts')


def rdma_read(server, port, idx):
    p = subprocess.run([RDMA_TS, 'read', server, str(port), str(idx)], capture_output=True)
    if p.returncode != 0:
        sys.stderr.write('read idx=%s failed: %s\n' % (idx, p.stderr.decode('utf-8', 'ignore')))
        sys.exit(1)
    return p.stdout


def parse_csv(data):
    rows = [[int(v) for v in ln.split(',')]
            for ln in data.decode('ascii', 'ignore').splitlines()
            if ln and not ln.startswith('label')]
    arr = np.asarray(rows, dtype=np.int64)
    return arr[:, 1:].copy(), arr[:, 0].copy()


def main():
    mode, idx, S, server, port = (sys.argv[1], int(sys.argv[2]), int(sys.argv[3]),
                                  sys.argv[4], sys.argv[5])
    t0 = time.time()
    data = rdma_read(server, port, idx)                 # @transfer: one-sided RDMA READ
    transfer_ms = (time.time() - t0) * 1000.0

    t1 = time.time()
    X, y = parse_csv(data)
    if mode == 'train':
        g = sgd_core.grad_sum(X, y, sgd_core.init_weights(X.shape[1]))
        compute_ms = (time.time() - t1) * 1000.0
        extra = 'grad=%s' % base64.b64encode(g.tobytes()).decode()
    else:                                               # infer
        _, _, Wm = infer_core.load_model(sys.argv[6])
        correct, total, predsum = infer_core.evaluate(X, y, Wm)
        compute_ms = (time.time() - t1) * 1000.0
        extra = 'correct=%d total=%d predsum=%d' % (correct, total, predsum)

    busy_ms = transfer_ms + compute_ms
    sys.stdout.write('RESULT idx=%d busy_ms=%.3f transfer_ms=%.3f compute_ms=%.3f %s\n'
                     % (idx, busy_ms, transfer_ms, compute_ms, extra))


if __name__ == '__main__':
    main()
