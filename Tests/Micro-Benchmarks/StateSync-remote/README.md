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
| Cloudburst LDPC, cold (`cloudburst-cold`) | Redis on the **producer** node A; consumer fetches remote | cache colocated with producer (1-hop) |
| Cloudburst LDPC, warm (`cloudburst-warm`) | Redis on the **consumer** node B; producer writes remote | cache colocated with consumer (1-hop) |
| **Shared memory + RDMA (ours, `rdma-shm`)** | A RDMA-WRITEs into B's registered memory | system's `RemoteSend`/`RemoteRecv` (`connect::RdmaRemote`) |

The three KV approaches are all Redis ping-pongs that differ only in **where the
Redis lives** relative to the two compute nodes (a *mechanism model* of LDPC,
not real Anna):
- **`redis-remote`** — Redis on a separate **third node C**; both producer and
  consumer cross the network (**two-hop**).
- **`cloudburst-cold`** — Redis on the **producer node A**; the producer writes
  locally, the consumer fetches over the network (**one-hop**).
- **`cloudburst-warm`** — Redis on the **consumer node B**; the producer writes
  over the network, the consumer reads locally (**one-hop**).

So colocating the cache on either compute node (cold/warm) saves one of the two
network crossings that the fully disaggregated `redis-remote` pays.

For the cross-node delivery a copy is inevitable (bytes must cross the wire); the
question is the *overhead beyond the raw transfer*: object-store/KV serialization
+ store stack vs. a single one-sided RDMA WRITE into peer memory.

## Topology (3-node)

| Node | Role here | IP | Runs Redis for |
|------|-----------|----|----------------|
| A | producer (compute) | `10.10.1.2` | `cloudburst-cold` (`CB_PRODUCER_HOST`) |
| B | consumer (compute) | `10.10.1.1` | `cloudburst-warm` (`CB_CONSUMER_HOST`) |
| C | disaggregated store | `10.10.1.4` | `redis-remote` + MinIO (`s3`, `s3-disk`) |

Three Redis instances, one per node, model the three cache placements:
`redis-remote` → node C (two-hop), `cloudburst-cold` → node A / producer (one-hop),
`cloudburst-warm` → node B / consumer (one-hop). `rdma-shm` is a direct A↔B
one-sided write on the experiment NIC (`10.10.1.x`, RoCE `mlx4_0`).

## Reused tooling

`deploy_backends.sh` and `reset.sh` are copied from `StateSync-local`. The
remote study only needs the disk S3 and Redis, so bring them up on node B with:

```bash
./deploy_backends.sh backend --no-s3      # Redis + S3(disk); skips the RAM S3 row
```

The RDMA approach needs no daemon — it uses the cluster's RDMA fabric directly
via a small harness built on `connect::RdmaRemote` (see below).

> **Fair large-value transfer (redis/cloudburst rows).** Install `hiredis`
> alongside `redis` (`pip install redis hiredis minio`). The blocking-list
> ping-pong carries the payload as a multi-MiB list element; redis-py's
> pure-Python RESP parser reassembles it in interpreted code and unfairly
> inflates the Redis/Cloudburst rows at ≥16 MiB. `bench_remote.py` now requires
> the `hiredis` C parser + a 1 MiB socket read buffer (see the same note in
> `../StateSync-local/README.md`); without `hiredis` those rows are skipped.

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
# 1. one Redis per node + the object stores:
#    NODE C (10.10.1.4): disaggregated Redis + BOTH MinIO (disk :9010, RAM :9000)
./deploy_backends.sh backend                                  # all backends (no --no-s3)
#    NODE A (10.10.1.2): Redis for cloudburst-cold (producer-colocated cache)
./deploy_backends.sh up --bind 10.10.1.2 --no-s3 --no-s3-disk --no-env
#    NODE B (10.10.1.1): Redis for cloudburst-warm (consumer-colocated cache)
./deploy_backends.sh up --bind 10.10.1.1 --no-s3 --no-s3-disk --no-env

