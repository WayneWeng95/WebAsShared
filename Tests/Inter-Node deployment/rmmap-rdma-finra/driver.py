#!/usr/bin/env python3
# driver.py — RMMap (RDMA) inter-node FINRA driver (runs on node 0).
#
# The faithful RMMap data path for FINRA (a MAP-only audit fan, no shuffle), WITHOUT the
# MITOSIS kernel module: user-space one-sided RDMA READ over the RoCE NIC. node 0 mmaps +
# reg_mr's the trades corpus ONCE as S newline-aligned chunks (the "publish"); every audit
# worker RDMA-READs the trades it needs straight out of that memory — no KVS upload, no
# per-worker GET — then counts with the SAME finra_rules.audit_rule as every other bar.
#
#   - 5 STATELESS rules (0,1,5,6,7): each sharded into S workers; worker (rule, s) RDMA-READs
#     chunk[s] (its 1/S slice) and counts that rule. Per-shard counts SUM exactly.
#   - 3 STATEFUL rules (2,3,4): one full-data worker each; it RDMA-READs ALL S chunks,
#     concatenates = the whole corpus, and counts. Effective fan = 3 + 5·S (F=60 → S=11 → 58).
#
# Metrics match ../k8s/cloudburst + .../Inter-Node Application_Benchmark/{wasmem,faasm}:
#   makespan_ms  = per-rep wall of the fan-out. The corpus is published ONCE → no per-rep
#                  upload (the register-once/read-many win the KVS bars don't get).
#   total_job_ms = Σ worker busy (RDMA read + parse + audit). One-time publish reported
#                  separately (publish_once). gate = total violations (= 2,271,415).
#
# Deployment note (see README): workers run as host processes over SSH (how RDMA jobs run),
# not pods. The transfer mechanism under study is unchanged.
import argparse
import os
import statistics
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import finra_rules

HERE = os.path.dirname(os.path.abspath(__file__))


def build_specs(S):
    """The 5·S stateless + 3 stateful worker specs: (rule, mode, idx)."""
    specs = []
    for rule in finra_rules.STATELESS:
        for s in range(S):
            specs.append((rule, 'shard', s))
    for rule in finra_rules.STATEFUL:
        specs.append((rule, 'full', 0))
    return specs


def run_node(node, self_host, server_ip, port, S, node_specs):
    """Launch ALL of this node's workers in ONE SSH (batched, like the WordCount harness):
    the remote launcher spawns them in parallel, waits, then cats their RESULT lines."""
    # each spec → "<rule> <mode> <idx>"; the launcher loop indexes them as $k for the temp files.
    spec_args = ' '.join("'%d %s %d'" % (r, m, i) for r, m, i in node_specs)
    launch = (
        "k=0; for spec in %s; do RDMA_TS='%%s' python3 '%%s' $spec %d %s %d "
        ">/tmp/finra_$k.out 2>/tmp/finra_$k.err & k=$((k+1)); done; wait; "
        "k=0; for spec in %s; do cat /tmp/finra_$k.out; k=$((k+1)); done"
        % (spec_args, S, server_ip, port, spec_args))
    if node != self_host:
        cmd = ['ssh', node, launch % ('/tmp/rdma_ts', '/tmp/finra_worker.py')]
    else:
        cmd = ['bash', '-c', launch % (os.path.join(HERE, 'rdma_ts'),
                                       os.path.join(HERE, 'finra_worker.py'))]
    p = subprocess.run(cmd, capture_output=True, text=True)
    lines = [l for l in p.stdout.splitlines() if l.startswith('RESULT')]
    if len(lines) != len(node_specs):
        raise RuntimeError('node %s: %d/%d workers returned. stderr: %s'
                           % (node, len(lines), len(node_specs), p.stderr[-400:]))
    return lines


