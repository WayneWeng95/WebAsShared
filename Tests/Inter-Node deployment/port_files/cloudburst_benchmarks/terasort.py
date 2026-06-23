# TeraSort — data-parallel sort with an all-to-all shuffle on Cloudburst.
#
#  Mirrors the WebAsShared / RMMap / Faasm TeraSort shape as a Cloudburst DAG:
#
#      ts_split ─► ts_part_0 ─┐                  ┌─► ts_merge_0 ─┐
#               ├─► ts_part_1 ┼─ (all-to-all) ──┼─► ts_merge_1 ─┼─► ts_collect
#               ├─►   ...     │  via the KVS     ├─►   ...       │
#               └─► ts_part_N ┘                  └─► ts_merge_N ─┘
#
#  - ts_split    : reads the records, cuts them into N contiguous chunks, puts
#                  each into the KVS under `<uid>_chunk_<i>`, returns the uid.
#  - ts_part_i   : reads its chunk, RANGE-PARTITIONS every record by key-prefix
#                  into N owner buckets, puts each bucket as `<uid>_bucket_<i>_<j>`
#                  and returns the uid (flows to every merger).
#  - ts_merge_j  : reads its column {<uid>_bucket_<i>_<j> : i} from the KVS,
#                  concatenates and SORTS its key range, returns a summary
#                  (range, records, keysum, sorted, first, last).
#  - ts_collect  : receives all N summaries, sums records+keysum (the fan-out-
#                  invariant gate), checks every range sorted and globally ordered.
#
#  Unlike WordCount (small per-mapper partial maps), here the ENTIRE dataset
#  crosses the shuffle: every record is put once by its partitioner and got once
#  by its merger, so ~1x the input is serialized THROUGH the KVS each way — the
#  maximal-data-movement contrast to WebAsShared's zero-copy page-chain shuffle.
#
#  Wired into the Redis runner the same way wordcount.py is.

import os
import sys
import time
import uuid

import cloudpickle as cp

from cloudburst.shared.reference import CloudburstReference

# Number of shuffle workers; keep in sync across systems for a fair comparison.
NUM_WORKERS = int(os.environ.get('TS_NUM_WORKERS', '4'))
# Records file (100-byte Gensort lines). Point at the same input as the others.
RECORDS_PATH = os.environ.get('TS_RECORDS', '/tmp/ts_records.dat')

KEY_LEN = 10
KEY_LO = 33          # must match gen_records.py / the guest
KEY_SPAN = 64


def _owner(first_byte, n):
    b = min(max(first_byte, KEY_LO), KEY_LO + KEY_SPAN - 1) - KEY_LO
    o = b * n // KEY_SPAN
    return n - 1 if o >= n else o


