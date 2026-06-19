#!/usr/bin/env python3
"""Driver for the Flink StateFun streaming-application baseline.

Drives the MediaReview / SocialNetwork StateFun functions over Kafka, reusing
the SAME deterministic event stream as the WebAsShared run (../../gen_events.py),
so the correctness gate is directly comparable:

  MediaReview   : login_ok == #login,        total == #events
  SocialNetwork : login_ok == #login, profile_reads == #profile,
                  timeline_reads == 2*#timeline, tweet_writes == 2*#post

Subcommands (mirrors evaluation/utils/producer.py + measure.py conventions:
Wrapper{request_id, Any} in on workload-prefixed topics, Response out on
`responses`):

  seed       <broker> <workload> <users>
  replay     <broker> <workload> <events.csv>           # gate: produce once, unique rids
  gate       <broker> <workload> <events.csv>           # tally responses, check gate
  throughput <broker> <workload> <events.csv> <target>  # ramp: produce <target> ev/s for 3 min

`events.csv` is the output of ../../gen_events.py (one "op,key[,val]" per line).
Requires confluent_kafka + protobuf (present in the producer image / host, same
as the vendored harness). messages_pb2 is generated from ../protobuf/messages.proto.
"""
import sys
import os
import time
import uuid

# messages_pb2 lives in ../protobuf in the repo (driver/ + ../protobuf) and in
# ./protobuf inside the producer image (/app + /app/protobuf). Try both.
_HERE = os.path.dirname(os.path.abspath(__file__))
for _p in (os.path.join(_HERE, '..', 'protobuf'), os.path.join(_HERE, 'protobuf'), _HERE):
    if os.path.isdir(_p):
        sys.path.insert(0, _p)

from google.protobuf.any_pb2 import Any
from confluent_kafka.cimpl import Producer, Consumer
from confluent_kafka import TopicPartition

from messages_pb2 import (
    Wrapper, Response,
    MRSeed, MRLogin, MRRate, MRReview,
    SNSeed, SNLogin, SNProfile, SNTimeline, SNPost,
)

# op -> (kafka topic, builder(key, val) -> protobuf message)
MR_OPS = {
    'login':  ('mr_login',  lambda k, v: MRLogin(id=k, password=v)),
    'rate':   ('mr_rate',   lambda k, v: MRRate(id=k, rate=v)),
    'review': ('mr_review', lambda k, v: MRReview(id=k, review=v)),
}
SN_OPS = {
    'login':    ('sn_login',    lambda k, v: SNLogin(id=k, password=v)),
    'profile':  ('sn_profile',  lambda k, v: SNProfile(id=k)),
    'timeline': ('sn_timeline', lambda k, v: SNTimeline(id=k)),
    'post':     ('sn_post',     lambda k, v: SNPost(id=k, tweet=v)),
}
OPS = {'mediareview': MR_OPS, 'socialnetwork': SN_OPS}

# What the run's correctness gate expects: per accessed key, one 200 response.
# timeline/post touch 2 keys, so each contributes 2.
GATE_MULTIPLIER = {'timeline': 2, 'post': 2}


def wrap(outgoing):
    """Pack an op message into Wrapper{request_id, Any}, like producer.py."""
    rid = str(uuid.uuid4()).replace('-', '')
    wrapped = Wrapper()
    wrapped.request_id = rid
    any_msg = Any()
    any_msg.Pack(outgoing)
    wrapped.message.CopyFrom(any_msg)
    return rid, wrapped


def create_producer(broker):
    return Producer({'bootstrap.servers': broker})


def create_consumer(broker, topic):
    c = Consumer({
        'bootstrap.servers': broker,
        'group.id': str(uuid.uuid4()),
        'auto.offset.reset': 'earliest',
        'api.version.request': True,
        'max.poll.interval.ms': 60000,
    })
    c.subscribe([topic])
    return c


def parse_events(path):
    """Yield (op, key, val) from a gen_events.py CSV (skips the '#' header)."""
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith('#'):
                continue
            parts = line.split(',')
            op = parts[0]
            key = parts[1]
            val = parts[2] if len(parts) > 2 else ''
            yield op, key, val


