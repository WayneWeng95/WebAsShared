# StateSync-remote Micro-Benchmark

Measures **cross-node state-transfer latency**: moving one unit of state from a
producer on node A to a consumer on node B. This is the distributed counterpart
of `../StateSync-local` (which measured intra-node transfer). Where the local
study asked "what does it cost to hand state between two functions on one host,"
this one asks "what does it cost to deliver state between two machines."

## Approaches under test

| Approach (`id`) | Path A → B | Mechanism |
|-----------------|-----------|-----------|
| External storage, remote — disk (`s3-disk`) | A PUTs to MinIO (NVMe), B GETs | S3 / object-store chaining (disk) |
| External storage, remote — RAM (`s3`) | A PUTs to MinIO (tmpfs), B GETs | S3 / object-store chaining (RAM upper bound) |
| External in-memory, remote (`redis-remote`) | A SETs to Redis, B GETs | disaggregated KV state |
| Cloudburst LDPC, cold (`cloudburst-cold`) | consumer cache MISS → fetch from remote KV | Cloudburst-style KV (Anna≈Redis) + colocated cache, first/unique read |
| Cloudburst LDPC, warm (`cloudburst-warm`) | consumer cache HIT → local memory | same, but repeated/shared read served from the colocated cache |
| **Shared memory + RDMA (ours, `rdma-shm`)** | A RDMA-WRITEs into B's registered memory | system's `RemoteSend`/`RemoteRecv` (`connect::RdmaRemote`) |

The **Cloudburst** rows are a faithful *mechanism model* of LDPC (Logical
Disaggregation with Physical Colocation), not a real Anna/Cloudburst deployment:
state lives in a disaggregated KV (proxied by Redis) with a cache colocated with
the consumer. The **cold** row is the producer→consumer handoff (cache miss →
cross-node KV fetch, ≈ `redis-remote`); the **warm** row is a repeated/shared
read served from the local cache (the LDPC win — local memcpy, no network). This
shows Cloudburst's caching does *not* accelerate the first handoff but makes
subsequent reads local.

For the cross-node delivery a copy is inevitable (bytes must cross the wire); the
question is the *overhead beyond the raw transfer*: object-store/KV serialization
+ store stack vs. a single one-sided RDMA WRITE into peer memory.

## Topology (3-node)

| Node | Role here | IP |
|------|-----------|----|
| A | producer (compute) | `10.10.1.2` |
| B | consumer (compute) | `10.10.1.1` |
| C | disaggregated store: Redis + MinIO | `10.10.1.4` |

Store on a **separate node C** (the realistic disaggregated-state tier): both A
and B dial C, so the store rows traverse the network, while `rdma-shm` is a
direct A↔B one-sided write on the experiment NIC (`10.10.1.x`, RoCE `mlx4_0`).
`cloudburst-warm` is consumer-local (the colocated cache), so it needs no network
and no store.

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

## Running (3-node)

```bash
# 1. store on NODE C (10.10.1.4): Redis + BOTH MinIO instances (disk :9010, RAM :9000)
./deploy_backends.sh backend                    # all backends (no --no-s3)

# 2. point NODE A *and* NODE B at the store on C (write backends.env on both):
printf 'REDIS_HOST=10.10.1.4\nREDIS_PORT=6379\nS3_ENDPOINT=http://10.10.1.4:9000\nS3_DISK_ENDPOINT=http://10.10.1.4:9010\nS3_ACCESS_KEY=minioadmin\nS3_SECRET_KEY=minioadmin123\nS3_BUCKET=statesync\n' > backends.env

# All producers/clients UPSERT into one shared results.csv on node A (the default
# --csv), each replacing only its own approach's rows. Run them in any order.

# 3. store-based + cloudburst-cold rows — consumer on B, producer on A (one at a time):
#    NODE B (consumer):                              NODE A (producer):
./bench_remote.py consumer --approach s3-disk          ./bench_remote.py producer --approach s3-disk
./bench_remote.py consumer --approach s3               ./bench_remote.py producer --approach s3
./bench_remote.py consumer --approach redis-remote     ./bench_remote.py producer --approach redis-remote
./bench_remote.py consumer --approach cloudburst-cold  ./bench_remote.py producer --approach cloudburst-cold

# 4. cloudburst-warm row — NODE A only (colocated cache hit; no consumer/store):
./bench_remote.py producer --approach cloudburst-warm

# 5. RDMA row — server on B, client on A (A↔B direct; node C not involved):
#    NODE B:
../../../Executor/target/release/examples/rdma_latency server 7900
#    NODE A:
../../../Executor/target/release/examples/rdma_latency client 10.10.1.1 7900

# 6. plot — NODE A (reads the single results.csv, renders figs/)
./plot_remote.py
```

## Status

Harness complete and validated (RDMA loopback + Redis/Cloudburst loopback;
S3 wired). Real 3-node run collected for s3-disk, redis-remote, cloudburst, and
rdma-shm. `plot_remote.py` merges all `*_results.csv` into `results.csv` and
renders `figs/latency_throughput_remote.png` + `latency_remote_bars.png`.
