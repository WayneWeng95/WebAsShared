# RTSFaaS — single-machine baseline (working)

RTSFaaS (MorphStream FaaS branch) is designed for a **5-machine RDMA cluster +
TiKV**. This directory captures the working configuration to run it **on one
machine** (driver + database + worker + client as four privileged Docker
containers over the local RoCE NIC, in-memory store, no TiKV), so it can serve
as a live throughput baseline next to WebAsShared and Flink StateFun.

Verified on this host (Mellanox `mlx4_0` RoCE, 16 cores, 61 GB) — see measured
numbers in `../../../results_throughput_comparison.md`.

## Prerequisites

1. **Toolchain**: `openjdk-8-jdk`, `maven` (`apt-get install -y openjdk-8-jdk maven`).
2. **RDMA CM device**: the connection manager needs `/dev/infiniband/rdma_cm`:
   ```
   sudo modprobe rdma_ucm
   ```
3. **A RoCE/IB NIC with an IP and a resolvable hostname.** Here `mlx4_0` port 2 =
   `eno1d1` = `10.10.1.2`, which `/etc/hosts` resolves to `node-0-link-0`. RDMA
   cannot use `127.0.0.1`; the host string must match the **reverse-DNS name** of
   the NIC IP (see fix #5 below). Adjust `Env/Cluster.env` to your NIC's hostname.

## Required source patch (in the RTSFaaS repo)

`benchmark.rdma.RDMABenchmark`'s **database branch does not block** — it starts
the RDMA server then returns, so the JVM exits and the in-memory tables vanish
before workers connect. Add one line after "Database started":

```java
} else if (MorphStreamEnv.get().isDatabase()) {
    ...
    LOG.info("Database started in " + (end - start - 1) + " ms");
    Thread.currentThread().join();   // keep JVM alive to serve RDMA (single-node)
}
```

Then rebuild: `mvn -q -pl morph-clients -am package -DskipTests` and
`sudo docker build --platform=linux/amd64 -t rtfaas:1.0 .`

## What differs from the shipped cluster config (and why)

| # | Change | Reason |
|---|--------|--------|
| 1 | `modprobe rdma_ucm` | `rdma_create_event_channel` needs `/dev/infiniband/rdma_cm` |
| 2 | keep `isRDMA=1` | `isRDMA=0` is unwired — `MorphStreamDriver` always builds `RdmaDriverManager` (no socket path) |
| 3 | `MediaReview.env`: `keyNumberForEvents=1,1,1` (was `3`) | `mediaReview` spans 3 tables; the generator needs one key-count per table or `keyNumbers[i]` overflows |
| 4 | `Cluster.env`: `gatewayHost=localhost` | the client hardcodes `getSocket("localhost")`; the ZMQ socket must be keyed by that exact string |
| 5 | `Cluster.env`: `driverHost`/`workerHosts=node-0-link-0` (NIC hostname, not IP) | driver matches a worker by `workerHosts[i].equals(inetSocketAddress.getHostName())`, which is the **reverse-DNS name** of the NIC IP |
| 6 | `Database.env`: `isRemoteDB=1, isTiKV=0, isDynamoDB=0` + run the `database` role | RDMA worker requires a `RemoteStorageManager` (LocalStorageManager → ClassCast); `!TiKV && !DynamoDB` uses the built-in RDMA store — **no external TiKV needed** |
| 7 | role scripts: `-Xms2g -Xmx8g` (was `-Xms32g`) | 4 JVMs must fit in 61 GB |

`number` (System.env) and `qps` (workload env) set the event volume / input rate.

## Run

```
sudo modprobe rdma_ucm           # once per boot
sudo ./run-single-node.sh        # runs MediaReview then SocialNetwork, prints throughput/latency
```

The launcher starts driver → database → worker → client (with startup delays for
RDMA handshakes), waits for the client to finish, and prints:
- **client** end-to-end throughput (records/sec) + latency — the system-level number;
- **worker** per-executor engine throughput (k input_event/s) — TSTREAM's internal
  processing rate (in-memory, unthrottled);
- **driver** DAG completion rate + latency.

## Scoping / honesty

This is a **single-machine** RTSFaaS (1 worker, RoCE loopback, in-memory store) —
not its 5-node RDMA + TiKV design point, so absolute numbers are not the paper's.
It exists to give a live, same-host baseline; the engine (worker) throughput is an
in-cache microbenchmark rate, while the client end-to-end throughput reflects the
batch + driver coordination latency. State this when comparing.
