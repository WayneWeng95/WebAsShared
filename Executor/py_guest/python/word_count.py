"""Word-count and streaming pipeline workloads.

All slot numbers are supplied by the DAG JSON as arg/arg2 — none are hardcoded here.
"""

import shm


# ── Word-count demo ───────────────────────────────────────────────────────────

def wc_distribute(n_workers, base_slot):
    """Read all input records and distribute them round-robin to stream slots
    base_slot … base_slot+n_workers-1."""
    lines = shm.read_all_inputs()
    for i, (origin, line) in enumerate(lines):
        shm.append_stream_data(base_slot + (i % n_workers), line)


def wc_map(in_slot, out_slot):
    """Count word frequencies in stream slot `in_slot`, emit one record per
    unique word to stream slot `out_slot`."""
    counts = {}
    for origin, rec in shm.read_all_stream_records(in_slot):
        for token in rec.decode('utf-8', errors='replace').split():
            word = ''.join(c.lower() for c in token if c.isalpha())
            if word:
                counts[word] = counts.get(word, 0) + 1

    for word, count in counts.items():
        shm.append_stream_data(out_slot,
                               ('word=' + word + '\x1f' + str(count)).encode())


def wc_reduce(in_slot):
    """Merge all map records from stream slot `in_slot` and write the summary
    to I/O slot 1 via write_output."""
    # Count records before reading payloads (no allocation).
    n_records = shm.count_stream_records(in_slot)

    totals = {}
    for origin, rec in shm.read_all_stream_records(in_slot):
        s = rec.decode('utf-8', errors='replace')
        if not s.startswith('word=') or '\x1f' not in s:
            continue
        body  = s[5:]
        sep   = body.index('\x1f')
        word  = body[:sep]
        count = int(body[sep + 1:])
        if word:
            totals[word] = totals.get(word, 0) + count

    # Write result lines to I/O slot 1 via write_output.
    shm.write_output(b'=== word_count ===')
    shm.write_output(('map_records_received=' + str(n_records)).encode())
    shm.write_output(('unique_words=' + str(len(totals))).encode())
    shm.write_output(('total_occurrences=' + str(sum(totals.values()))).encode())
    for word, count in sorted(totals.items()):
        shm.write_output((word + ': ' + str(count)).encode())


# ── 4-stage streaming pipeline demo ──────────────────────────────────────────

PIPELINE_BATCH = 20


def pipeline_source(out_slot, round_num):
    for i in range(PIPELINE_BATCH):
        v   = round_num * 1000 + i
        rec = ('r=%d,i=%02d,v=%05d' % (round_num, i, v)).encode()
        shm.append_stream_data(out_slot, rec)


def pipeline_filter(in_slot, out_slot):
    for _origin, rec in shm.read_all_stream_records(in_slot):
        s = rec.decode('utf-8', errors='replace')
        parts = s.split(',')
        if len(parts) >= 2:
            item_val = int(parts[1].split('=')[1]) if '=' in parts[1] else -1
            if item_val % 2 == 0:
                shm.append_stream_data(out_slot, rec)


def pipeline_transform(in_slot, out_slot):
    for _origin, rec in shm.read_all_stream_records(in_slot):
        shm.append_stream_data(out_slot, rec + b'|T')


def pipeline_sink(in_slot, summary_slot):
    count, value_sum = 0, 0
    for _origin, rec in shm.read_all_stream_records(in_slot):
        s = rec.decode('utf-8', errors='replace')
        parts = s.split(',')
        if len(parts) >= 3:
            v_part = parts[2].split('=')
            if len(v_part) >= 2:
                try:
                    value_sum += int(v_part[1].rstrip('|T'))
                except ValueError:
                    pass
        count += 1
    if count > 0:
        summary = ('batch_count=%d,value_sum=%d' % (count, value_sum)).encode()
        shm.append_stream_data(summary_slot, summary)
