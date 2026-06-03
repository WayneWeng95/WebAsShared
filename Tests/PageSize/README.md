# PageSize experiment

Evaluates how the engine's shared-memory **page size** affects PUT and GET
latency, across the same transfer-size schedule as [`../StateSync`](../StateSync)
and [`../Micro-Benchmarks`](../Micro-Benchmarks).

This is **harness-only** — the real engine is *not* modified. It reuses the
`shm_statesync` harness (`Executor/connect/examples/shm_statesync.rs`), whose
`--page-bytes N` flag rewrites the same page-chain PUT/GET with a different page
granularity, isolating the effect of page size alone.

| Axis | Values |
|---|---|
| page size | **4 KiB** (the engine default), **64 KiB**, **2 MiB** |
| transfer size | 16 KiB, 1 MiB, 16 MiB, 128 MiB |
| metric | PUT and GET p50/p99 latency (µs) + GiB/s, per (page, size) cell |

Each run emits both engine rows: `shm-copy-engine` (GET materializes the payload)
and `shm-zerocopy-engine` (GET is the O(1) Bridge splice). PUT is identical for
both (it's the write). Row names are page-suffixed (`…-64KiB`, `…-2MiB`) so all
three page sizes accumulate into one `results.csv`.

## Running

```bash
./run.sh all                 # build + sweep (page x size) + plot
./run.sh run                 # sweep only -> results.csv
./run.sh run --iters 100     # extra args pass through to the harness
./run.sh plot                # figs/ from results.csv
```

## Figures

- `figs/pagesize_put_get.png` — combined PUT | GET **mean** latency panel, one
  line per page size, x = transfer size (StateSync style), plus a dashed
  **Shared memory (zero-copy)** comparison line (the modelled `shm-zerocopy` row
  from StateSync-local). Uses the **copy** engine row for the page lines (both PUT
  and the materialize-GET). `--row zerocopy` renders the splice GET; `--metric p50`
  for medians; `--no-baseline` drops the comparison line.
- `figs/put_page_sweep.png` — PUT bandwidth (GiB/s) vs page size at 128 MiB, with
  the flat-memcpy baseline.

## What it shows

- **PUT improves with page size.** At 128 MiB, PUT climbs ~4.5 → 5.6 → 7.7 GiB/s
  for 4 KiB → 64 KiB → 2 MiB pages, toward the flat-memcpy ceiling (~8.9 GiB/s).
  Bigger pages mean fewer, larger `memcpy` spans that reach the prefetch and
  non-temporal-store sweet spots. (The controlled probe that proved page/chunk
  size — not header gaps or atomic fences — is the cause is
  `cargo run --release --example shm_put_probe`.)
- **GET is page-insensitive.** The copy GET is bounded by the 128 MiB
  materialize (≈1.4 GiB/s) regardless of page size; the zero-copy splice GET is
  flat (~0.2 µs) regardless of page *or* transfer size (it only moves head/tail
  pointers). So the page-size lever is a **PUT** lever.

Nothing here changes the engine; it is a measurement of how page granularity
would trade off if the engine's 4 KiB page were enlarged (which would also
coarsen allocation/splice granularity and waste memory on small records — hence
4 KiB as the default, with a jumbo span being the natural future fast-path for
large single records).
