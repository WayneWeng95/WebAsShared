#!/usr/bin/env python3
"""StateSync-remote — cross-node state-transfer latency for the store-based rows.

Measures the `redis-remote` and `s3-disk` approaches as a ping-pong round-trip
between two nodes, reported as one-way latency = RTT/2 (the rdma_latency Rust
harness handles the `rdma-shm` row with the same CSV schema).

Run the consumer on node B first, then the producer on node A — one approach
per invocation so the two sides stay in lockstep:

    # node B (consumer / store host):
    python3 bench_remote.py consumer --approach redis-remote
    # node A (producer):
    python3 bench_remote.py producer --approach redis-remote --csv redis_results.csv

    # then repeat for s3-disk:
    python3 bench_remote.py consumer --approach s3-disk
    python3 bench_remote.py producer --approach s3-disk --csv s3_results.csv

Endpoints come from backends.env (written by deploy_backends.sh) / env / defaults.

Channels
--------
redis-remote : blocking lists (FIFO) — producer RPUSH req / BLPOP resp,
               consumer BLPOP req / RPUSH resp.  No polling.
s3-disk      : sequence-keyed objects with a polling GET (S3 has no blocking
               notify).  The poll jitter IS part of S3's real overhead, so we
               keep it (per design).  Objects are deleted after use to bound the
               bucket.
"""

import argparse
import os
import statistics
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ENV_FILE = os.path.join(HERE, "backends.env")

# Size schedule — must match rdma_latency.rs and StateSync-local.
SIZES = [16 * 1024, 1024 * 1024, 16 * 1024 * 1024, 128 * 1024 * 1024]
ITERS = 30
WARMUP = 5
S3_POLL_S = 0.001          # inter-poll sleep for the s3 consumer/producer wait


def load_env():
    cfg = {
        "REDIS_HOST": "127.0.0.1", "REDIS_PORT": "6379", "REDIS_PASS": "",
        "CB_CONSUMER_HOST": "127.0.0.1",   # Cloudburst cache on consumer node (warm)
        "CB_PRODUCER_HOST": "127.0.0.1",   # Cloudburst cache on producer node (cold)
        "S3_ENDPOINT": "", "S3_DISK_ENDPOINT": "", "S3_ACCESS_KEY": "minioadmin",
        "S3_SECRET_KEY": "minioadmin123", "S3_BUCKET": "statesync",
    }
    if os.path.exists(ENV_FILE):
        for line in open(ENV_FILE):
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                k, v = line.split("=", 1)
                cfg[k.strip()] = v.strip()
    cfg.update({k: os.environ[k] for k in cfg if k in os.environ})
    return cfg


# ── Shared Redis client builder ───────────────────────────────────────────────
def make_redis(host, port, password):
    """Build a redis-py client configured for a FAIR large-value comparison.

    redis-py's pure-Python RESP parser reassembles a multi-MiB bulk reply in
    interpreted code, which inflates large-value GET/BLPOP and lets Redis
    unfairly top the chart over MinIO (whose urllib3 read path is C-accelerated).
    We REQUIRE the `hiredis` C parser (redis-py auto-selects it once installed)
    and a 1 MiB socket read buffer so the figures reflect the *store*, not the
    client library.  Without hiredis the row is skipped rather than reported
    unfairly.
    """
    import redis
    try:
        import hiredis  # noqa: F401
    except ImportError as e:
        raise RuntimeError(
            f"hiredis not installed ({e}); the Redis/Cloudburst rows would be "
            f"client-parser-bound and unfair -> pip install hiredis")
    pool = redis.ConnectionPool(host=host, port=int(port),
                                password=password or None,
                                socket_timeout=30, socket_read_size=1 << 20)
    return redis.Redis(connection_pool=pool)


# ── Redis channel (blocking lists) ────────────────────────────────────────────
class RedisChannel:
    REQ, RESP = "ss:req", "ss:resp"

    def __init__(self, cfg):
        self.r = make_redis(cfg["REDIS_HOST"], cfg["REDIS_PORT"], cfg["REDIS_PASS"])
        self.r.ping()

    def reset(self):
        self.r.delete(self.REQ, self.RESP)

    def deliver_us(self, payload, seq):
        t0 = time.perf_counter_ns()
        self.r.rpush(self.REQ, payload)
        self.r.blpop(self.RESP, timeout=30)
        rtt = (time.perf_counter_ns() - t0) / 1000.0
        return rtt / 2.0                                # one-way

    def consumer_serve(self, seq):
        item = self.r.blpop(self.REQ, timeout=60)
        if item is None:
            raise TimeoutError("consumer timed out waiting for req")
        self.r.rpush(self.RESP, item[1])


