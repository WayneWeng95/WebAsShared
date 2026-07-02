#!/usr/bin/env python3
# driver.py — RMMap (RDMA) inter-node ML driver (SGD-training OR inference), runs on node 0.
#
# The faithful RMMap data path for the two ML workloads (both MAP-only, like FINRA): node 0
# mmaps + reg_mr's the samples CSV ONCE as W newline-aligned shards (the "publish"); every
# worker RDMA-READs its shard straight out of that memory — no KVS upload, no per-worker GET
# — then runs the SAME integer kernel (sgd_core.grad_sum / infer_core.evaluate) as the
# Cloudburst/Faasm bars. Only the *transfer* differs.
#
#   --mode train : workers return [C,F] i64 gradients (base64); driver Σ → apply_update →
#                  weight_checksum (gate 1232) + accuracy (on node 0's full X,y).
#   --mode infer : workers return (correct,total,predsum); driver Σ → accuracy + prediction
#                  checksum (gate 18,633,154 with the regenerated model; self-consistent).
#
# Metrics match ../k8s/cloudburst + .../{wasmem,faasm}: makespan = per-rep wall (published
# ONCE → no per-rep upload); total_job = Σ worker busy (RDMA read + parse + compute).
import argparse
import base64
import os
import statistics
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sgd_core
import infer_core

HERE = os.path.dirname(os.path.abspath(__file__))


def run_node(node, self_host, mode, server_ip, port, W, idxs, model_path):
    """Launch ALL of this node's workers in ONE SSH (batched), gather RESULT lines.
    Worker args: <mode> <idx> <S=W> <server> <port> [model]."""
    ids = ' '.join(str(i) for i in idxs)
    tail = ('%d %s %d %s' % (W, server_ip, port, model_path)) if mode == 'infer' \
        else ('%d %s %d' % (W, server_ip, port))
    fmt = ("for i in {ids}; do RDMA_TS='{ts}' python3 '{wk}' {mode} $i {tail} "
           ">/tmp/ml_$i.out 2>/tmp/ml_$i.err & done; wait; "
           "for i in {ids}; do cat /tmp/ml_$i.out; done")
    if node != self_host:
        sh = fmt.format(ids=ids, ts='/tmp/rdma_ts', wk='/tmp/ml_worker.py', mode=mode, tail=tail)
        cmd = ['ssh', node, sh]
    else:
        sh = fmt.format(ids=ids, ts=os.path.join(HERE, 'rdma_ts'),
                        wk=os.path.join(HERE, 'ml_worker.py'), mode=mode, tail=tail)
        cmd = ['bash', '-c', sh]
    p = subprocess.run(cmd, capture_output=True, text=True)
    lines = [l for l in p.stdout.splitlines() if l.startswith('RESULT')]
    if len(lines) != len(idxs):
        raise RuntimeError('node %s: %d/%d workers returned. stderr: %s'
                           % (node, len(lines), len(idxs), p.stderr[-400:]))
    return lines


