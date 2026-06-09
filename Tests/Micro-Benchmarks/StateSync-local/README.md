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

| # | Approach (`bench.py` id) | Backend | Locality | Representative system / paper |
|---|----------|---------|----------|-------------------------------|
| 1 | External storage, remote — disk (`s3-disk`) | S3-compatible object store (MinIO), disk-backed data dir | remote node, over network | S3 / object-store function chaining |
| 2 | External storage, remote — RAM (`s3`) | S3-compatible object store (MinIO), tmpfs data dir | remote node, over network | object store on fast/RAM tier |
| 3 | External in-memory, remote (`redis-remote`) | Redis | remote node, over network | Disaggregated KV state stores |
| 4 | Local in-memory (`redis-local`) | Redis (loopback) | same node, over loopback TCP | Faasm two-tier local state, Pocket/Jiffy-style |
| 5 | Container shared memory (`shm-copy`) | host shared memory, copy-based | same node, intra-host mmap | *Serialization/Deserialization-free State Transfer in Serverless Workflows* |
| 6 | **SHM zero-copy — ours (`shm-zerocopy`)** | page-chain slots in mapped SHM | same node, zero-copy | *Roadrunner: Accelerating Data Delivery to WebAssembly-Based Serverless Functions* |

The two S3 rows isolate the **storage-medium** effect: `s3-disk` writes objects to
the NVMe root fs (`/var/tmp`), `s3` writes to tmpfs (`/tmp`, RAM). Both run as
separate MinIO instances on the backend node (ports 9010 and 9000).

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

### Two-node experiment (no SSH — one command per node)

Run **one command on each machine** (this is the path to use when passwordless
SSH between nodes isn't available):

```bash
# On the BACKEND node (10.10.1.1) — Redis + S3 bound to the experiment NIC:
./deploy_backends.sh backend

# On the COMPUTE node (10.10.1.2) — loopback Redis + merged backends.env,
# pointed at the backend node's IP:
./deploy_backends.sh compute --remote 10.10.1.1
```

`compute` starts the loopback Redis (the `redis-local` row) and writes
`backends.env` so the `redis-remote` and `s3` rows resolve to `10.10.1.1` and
`redis-local` resolves to loopback. Then, on the compute node:

```bash
python3 bench.py --csv results.csv
```

Verify / tear down on each node:

```bash
./deploy_backends.sh status     # on either node
./deploy_backends.sh down       # on each node
```

> If the backend uses non-default ports/creds (`backend --redis-port N …`),
> pass the same flags to `compute` so the merged `backends.env` matches.

### One-command variant (requires SSH)

If passwordless SSH to the backend node *does* work, both sides can be done
from the compute node in one shot:

```bash
./deploy_backends.sh two-node 10.10.1.1
./deploy_backends.sh two-node 10.10.1.1 down
```

### Manual / single-node

```bash
./deploy_backends.sh up                          # Redis + S3, auto-detected NIC
./deploy_backends.sh up --bind 127.0.0.1 --no-s3 # loopback Redis only
./deploy_backends.sh remote 10.10.1.1 up         # deploy on a remote node
./deploy_backends.sh status
./deploy_backends.sh down
```

Each `up` writes `backends.env` (resolved endpoints + credentials) next to the
script, ready to be read by the benchmark harness.

### Reset a node

`reset.sh` returns a node to a clean state and verifies it (stops backends,
clears runtime/data artifacts, checks processes + ports + the distro Redis
service). Run it on each node before or after a run:

```bash
./reset.sh                # stop + clean (keeps downloaded binaries) + check
./reset.sh --check-only   # report state without touching anything
./reset.sh --purge-bin    # also delete the downloaded minio/mc binaries
./reset.sh --keep-data    # clean metadata but keep the data dirs
```

It only kills instances *this benchmark* started (matched by our run/data dirs),
so unrelated Redis/MinIO servers on the host are never touched.

## Running the benchmark (`bench.py`)

`bench.py` measures producer→consumer **state delivery cost** for each approach.
Every approach implements the same `put(key, payload)` / `get(key)` pair, so the
timing loop is identical and the comparison is fair. It sweeps state size and an
optional reader fan-out, and reports p50 / p99 latency (µs) and throughput
(GiB/s) for PUT and GET separately. Backends whose client lib or endpoint is
unavailable are **skipped** with a message rather than failing the run.

```bash
# (optional) install clients for the external rows.  hiredis is REQUIRED for the
# redis rows — see "Fair large-value GET" below; without it those rows are skipped.
pip install redis hiredis minio

# (optional) source endpoints written by deploy_backends.sh
#   the script auto-reads ./backends.env if present

python3 bench.py                                   # all available approaches
python3 bench.py --approaches shm-copy shm-zerocopy
python3 bench.py --sizes 64 4096 1048576 --iters 500 --warmup 50
python3 bench.py --readers 8                       # fan-out: 1 put, N gets
python3 bench.py --csv results.csv                 # raw rows for plotting
python3 bench.py --max-bytes-per-size 0            # disable the budget (full iters)
```

**Per-size data budget.** Large payloads at full `--iters` move enormous
volumes (e.g. 256 MiB × 200 iters × 2 remote rows ≈ 220 GiB over the wire).
To keep a single run practical, `--max-bytes-per-size` (default **4 GiB**)
caps iterations per size: small payloads keep the full `--iters`, large ones
auto-scale down (e.g. 256 MiB → ~8 iters). The resolved per-size iteration
count is printed before the run and recorded in the CSV (`iters` column). Set
`--max-bytes-per-size 0` for unlimited.

> **Fair large-value GET (redis rows).** redis-py's *pure-Python* RESP parser
> reassembles a multi-MiB bulk reply in interpreted code (a `recv()` loop into a
> `BytesIO` + a final copy-out). That made Redis GET fall *behind* MinIO at
> ≥16 MiB — an artifact of the client library, not the store: MinIO's urllib3
> read path is C-accelerated, Redis's default was not. `bench.py` now **requires
> the `hiredis` C parser** and uses a 1 MiB socket read buffer, which parses the
> reply in C and cuts the `recv()` syscall count ~16×. Measured on one node:
> 16 MiB GET **0.21 → 0.47 GiB/s**, 128 MiB **0.18 → 0.34 GiB/s** — Redis is back
> to faster-than-S3 at every size, as an in-memory store should be. If `hiredis`
> is missing the redis rows are **skipped** (with a message) rather than reported
> unfairly. `redis-local` and `redis-remote` are affected identically, confirming
> the cost was client-side reassembly, not the network.

> **S3 storage medium.** Two S3 rows are measured: `s3` (MinIO data on `/tmp`,
> tmpfs/RAM) and `s3-disk` (MinIO data on `/var/tmp`, NVMe). The disk row is the
> realistic "external object storage" baseline; the RAM row is an optimistic
> upper bound that isolates the storage-medium cost. Override paths with
> `S3_DATADIR=` / `S3_DISK_DATADIR=` before `./deploy_backends.sh backend`.

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