# ── S3 channel (sequence-keyed objects + polling) ─────────────────────────────
class S3Channel:
    def __init__(self, cfg, endpoint_key="S3_DISK_ENDPOINT"):
        from minio import Minio
        ep = cfg[endpoint_key]
        if not ep:
            raise RuntimeError(f"{endpoint_key} not set (deploy_backends.sh backend)")
        self.bucket = cfg["S3_BUCKET"]
        secure = ep.startswith("https://")
        host = ep.split("://", 1)[1] if "://" in ep else ep
        self.c = Minio(host, access_key=cfg["S3_ACCESS_KEY"],
                       secret_key=cfg["S3_SECRET_KEY"], secure=secure)
        if not self.c.bucket_exists(self.bucket):
            self.c.make_bucket(self.bucket)

    def reset(self):
        try:
            objs = self.c.list_objects(self.bucket, prefix="ss/", recursive=True)
            for o in objs:
                self.c.remove_object(self.bucket, o.object_name)
        except Exception:
            pass

    def _get(self, key):
        from minio.error import S3Error
        try:
            resp = self.c.get_object(self.bucket, key)
            try:
                return resp.read()
            finally:
                resp.close(); resp.release_conn()
        except S3Error:
            return None

    def _put(self, key, data):
        import io
        self.c.put_object(self.bucket, key, io.BytesIO(data), length=len(data))

    def deliver_us(self, payload, seq):
        import io
        rk, sk = f"ss/req/{seq}", f"ss/resp/{seq}"
        t0 = time.perf_counter_ns()
        self.c.put_object(self.bucket, rk, io.BytesIO(payload), length=len(payload))
        while True:                       # poll for the consumer's echo
            data = self._get(sk)
            if data is not None:
                break
            time.sleep(S3_POLL_S)
        rtt = (time.perf_counter_ns() - t0) / 1000.0
        try:
            self.c.remove_object(self.bucket, sk)   # untimed cleanup
        except Exception:
            pass
        return rtt / 2.0                  # one-way

    def consumer_serve(self, seq):
        rk, sk = f"ss/req/{seq}", f"ss/resp/{seq}"
        while True:
            data = self._get(rk)
            if data is not None:
                break
            time.sleep(S3_POLL_S)
        self._put(sk, data)
        try:
            self.c.remove_object(self.bucket, rk)
        except Exception:
            pass


# ── Cloudburst LDPC model — cache placement (consumer vs producer node) ───────
class CloudburstChannel:
    """Mechanism model of Cloudburst's LDPC (Logical Disaggregation with Physical
    Colocation). All three KV approaches are Redis ping-pongs that differ only in
    WHERE the cache/KV lives relative to the two compute nodes:

      cloudburst-warm : Redis colocated on the CONSUMER node (`CB_CONSUMER_HOST`)
                        — producer writes over the network, consumer reads local.
      cloudburst-cold : Redis colocated on the PRODUCER node (`CB_PRODUCER_HOST`)
                        — producer writes local, consumer fetches over network.
      redis-remote    : Redis on a separate THIRD node (`REDIS_HOST`) — both
                        producer and consumer cross the network (two-hop).

    warm and cold are single-hop (one side local, one side remote); redis-remote
    is two-hop. Models the LDPC placement effect, not real Anna.
    """
    REQ, RESP = "cb:req", "cb:resp"

    def __init__(self, cfg, warm):
        self.warm = warm
        # warm: cache on consumer node; cold: cache on producer node.
        host = cfg["CB_CONSUMER_HOST"] if warm else cfg["CB_PRODUCER_HOST"]
        self.r = make_redis(host, cfg["REDIS_PORT"], cfg["REDIS_PASS"])
        self.r.ping()

    def reset(self):
        self.r.delete(self.REQ, self.RESP)

    def deliver_us(self, payload, seq):
        t0 = time.perf_counter_ns()
        self.r.rpush(self.REQ, payload)
        self.r.blpop(self.RESP, timeout=30)
        return (time.perf_counter_ns() - t0) / 1000.0 / 2.0   # one-way

    def consumer_serve(self, seq):
        item = self.r.blpop(self.REQ, timeout=60)
        if item is None:
            raise TimeoutError("cloudburst consumer timed out")
        self.r.rpush(self.RESP, item[1])


# Registry of channel constructors (cfg -> channel).
CHANNELS = {
    "redis-remote":    lambda cfg: RedisChannel(cfg),
    "s3-disk":         lambda cfg: S3Channel(cfg, "S3_DISK_ENDPOINT"),
    "s3":              lambda cfg: S3Channel(cfg, "S3_ENDPOINT"),
    "cloudburst-cold": lambda cfg: CloudburstChannel(cfg, warm=False),
    "cloudburst-warm": lambda cfg: CloudburstChannel(cfg, warm=True),
}


