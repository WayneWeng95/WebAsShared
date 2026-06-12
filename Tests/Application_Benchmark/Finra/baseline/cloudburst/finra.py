#  FINRA audit — map-reduce-shaped DAG on Cloudburst (Redis-backed runner).
#
#      fetch ──► rule_0 ──┐
#            ├─► rule_1 ──┤
#            ├─►  ...     ├─► merge   (sum of per-rule violation counts)
#            └─► rule_7 ──┘
#
#  - fetch    : parse the trades CSV → list of trade dicts; returns it (flows to
#               every rule — a BROADCAST: all 8 rules read the same trades).
#  - rule_i   : run audit rule i over the trades → violation count (int).
#  - merge    : sum the 8 counts → total_violations.
#
#  The trades crossing fetch → each rule are the inter-stage state Cloudburst
#  serializes through its KVS (Redis here), 8× (the broadcast) — the cost
#  WebAsShared's zero-copy page-chain avoids. Same 8-rule spec as finra.rs, so
#  total_violations matches our native system exactly.

import os
import sys
import time
import uuid

from cloudburst.shared.reference import CloudburstReference

# Shared rule logic lives in the Finra benchmark root.
sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..', '..'))
import finra_rules as fr   # noqa: E402

CORPUS_PATH = os.environ.get('WC_CORPUS', '/tmp/trades.csv')


def run(cloudburst_client, num_requests, sckt):
    def fetch(cloudburst, text):
        if isinstance(text, bytes):
            text = text.decode('utf-8', 'ignore')
        return fr.parse_trades(text)

    def make_rule(rid):
        def rule(cloudburst, trades):
            return fr.audit_rule(trades, rid)
        return rule

    def merge(cloudburst, *counts):
        return sum(c for c in counts if c is not None)

    cloud_fetch = cloudburst_client.register(fetch, 'finra_fetch')
    rule_names = []
    for i in range(8):
        name = 'finra_rule_' + str(i)
        if not cloudburst_client.register(make_rule(i), name):
            print('Failed to register %s' % name)
            sys.exit(1)
        rule_names.append(name)
    cloud_merge = cloudburst_client.register(merge, 'finra_merge')
    if not (cloud_fetch and cloud_merge):
        print('Failed to register FINRA functions.')
        sys.exit(1)

    dag_name = 'finra'
    functions = ['finra_fetch'] + rule_names + ['finra_merge']
    connections = [('finra_fetch', r) for r in rule_names] + \
                  [(r, 'finra_merge') for r in rule_names]
    ok, err = cloudburst_client.register_dag(dag_name, functions, connections)
    if not ok:
        print('Failed to register DAG: %s' % str(err))
        sys.exit(1)

    with open(CORPUS_PATH, 'r', encoding='utf-8', errors='ignore') as f:
        corpus = f.read()

    total_time = []
    oids = []
    for _ in range(num_requests):
        oid = str(uuid.uuid4())
        cloudburst_client.put_object(oid, corpus)
        oids.append(oid)
    for i in range(num_requests):
        arg_map = {'finra_fetch': [CloudburstReference(oids[i], True)]}
        start = time.time()
        cloudburst_client.call_dag(dag_name, arg_map, True)
        total_time += [time.time() - start]

    if sckt:
        import cloudpickle as cp
        sckt.send(cp.dumps(total_time))
    return total_time, [], [], 0