def run(cloudburst_client, num_requests, sckt):
    ''' DEFINE AND REGISTER FUNCTIONS '''

    def split(cloudburst, text):
        import uuid as _uuid
        uid = str(_uuid.uuid4())
        if isinstance(text, bytes):
            text = text.decode('latin-1')
        lines = [ln for ln in text.split('\n') if ln]
        n = NUM_WORKERS
        per = (len(lines) + n - 1) // n
        for i in range(n):
            chunk = '\n'.join(lines[i * per:(i + 1) * per])
            cloudburst.put(uid + '_chunk_' + str(i), chunk)
        return uid

    # One partitioner registration per worker index (same body, distinct names).
    def make_part(idx):
        def partition(cloudburst, uid):
            n = NUM_WORKERS
            text = cloudburst.get(uid + '_chunk_' + str(idx))
            if text is None:
                return uid
            if isinstance(text, bytes):
                text = text.decode('latin-1')
            buckets = [[] for _ in range(n)]
            for ln in text.split('\n'):
                if not ln:
                    continue
                buckets[_owner(ord(ln[0]), n)].append(ln)
            for j in range(n):
                cloudburst.put(uid + '_bucket_' + str(idx) + '_' + str(j),
                               '\n'.join(buckets[j]))
            return uid

        return partition

    # One merger registration per range j.
    def make_merge(j):
        def merge(cloudburst, *uids):
            n = NUM_WORKERS
            uid = uids[0]
            recs = []
            for i in range(n):
                b = cloudburst.get(uid + '_bucket_' + str(i) + '_' + str(j))
                if b:
                    recs.extend(ln for ln in b.split('\n') if ln)
            recs.sort(key=lambda r: r[:KEY_LEN])
            keysum = sum(ord(c) for r in recs for c in r[:KEY_LEN])
            srt = all(recs[k][:KEY_LEN] <= recs[k + 1][:KEY_LEN]
                      for k in range(len(recs) - 1))
            first = recs[0][:KEY_LEN] if recs else ''
            last = recs[-1][:KEY_LEN] if recs else ''
            return {'range': j, 'records': len(recs), 'keysum': keysum,
                    'sorted': srt, 'first': first, 'last': last}

        return merge

    def collect(cloudburst, *summaries):
        rng = {s['range']: s for s in summaries if s}
        records = sum(s['records'] for s in rng.values())
        keysum = sum(s['keysum'] for s in rng.values())
        allsorted = all(s['sorted'] for s in rng.values())
        ordered = True
        prev_last = None
        for j in sorted(rng):
            s = rng[j]
            if s['records'] == 0:
                continue
            if prev_last is not None and s['first'] < prev_last:
                ordered = False
            prev_last = s['last']
        return {'records': records, 'keysum': keysum,
                'sorted': allsorted, 'ordered': ordered}

    cloud_split = cloudburst_client.register(split, 'ts_split')
    part_names = []
    for i in range(NUM_WORKERS):
        name = 'ts_part_' + str(i)
        if not cloudburst_client.register(make_part(i), name):
            print('Failed to register %s' % name)
            sys.exit(1)
        part_names.append(name)
    merge_names = []
    for j in range(NUM_WORKERS):
        name = 'ts_merge_' + str(j)
        if not cloudburst_client.register(make_merge(j), name):
            print('Failed to register %s' % name)
            sys.exit(1)
        merge_names.append(name)
    cloud_collect = cloudburst_client.register(collect, 'ts_collect')

    if not (cloud_split and cloud_collect):
        print('Failed to register TeraSort functions.')
        sys.exit(1)
    print('Successfully registered ts_split, %d partitioners, %d mergers, '
          'ts_collect.' % (NUM_WORKERS, NUM_WORKERS))

    ''' CREATE DAG '''
    dag_name = 'terasort'
    functions = ['ts_split'] + part_names + merge_names + ['ts_collect']
    connections = [('ts_split', p) for p in part_names] + \
                  [(p, m) for p in part_names for m in merge_names] + \
                  [(m, 'ts_collect') for m in merge_names]
    success, error = cloudburst_client.register_dag(dag_name, functions,
                                                     connections)
    if not success:
        print('Failed to register DAG: %s' % str(error))
        sys.exit(1)

    ''' LOAD RECORDS '''
    try:
        with open(RECORDS_PATH, 'r', encoding='latin-1') as f:
            records = f.read()
    except FileNotFoundError:
        print('Records not found at %s — set TS_RECORDS.' % RECORDS_PATH)
        sys.exit(1)

    ''' RUN DAG '''
    total_time = []
    oids = []
    for _ in range(num_requests):
        oid = str(uuid.uuid4())
        cloudburst_client.put_object(oid, records)
        oids.append(oid)

    for i in range(num_requests):
        arg_map = {'ts_split': [CloudburstReference(oids[i], True)]}
        start = time.time()
        cloudburst_client.call_dag(dag_name, arg_map, True)
        end = time.time()
        total_time += [end - start]

    if sckt:
        sckt.send(cp.dumps(total_time))

    return total_time, [], [], 0