def pct(xs, p):
    s = sorted(xs)
    k = max(0, min(len(s) - 1, int(round((p / 100.0) * (len(s) - 1)))))
    return s[k]


def fmt_size(n):
    if n < 1024: return f"{n}B"
    if n < 1024 ** 2: return f"{n // 1024}KiB"
    if n < 1024 ** 3: return f"{n // 1024 ** 2}MiB"
    return f"{n // 1024 ** 3}GiB"


HEADER = ["approach", "size_bytes", "iters",
          "lat_mean_us", "lat_p50_us", "lat_p99_us", "gibps"]


def upsert_rows(csv_path, approach, rows):
    """Replace this approach's rows in csv_path (keeping all others), so every
    run accumulates into one shared results.csv idempotently."""
    import csv
    kept = []
    if os.path.exists(csv_path):
        with open(csv_path, newline="") as fh:
            rd = csv.reader(fh)
            next(rd, None)                         # skip header
            kept = [r for r in rd if r and r[0] != approach]
    with open(csv_path, "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(HEADER)
        w.writerows(kept)
        w.writerows(rows)
    print(f"[bench_remote] upserted {len(rows)} '{approach}' rows -> {csv_path} "
          f"({len(kept) + len(rows)} total)")


def run_producer(ch, approach, csv_path):
    GiB = 1024 ** 3
    rows = []
    print(f"{approach} — one-way latency")
    print(f"{'size':>8}  {'mean us':>12}{'p50 us':>12}{'p99 us':>12}  {'GiB/s':>10}")
    seq = 0
    for size in SIZES:
        payload = os.urandom(size)
        for _ in range(WARMUP):
            ch.deliver_us(payload, seq); seq += 1
        oneway = []
        for _ in range(ITERS):
            oneway.append(ch.deliver_us(payload, seq)); seq += 1
        mean = statistics.fmean(oneway)
        p50, p99 = pct(oneway, 50), pct(oneway, 99)
        gibps = size / (mean * 1e-6) / GiB
        print(f"{fmt_size(size):>8}  {mean:>12.2f}{p50:>12.2f}{p99:>12.2f}  {gibps:>10.3f}")
        rows.append((approach, size, ITERS, mean, p50, p99, gibps))

    if csv_path:
        upsert_rows(csv_path, approach, rows)


def run_consumer(ch, approach):
    total = len(SIZES) * (WARMUP + ITERS)
    print(f"{approach} consumer — echoing {total} transfers")
    for seq in range(total):
        ch.consumer_serve(seq)
    print(f"[bench_remote] consumer done ({total} transfers)")


def main():
    ap = argparse.ArgumentParser(description="StateSync-remote store-based latency")
    ap.add_argument("role", choices=["producer", "consumer"])
    ap.add_argument("--approach", required=True, choices=list(CHANNELS))
    ap.add_argument("--csv", default="results.csv",
                    help="(producer) upsert results into this shared CSV (default results.csv)")
    args = ap.parse_args()

    cfg = load_env()
    if args.approach == "cloudburst-warm":
        target = f"cache on consumer node {cfg['CB_CONSUMER_HOST']}:{cfg['REDIS_PORT']} (1-hop)"
    elif args.approach == "cloudburst-cold":
        target = f"cache on producer node {cfg['CB_PRODUCER_HOST']}:{cfg['REDIS_PORT']} (1-hop)"
    elif args.approach == "redis-remote":
        target = f"KV {cfg['REDIS_HOST']}:{cfg['REDIS_PORT']} (2-hop)"
    elif args.approach == "s3":
        target = f"s3-ram {cfg['S3_ENDPOINT'] or '(unset)'}"
    else:
        target = f"s3-disk {cfg['S3_DISK_ENDPOINT'] or '(unset)'}"
    src = "backends.env" if os.path.exists(ENV_FILE) else "defaults (no backends.env!)"
    print(f"[bench_remote] {args.approach} -> {target}   [from {src}]")
    try:
        ch = CHANNELS[args.approach](cfg)
    except Exception as e:
        raise SystemExit(
            f"[bench_remote] cannot reach the backend ({e}).\n"
            f"  - is the backend running on node B?   ./deploy_backends.sh status\n"
            f"  - does backends.env point at node B?  REDIS_HOST / S3_DISK_ENDPOINT\n"
            f"  resolved target: {target}")
    ch.reset()   # clear any stale req/resp from a previous run
    if args.role == "producer":
        run_producer(ch, args.approach, args.csv)
    else:
        run_consumer(ch, args.approach)


if __name__ == "__main__":
    main()
