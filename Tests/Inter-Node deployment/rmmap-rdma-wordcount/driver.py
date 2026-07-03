#!/usr/bin/env python3
# driver.py — RMMap (RDMA) inter-node WordCount driver (runs on node 0).
#
# The faithful realization of RMMap/DMERGE's data path on this cluster WITHOUT the
# MITOSIS kernel module (which won't build here): user-space one-sided RDMA READ over
# the RoCE NIC. node 0 mmaps the corpus and registers it ONCE as an RDMA MR (the
# "publish"); 60 mapper processes spread across the 4 nodes each RDMA-READ their
# newline-aligned chunk directly out of that memory — no serialize, no Redis staging,
# the NIC DMAs the bytes into the mapper — then count with the SAME wc_ops.wc_map as
# every other bar. Only the *transfer* differs from the RMMap-ES bar.
#
# Metrics match ../k8s/cloudburst + .../Inter-Node Application_Benchmark/{wasmem,faasm}:
#   makespan_ms   = per-rep wall time of the fan-out + reduce (steady-state latency;
#                   the corpus is published once, so per-rep has no upload — the
#                   register-once/read-many DMERGE win the KVS bars don't get).
#   total_job_ms  = Σ mapper busy_ms (RDMA transfer + count) + reduce(node0). The
#                   one-time publish (mmap+reg_mr) is reported separately (publish_ms),
#                   amortized to ~0 per rep.
#   occurrences   = total word count (gate; == the Cloudburst/RMMap-ES runs = 660,848,961).
#
# Deployment note (see README): mappers run as host processes fanned out over SSH (how
# RDMA jobs run) rather than Knative pods, to avoid exposing the RDMA device into
# containers. The transfer mechanism under study is unchanged.
#
# Usage:
#   ./driver.py --corpus TestData/corpus_4gb.txt --fanout 60 --reps 15 \
#       --server-ip 10.10.1.2 --port 18515 --nodes node-0,node-1,node-2,node-3 \
#       --csv ".../Inter-Node Application_Benchmark/rmmap/results_wordcount.csv"
import argparse
import os
import statistics
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import wc_ops                              # shared reducer body (wc_reduce_lines)


def run_node(node, server_ip, port, idxs, self_host):
    """Launch ALL of this node's mappers in ONE shot (one SSH per node, not per mapper —
    60 concurrent SSH trips sshd MaxStartups). The remote launcher spawns its mappers in
    parallel, then cats their RESULT lines AND their serialized partial records. Returns
    every stdout line (RESULT metrics + "word\\tcount" shuffle payload)."""
    ids = ' '.join(str(i) for i in idxs)
    # NB: single-quote the binary/mapper paths — node 0's repo dir is "Inter-Node
    # deployment" (has a space) and would otherwise break the shell word-splitting.
    launch = ("for i in %s; do RDMA_WC='%%s' python3 '%%s' %s %d $i "
              ">/tmp/rdma_m_$i.out 2>/tmp/rdma_m_$i.err & done; wait; "
              "for i in %s; do cat /tmp/rdma_m_$i.out; done" % (ids, server_ip, port, ids))
    if node != self_host:
        cmd = ['ssh', node, launch % ('/tmp/rdma_wc', '/tmp/rdma_mapper.py')]
        env = os.environ
    else:
        cmd = ['bash', '-c', launch % (os.path.join(HERE, 'rdma_wc'),
                                       os.path.join(HERE, 'rdma_mapper.py'))]
        env = os.environ
    p = subprocess.run(cmd, capture_output=True, text=True, env=env)
    all_lines = p.stdout.splitlines()
    n_result = sum(1 for ln in all_lines if ln.startswith('RESULT'))
    if n_result != len(idxs):
        raise RuntimeError('node %s: %d/%d mappers returned. stderr: %s'
                           % (node, n_result, len(idxs), p.stderr[-300:]))
    # Return BOTH the RESULT metric lines and each mapper's serialized partial records
    # ("word\tcount"); node 0 merges the latter in run_once (the real reduce).
    return all_lines