# ---------------------------------------------------------------------------
# seed: populate per-key state so login/profile/timeline reads can hit.
# ---------------------------------------------------------------------------
def seed(broker, workload, users):
    producer = create_producer(broker)
    users = int(users)
    for k in range(users):
        key = str(k)
        if workload == 'mediareview':
            topic = 'mr_seed'
            msg = MRSeed(id=key, password='pw' + key)
        else:
            topic = 'sn_seed'
            msg = SNSeed(id=key, password='pw' + key,
                         profile='prof' + key, tweet='tw' + key)
        _, wrapped = wrap(msg)
        producer.produce(topic, key=key, value=wrapped.SerializeToString())
        if k % 1000 == 0:
            producer.flush()
    producer.flush()
    print('Seeded {} keys for {}'.format(users, workload))


# ---------------------------------------------------------------------------
# replay: produce the whole event stream once (for the correctness gate).
# ---------------------------------------------------------------------------
def replay(broker, workload, events_csv):
    ops = OPS[workload]
    producer = create_producer(broker)
    count = 0
    for op, key, val in parse_events(events_csv):
        if op not in ops:
            continue
        topic, build = ops[op]
        _, wrapped = wrap(build(key, val))
        producer.produce(topic, key=key, value=wrapped.SerializeToString())
        count += 1
        if count % 1000 == 0:
            producer.flush()
    producer.flush()
    print('Replayed {} events for {}'.format(count, workload))


# ---------------------------------------------------------------------------
# gate: consume `responses`, tally by (op, status), check against expectation.
# ---------------------------------------------------------------------------
def expected_counts(workload, events_csv):
    exp = {}
    total = 0
    for op, key, val in parse_events(events_csv):
        if op not in OPS[workload]:
            continue
        total += 1
        exp[op] = exp.get(op, 0) + GATE_MULTIPLIER.get(op, 1)
    return total, exp


def gate(broker, workload, events_csv):
    total_events, exp = expected_counts(workload, events_csv)
    consumer = create_consumer(broker, 'responses')
    ok = {}     # op -> count(status 200)
    seen = 0
    while True:
        msgs = consumer.consume(timeout=5, num_messages=500)
        if len(msgs) == 0:
            break
        for msg in msgs:
            response = Response()
            response.ParseFromString(msg.value())
            seen += 1
            if response.status_code == 200:
                ok[response.op] = ok.get(response.op, 0) + 1
    consumer.close()

    print('total_events={}'.format(total_events))
    passed = True
    for op in sorted(exp):
        got = ok.get(op, 0)
        want = exp[op]
        flag = 'OK' if got == want else 'MISMATCH'
        if got != want:
            passed = False
        # login_ok / profile_reads / timeline_reads / tweet_writes naming
        label = {'login': 'login_ok', 'profile': 'profile_reads',
                 'timeline': 'timeline_reads', 'post': 'tweet_writes',
                 'rate': 'rate_writes', 'review': 'review_writes'}.get(op, op)
        print('{}={} (expected {}) [{}]'.format(label, got, want, flag))
    print('responses_seen={}'.format(seen))
    print('GATE: ' + ('PASS' if passed else 'FAIL'))
    return 0 if passed else 1


# ---------------------------------------------------------------------------
# throughput: ramp producer at <target> ev/s for 3 min; achieved tput measured
# from the `responses` topic timestamps (same method as evaluation/measure.py).
# ---------------------------------------------------------------------------
def current_milli():
    return int(round(time.time() * 1000))


def _responses_end_offsets(broker):
    """Snapshot the current end offsets of `responses` so a subsequent measure
    counts only NEW responses (isolates each ramp target on a running stack,
    like evaluation/measure.py's get_partitions_with_offsets)."""
    c = Consumer({'bootstrap.servers': broker, 'group.id': str(uuid.uuid4()),
                  'auto.offset.reset': 'earliest', 'enable.auto.commit': False})
    c.subscribe(['responses'])
    parts = []
    for _ in range(20):
        c.poll(0.5)
        parts = c.assignment()
        if parts:
            break
    tps = []
    for p in parts:
        _, hi = c.get_watermark_offsets(p, timeout=5)
        tps.append(TopicPartition(p.topic, p.partition, hi))
    c.close()
    return tps