def run_once(a, nodes, W, F, N, X, y, model_local, model_remote):
    by_node = {nd: [] for nd in nodes}
    for i in range(W):
        by_node[nodes[i % len(nodes)]].append(i)
    _mx = max((len(v) for v in by_node.values()), default=0)   # hard 16/node cap
    if _mx > 16:
        raise SystemExit('16/node cap exceeded: %d mappers on one node over %d nodes' % (_mx, len(nodes)))
    t0 = time.time()
    with ThreadPoolExecutor(max_workers=len(nodes)) as pool:
        futs = [pool.submit(run_node, nd, a.self_host, a.mode, a.server_ip, a.port, W,
                            by_node[nd], model_local if nd == a.self_host else model_remote)
                for nd in nodes if by_node[nd]]
        lines = []
        for fut in as_completed(futs):
            lines += fut.result()
    makespan_ms = (time.time() - t0) * 1000.0

    sum_busy = 0.0
    for ln in lines:
        for tok in ln.split():
            if tok.startswith('busy_ms='):
                sum_busy += float(tok.split('=', 1)[1])

    if a.mode == 'train':
        gsum = np.zeros((sgd_core.N_CLASSES, F), dtype=np.int64)
        for ln in lines:
            for tok in ln.split():
                if tok.startswith('grad='):
                    gsum += np.frombuffer(base64.b64decode(tok.split('=', 1)[1]),
                                          dtype=np.int64).reshape(sgd_core.N_CLASSES, F)
        Wt = sgd_core.apply_update(sgd_core.init_weights(F), gsum, N * sgd_core.SGD_LR_K)
        correct, total = sgd_core.accuracy(Wt, X, y)
        return makespan_ms, sum_busy, int(sgd_core.checksum(Wt)), 100.0 * correct / total

    correct = total = gate = 0
    for ln in lines:
        for tok in ln.split():
            if tok.startswith('correct='):
                correct += int(tok.split('=', 1)[1])
            elif tok.startswith('total='):
                total += int(tok.split('=', 1)[1])
            elif tok.startswith('predsum='):
                gate += int(tok.split('=', 1)[1])
    return makespan_ms, sum_busy, gate, (100.0 * correct / total if total else 0.0)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--mode', choices=['train', 'infer'], required=True)
    ap.add_argument('--data', required=True)
    ap.add_argument('--model', default='TestData/ml_inference_model.csv')
    ap.add_argument('--fanout', type=int, default=60)
    ap.add_argument('--reps', type=int, default=15)
    ap.add_argument('--server-ip', default='10.10.1.2')
    ap.add_argument('--port', type=int, default=18515)
    ap.add_argument('--nodes', default='node-0,node-1,node-2,node-3')
    ap.add_argument('--self-host', default='node-0')
    ap.add_argument('--expect', type=int, default=None)
    ap.add_argument('--csv')
    a = ap.parse_args()

    if not os.path.exists(a.data):
        sys.exit('data not found: %s' % a.data)
    nodes = a.nodes.split(',')
    W = a.fanout

    # node 0 loads X,y once (untimed) — N + F for the update, and accuracy for train.
    X, y, F = sgd_core.load_csv(a.data)
    N = X.shape[0]

    # sync binary + worker + cores (+ model for infer) to /tmp on remote nodes.
    model_remote = '/tmp/ml_model.csv'
    model_local = a.model if a.mode == 'infer' else '-'
    for node in nodes:
        if node == a.self_host:
            continue
        for src, dst in [('rdma_ts', 'rdma_ts'), ('ml_worker.py', 'ml_worker.py'),
                         ('sgd_core.py', 'sgd_core.py'), ('infer_core.py', 'infer_core.py')]:
            subprocess.run(['scp', '-q', os.path.join(HERE, src), '%s:/tmp/%s' % (node, dst)], check=True)
        if a.mode == 'infer':
            subprocess.run(['scp', '-q', a.model, '%s:%s' % (node, model_remote)], check=True)

    # --- start the samples RDMA server (publish the CSV as W newline-aligned shards) ---
    srv_log = open('/tmp/rdma_ml_srv.log', 'w')
    srv = subprocess.Popen([os.path.join(HERE, 'rdma_ts'), 'serve', a.data, str(W),
                            a.server_ip, str(a.port)], stderr=srv_log)
    print('[rmmap-rdma-ml:%s] samples=%d F=%d workers=%d publishing...' % (a.mode, N, F, W), flush=True)
    time.sleep(2.0)

    try:
        pub0 = time.time()
        run_node(a.self_host, a.self_host, a.mode, a.server_ip, a.port, W, [0], model_local)
        publish_ms = (time.time() - pub0) * 1000.0
        print('[rmmap-rdma-ml:%s] published: warmup=%.0f ms' % (a.mode, publish_ms), flush=True)

        mk, tj = [], []
        gate = acc = None
        success = True
        for r in range(a.reps):
            m, j, g, ac = run_once(a, nodes, W, F, N, X, y, model_local, model_remote)
            mk.append(m); tj.append(j)
            if gate is None:
                gate, acc = g, ac
            elif g != gate:
                print('[rmmap-rdma-ml:%s] GATE FAIL rep%d: %d != %d' % (a.mode, r, g, gate)); success = False
            print('[rmmap-rdma-ml:%s] rep%d makespan=%.0f ms total_job=%.0f ms gate=%d acc=%.2f%%' %
                  (a.mode, r, m, j, g, ac), flush=True)
    finally:
        srv.terminate(); srv_log.close()

    if a.expect is not None and gate != a.expect:
        print('[rmmap-rdma-ml:%s] GATE FAIL: %d != expected %d' % (a.mode, gate, a.expect)); success = False
    mk_m = statistics.mean(mk); mk_s = statistics.pstdev(mk) if len(mk) > 1 else 0.0
    tj_m = statistics.mean(tj)
    label = 'weight_checksum' if a.mode == 'train' else 'prediction_checksum'
    print('[rmmap-rdma-ml:%s] === samples=%d workers=%d makespan=%.0f ± %.1f ms total_job=%.0f ms '
          'acc=%.2f%% %s=%d %s ===' %
          (a.mode, N, W, mk_m, mk_s, tj_m, acc, label, gate, 'OK' if success else 'GATE-FAIL'))

    if a.csv:
        os.makedirs(os.path.dirname(os.path.abspath(a.csv)), exist_ok=True)
        new = not os.path.exists(a.csv)
        with open(a.csv, 'a') as f:
            if new:
                f.write('samples,workers,nodes_used,variant,makespan_mean_ms,makespan_std_ms,'
                        'total_job_mean_ms,accuracy_pct,%s,expect,success,reps,'
                        'cold_start_ms,cold_makespan_ms\n' % label)
            # cold_start_ms = one-time RDMA MR publish (RMMap cold start); cold_makespan = warm + publish.
            f.write('%d,%d,%d,rdma,%.0f,%.1f,%.0f,%.2f,%d,%d,%s,%d,%.0f,%.0f\n' %
                    (N, W, len(nodes), mk_m, mk_s, tj_m, round(acc, 2), gate,
                     a.expect if a.expect is not None else gate, success, a.reps,
                     publish_ms, mk_m + publish_ms))
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