def run_once(server_ip, port, n, nodes, self_host):
    # round-robin chunk indices to nodes, then one batched launch per node in parallel.
    by_node = {nd: [] for nd in nodes}
    for i in range(n):
        by_node[nodes[i % len(nodes)]].append(i)
    _mx = max((len(v) for v in by_node.values()), default=0)   # hard 16/node cap
    if _mx > 16:
        raise SystemExit('16/node cap exceeded: %d mappers on one node over %d nodes' % (_mx, len(nodes)))
    t0 = time.time()
    with ThreadPoolExecutor(max_workers=len(nodes)) as pool:
        futs = [pool.submit(run_node, nd, server_ip, port, by_node[nd], self_host)
                for nd in nodes if by_node[nd]]
        lines = []
        for fut in as_completed(futs):
            lines += fut.result()

    # --- @shuffle + @reduce (INSIDE the timed window) --------------------------------
    # The mapper partials arrived above over the control channel (the shuffle). Now merge
    # them into the final {word:count} with the SAME reducer body as Cloudburst's
    # wc_reducer and Faasm's wc_reducer — the reduce stage those systems include in their
    # makespan, so RMMap must too. Only RESULT lines carry busy_ms; everything else is a
    # "word\tcount" partial record.
    sum_map_ms = 0.0
    for ln in lines:
        if ln.startswith('RESULT'):
            for tok in ln.split():
                if tok.startswith('busy_ms='):
                    sum_map_ms += float(tok.split('=', 1)[1])
    merged = wc_ops.wc_reduce_lines(
        ln for ln in lines if ln and not ln.startswith('RESULT'))
    total_occ = sum(merged.values())               # gate is now derived from the real merge
    unique = len(merged)
    makespan_ms = (time.time() - t0) * 1000.0      # fan-out + shuffle + reduce
    total_job_ms = sum_map_ms
    return makespan_ms, total_job_ms, total_occ, unique


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--corpus', default='TestData/corpus_4gb.txt')
    ap.add_argument('--fanout', type=int, default=60)
    ap.add_argument('--reps', type=int, default=15)
    ap.add_argument('--server-ip', default='10.10.1.2')
    ap.add_argument('--port', type=int, default=18515)
    ap.add_argument('--nodes', default='node-0,node-1,node-2,node-3')
    ap.add_argument('--self-host', default='node-0', help='this host name in --nodes (run locally)')
    ap.add_argument('--expect', type=int, default=None)
    ap.add_argument('--csv')
    a = ap.parse_args()

    if not os.path.exists(a.corpus):
        sys.exit('corpus not found: %s' % a.corpus)
    nodes = a.nodes.split(',')
    size_mb = round(os.path.getsize(a.corpus) / (1024 * 1024))

    # --- start the RDMA memory server (publish the corpus) ---
    srv_log = open('/tmp/rdma_srv.log', 'w')
    srv = subprocess.Popen([os.path.join(HERE, 'rdma_wc'), 'serve', a.corpus,
                            str(a.fanout), a.server_ip, str(a.port)],
                           stderr=srv_log)
    print('[rmmap-rdma] server starting (corpus=%s %d MB fanout=%d)...' %
          (a.corpus, size_mb, a.fanout), flush=True)
    time.sleep(2.0)                      # let mmap + listen come up

    try:
        # pre-warm: one mapper read triggers the one-time MR registration (publish).
        pub0 = time.time()
        run_node(a.self_host, a.server_ip, a.port, [0], a.self_host)
        publish_ms = (time.time() - pub0) * 1000.0
        reg_ms = None
        for ln in open('/tmp/rdma_srv.log'):
            if ln.startswith('PUBLISH'):
                for tok in ln.split():
                    if tok.startswith('reg_mr_ms='):
                        reg_ms = float(tok.split('=', 1)[1])
        print('[rmmap-rdma] published: warmup=%.0f ms reg_mr=%s ms' %
              (publish_ms, ('%.0f' % reg_ms) if reg_ms is not None else '?'), flush=True)

        mk_ms, tj_ms = [], []
        occ = uniq = None
        success = True
        for r in range(a.reps):
            m, tj, tot, u = run_once(a.server_ip, a.port, a.fanout, nodes, a.self_host)
            mk_ms.append(m)
            tj_ms.append(tj)
            if occ is None:
                occ, uniq = tot, u
            elif tot != occ:
                print('[rmmap-rdma] GATE FAIL rep%d: occ %d != %d' % (r, tot, occ))
                success = False
            print('[rmmap-rdma] rep%d makespan=%.0f ms total_job=%.0f ms occ=%d unique=%d' %
                  (r, m, tj, tot, u), flush=True)
    finally:
        srv.terminate()
        srv_log.close()

    if a.expect is not None and occ != a.expect:
        print('[rmmap-rdma] GATE FAIL: occ %d != expected %d' % (occ, a.expect))
        success = False

    mk_mean = statistics.mean(mk_ms)
    mk_std = statistics.pstdev(mk_ms) if len(mk_ms) > 1 else 0.0
    tj_mean = statistics.mean(tj_ms)
    print('[rmmap-rdma] === size=%d MB fanout=%d makespan=%.0f ± %.1f ms total_job=%.0f ms '
          'publish_once=%.0f ms occ=%d %s ===' %
          (size_mb, a.fanout, mk_mean, mk_std, tj_mean, publish_ms, occ,
           'OK' if success else 'GATE-FAIL'))

    if a.csv:
        os.makedirs(os.path.dirname(os.path.abspath(a.csv)), exist_ok=True)
        new = not os.path.exists(a.csv)
        with open(a.csv, 'a') as f:
            if new:
                f.write('size_mb,mappers,nodes_used,variant,makespan_mean_ms,makespan_std_ms,'
                        'total_job_mean_ms,occurrences,expect,success,reps,'
                        'cold_start_ms,cold_makespan_ms\n')
            # cold_start_ms = the one-time RDMA MR publish/reg_mr (RMMap's cold start, the
            # analogue of Cloudburst's on-wave pod launch); cold_makespan = warm + that publish.
            f.write('%d,%d,%d,rdma,%.0f,%.1f,%.0f,%d,%s,%s,%d,%.0f,%.0f\n' %
                    (size_mb, a.fanout, len(nodes), mk_mean, mk_std, tj_mean, occ,
                     a.expect if a.expect is not None else occ, success, a.reps,
                     publish_ms, mk_mean + publish_ms))
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
