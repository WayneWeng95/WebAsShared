# StateSync — framework evaluation

Runs the **same 1→1 state-delivery test case** as
[`../Micro-Benchmarks`](../Micro-Benchmarks), but through the framework's **real
data-passing paths** instead of the modelled `/dev/shm` mmap / generic baselines.
Where the micro-benchmarks place *our* row next to S3 / Redis / Cloudburst, this
folder isolates and exercises *our* row alone, end-to-end, on the actual engine
code:

| Scenario | Path exercised | Harness | Metrics |
|---|---|---|---|
| **local** | engine page-chain — `common::Superblock` stream-slot chains, formatted exactly like `host::shm::format_shared_memory` | `Executor/connect/examples/shm_statesync.rs` | put/get p50·p99 latency (µs), GiB/s |
| **remote** | engine RemoteSend — `connect::RdmaRemote` (MR memcpy + one-sided RDMA WRITE + TCP done-signal) | `Executor/connect/examples/rdma_latency.rs` | one-way latency = RTT/2, GiB/s |

Both harnesses reuse the engine's own types/protocol, so the numbers are faithful
to the framework (not a model). CSV schemas match the micro-benchmarks, so rows
are directly comparable / mergeable.

The local harness emits two rows. **PUT** is identical for both: the producer
writes the payload into its slot's page chain. To stay fair to the modelled rows
(which reuse a fixed mmap region with no per-op allocation), the chain is
**pre-allocated once and reused**, so the PUT timer measures only the data write,
not page allocation. (Allocation turns out to be just ~6–12% of PUT anyway — the
page-chain PUT is memcpy-bound; pass `--include-alloc` to fold it back in.) The
two rows differ only in how the consumer **GET**s it:

- `shm-copy-engine` — materializes the payload out of the chain (one memcpy per
  page span). Tracks memcpy bandwidth; cost grows with size.
- `shm-zerocopy-engine` — the engine's real zero-copy **delivery**: a routing
  **Bridge splice** (`host::routing::chain_splicer::chain_onto`, mirrored
  byte-for-byte) that hands the producer's whole page chain to the consumer's
  slot by moving head/tail pointers. **O(1)** — no walk, no copy — so it stays
  flat (~0.05 µs) regardless of state size. Fan-out (`--readers N`) becomes a
  Broadcast (link the src chain onto N consumer slots, each O(1)).

This is the **fair** counterpart to the modelled `shm-zerocopy` row in
`bench.py`, whose GET is an O(1) `memoryview` that never reads the payload. Both
deliver in constant time, but the splice actually transfers ownership of the data
to the next stage (the modelled view just wraps a pointer in a Python object, so
it also carries that object-creation overhead — which is why the engine splice is
~30–50× faster still).

## Running

```bash
./run.sh build                 # build both harnesses (release)
./run.sh local                 # local engine page-chain put/get -> results_local.csv
./run.sh local --readers 8 --sizes "16384 1048576" --iters 100
./run.sh loopback              # remote RDMA, server+client on this node -> results_remote.csv
./run.sh plot                  # compare engine zero-copy vs StateSync-local baselines -> figs/
./run.sh all                   # build + local + loopback + plot (single-node smoke)
./run.sh summary               # pretty-print the two results CSVs
```

## Page-size effect

The effect of page size on PUT/GET has moved to its own experiment,
[`../PageSize`](../PageSize) — it sweeps the engine page-chain across
4 KiB / 64 KiB / 2 MiB pages over the same transfer sizes and shows PUT bandwidth
climbing toward a flat memcpy as pages grow (the zero-copy splice GET is
page-size agnostic). The controlled gap/fence/chunk probe lives there too.

## Comparison plot

`plot_compare.py` (also wired as `./run.sh plot`) renders the **engine zero-copy**
GET/PUT against the existing `redis-local`, modelled `shm-copy`, and modelled
`shm-zerocopy` rows from `../Micro-Benchmarks/StateSync-local/results.csv`, into
`figs/`:

- `compare_put_get.png` — combined PUT | GET latency panel vs size (log y, shared
  legend), in the style of `StateSync-local/figs/latency_put_get.png`
- `compare_get_throughput.png` — GET throughput (GiB/s) vs size

and prints a speedup table. The engine zero-copy delivery (Bridge splice) is
**flat with size** (~0.05 µs), beating local Redis GET by 3–4 orders of magnitude
and the modelled mmap zero-copy by ~30–50× (the latter pays Python
`memoryview`-object overhead on every GET). PUT — a real write into SHM — is the
same for both engine rows and is memcpy-bound, ~4–8× faster than Redis.

Real cross-node remote run (the meaningful RDMA measurement — needs two nodes on
the RoCE fabric, e.g. `10.10.1.x`):

```bash
# node B (consumer):   ./run.sh remote-server 7900
# node A (producer):   ./run.sh remote-client 10.10.1.1 7900
```

Outputs `results_local.csv` and `results_remote.csv` in this directory.
`loopback` is a single-node smoke of the RDMA path (no wire — faster than a real
two-node run; use `remote-server`/`remote-client` for the real figure).
