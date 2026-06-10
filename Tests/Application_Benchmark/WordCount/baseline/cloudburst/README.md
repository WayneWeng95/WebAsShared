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

## Run

```bash
cd /opt/myapp/compare_system/cloudburst
pip3 install -r requirements.txt
./scripts/start-cloudburst-local.sh n n          # local mode (Redis backing store)

export WC_CORPUS=/path/to/corpus.txt             # same corpus as the other systems
export WC_NUM_MAPPERS=4                           # same N as the other systems
# trigger the 'wordcount' benchmark with K requests via the benchmark server
#   (server.py BENCHMARK_START_PORT; msg = "<resp_addr>:wordcount:<num_requests>")
python3 -m cloudburst.server.benchmarks.server conf/cloudburst-config.yml
```

Tunables (env): `WC_NUM_MAPPERS` (mapper count), `WC_CORPUS` (input path).

> **Backing store = Redis, not Anna** (suite decision — see
> [`../../README.md`](../../README.md)). Report E2E latency
> (`utils.print_latency_stats`), throughput, peak memory, and Redis put/get bytes
> (the serialized transfer). Don't compare against Cloudburst's Anna-locality numbers.
