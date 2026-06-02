# StateSync-remote Micro-Benchmark

Measures **cross-node state-transfer latency**: moving one unit of state from a
producer on node A to a consumer on node B. This is the distributed counterpart
of `../StateSync-local` (which measured intra-node transfer). Where the local
study asked "what does it cost to hand state between two functions on one host,"
this one asks "what does it cost to deliver state between two machines."

## Approaches under test (3)

| # | Approach (`id`) | Path A → B | Copy? | Representative |
|---|-----------------|-----------|-------|----------------|
| 1 | External storage, remote (`s3-disk`) | A PUTs to MinIO (disk), B GETs | serialize + 2× store I/O | S3 / object-store chaining |
| 2 | External in-memory, remote (`redis-remote`) | A SETs to Redis, B GETs | serialize + KV store | disaggregated KV state |
| 3 | **Shared memory + RDMA (ours, `rdma-shm`)** | A RDMA-WRITEs into B's registered memory | one memcpy into the MR (unavoidable cross-machine) | system's `RemoteSend`/`RemoteRecv` (`connect::RdmaRemote`) |

Unlike the local study, **a copy is inevitable for all three** — the bytes
must physically cross the wire. The question is how much *overhead beyond the
raw transfer* each mechanism adds: object-store/KV serialization + store
software stack vs. a single one-sided RDMA WRITE straight into peer memory.

## Topology

From `NodeAgent/agent_*.toml` (`cluster.ips = ["10.10.1.2", "10.10.1.1"]`):

| Node | Role here | IP |
|------|-----------|----|
| A | producer (compute) | `10.10.1.2` |
| B | consumer + store host | `10.10.1.1` |

The S3/Redis backends live on **node B** (the consumer), so the producer→store
write is the single network traversal and the consumer reads locally — one wire
crossing, directly comparable to RDMA's one-sided A→B write. The RDMA path uses
the experiment NIC (`10.10.1.x`, RoCE on `mlx4_0`) directly.

## Reused tooling

`deploy_backends.sh` and `reset.sh` are copied from `StateSync-local`. The
remote study only needs the disk S3 and Redis, so bring them up on node B with:

```bash
./deploy_backends.sh backend --no-s3      # Redis + S3(disk); skips the RAM S3 row
```

The RDMA approach needs no daemon — it uses the cluster's RDMA fabric directly
via a small harness built on `connect::RdmaRemote` (see below).

## Measurement model

Latency is a **ping-pong round-trip on node A**, reported as one-way = RTT / 2
(the `ib_write_lat` convention) — this avoids cross-machine clock-sync error:

```
A: t0 = now();  send(payload) → B;  wait(echo) ← B;  rtt = now() - t0
B: wait(payload) ← A;  send(echo) → A
one-way latency = rtt / 2
```

This is not a pure-network test: each transfer includes the mechanism's real
control overhead, so the figures show end-to-end *execution behavior* of all
three methods, not just wire time.

- **rdma-shm**: `RdmaRemote::write_state` (A→B) then `wait_peer_write`; B echoes.
  Includes the MR memcpy + one-sided RDMA WRITE + the TCP done-signal the real
  `RemoteSend` protocol uses (kept on purpose).
- **redis-remote**: blocking-list round-trip (A `RPUSH` req / `BLPOP` resp;
  B `BLPOP` req / `RPUSH` resp) — no polling.
- **s3-disk**: sequence-keyed objects with a polling GET (S3 has no blocking
  notify). The poll jitter is kept — it is part of S3's real overhead.

Swept over 16 KiB / 1 MiB / 16 MiB / 128 MiB (`SIZES` in both harnesses — keep
them identical so the ping-pong stays in lockstep), 30 iters + 5 warmup, mean /
p50 / p99 and GiB/s. CSV schema (shared by both harnesses):
`approach,size_bytes,iters,lat_mean_us,lat_p50_us,lat_p99_us,gibps`.

## Running

```bash
# 1. backends on node B (10.10.1.1): Redis + disk S3 only
./deploy_backends.sh backend --no-s3            # skips the RAM-S3 row

# 2. point node A's harness at node B (write backends.env on node A)
cat > backends.env <<'EOF'
REDIS_HOST=10.10.1.1
REDIS_PORT=6379
S3_DISK_ENDPOINT=http://10.10.1.1:9010
S3_ACCESS_KEY=minioadmin
S3_SECRET_KEY=minioadmin123
S3_BUCKET=statesync
EOF

# 3. store-based rows — consumer on B, producer on A (one approach at a time):
#    node B:                                         node A:
./bench_remote.py consumer --approach redis-remote   # then on A:
./bench_remote.py producer --approach redis-remote --csv redis_results.csv
./bench_remote.py consumer --approach s3-disk        # then on A:
./bench_remote.py producer --approach s3-disk    --csv s3_results.csv

# 4. RDMA row — server on B, client on A (built in the Executor workspace):
cd ../../../Executor && cargo build --release -p connect --example rdma_latency
#    node B:
./target/release/examples/rdma_latency server 7900
#    node A:
./target/release/examples/rdma_latency client 10.10.1.1 7900 \
    --csv ../Tests/Micro-Benchmarks/StateSync-remote/rdma_results.csv

# 5. plot all three (reads *_results.csv in cwd)
./plot_remote.py            # -> figs/latency_remote.png, throughput_remote.png, bars
```

## Status

Harness complete and smoke-tested: `rdma_latency` (Rust, validated on RDMA
loopback), `bench_remote.py` (Redis path validated on loopback; S3 path wired),
and `plot_remote.py`. Pending: a real two-node run on the cluster to collect
`*_results.csv`, then `plot_remote.py`.