def run_once(server_ip, port, S, nodes, self_host):
    specs = build_specs(S)
    by_node = {nd: [] for nd in nodes}
    for k, spec in enumerate(specs):                 # round-robin specs to nodes
        by_node[nodes[k % len(nodes)]].append(spec)
    _mx = max((len(v) for v in by_node.values()), default=0)   # hard 16/node cap
    if _mx > 16:
        raise SystemExit('16/node cap exceeded: %d mappers on one node over %d nodes' % (_mx, len(nodes)))
    t0 = time.time()
    with ThreadPoolExecutor(max_workers=len(nodes)) as pool:
        futs = [pool.submit(run_node, nd, self_host, server_ip, port, S, by_node[nd])
                for nd in nodes if by_node[nd]]
        lines = []
        for fut in as_completed(futs):
            lines += fut.result()
    makespan_ms = (time.time() - t0) * 1000.0
    sum_busy, total_viol = 0.0, 0
    for ln in lines:
        for tok in ln.split():
            if tok.startswith('busy_ms='):
                sum_busy += float(tok.split('=', 1)[1])
            elif tok.startswith('viol='):
                total_viol += int(tok.split('=', 1)[1])
    return makespan_ms, sum_busy, total_viol, len(specs)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--trades', default='TestData/finra_5m.csv')
    ap.add_argument('--fanout', type=int, default=60, help='audit-rule fan width (F → S=round((F-3)/5))')
    ap.add_argument('--reps', type=int, default=15)
    ap.add_argument('--server-ip', default='10.10.1.2')
    ap.add_argument('--port', type=int, default=18515)
    ap.add_argument('--nodes', default='node-0,node-1,node-2,node-3')
    ap.add_argument('--self-host', default='node-0')
    ap.add_argument('--expect', type=int, default=None)
    ap.add_argument('--csv')
    a = ap.parse_args()

    if not os.path.exists(a.trades):
        sys.exit('trades not found: %s' % a.trades)
    nodes = a.nodes.split(',')
    n_trades = max(0, open(a.trades, 'rb').read(-1).count(b'\n') - 1) if os.path.getsize(a.trades) < 5e8 \
        else None
    # cheaper: count via wc -l once (avoid re-reading a big file into the driver)
    if n_trades is None:
        n_trades = max(0, int(subprocess.run(['wc', '-l', a.trades], capture_output=True,
                                             text=True).stdout.split()[0]) - 1)
    S = max(1, round((a.fanout - len(finra_rules.STATEFUL)) / len(finra_rules.STATELESS)))
    n_workers = len(finra_rules.STATELESS) * S + len(finra_rules.STATEFUL)

    # sync binary + worker + rules to /tmp on remote nodes (node 0 runs local).
    for node in nodes:
        if node == a.self_host:
            continue
        for src, dst in [('rdma_ts', 'rdma_ts'), ('finra_worker.py', 'finra_worker.py'),
                         ('finra_rules.py', 'finra_rules.py')]:
            subprocess.run(['scp', '-q', os.path.join(HERE, src), '%s:/tmp/%s' % (node, dst)],
                           check=True)

    # --- start the trades RDMA server (publish the corpus as S chunks) ---
    srv_log = open('/tmp/rdma_finra_srv.log', 'w')
    srv = subprocess.Popen([os.path.join(HERE, 'rdma_ts'), 'serve', a.trades, str(S),
                            a.server_ip, str(a.port)], stderr=srv_log)
    print('[rmmap-rdma-finra] trades server starting (%d trades, S=%d chunks, %d workers)...' %
          (n_trades, S, n_workers), flush=True)
    time.sleep(2.0)

    try:
        # pre-warm: one read triggers the one-time MR registration (publish).
        pub0 = time.time()
        run_node(a.self_host, a.self_host, a.server_ip, a.port, S, [(0, 'shard', 0)])
        publish_ms = (time.time() - pub0) * 1000.0
        print('[rmmap-rdma-finra] published trades: warmup=%.0f ms' % publish_ms, flush=True)

        mk_ms, tj_ms = [], []
        viol = None
        success = True
        for r in range(a.reps):
            m, tj, v, w = run_once(a.server_ip, a.port, S, nodes, a.self_host)
            mk_ms.append(m)
            tj_ms.append(tj)
            if viol is None:
                viol = v
            elif v != viol:
                print('[rmmap-rdma-finra] GATE FAIL rep%d: %d != %d' % (r, v, viol))
                success = False
            print('[rmmap-rdma-finra] rep%d makespan=%.0f ms total_job=%.0f ms violations=%d workers=%d' %
                  (r, m, tj, v, w), flush=True)
    finally:
        srv.terminate()
        srv_log.close()

    if a.expect is not None and viol != a.expect:
        print('[rmmap-rdma-finra] GATE FAIL: %d != expected %d' % (viol, a.expect))
        success = False

    mk_mean = statistics.mean(mk_ms)
    mk_std = statistics.pstdev(mk_ms) if len(mk_ms) > 1 else 0.0
    tj_mean = statistics.mean(tj_ms)
    print('[rmmap-rdma-finra] === trades=%d workers=%d makespan=%.0f ± %.1f ms total_job=%.0f ms '
          'publish_once=%.0f ms violations=%d %s ===' %
          (n_trades, n_workers, mk_mean, mk_std, tj_mean, publish_ms, viol,
           'OK' if success else 'GATE-FAIL'))

    if a.csv:
        os.makedirs(os.path.dirname(os.path.abspath(a.csv)), exist_ok=True)
        new = not os.path.exists(a.csv)
        with open(a.csv, 'a') as f:
            if new:
                f.write('trades,fanout,nodes_used,variant,makespan_mean_ms,makespan_std_ms,'
                        'total_job_mean_ms,violations,expect,success,reps,'
                        'cold_start_ms,cold_makespan_ms\n')
            # cold_start_ms = one-time RDMA MR publish (RMMap cold start); cold_makespan = warm + publish.
            f.write('%d,%d,%d,rdma,%.0f,%.1f,%.0f,%d,%d,%s,%d,%.0f,%.0f\n' %
                    (n_trades, n_workers, len(nodes), mk_mean, mk_std, tj_mean, viol,
                     a.expect if a.expect is not None else viol, success, a.reps,
                     publish_ms, mk_mean + publish_ms))
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
