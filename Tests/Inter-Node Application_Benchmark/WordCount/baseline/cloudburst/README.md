# Cloudburst WordCount port

Map-reduce WordCount as a Cloudburst DAG — the Cloudburst baseline for
[`../../README.md`](../../README.md). **This is a new port** (Cloudburst shipped no WordCount).

## Files (canonical copy; installed into the Cloudburst tree)

| File | Installed to (in `../../../../../../compare_system/cloudburst/`) |
|------|-----------------------------------------------------------------|
| `wordcount.py` | `cloudburst/server/benchmarks/wordcount.py` |
| —              | wired into `cloudburst/server/benchmarks/server.py` (import + `bname == 'wordcount'` in `run_bench`) |

DAG (`splitter → mapper×N → reducer`), following the `predserving.py` idiom
(`register` / `register_dag` / `call_dag`):

```
wc_split ──► wc_mapper_0 ──┐
         ├─► wc_mapper_1 ──┤
         ├─►   ...         ├─► wc_reducer
         └─► wc_mapper_N ──┘
```

- `wc_split` cuts the corpus into N newline-aligned chunks, `put`s each into the
  KVS as `<uid>_chunk_<i>`, returns the uid (flows to every mapper).
- `wc_mapper_i` (one registration per index, like `sqnet1/2/3`) `get`s its chunk,
  counts words, returns a `{word: count}` dict.
- `wc_reducer` receives all N dicts as positional args and merges them.

The partial dicts crossing mapper→reducer are serialized through the KVS — the
state-transfer cost we contrast with our zero-copy / RDMA path.

## How this actually runs here (Redis-backed runner)

> **Correction to the suite docs.** Cloudburst's state plane is the **Anna KVS**,
> *not* Redis — `cloudburst.put/get` flow through `anna_client`
> (`server/executor/user_library.py`), and the `redis`/`s3` names in `server.py`
> are data-*locality* micro-benchmarks (an external store as a data source), not
> the backing store. Cloudburst does **not** support Redis as its state store
> natively. The full Anna control plane is also a 2019 / Python-3.6 stack
> (protobuf-generated stubs, pyzmq, cloudpickle 0.6, the Anna C++ client) that
> does not stand up on this host (Python 3.14, no Anna). So we **added a
> Redis-backed runner** that executes the *real* `wordcount.py` bodies + DAG
> through Redis.

Two files were **added to the Cloudburst tree** (`../../../../../../compare_system/cloudburst/`):

| File | Role |
|------|------|
| `cloudburst/shared/redis_kvs.py` | `RedisKvsClient` — Anna-compatible `get(keys)`/`put(keys,values)` surface backed by Redis, with put/get **byte accounting** (the serialized-transfer metric). |
| `cloudburst/server/benchmarks/redis_runner.py` | `RedisCloudburst` — implements both API levels the benchmark touches (connection: `register`/`register_dag`/`put_object`/`call_dag`; in-function KVS: `put`/`get`) and a single-process topological DAG executor that routes **every inter-stage value through Redis**, cloudpickled. |

`wordcount.py` itself is **unchanged** — `run(client, num_requests, sckt)` is
driven exactly as `server.py`'s `run_bench` would drive it.

### Run

```bash
# redis-server up on :6379, cloudpickle installed (pip3 install --break-system-packages cloudpickle)
cd Tests/Application_Benchmark/WordCount/baseline/cloudburst
./run.sh                                  # 50MB corpus, N=1..16, 3 requests → results.csv
./run.sh /path/to/corpus.txt "1 2 4 8 16" 3
```

Each N runs in a **fresh process** (clean per-N peak RSS). Output columns:
`size_mb,workers,topo,e2e_ms_median,throughput_mb_s,words_per_s,peak_mem_mb,reps,kvs_put_mb,kvs_get_mb,total_occurrences`.

### Fidelity (what's faithful vs. dropped)

- **Faithful:** the actual split/mapper/reducer logic and DAG topology; every
  chunk and partial-`{word:count}` dict **serialized (cloudpickle) and
  round-tripped through Redis** — the inter-stage state-transfer cost (`kvs_put_mb`
  + `kvs_get_mb`) the comparison is about. Word counts validated
  byte-for-byte against our system (`total_occurrences=8940339` on `corpus_large`).
- **Dropped (state this in the writeup):** the zmq scheduler, multi-executor
  placement/scaling, and Anna lattice consistency. Because the DAG runs in **one
  process, mappers execute sequentially** — so throughput is ~flat in N (no
  parallel speedup). Compare **per-N absolute throughput/latency and the KVS byte
  volume**, *not* the scaling slope. (A multiprocessing executor could restore
  the parallel curve if needed — see the suite TODO.)
- Don't compare against Cloudburst's published **Anna-locality** numbers.

### Results (2026-06-12, this dev box, `corpus_large` 50 MB, 3 requests)

| N | e2e ms (med) | MB/s | peak MB | KVS put+get MB | occ |
|---|--------------|------|---------|----------------|-----|
| 1 | 5529 | 9.0 | 746 | 600 | 8940339 |
| 2 | 5706 | 8.8 | 473 | 600 | 8940339 |
| 4 | 5608 | 8.9 | 311 | 600 | 8940339 |
| 8 | 5287 | 9.5 | 259 | 600 | 8940339 |
| 16 | 5637 | 8.9 | 254 | 600 | 8940339 |

~600 MB pushed through Redis to process 150 MB of corpus (3×50 MB) ≈ **4×
serialization amplification** — vs our zero-copy page-chain (0 bytes serialized).
Ours on the same 50 MB: 17.5 MB/s (N=1) → 37.6 MB/s (N=8), `serialization = 0`.