# 2. point NODE A *and* NODE B at all stores. WRITE THIS AFTER step 1 — `deploy up`
#    without --no-env (and `reset.sh`) overwrite backends.env. The grouped-echo
#    form is paste-safe (a long printf can line-wrap and corrupt keys):
{ echo REDIS_HOST=10.10.1.4; echo REDIS_PORT=6379; \
  echo CB_CONSUMER_HOST=10.10.1.1; echo CB_PRODUCER_HOST=10.10.1.2; \
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

## Multi-node bandwidth (`rdma_bw`)

`rdma_latency` measures a **ping-pong RTT/2** — its GiB/s is dragged below the
wire because each transfer pays a host memcpy + a TCP done-signal in the timed
path. `rdma_bw` is the bandwidth companion: it streams many **pipelined,
one-directional** one-sided RDMA WRITEs (the `ib_write_bw` model — no per-write
memcpy or TCP), so the number reflects the wire. It is built on the same
`connect::RdmaRemote` primitive as the `rdma-shm` row.

> **Hardware ceiling (measured on this cluster).** Each node has **one 10 GbE
> experiment port** (ConnectX-3 Pro, `eno1d1`; `ethtool` tops out at
> `10000baseKR` — there is no 40 G mode). So a single link maxes at **~1.13
> GiB/s**, and the only way to show bandwidth *scaling* is multiple links whose
> ports don't overlap. Loopback `ib_write_bw` and `rdma_bw` both hit ~3.8 GiB/s
> (intra-HCA, PCIe-bound) — confirming the verbs path is not the limit.

### Network lanes (both ports of the ConnectX-3 Pro)

The card (`mlx4_0`) exposes **two physical 10 GbE ports**, but only one is a
usable experiment fabric:

| | Lane A | Lane B |
|---|---|---|
| netdev | `eno1` | `eno1d1` |
| RDMA port | port 1 | **port 2** ← `rdma_bw` / `rdma_latency` use this (`RDMA_PORT=2`, `GID_INDEX=2`) |
| IP / subnet | `128.110.218.1/21` | `10.10.1.2/24` |
| role | control / management (SSH) | experiment fabric |
| link mode | `10000baseKR`, ACTIVE/LinkUp | `10000baseKR`, ACTIVE/LinkUp |
| wire speed | **10 Gb/s (~1.13 GiB/s)** | **10 Gb/s (~1.13 GiB/s)** |
| RDMA loopback | 4.30 GiB/s (verbs/PCIe, intra-HCA) | 3.93 GiB/s (verbs/PCIe, intra-HCA) |
| peers | 128.110.x (routed) | `10.10.1.{1,3,4}` all reachable |

Both lanes are genuine 10 GbE — `ethtool` advertises nothing above `10000baseKR`
on either, so neither has a 40 G mode. The two ports share one **PCIe 3.0 x8
(~63 Gbps)** link, which has ample headroom for both at once (~20 Gbps ≈ 2.26
GiB/s aggregate; one port already pushed ~31 Gbps in loopback) — the card is not
the bottleneck. **But Lane A is the CloudLab control/management network**
(different subnet, routed, carries SSH), not a clean isolated data fabric, so in
practice each node has **one** experiment lane. To truly double per-node
bandwidth you'd need CloudLab to provision a *second experiment LAN* and bind
RDMA port 1 to it; absent that, per-node bandwidth is capped at one 10 GbE lane
and aggregate scaling comes from **multiple nodes** (the disjoint-pairs pattern
below), not dual-lane on a single node.

> **What actually sets the ceiling.** It is the **10 GbE link rate of a single
> port (~1.13 GiB/s)** — the only thing in the whole chain running at 10 Gb/s.
> Everything upstream is far faster: PCIe 3.0 x8 (~63 Gbps) and the HCA/verbs
> path (~31 Gbps, 3.8 GiB/s loopback). The *lanes* (the two ports) do not raise
> or lower that per-link ceiling — they only set how many 10 G ceilings can run
> in parallel (1 port ≈ 1.13 GiB/s; 2 ports ≈ 2.26 GiB/s aggregate, PCIe
> permitting). So a single link can't go faster than its port's link rate; the
> only way to grow aggregate bandwidth is to **add more independent 10 G links
> across nodes** — each its own 1.13 GiB/s ceiling, and they sum. In short: the
> port's link rate creates the ceiling; the number of lanes/nodes just
> multiplies how many of those ceilings run at once.

Build once: `cargo build --release -p connect --example rdma_bw`
(binary at `../../../Executor/target/release/examples/rdma_bw`).

Two composable roles — `recv` (sink) and `send` (source, one stream per IP,
reports per-link + aggregate GiB/s, upserts into a shared `bw.csv` by `--tag`):

```bash
BW=../../../Executor/target/release/examples/rdma_bw

# (1) pair — single-link line rate (expect ≈ 1.0–1.1 GiB/s, ~80–95% of 10 GbE)
#   NODE B:  $BW recv 7900
#   NODE A:  $BW send 10.10.1.1 --tag pair --csv bw.csv

# (2) fan-out 1->N — one sender's egress port shared across N peers
#     (aggregate stays ≈ one port; each peer ≈ 1/N)
#   NODES B,C,D:  $BW recv 7900
#   NODE  A:      $BW send 10.10.1.1 10.10.1.3 10.10.1.4 --tag fanout3 --csv bw.csv

# (3) incast N->1 — N senders into one receiver's ingress port
#     (each sender ≈ 1/N; SUM ≈ one port). Receiver accepts N on one port:
#   NODE B:        $BW recv 7900 3
#   NODES A,C,D:   $BW send 10.10.1.1 --tag incast --csv bw.csv
#     (each sender writes its own 'incast'+host row; combine by summing per-size)

# (4) disjoint pairs — the SCALING result: independent pairs on distinct nodes
#     don't share a port, so aggregate ≈ P × single-link. Run two pairs at once:
#   NODE B: $BW recv 7900     NODE D: $BW recv 7900
#   NODE A: $BW send 10.10.1.1 --tag pairs2 --csv bw.csv
#   NODE C: $BW send 10.10.1.4 --tag pairs2 --csv bw.csv   # sum the two ≈ 2.2 GiB/s

# plot (NODE A): aggregate + per-link GiB/s vs size, with the 10 GbE ceiling line
./plot_bw.py --csv bw.csv     # -> ../../Figures/bandwidth_remote.pdf, bandwidth_per_link.pdf
```

The story the figures tell: **fan-out / incast are capped at one port** (≈1.1
GiB/s — the bottleneck is the shared node's NIC), while **disjoint pairs scale
linearly** (each added pair adds ~1.1 GiB/s of fabric bandwidth). That contrast
— not a faster single link — is how RDMA aggregate bandwidth grows with nodes on
a 10 GbE-per-port fabric.

## Status

Latency harness complete and validated (RDMA loopback + Redis/Cloudburst
loopback; S3 wired). Real 3-node run collected for s3-disk, redis-remote,
cloudburst, and rdma-shm. `plot_remote.py` merges all `*_results.csv` into
`results.csv` and renders `figs/latency_throughput_remote.png` +
`latency_remote_bars.png`.

Bandwidth harness (`rdma_bw` + `plot_bw.py`) built and validated on a single
node: 1-stream loopback = 3.81 GiB/s @128 MiB (matches `ib_write_bw` 3.83); a
2-stream run shares the HCA at 4.06 GiB/s aggregate / 2.03 each (the shared-port
behavior fan-out will show cross-node). Ready for the 4-node run.
