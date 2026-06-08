"""Deterministic 4-stage streaming pipeline for PyPipeline tests.

Python mirror of guest/src/workloads/stream_pipeline.rs — identical record
formats and arithmetic, so a PyPipeline run produces exactly the same per-round
summaries as the Rust StreamPipeline (and the analytic baseline):

  pyp_source round r -> 20 records "r={r},i={i:02},v={r*1000+i:05}"
  pyp_filter         -> keeps even item index            (10 records / round)
  pyp_transform      -> appends "|T"
  pyp_sink           -> "batch_count=10,value_sum=10000*r+90" per round

Each consumer bounds its read by the host-published watermark via
`shm.pipe_read_window`, which is what makes the software-pipelined rounds
per-round correct (see the race fix in stream_pipeline.rs / pipeline.rs).
"""

import shm

PIPELINE_BATCH = 20


def pyp_source(out_slot, round):
    for i in range(PIPELINE_BATCH):
        v = round * 1000 + i
        shm.append_stream_data(out_slot, b"r=%d,i=%02d,v=%05d" % (round, i, v))


def pyp_filter(in_slot, out_slot):
    all_recs = shm.read_all_stream_records(in_slot)
    start, end = shm.pipe_read_window(in_slot, len(all_recs))
    for _origin, rec in all_recs[start:end]:
        # item index is the 2nd comma field: "i=NN"
        try:
            item = int(rec.split(b",")[1].split(b"=")[1])
        except (IndexError, ValueError):
            continue
        if item % 2 == 0:
            shm.append_stream_data(out_slot, rec)
    shm.pipe_set_cursor(in_slot, end)


def pyp_transform(in_slot, out_slot):
    all_recs = shm.read_all_stream_records(in_slot)
    start, end = shm.pipe_read_window(in_slot, len(all_recs))
    for _origin, rec in all_recs[start:end]:
        shm.append_stream_data(out_slot, rec + b"|T")
    shm.pipe_set_cursor(in_slot, end)


def pyp_sink(in_slot, summary_slot):
    all_recs = shm.read_all_stream_records(in_slot)
    start, end = shm.pipe_read_window(in_slot, len(all_recs))
    window = all_recs[start:end]
    count = len(window)
    if count > 0:
        value_sum = 0
        for _origin, rec in window:
            try:
                v = rec.split(b",")[2].split(b"=")[1].rstrip(b"|T")
                value_sum += int(v)
            except (IndexError, ValueError):
                pass
        shm.append_stream_data(summary_slot, b"batch_count=%d,value_sum=%d" % (count, value_sum))
    shm.pipe_set_cursor(in_slot, end)
