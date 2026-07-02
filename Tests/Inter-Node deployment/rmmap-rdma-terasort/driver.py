#!/usr/bin/env python3
# driver.py — RMMap (RDMA) inter-node TeraSort driver (runs on node 0).
#
# The faithful realization of RMMap/DMERGE's data path for TeraSort's all-to-all shuffle,
# WITHOUT the MITOSIS kernel module (which won't build here): user-space one-sided RDMA
# READ over the RoCE NIC for BOTH the input read AND the shuffle. Two waves:
#
#   publish (once): node 0 mmaps + reg_mr's the records corpus — the input "publish".
#   per rep:
#     1. partition wave — N partitioners (SSH fan-out, one per node): each RDMA-READs its
#        input chunk straight out of node 0's memory, range-partitions into N owner buckets,
#        and PUBLISHES them (writes a bucket file + reg_mr via `rdma_ts serve_idx`) on its
#        own node. The shuffle SCATTER, register-once-per-rep — no KVS bucket SET.
#     2. merge wave — N mergers (SSH fan-out): each RDMA-READs its owner column (bucket[j]
#        from every partitioner's published memory), then SORTS with the same ts_ops body.
#        The shuffle GATHER — no KVS bucket GET, the NIC DMAs each column in.
#     3. aggregate the per-owner summaries → the fan-out-invariant gate; tear down the
#        partitioner servers (buckets are re-published next rep, matching the KVS bars that
#        re-scatter each rep).
#
# Metrics match ../k8s/cloudburst + .../Inter-Node Application_Benchmark/{wasmem,faasm}:
#   makespan_ms  = per-rep wall of partition→merge (steady-state; the input corpus is
#                  published ONCE, so per rep has no input upload — the register-once/
#                  read-many DMERGE win the KVS bars don't get for the input read).
#   total_job_ms = Σ partition busy (RDMA read + partition) + Σ merge busy (RDMA gather +
#                  sort). The one-time input publish is reported separately (publish_once).
#   gate = (records, keysum, sorted), self-consistent across reps (== the Cloudburst run).
#
# Deployment note (see README + WordCount harness): partitioners/mergers run as host
# processes over SSH (how RDMA jobs run), not pods — exposing the RDMA device into
# containers needs a device plugin. The transfer mechanism under study is unchanged.
import argparse
import os
import statistics
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed

HERE = os.path.dirname(os.path.abspath(__file__))

# RoCE fabric IP per node (eno1d1 = mlx4_0 port 2). node 0 = 10.10.1.2 (the input server).
NODE_IP = {'node-0': '10.10.1.2', 'node-1': '10.10.1.1',
           'node-2': '10.10.1.4', 'node-3': '10.10.1.3',
           'node-4': '10.10.1.8', 'node-5': '10.10.1.7',
           'node-6': '10.10.1.6', 'node-7': '10.10.1.5',
           'node-8': '10.10.1.9'}


def ssh_or_local(node, self_host, remote_cmd, local_cmd):
    """Build an argv: run locally on node 0 (HERE paths, has a space), else SSH."""
    if node == self_host:
        return ['bash', '-c', local_cmd]
    return ['ssh', node, remote_cmd]


def launch_partitioner(node, self_host, server_ip, input_port, idx, n, bind_ip, serve_port):
    """Start partitioner idx on `node`; return its parsed PART_READY tokens."""
    args = '%s %d %d %d %s %d' % (server_ip, input_port, idx, n, bind_ip, serve_port)
    remote = ("RDMA_TS=/tmp/rdma_ts python3 /tmp/ts_partitioner.py " + args)
    local = ("RDMA_TS='%s' python3 '%s' %s"
             % (os.path.join(HERE, 'rdma_ts'), os.path.join(HERE, 'partitioner.py'), args))
    p = subprocess.run(ssh_or_local(node, self_host, remote, local),
                       capture_output=True, text=True)
    line = next((l for l in p.stdout.splitlines() if l.startswith('PART_READY')), None)
    if line is None:
        raise RuntimeError('partitioner %d on %s failed.\nstdout:%s\nstderr:%s'
                           % (idx, node, p.stdout[-300:], p.stderr[-500:]))
    return {k: v for k, v in (t.split('=', 1) for t in line.split()[1:])}


