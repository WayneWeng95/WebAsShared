# Streaming pipeline tests (single-node)

A correctness test pass for the software-pipelined streaming nodes
`StreamPipeline` (Rust) and `PyPipeline` (Python). Covers problems.md item #2.

```bash
./Tests/Streaming/run.sh        # builds if needed, then runs
# or, if already built:
python3 Tests/Streaming/run.py
```

Exit code 0 = all pass, 1 = failure. The Python section self-skips if `python3`
is unavailable; the image section self-skips if `TestData/*.ppm` are missing.

## What it checks

The deterministic 4-stage pipeline (`pipeline_source → filter → transform →
sink`, mirrored in Python as `pyp_*`) has an analytic ground truth: round *r*
emits 20 records (`v = r*1000 + i`), the filter keeps even item indices (10
records/round), and the sink emits `batch_count=10,value_sum=10000*r+90`.

1. **Per-round correctness** — pipelined output equals the analytic baseline
   *and* a non-pipelined baseline (the same stages run serially via
   `WasmGrouping` / `PyGrouping`).
2. **Determinism** — repeated runs are byte-identical.
3. **Slot lifecycle across ticks** — exactly *N* summaries, each
   `batch_count == 10`, i.e. every stage consumed exactly one round per tick.
4. **Reset / multi-run** — `mode:"reset"` + `runs:K` reproduces the same per-run
   result (cursors + read watermarks reset at pipeline start, slots freed).
5. **`Output { split_records: true }`** — a pipeline emitting one record per
   round fans record *i* → `paths[i]` (both a synthetic I/O sink and the real
   image pipeline).
6. **Rust / Python parity** — `PyPipeline` produces the identical result.
7. **Image pipeline** — the real `img_*` StreamPipeline (one image/round) matches
   its non-pipelined baseline byte-for-byte and writes valid per-round PGMs.

## The bug this guards against

Before the fix, adjacent pipelined stages shared a stream slot and the consumer
of round *R* read "all records since its cursor" while the producer of round
*R+1* was concurrently appending to the same slot — so per-round batching was
non-deterministic (only the cumulative total survived). The fix has the host
publish a per-slot read watermark (`stream_hi_{slot}`) before each tick's
scatter; consumers bound their read to it. See
`Executor/host/src/runtime/dag_runner/pipeline.rs` and
`Executor/guest/src/workloads/stream_pipeline.rs` (`pipe_read_window`).
