#!/usr/bin/env python3
# driver.py — RMMap (RDMA) inter-node Matrix (SUMMA) driver (runs on node 0).
#
# The faithful RMMap data path for the SUMMA block multiply C = A·B, WITHOUT the MITOSIS
# kernel module: user-space one-sided RDMA READ over the RoCE NIC. The dominant transfer is
# the PANEL BROADCAST (tile→blocks): each of the R·C blocks reads an A_i row-panel + a B_j
# col-panel, so ~R·C·(panel pair) bytes move. node 0 publishes ONE buffer holding all panels
# and registers it as a remote-read MR ONCE; every block RDMA-READs the two panels it needs
# straight out of that memory — no KVS upload, no per-block panel GET.
#
#   publish (once): build [A_row_panel_0..A_row_panel_{R-1}, B_col_panel_0..B_col_panel_{C-1}]
#                   (A row-panels are already contiguous; B col-panels repacked contiguous) +
#                   an R+C bounds sidecar → `rdma_ts serve_idx` (register-once / read-many).
#   per rep:        R·C block workers (SSH fan-out) — each RDMA-READs chunk[i]=A_i and
#                   chunk[R+j]=B_j, computes C_ij with the SAME naive einsum(optimize=False)
#                   kernel as the Cloudburst/Faasm bars, and reports its partial checksum
#                   int(C_ij.sum()). The driver sums the partials → the gate (Σ all C entries).
#
# Reporting partial checksums (not shipping the 2 MB C blocks to a reducer) mirrors the
# WordCount RDMA mappers reporting counts — the C-block reduce (64×2 MB) is small vs the
# panel broadcast (64×32 MB) and is the same across systems, so it is not the compared cost.
#
# Metrics match ../k8s/cloudburst + .../Inter-Node Application_Benchmark/{wasmem,faasm}:
#   makespan_ms  = per-rep wall of the block wave (panels published ONCE → no per-rep upload).
#   total_job_ms = Σ block busy (RDMA panel read + einsum). publish reported separately.
#   gate = checksum (Σ of all C entries) = 1,391,095,867,672. gflops = 2N³/makespan.
import argparse
import os
import statistics
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
NODE_IP = {'node-0': '10.10.1.2', 'node-1': '10.10.1.1',
           'node-2': '10.10.1.4', 'node-3': '10.10.1.3'}


def grid(w):
    r, k = 1, 1
    while k * k <= w:
        if w % k == 0:
            r = k
        k += 1
    return r, w // r


def publish_buffer(a_path, b_path, N, R, C):
    """Build the combined panel buffer + bounds, write both to /tmp. Returns (buf_path,
    bounds_path, BR, BC). A row-panels are A as-is (row-major, contiguous); B col-panels are
    repacked contiguous. Layout: R A-panels then C B-panels; chunk i=A_i, chunk R+j=B_j."""
    BR, BC = N // R, N // C
    A = np.fromfile(a_path).reshape(N, N)
    B = np.fromfile(b_path).reshape(N, N)
    a_blob = A.tobytes()                                   # R contiguous BR×N row-panels
    b_blob = b''.join(np.ascontiguousarray(B[:, j * BC:(j + 1) * BC]).tobytes() for j in range(C))
    buf_path = '/tmp/mat_pub.dat'
    with open(buf_path, 'wb') as f:
        f.write(a_blob)
        f.write(b_blob)
    bounds, off = [0], 0
    for _ in range(R):
        off += BR * N * 8
        bounds.append(off)
    for _ in range(C):
        off += N * BC * 8
        bounds.append(off)
    bnd_path = '/tmp/mat_pub.bnd'
    with open(bnd_path, 'w') as f:
        f.write('\n'.join(str(x) for x in bounds) + '\n')
    return buf_path, bnd_path, BR, BC


def run_node(node, self_host, server_ip, port, R, BR, BC, N, cells):
    """Launch ALL of this node's block workers in ONE SSH (batched), gather RESULT lines."""
    spec_args = ' '.join("'%d %d'" % (i, j) for (i, j) in cells)
    launch = (
        "k=0; for ij in %s; do RDMA_TS='%%s' python3 '%%s' $ij %d %d %d %d %s %d "
        ">/tmp/mat_$k.out 2>/tmp/mat_$k.err & k=$((k+1)); done; wait; "
        "k=0; for ij in %s; do cat /tmp/mat_$k.out; k=$((k+1)); done"
        % (spec_args, R, BR, BC, N, server_ip, port, spec_args))
    if node != self_host:
        cmd = ['ssh', node, launch % ('/tmp/rdma_ts', '/tmp/matrix_worker.py')]
    else:
        cmd = ['bash', '-c', launch % (os.path.join(HERE, 'rdma_ts'),
                                       os.path.join(HERE, 'matrix_worker.py'))]
    p = subprocess.run(cmd, capture_output=True, text=True)
    lines = [l for l in p.stdout.splitlines() if l.startswith('RESULT')]
    if len(lines) != len(cells):
        raise RuntimeError('node %s: %d/%d blocks returned. stderr: %s'
                           % (node, len(lines), len(cells), p.stderr[-400:]))
    return lines