def launch_merger(node, self_host, j, n, endpoints):
    """Start merger j on `node` reading the N partitioner endpoints; parse its RESULT."""
    eps = ' '.join('%s:%d' % ep for ep in endpoints)
    args = '%d %d %s' % (j, n, eps)
    remote = ("RDMA_TS=/tmp/rdma_ts python3 /tmp/ts_merger.py " + args)
    local = ("RDMA_TS='%s' python3 '%s' %s"
             % (os.path.join(HERE, 'rdma_ts'), os.path.join(HERE, 'merger.py'), args))
    p = subprocess.run(ssh_or_local(node, self_host, remote, local),
                       capture_output=True, text=True)
    line = next((l for l in p.stdout.splitlines() if l.startswith('RESULT')), None)
    if line is None:
        raise RuntimeError('merger %d on %s failed.\nstdout:%s\nstderr:%s'
                           % (j, node, p.stdout[-300:], p.stderr[-500:]))
    return {k: v for k, v in (t.split('=', 1) for t in line.split()[1:])}


def kill_servers(nodes, self_host, n):
    """Tear down the per-rep partitioner serve_idx servers (buckets re-published next rep)."""
    for i in range(n):
        node = nodes[i % len(nodes)]
        cmd = 'kill $(cat /tmp/ts_srv_%d.pid) 2>/dev/null; rm -f /tmp/ts_srv_%d.pid' % (i, i)
        subprocess.run(ssh_or_local(node, self_host, cmd, cmd), capture_output=True)


def run_once(server_ip, input_port, n, nodes, self_host, serve_base):
    # partitioner i and merger i live on nodes[i % len]; partitioner i serves on serve_base+i.
    endpoints = [(NODE_IP[nodes[i % len(nodes)]], serve_base + i) for i in range(n)]
    t0 = time.time()
    # --- partition wave (scatter): RDMA-read input chunk, partition, publish buckets ---
    pbusy = 0.0
    with ThreadPoolExecutor(max_workers=n) as pool:
        futs = {pool.submit(launch_partitioner, nodes[i % len(nodes)], self_host,
                            server_ip, input_port, i, n, endpoints[i][0], endpoints[i][1]): i
                for i in range(n)}
        for fut in as_completed(futs):
            pbusy += float(fut.result()['busy_ms'])

    # --- merge wave (gather + sort): RDMA-read each owner column from all partitioners ---
    mbusy = 0.0
    records = keysum = 0
    allsorted = True
    ranges = {}                       # j -> (first_hex, last_hex) for the global-order check
    with ThreadPoolExecutor(max_workers=n) as pool:
        futs = {pool.submit(launch_merger, nodes[j % len(nodes)], self_host, j, n,
                            endpoints): j for j in range(n)}
        for fut in as_completed(futs):
            r = fut.result()
            j = int(r['idx'])
            mbusy += float(r['busy_ms'])
            records += int(r['records'])
            keysum += int(r['keysum'])
            if r['sorted'] != '1':
                allsorted = False
            if int(r['records']) > 0:
                ranges[j] = (r['first'], r['last'])
    makespan_ms = (time.time() - t0) * 1000.0
    kill_servers(nodes, self_host, n)

    # global cross-range order: sort owners by their first key, check no overlap.
    ordered, prev_last = True, None
    for j in sorted(ranges, key=lambda k: bytes.fromhex(ranges[k][0])):
        first, last = (bytes.fromhex(x) for x in ranges[j])
        if prev_last is not None and first < prev_last:
            ordered = False
        prev_last = last
    srt = 1 if (allsorted and ordered) else 0
    total_job_ms = pbusy + mbusy
    return makespan_ms, total_job_ms, records, keysum, srt


