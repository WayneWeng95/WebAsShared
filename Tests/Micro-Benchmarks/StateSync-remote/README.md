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
| Cloudburst LDPC, cold (`cloudburst-cold`) | A → KV on node C → B (both cross network) | cache MISS → disaggregated KV (≈ `redis-remote`) |
| Cloudburst LDPC, warm (`cloudburst-warm`) | A → cache on node B → B reads locally | cache HIT → cache colocated with the consumer |
| **Shared memory + RDMA (ours, `rdma-shm`)** | A RDMA-WRITEs into B's registered memory | system's `RemoteSend`/`RemoteRecv` (`connect::RdmaRemote`) |

The **Cloudburst** rows are a faithful *mechanism model* of LDPC (Logical
Disaggregation with Physical Colocation), not a real Anna/Cloudburst deployment.
Both are Redis ping-pongs that differ only in **where the cache lives**:
- **cold** — cache miss: state is read from the *disaggregated* KV on node C, so
  both producer and consumer cross the network (≈ `redis-remote`).
- **warm** — cache hit: the cache (Redis) is *colocated on the consumer node B*.
  The producer pushes state to it over the network (one hop), and the consumer
  reads from its **local** Redis. Removing the consumer's network round-trip is
  the LDPC win — warm beats `redis-remote`/cold by exactly the consumer's saved
  hop.

For the cross-node delivery a copy is inevitable (bytes must cross the wire); the
question is the *overhead beyond the raw transfer*: object-store/KV serialization
+ store stack vs. a single one-sided RDMA WRITE into peer memory.

## Topology (3-node)

| Node | Role here | IP |
|------|-----------|----|
| A | producer (compute) | `10.10.1.2` |
| B | consumer (compute) + Cloudburst colocated cache (Redis) | `10.10.1.1` |
| C | disaggregated store: Redis + MinIO | `10.10.1.4` |

Store on a **separate node C** (the realistic disaggregated-state tier): the
`s3*`, `redis-remote`, and `cloudburst-cold` rows dial C, so they traverse the
network on both sides. `cloudburst-warm` dials a **Redis colocated on node B**
(`CB_CACHE_HOST`), so the consumer's reads stay on-node. `rdma-shm` is a direct
A↔B one-sided write on the experiment NIC (`10.10.1.x`, RoCE `mlx4_0`).

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
# 1a. disaggregated store on NODE C (10.10.1.4): Redis + BOTH MinIO (disk :9010, RAM :9000)
./deploy_backends.sh backend                    # all backends (no --no-s3)
# 1b. Cloudburst colocated cache on NODE B (10.10.1.1): just Redis, bound to B
./deploy_backends.sh up --bind 10.10.1.1 --no-s3 --no-s3-disk

# 2. point NODE A *and* NODE B at the stores (write backends.env on both; the
#    grouped-echo form is paste-safe — a long printf can line-wrap and corrupt keys):
{ echo REDIS_HOST=10.10.1.4; echo REDIS_PORT=6379; echo CB_CACHE_HOST=10.10.1.1; \
  echo S3_ENDPOINT=http://10.10.1.4:9000; echo S3_DISK_ENDPOINT=http://10.10.1.4:9010; \
  echo S3_ACCESS_KEY=minioadmin; echo S3_SECRET_KEY=minioadmin123; echo S3_BUCKET=statesync; } > backends.env

# All producers/clients UPSERT into one shared results.csv on node A (the default
# --csv), each replacing only its own approach's rows. Run them in any order.

# 3. all store/KV/cloudburst rows — consumer on B, producer on A (one at a time):
#    NODE B (consumer):                              NODE A (producer):
./bench_remote.py consumer --approach s3-disk          ./bench_remote.py producer --approach s3-disk
./bench_remote.py consumer --approach s3               ./bench_remote.py producer --approach s3
./bench_remote.py consumer --approach redis-remote     ./bench_remote.py producer --approach redis-remote
./bench_remote.py consumer --approach cloudburst-cold  ./bench_remote.py producer --approach cloudburst-cold
./bench_remote.py consumer --approach cloudburst-warm  ./bench_remote.py producer --approach cloudburst-warm

# 4. RDMA row — server on B, client on A (A↔B direct; node C not involved):
#    NODE B:
../../../Executor/target/release/examples/rdma_latency server 7900
#    NODE A:
../../../Executor/target/release/examples/rdma_latency client 10.10.1.1 7900

# 5. plot — NODE A (reads the single results.csv, renders figs/)
./plot_remote.py
```

## Status

Harness complete and validated (RDMA loopback + Redis/Cloudburst loopback;
S3 wired). Real 3-node run collected for s3-disk, redis-remote, cloudburst, and
rdma-shm. `plot_remote.py` merges all `*_results.csv` into `results.csv` and
renders `figs/latency_throughput_remote.png` + `latency_remote_bars.png`.
