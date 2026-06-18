#  SUMMA matrix-multiply — 2D block DAG on Cloudburst (Redis-backed runner).
#
#  Mirrors the r×c block decomposition of the WebAsShared and Faasm matrix
#  workloads, expressed as a Cloudburst DAG (the algorithm of
#  ../../../../../Benchmarks/Cloudburst/summa.py, adapted from Anna send/recv to
#  the Redis runner per ../../../EXPERIMENT_RUNBOOK.md §4.4):
#
#      mat_tile ──► mat_block_0_0 ──┐
#               ├─► mat_block_i_j ──┤
#               └─► mat_block_R_C ──┴─► mat_reduce
#
#  - mat_tile      : reads A,B, cuts A into R block-rows and B into C block-cols,
#                    puts each panel into the KVS, returns a uid (flows to blocks).
#  - mat_block_i_j : reads its A_i / B_j panels from the KVS, computes
#                    C_ij = A_i*·B_*j (numpy BLAS — Cloudburst's native kernel),
#                    returns the C_ij block.
#  - mat_reduce    : receives all C blocks, folds the checksum (Σ of all entries).
#
#  The panels (tile→blocks, the SUMMA broadcast) and the C blocks (blocks→reduce)
#  are the inter-stage state Cloudburst serializes (cloudpickle) through its KVS
#  (Redis here) — the transfer WebAsShared's zero-copy page-chain avoids.
#
#  CAVEAT (parallelism): the Redis runner executes the DAG in one process, so the
#  blocks run sequentially, not on concurrent executors. The SERIALIZED-TRANSFER
#  metrics (kvs_put_mb/kvs_get_mb and the e2e latency including cloudpickle
#  ser/deser through Redis) are faithful; the parallel speedup slope is not.

import os
import uuid

import numpy as np
import cloudpickle as cp

from cloudburst.shared.reference import CloudburstReference

N = int(os.environ.get('MAT_N', '1024'))
W = int(os.environ.get('MAT_WORKERS', '4'))


def _grid(w):
    r = 1
    k = 1
    while k * k <= w:
        if w % k == 0:
            r = k
        k += 1
    return r, w // r


R, C = _grid(W)
BR, BC = N // R, N // C


def run(cloudburst_client, num_requests, sckt):
    def tile(cloudburst, A, B):
        uid = uuid.uuid4().hex
        for i in range(R):
            cloudburst.put(uid + '_a_%d' % i,
                           np.ascontiguousarray(A[i * BR:(i + 1) * BR, :]))
        for j in range(C):
            cloudburst.put(uid + '_b_%d' % j,
                           np.ascontiguousarray(B[:, j * BC:(j + 1) * BC]))
        return uid

    def make_block(i, j):
        def block(cloudburst, uid):
            a = cloudburst.get(uid + '_a_%d' % i)   # BR × N
            b = cloudburst.get(uid + '_b_%d' % j)   # N × BC
            # NAIVE O(N^3) contraction (no BLAS) — np.einsum(optimize=False) runs
            # numpy's nested-loop kernel, NOT a tuned GEMM, so the block compute
            # matches the WASM baselines' naive ikj kernel. This makes the
            # cross-system comparison about the data substrate, not the kernel.
            return np.einsum('ik,kj->ij', a, b, optimize=False)   # BR × BC
        return block

    def reduce(cloudburst, *blocks):
        return int(sum(int(np.asarray(b).sum()) for b in blocks if b is not None))

    cloudburst_client.register(tile, 'mat_tile')
    bnames = []
    for i in range(R):
        for j in range(C):
            nm = 'mat_block_%d_%d' % (i, j)
            cloudburst_client.register(make_block(i, j), nm)
            bnames.append(nm)
    cloudburst_client.register(reduce, 'mat_reduce')

    funcs = ['mat_tile'] + bnames + ['mat_reduce']
    conns = [('mat_tile', b) for b in bnames] + [(b, 'mat_reduce') for b in bnames]
    ok, err = cloudburst_client.register_dag('summa', funcs, conns)
    if not ok:
        raise RuntimeError('register_dag failed: %s' % err)

    A = np.fromfile(os.environ['MAT_A']).reshape(N, N)
    B = np.fromfile(os.environ['MAT_B']).reshape(N, N)

    # Stage the inputs OUTSIDE the timed region (input staging excluded).
    oids = []
    for _ in range(num_requests):
        oa, ob = uuid.uuid4().hex, uuid.uuid4().hex
        cloudburst_client.put_object(oa, A)
        cloudburst_client.put_object(ob, B)
        oids.append((oa, ob))

    import time
    times = []
    for (oa, ob) in oids:
        arg_map = {'mat_tile': [CloudburstReference(oa, True),
                                CloudburstReference(ob, True)]}
        t = time.time()
        cloudburst_client.call_dag('summa', arg_map, True)
        times.append(time.time() - t)

    if sckt:
        sckt.send(cp.dumps(times))
    return times, [], [], 0