def sync_files(nodes, self_host):
    """Push the binary + Python workers to /tmp on every remote node (node 0 runs local)."""
    files = [('rdma_ts', 'rdma_ts'), ('partitioner.py', 'ts_partitioner.py'),
             ('merger.py', 'ts_merger.py'), ('ts_ops.py', 'ts_ops.py')]
    for node in nodes:
        if node == self_host:
            continue
        for src, dst in files:
            subprocess.run(['scp', '-q', os.path.join(HERE, src), '%s:/tmp/%s' % (node, dst)],
                           check=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--records', default='TestData/terasort_1.2gb.txt')
    ap.add_argument('--fanout', type=int, default=4, help='shuffle width (N partitioners = N mergers = N range owners)')
    ap.add_argument('--reps', type=int, default=15)
    ap.add_argument('--server-ip', default='10.10.1.2', help='node 0 RoCE IP (input server)')
    ap.add_argument('--input-port', type=int, default=18515)
    ap.add_argument('--serve-base', type=int, default=18601, help='partitioner i serves on serve_base+i')
    ap.add_argument('--nodes', default='node-0,node-1,node-2,node-3')
    ap.add_argument('--self-host', default='node-0')
    ap.add_argument('--expect', default=None, help="gate 'records,keysum,sorted'")
    ap.add_argument('--csv')
    a = ap.parse_args()

    if not os.path.exists(a.records):
        sys.exit('records not found: %s' % a.records)
    expect = None
    if a.expect is not None:
        expect = tuple(int(x) for x in a.expect.split(','))
        if len(expect) != 3:
            sys.exit("--expect must be 'records,keysum,sorted'")
    nodes = a.nodes.split(',')
    size_mb = round(os.path.getsize(a.records) / (1024 * 1024))
    sync_files(nodes, a.self_host)

    # --- start the input RDMA server (publish the records corpus once) ---
    srv_log = open('/tmp/rdma_ts_srv.log', 'w')
    srv = subprocess.Popen([os.path.join(HERE, 'rdma_ts'), 'serve', a.records,
                            str(a.fanout), a.server_ip, str(a.input_port)], stderr=srv_log)
    print('[rmmap-rdma-ts] input server starting (records=%s %d MB fanout=%d)...' %
          (a.records, size_mb, a.fanout), flush=True)
    time.sleep(2.0)

    try:
        # pre-warm: one partitioner read triggers the one-time input MR registration.
        pub0 = time.time()
        launch_partitioner(a.self_host, a.self_host, a.server_ip, a.input_port, 0,
                           a.fanout, NODE_IP[a.self_host], a.serve_base)
        publish_ms = (time.time() - pub0) * 1000.0
        kill_servers(nodes, a.self_host, a.fanout)     # drop the warmup partitioner's server
        print('[rmmap-rdma-ts] published input: warmup=%.0f ms' % publish_ms, flush=True)

        mk_ms, tj_ms = [], []
        gate_seen = None
        success = True
        for r in range(a.reps):
            m, tj, rec, ks, srt = run_once(a.server_ip, a.input_port, a.fanout, nodes,
                                           a.self_host, a.serve_base)
            mk_ms.append(m)
            tj_ms.append(tj)
            g = (rec, ks, srt)
            if gate_seen is None:
                gate_seen = g
            elif g != gate_seen:
                print('[rmmap-rdma-ts] GATE FAIL rep%d: %s != %s' % (r, g, gate_seen))
                success = False
            if srt != 1:
                print('[rmmap-rdma-ts] SORT FAIL rep%d' % r)
                success = False
            print('[rmmap-rdma-ts] rep%d makespan=%.0f ms total_job=%.0f ms records=%d keysum=%d sorted=%d' %
                  (r, m, tj, rec, ks, srt), flush=True)
    finally:
        srv.terminate()
        srv_log.close()

    if expect is not None and gate_seen != expect:
        print('[rmmap-rdma-ts] GATE FAIL: %s != expected %s' % (gate_seen, expect))
        success = False

    mk_mean = statistics.mean(mk_ms)
    mk_std = statistics.pstdev(mk_ms) if len(mk_ms) > 1 else 0.0
    tj_mean = statistics.mean(tj_ms)
    rec, ks, srt = gate_seen
    print('[rmmap-rdma-ts] === size=%d MB fanout=%d makespan=%.0f ± %.1f ms total_job=%.0f ms '
          'publish_once=%.0f ms records=%d keysum=%d sorted=%d %s ===' %
          (size_mb, a.fanout, mk_mean, mk_std, tj_mean, publish_ms, rec, ks, srt,
           'OK' if success else 'GATE-FAIL'))

    if a.csv:
        os.makedirs(os.path.dirname(os.path.abspath(a.csv)), exist_ok=True)
        new = not os.path.exists(a.csv)
        exp_str = '|'.join(str(x) for x in (expect if expect is not None else gate_seen))
        with open(a.csv, 'a') as f:
            if new:
                f.write('size_mb,workers,nodes_used,variant,makespan_mean_ms,makespan_std_ms,'
                        'total_job_mean_ms,records,keysum,sorted,expect,success,reps,'
                        'cold_start_ms,cold_makespan_ms\n')
            # cold_start_ms = one-time RDMA MR publish (RMMap cold start); cold_makespan = warm + publish.
            f.write('%d,%d,%d,rdma,%.0f,%.1f,%.0f,%d,%d,%d,%s,%s,%d,%.0f,%.0f\n' %
                    (size_mb, a.fanout, len(nodes), mk_mean, mk_std, tj_mean, rec, ks, srt,
                     exp_str, success, a.reps,
                     publish_ms, mk_mean + publish_ms))
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