def throughput(broker, workload, events_csv, target):
    ops = OPS[workload]
    events = [e for e in parse_events(events_csv) if e[0] in ops]
    if not events:
        print('no events')
        return 1
    target = int(float(target))
    duration_s = int(os.environ.get('STREAM_DURATION_S', '180'))

    # Mark where `responses` is now; only count what this target produces.
    baseline = _responses_end_offsets(broker)

    producer = create_producer(broker)
    resolution = 5
    per_interval = max(1, target // resolution)
    ms_per_update = 1000.0 / resolution

    start = current_milli()
    last = start
    lag = 0.0
    count = 0
    i = 0
    while current_milli() < start + duration_s * 1000:
        now = current_milli()
        lag += now - last
        last = now
        while lag >= ms_per_update:
            for _ in range(per_interval):
                op, key, val = events[i % len(events)]
                i += 1
                topic, build = ops[op]
                _, wrapped = wrap(build(key, val))
                producer.produce(topic, key=key, value=wrapped.SerializeToString())
                count += 1
            producer.flush()
            lag -= ms_per_update
    print('Produced {} messages in {}s (input tput {:.0f}/s, target {})'.format(
        count, duration_s, count / duration_s, target))

    # Achieved throughput from the NEW responses only (assigned from baseline).
    consumer = Consumer({'bootstrap.servers': broker, 'group.id': str(uuid.uuid4()),
                         'enable.auto.commit': False})
    if baseline:
        consumer.assign(baseline)
    else:
        consumer.subscribe(['responses'])
    first = True
    t0 = t1 = 0.0
    seen = 0
    while True:
        msgs = consumer.consume(timeout=5, num_messages=500)
        if len(msgs) == 0:
            break
        for msg in msgs:
            ts = msg.timestamp()[1] / 1000.0
            if first:
                t0 = ts
                first = False
            t1 = ts
            seen += 1
    consumer.close()
    achieved = seen / (t1 - t0) if (t1 - t0) > 0 else 0
    print('responses={} achieved_throughput={:.0f}/s'.format(seen, achieved))
    return 0


# ---------------------------------------------------------------------------
# batch: COMMON comparable metric — fixed-batch, max-rate, end-to-end events/s.
# Produce all N events as fast as possible, then drain `responses`; throughput =
# N_events / wall-clock(start-produce -> last-response). Unit is *events* (not
# state-accesses), so it is comparable to WebAsShared events/s and RTSFaaS
# records/s. Run with FT off + matched cores for a fair 3-way comparison.
# ---------------------------------------------------------------------------
def batch(broker, workload, events_csv):
    ops = OPS[workload]
    events = [(op, k, v) for (op, k, v) in parse_events(events_csv) if op in ops]
    n_events = len(events)
    # how many responses this batch will emit (timeline/post emit 2)
    expected_resp = sum(GATE_MULTIPLIER.get(op, 1) for op, _, _ in events)

    baseline = _responses_end_offsets(broker)
    producer = create_producer(broker)
    t0 = time.time()
    for op, key, val in events:
        topic, build = ops[op]
        _, wrapped = wrap(build(key, val))
        producer.produce(topic, key=key, value=wrapped.SerializeToString())
    producer.flush()

    consumer = Consumer({'bootstrap.servers': broker, 'group.id': str(uuid.uuid4()),
                         'enable.auto.commit': False})
    if baseline:
        consumer.assign(baseline)
    else:
        consumer.subscribe(['responses'])
    seen = 0
    t_last = t0
    idle = 0
    while seen < expected_resp and idle < 4:
        msgs = consumer.consume(timeout=5, num_messages=1000)
        if not msgs:
            idle += 1
            continue
        idle = 0
        seen += len(msgs)
        t_last = time.time()
    consumer.close()
    wall = t_last - t0
    tput = n_events / wall if wall > 0 else 0
    print('events={} responses={}/{} wall_s={:.3f} throughput_events_s={:.0f}'.format(
        n_events, seen, expected_resp, wall, tput))
    return 0


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    cmd = sys.argv[1]
    if cmd == 'seed':
        return seed(sys.argv[2], sys.argv[3], sys.argv[4])
    if cmd == 'replay':
        return replay(sys.argv[2], sys.argv[3], sys.argv[4])
    if cmd == 'gate':
        return gate(sys.argv[2], sys.argv[3], sys.argv[4])
    if cmd == 'throughput':
        return throughput(sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5])
    if cmd == 'batch':
        return batch(sys.argv[2], sys.argv[3], sys.argv[4])
    print('unknown command: ' + cmd)
    return 2


if __name__ == '__main__':
    sys.exit(main() or 0)
