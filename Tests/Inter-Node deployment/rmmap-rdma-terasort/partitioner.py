#!/usr/bin/env python3
# partitioner.py — worker-side RMMap (RDMA) TeraSort PARTITIONER (the shuffle scatter).
# Runs on whichever node the driver SSHes into. It:
#   1. acquires its input chunk by ONE-SIDED RDMA READ straight out of node 0's registered
#      records memory (no KVS, no serialize) — `rdma_ts read <node0> <port> <i>`;
#   2. range-partitions every record into N owner buckets with the SAME ts_ops.ts_partition
#      as the Cloudburst/Faasm bars (only the transfer differs);
#   3. PUBLISHES its buckets for the shuffle: writes them concatenated to a local file with
#      a bounds sidecar, then starts a detached `rdma_ts serve_idx` server (registers the
#      bucket file as ONE remote-read MR). Every merger then RDMA-READs its column out of
#      this memory — the serialization-free shuffle (vs KVS bucket SET/GET).
#
# Out (stdout, one line): "PART_READY idx=.. busy_ms=.. transfer_ms=.. part_ms=.. records=..
#                          ip=.. port=.. pid=.."  (busy_ms = RDMA read + partition compute;
# the serve_idx process keeps running after this script exits, until the driver kills its pid).
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ts_ops

RDMA_TS = os.environ.get('RDMA_TS', '/tmp/rdma_ts')


def main():
    server, port, idx, n, bind_ip, serve_port = (
        sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]),
        sys.argv[5], sys.argv[6])

    # @transfer: one-sided RDMA READ of this partitioner's input chunk from node 0.
    t0 = time.time()
    p = subprocess.run([RDMA_TS, 'read', server, port, str(idx)], capture_output=True)
    transfer_ms = (time.time() - t0) * 1000.0
    if p.returncode != 0:
        sys.stderr.write('partitioner idx=%d read failed: %s\n'
                         % (idx, p.stderr.decode('utf-8', 'ignore')))
        sys.exit(1)
    chunk = p.stdout

    # @execute: identical range-partition body to the Cloudburst/Faasm bars.
    t1 = time.time()
    buckets = ts_ops.ts_partition(chunk, n)
    part_ms = (time.time() - t1) * 1000.0
    records = sum(b.count(b'\n') + (1 if b else 0) for b in buckets)

    # publish: concat the N bucket blobs into one file + an N+1 bounds sidecar, so a merger
    # can RDMA-READ bucket[j] = [bounds[j], bounds[j+1]) straight out of this memory.
    blob = b''.join(buckets)
    bounds, off = [0], 0
    for b in buckets:
        off += len(b)
        bounds.append(off)
    bfile = '/tmp/ts_bucket_%d.dat' % idx
    bndfile = '/tmp/ts_bucket_%d.bnd' % idx
    with open(bfile, 'wb') as f:
        f.write(blob)
    with open(bndfile, 'w') as f:
        f.write('\n'.join(str(x) for x in bounds) + '\n')

    # start the detached serve_idx server (survives this SSH session; driver kills its pid).
    log = open('/tmp/ts_srv_%d.log' % idx, 'w')
    srv = subprocess.Popen([RDMA_TS, 'serve_idx', bfile, bndfile, bind_ip, serve_port],
                           stdout=subprocess.DEVNULL, stderr=log,
                           start_new_session=True)
    with open('/tmp/ts_srv_%d.pid' % idx, 'w') as f:
        f.write(str(srv.pid))
    time.sleep(0.5)            # let bind+listen come up before the driver releases mergers

    busy_ms = transfer_ms + part_ms
    sys.stdout.write('PART_READY idx=%d busy_ms=%.3f transfer_ms=%.3f part_ms=%.3f '
                     'records=%d ip=%s port=%s pid=%d\n' %
                     (idx, busy_ms, transfer_ms, part_ms, records, bind_ip, serve_port,
                      srv.pid))


if __name__ == '__main__':
    main()
