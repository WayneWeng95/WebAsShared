#!/usr/bin/env python3
# finra_worker.py — worker-side RMMap (RDMA) FINRA auditor. Runs on whichever node the
# driver SSHes into. It acquires its trades by ONE-SIDED RDMA READ straight out of node 0's
# registered trades memory (no KVS, no serialize), then counts one audit rule with the SAME
# finra_rules.audit_rule as the Cloudburst/Faasm bars — only the *transfer* differs.
#
# node 0 serves the trades corpus as S newline-aligned chunks (`rdma_ts serve ... S ...`):
#   stateless rule (sharded): read chunk[idx] = its 1/S slice → audit. Only chunk 0 carries
#                             the CSV header, so skip_header = (idx == 0).
#   stateful  rule (full):    read ALL S chunks, concatenate = the whole corpus → audit
#                             (its cross-(account,symbol) state needs every record).
#
# Args: <rule> <mode> <idx> <S> <server_ip> <port>   mode = "shard" | "full"
# Out (stdout): "RESULT rule=.. idx=.. busy_ms=.. transfer_ms=.. audit_ms=.. trades=.. viol=.."
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import finra_rules

RDMA_TS = os.environ.get('RDMA_TS', '/tmp/rdma_ts')


def rdma_read(server, port, idx):
    p = subprocess.run([RDMA_TS, 'read', server, str(port), str(idx)], capture_output=True)
    if p.returncode != 0:
        sys.stderr.write('read idx=%s failed: %s\n' % (idx, p.stderr.decode('utf-8', 'ignore')))
        sys.exit(1)
    return p.stdout


def main():
    rule, mode, idx, S, server, port = (int(sys.argv[1]), sys.argv[2], int(sys.argv[3]),
                                        int(sys.argv[4]), sys.argv[5], sys.argv[6])
    # @transfer: one-sided RDMA READ of the trades this worker needs.
    t0 = time.time()
    if mode == 'full':
        data = b''.join(rdma_read(server, port, c) for c in range(S))  # reassemble full corpus
        skip_header = True
    else:
        data = rdma_read(server, port, idx)                            # this rule's 1/S slice
        skip_header = (idx == 0)                                       # only slice 0 has the header
    transfer_ms = (time.time() - t0) * 1000.0

    # @execute: identical parse + audit body to the Cloudburst/Faasm bars.
    t1 = time.time()
    trades = finra_rules.parse_trades(data, skip_header=skip_header)
    v = finra_rules.audit_rule(trades, rule)
    audit_ms = (time.time() - t1) * 1000.0

    busy_ms = transfer_ms + audit_ms
    sys.stdout.write('RESULT rule=%d idx=%d busy_ms=%.3f transfer_ms=%.3f audit_ms=%.3f '
                     'trades=%d viol=%d\n' % (rule, idx, busy_ms, transfer_ms, audit_ms,
                                              len(trades), v))


if __name__ == '__main__':
    main()
