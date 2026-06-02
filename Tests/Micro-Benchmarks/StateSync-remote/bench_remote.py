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
        "S3_DISK_ENDPOINT": "", "S3_ACCESS_KEY": "minioadmin",
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


# ── Redis channel (blocking lists) ────────────────────────────────────────────
class RedisChannel:
    REQ, RESP = "ss:req", "ss:resp"

    def __init__(self, cfg):
        import redis
        self.r = redis.Redis(host=cfg["REDIS_HOST"], port=int(cfg["REDIS_PORT"]),
                             password=cfg["REDIS_PASS"] or None, socket_timeout=30)
        self.r.ping()

    def reset(self):
        self.r.delete(self.REQ, self.RESP)

    def producer_rtt(self, payload, seq):
        t0 = time.perf_counter_ns()
        self.r.rpush(self.REQ, payload)
        self.r.blpop(self.RESP, timeout=30)
        return (time.perf_counter_ns() - t0) / 1000.0   # µs round-trip

    def consumer_serve(self, seq):
        item = self.r.blpop(self.REQ, timeout=60)
        if item is None:
            raise TimeoutError("consumer timed out waiting for req")
        self.r.rpush(self.RESP, item[1])


# ── S3 channel (sequence-keyed objects + polling) ─────────────────────────────
class S3Channel:
    def __init__(self, cfg):
        from minio import Minio
        ep = cfg["S3_DISK_ENDPOINT"]
        if not ep:
            raise RuntimeError("S3_DISK_ENDPOINT not set (deploy_backends.sh backend)")
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

    def producer_rtt(self, payload, seq):
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
        return rtt

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


CHANNELS = {"redis-remote": RedisChannel, "s3-disk": S3Channel}


def pct(xs, p):
    s = sorted(xs)
    k = max(0, min(len(s) - 1, int(round((p / 100.0) * (len(s) - 1)))))
    return s[k]


def fmt_size(n):
    if n < 1024: return f"{n}B"
    if n < 1024 ** 2: return f"{n // 1024}KiB"
    if n < 1024 ** 3: return f"{n // 1024 ** 2}MiB"
    return f"{n // 1024 ** 3}GiB"


def run_producer(ch, approach, csv_path):
    GiB = 1024 ** 3
    rows = []
    print(f"{approach} — one-way latency = RTT/2")
    print(f"{'size':>8}  {'mean us':>12}{'p50 us':>12}{'p99 us':>12}  {'GiB/s':>10}")
    seq = 0
    for size in SIZES:
        payload = os.urandom(size)
        for _ in range(WARMUP):
            ch.producer_rtt(payload, seq); seq += 1
        oneway = []
        for _ in range(ITERS):
            rtt = ch.producer_rtt(payload, seq); seq += 1
            oneway.append(rtt / 2.0)
        mean = statistics.fmean(oneway)
        p50, p99 = pct(oneway, 50), pct(oneway, 99)
        gibps = size / (mean * 1e-6) / GiB
        print(f"{fmt_size(size):>8}  {mean:>12.2f}{p50:>12.2f}{p99:>12.2f}  {gibps:>10.3f}")
        rows.append((approach, size, ITERS, mean, p50, p99, gibps))

    if csv_path:
        import csv
        with open(csv_path, "w", newline="") as fh:
            w = csv.writer(fh)
            w.writerow(["approach", "size_bytes", "iters",
                        "lat_mean_us", "lat_p50_us", "lat_p99_us", "gibps"])
            w.writerows(rows)
        print(f"[bench_remote] wrote {len(rows)} rows -> {csv_path}")


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
    ap.add_argument("--csv", default="", help="(producer) write results CSV here")
    args = ap.parse_args()

    cfg = load_env()
    ch = CHANNELS[args.approach](cfg)
    ch.reset()   # clear any stale req/resp from a previous run
    if args.role == "producer":
        run_producer(ch, args.approach, args.csv)
    else:
        run_consumer(ch, args.approach)


if __name__ == "__main__":
    main()
