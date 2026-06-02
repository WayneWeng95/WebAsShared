# StateSync Micro-Benchmark

Isolates and compares the cost of **synchronizing a unit of state between
serverless functions / pipeline stages** under a spectrum of state-transfer
mechanisms. This is the micro-benchmark backing the paper's central claim:
zero-copy shared memory delivers state to functions far cheaper than the
serialize → transport → deserialize path used by mainstream serverless
workflow systems.

## Approaches under test

Ordered from most disaggregated (slowest, most general) to most local
(fastest, our design). All run with **compute on a single node** first; the
backends for the two "external remote" rows live on a second cluster node.

| # | Approach | Backend | Locality | Representative system / paper |
|---|----------|---------|----------|-------------------------------|
| 1 | External storage, remote | S3-compatible object store (MinIO) | remote node, over network | S3 / object-store function chaining |
| 2 | External in-memory, remote | Redis | remote node, over network | Disaggregated KV state stores |
| 3 | Local in-memory | Redis (loopback) | same node, over loopback TCP | Faasm two-tier local state, Pocket/Jiffy-style |
| 4 | Container shared memory | host shared memory, copy-based | same node, intra-host mmap | *Serialization/Deserialization-free State Transfer in Serverless Workflows* |
| 5 | **SHM zero-copy (ours)** | page-chain slots in mapped SHM | same node, zero-copy | *Roadrunner: Accelerating Data Delivery to WebAssembly-Based Serverless Functions* |

Rows 4–5 need no external daemon — they are driven entirely by the harness
(rows 4–5 are implemented in a later step). Rows 1–3 need the Redis / S3
backends provisioned by `deploy_backends.sh`.

## Cluster topology

From `NodeAgent/agent_*.toml` (`cluster.ips = ["10.10.1.2", "10.10.1.1"]`):

| Node | Role | IP | Use in this benchmark |
|------|------|----|-----------------------|
| 0 | coordinator | `10.10.1.2` | compute node — producer + consumer run here |
| 1 | worker | `10.10.1.1` | remote state backends (Redis + S3) live here |

The `10.10.1.x` link is the experiment network shared with the RDMA data plane.

## Why native install, not Docker

`deploy_backends.sh` installs Redis and MinIO as **native processes bound
directly to the experiment NIC**. For a latency micro-benchmark, Docker's
default bridge networking adds NAT/veth round-trip overhead that would
contaminate the remote-Redis and remote-S3 measurements. Native binding keeps
the measured cost to *store + network* only.

Redis is configured **pure in-memory** (`save ""`, `appendonly no`,
`maxmemory-policy noeviction`) so writes never touch disk and benchmark state
is never silently evicted.

## Deploying the backends

On the **remote backend node** (`10.10.1.1`) — brings up Redis + S3:

```bash
./deploy_backends.sh up
```

On the **compute node** (`10.10.1.2`) — a loopback-only Redis for the
"local in-memory" row (no S3 needed locally):

```bash
./deploy_backends.sh up --bind 127.0.0.1 --no-s3
```

Or drive the remote node in one shot from the compute node:

```bash
./deploy_backends.sh remote 10.10.1.1 up
```

Inspect / tear down:

```bash
./deploy_backends.sh status
./deploy_backends.sh down
```

Each `up` writes `backends.env` (resolved endpoints + credentials) next to the
script, ready to `source` from the benchmark harness.

## Running the benchmark (`bench.py`)

`bench.py` measures producer→consumer **state delivery cost** for each approach.
Every approach implements the same `put(key, payload)` / `get(key)` pair, so the
timing loop is identical and the comparison is fair. It sweeps state size and an
optional reader fan-out, and reports p50 / p99 latency (µs) and throughput
(GiB/s) for PUT and GET separately. Backends whose client lib or endpoint is
unavailable are **skipped** with a message rather than failing the run.

```bash
# (optional) install clients for the external rows
pip install redis minio

# (optional) source endpoints written by deploy_backends.sh
#   the script auto-reads ./backends.env if present

python3 bench.py                                   # all available approaches
python3 bench.py --approaches shm-copy shm-zerocopy
python3 bench.py --sizes 64 4096 1048576 --iters 500 --warmup 50
python3 bench.py --readers 8                       # fan-out: 1 put, N gets
python3 bench.py --csv results.csv                 # raw rows for plotting
```

The two shared-memory rows run with **no external dependencies** (mmap over a
`/dev/shm` scratch file). They isolate the #4-vs-#5 contrast directly:
`shm-copy`'s GET materializes the payload (`bytes(view)` — one memcpy),
`shm-zerocopy`'s GET returns a `memoryview` with no copy. In a quick local run
the GET cost diverges sharply with size — copy stays memcpy-bound (~7.5 GiB/s)
while zero-copy GET is ~constant (microseconds regardless of size), and the gap
widens further under fan-out (copy pays N× the memcpy, zero-copy stays flat).

> **Fidelity note.** The shared-memory rows model the *concept* (copy vs.
> in-place handle) over a plain `/dev/shm` mmap, not the engine's actual
> page-chain `Superblock`. A follow-up can swap in the real path: reuse
> `Executor/py_guest/python/shm.py` (`append_stream_data` / `read_all_stream_records`)
> against a host-formatted SHM, or add a native Rust harness calling the host
> SHM primitives. The current harness is the lightweight first cut.

## Metrics & axes

- per-hop **latency** — p50, p99 (and mean in the CSV)
- **throughput** — GiB/s derived from median latency
- **scaling** with state size and reader fan-out
- *(planned)* CPU user/sys split to expose the serialization tax rows 1–4 pay
  and row 5 avoids

Data-flow patterns: point-to-point (1→1) and fan-out (1→N, via `--readers`)
are supported now; aggregation (N→1) is a planned addition.
