#  WordCount — map-reduce DAG on Cloudburst.
#
#  Mirrors the splitter -> mapper x N -> reducer shape of the RMMap and
#  WebAsShared WordCount workloads, expressed as a Cloudburst DAG:
#
#      wc_split ──► wc_mapper_0 ──┐
#               ├─► wc_mapper_1 ──┤
#               ├─►    ...        ├─► wc_reducer
#               └─► wc_mapper_N ──┘
#
#  - wc_split    : reads the corpus, cuts it into NUM_MAPPERS contiguous,
#                  newline-aligned chunks, puts each into the KVS under
#                  `<uid>_chunk_<i>`, and returns the uid (flows to every mapper).
#  - wc_mapper_i : reads its own chunk via the KVS, counts words, returns a
#                  {word: count} dict (the partial map).
#  - wc_reducer  : receives all N partial dicts as positional args, merges them.
#
#  The partial dicts crossing mapper -> reducer are the inter-stage state that
#  Cloudburst serializes through its KVS (Redis here, per the suite decision in
#  ../../README.md). That ser/deser cost is what WebAsShared's zero-copy
#  page-chain and RMMap-DMERGE's RDMA path avoid.
#
#  Wired into server.py's run_bench as bname == 'wordcount'.

import os
import sys
import time
import uuid

import cloudpickle as cp

from cloudburst.shared.reference import CloudburstReference

# Number of map workers; keep in sync across systems for a fair comparison.
NUM_MAPPERS = int(os.environ.get('WC_NUM_MAPPERS', '4'))
# Corpus to count. Point this at the same input used for the other systems.
CORPUS_PATH = os.environ.get('WC_CORPUS', '/tmp/wc_corpus.txt')


def run(cloudburst_client, num_requests, sckt):
    ''' DEFINE AND REGISTER FUNCTIONS '''

    def split(cloudburst, text):
        import uuid as _uuid
        uid = str(_uuid.uuid4())
        if isinstance(text, bytes):
            text = text.decode('utf-8', 'ignore')
        lines = text.split('\n')
        n = NUM_MAPPERS
        per = (len(lines) + n - 1) // n
        for i in range(n):
            chunk = '\n'.join(lines[i * per:(i + 1) * per])
            cloudburst.put(uid + '_chunk_' + str(i), chunk)
        return uid

    # One mapper registration per chunk index (same body, distinct names) — the
    # same idiom predserving.py uses for sqnet1/2/3. A factory binds `idx`.
    def make_mapper(idx):
        def mapper(cloudburst, uid):
            import re
            from collections import Counter
            text = cloudburst.get(uid + '_chunk_' + str(idx))
            if text is None:
                return {}
            if isinstance(text, bytes):
                text = text.decode('utf-8', 'ignore')
            counts = Counter()
            for w in re.findall(r'[a-z]+', text.lower()):
                counts[w] += 1
            return dict(counts)
        return mapper

    def reducer(cloudburst, *partials):
        from collections import Counter
        total = Counter()
        for p in partials:
            if p:
                total.update(p)
        return dict(total)

    cloud_split = cloudburst_client.register(split, 'wc_split')
    mapper_names = []
    for i in range(NUM_MAPPERS):
        name = 'wc_mapper_' + str(i)
        if not cloudburst_client.register(make_mapper(i), name):
            print('Failed to register %s' % name)
            sys.exit(1)
        mapper_names.append(name)
    cloud_reduce = cloudburst_client.register(reducer, 'wc_reducer')

    if not (cloud_split and cloud_reduce):
        print('Failed to register WordCount functions.')
        sys.exit(1)
    print('Successfully registered wc_split, %d mappers, wc_reducer.'
          % NUM_MAPPERS)

    ''' CREATE DAG '''
    dag_name = 'wordcount'
    functions = ['wc_split'] + mapper_names + ['wc_reducer']
    connections = [('wc_split', m) for m in mapper_names] + \
                  [(m, 'wc_reducer') for m in mapper_names]
    success, error = cloudburst_client.register_dag(dag_name, functions,
                                                     connections)
    if not success:
        print('Failed to register DAG: %s' % str(error))
        sys.exit(1)

    ''' LOAD CORPUS '''
    try:
        with open(CORPUS_PATH, 'r', encoding='utf-8', errors='ignore') as f:
            corpus = f.read()
    except FileNotFoundError:
        print('Corpus not found at %s — set WC_CORPUS.' % CORPUS_PATH)
        sys.exit(1)

    ''' RUN DAG '''
    total_time = []
    oids = []
    for _ in range(num_requests):
        oid = str(uuid.uuid4())
        cloudburst_client.put_object(oid, corpus)
        oids.append(oid)

    for i in range(num_requests):
        arg_map = {'wc_split': [CloudburstReference(oids[i], True)]}
        start = time.time()
        cloudburst_client.call_dag(dag_name, arg_map, True)
        end = time.time()
        total_time += [end - start]

    if sckt:
        sckt.send(cp.dumps(total_time))

    return total_time, [], [], 0