def run_once(server_ip, port, R, C, BR, BC, N, nodes, self_host):
    cells = [(i, j) for i in range(R) for j in range(C)]
    by_node = {nd: [] for nd in nodes}
    for k, ij in enumerate(cells):
        by_node[nodes[k % len(nodes)]].append(ij)
    t0 = time.time()
    with ThreadPoolExecutor(max_workers=len(nodes)) as pool:
        futs = [pool.submit(run_node, nd, self_host, server_ip, port, R, BR, BC, N, by_node[nd])
                for nd in nodes if by_node[nd]]
        lines = []
        for fut in as_completed(futs):
            lines += fut.result()
    makespan_ms = (time.time() - t0) * 1000.0
    sum_busy, checksum = 0.0, 0
    for ln in lines:
        for tok in ln.split():
            if tok.startswith('busy_ms='):
                sum_busy += float(tok.split('=', 1)[1])
            elif tok.startswith('partial='):
                checksum += int(tok.split('=', 1)[1])
    return makespan_ms, sum_busy, checksum


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--a', default='TestData/matrix_a_4096.bin')
    ap.add_argument('--b', default='TestData/matrix_b_4096.bin')
    ap.add_argument('--matrix-n', type=int, default=4096)
    ap.add_argument('--workers', type=int, default=64)
    ap.add_argument('--reps', type=int, default=15)
    ap.add_argument('--server-ip', default='10.10.1.2')
    ap.add_argument('--port', type=int, default=18515)
    ap.add_argument('--serve-port', type=int, default=18620)
    ap.add_argument('--nodes', default='node-0,node-1,node-2,node-3')
    ap.add_argument('--self-host', default='node-0')
    ap.add_argument('--expect', type=int, default=None)
    ap.add_argument('--csv')
    a = ap.parse_args()

    for f in (a.a, a.b):
        if not os.path.exists(f):
            sys.exit('missing: %s' % f)
    N, W = a.matrix_n, a.workers
    R, C = grid(W)
    if N % R or N % C:
        sys.exit('N=%d not divisible by grid %dx%d' % (N, R, C))
    nodes = a.nodes.split(',')

    # sync binary + worker to /tmp on remote nodes (node 0 runs local).
    for node in nodes:
        if node == a.self_host:
            continue
        for src in ('rdma_ts', 'matrix_worker.py'):
            subprocess.run(['scp', '-q', os.path.join(HERE, src), '%s:/tmp/%s' % (node, src)],
                           check=True)

    # --- publish: build the panel buffer ONCE, start serve_idx over it ---
    buf_path, bnd_path, BR, BC = publish_buffer(a.a, a.b, N, R, C)
    srv_log = open('/tmp/rdma_matrix_srv.log', 'w')
    srv = subprocess.Popen([os.path.join(HERE, 'rdma_ts'), 'serve_idx', buf_path, bnd_path,
                            a.server_ip, str(a.serve_port)], stderr=srv_log)
    print('[rmmap-rdma-matrix] N=%d grid=%dx%d workers=%d publishing panels (%d MB)...' %
          (N, R, C, W, os.path.getsize(buf_path) // (1024 * 1024)), flush=True)
    time.sleep(2.0)

    try:
        pub0 = time.time()
        run_node(a.self_host, a.self_host, a.server_ip, a.serve_port, R, BR, BC, N, [(0, 0)])
        publish_ms = (time.time() - pub0) * 1000.0
        print('[rmmap-rdma-matrix] published panels: warmup=%.0f ms' % publish_ms, flush=True)

        mk_ms, tj_ms = [], []
        cks = None
        success = True
        for r in range(a.reps):
            m, tj, c = run_once(a.server_ip, a.serve_port, R, C, BR, BC, N, nodes, a.self_host)
            mk_ms.append(m)
            tj_ms.append(tj)
            if cks is None:
                cks = c
            elif c != cks:
                print('[rmmap-rdma-matrix] GATE FAIL rep%d: %d != %d' % (r, c, cks))
                success = False
            print('[rmmap-rdma-matrix] rep%d makespan=%.0f ms total_job=%.0f ms checksum=%d' %
                  (r, m, tj, c), flush=True)
    finally:
        srv.terminate()
        srv_log.close()

    if a.expect is not None and cks != a.expect:
        print('[rmmap-rdma-matrix] GATE FAIL: %d != expected %d' % (cks, a.expect))
        success = False

    mk_mean = statistics.mean(mk_ms)
    mk_std = statistics.pstdev(mk_ms) if len(mk_ms) > 1 else 0.0
    tj_mean = statistics.mean(tj_ms)
    gflops = round(2.0 * N ** 3 / (mk_mean / 1000.0) / 1e9, 3) if mk_mean > 0 else 0
    print('[rmmap-rdma-matrix] === N=%d workers=%d makespan=%.0f ± %.1f ms total_job=%.0f ms '
          'gflops=%.3f publish_once=%.0f ms checksum=%d %s ===' %
          (N, W, mk_mean, mk_std, tj_mean, gflops, publish_ms, cks, 'OK' if success else 'GATE-FAIL'))

    if a.csv:
        os.makedirs(os.path.dirname(os.path.abspath(a.csv)), exist_ok=True)
        new = not os.path.exists(a.csv)
        with open(a.csv, 'a') as f:
            if new:
                f.write('matrix_n,workers,nodes_used,variant,makespan_mean_ms,makespan_std_ms,'
                        'total_job_mean_ms,gflops,checksum,expect,success,reps\n')
            f.write('%d,%d,%d,rdma,%.0f,%.1f,%.0f,%.3f,%d,%d,%s,%d\n' %
                    (N, W, len(nodes), mk_mean, mk_std, tj_mean, gflops, cks,
                     a.expect if a.expect is not None else cks, success, a.reps))
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
