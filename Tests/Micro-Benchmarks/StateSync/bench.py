#!/usr/bin/env python3
"""StateSync micro-benchmark — measure producer->consumer state delivery cost
across the five state-synchronization approaches.

Approaches (see README.md):
  s3            external storage, remote   (MinIO / S3, minio client)
  redis-remote  external in-memory, remote (Redis on the backend node)
  redis-local   external in-memory, local  (Redis on loopback)
  shm-copy      container shared memory     (mmap /dev/shm, consumer copies out)
  shm-zerocopy  SHM zero-copy (ours)        (mmap /dev/shm, consumer reads in place)

Every approach implements the same two operations so the timing loop is
identical and the comparison is fair:
    put(key, payload: bytes)      # producer publishes one unit of state
    get(key) -> int               # consumer retrieves it; returns bytes delivered

We time PUT and GET separately, sweep state size and (optionally) reader
fan-out, and report p50 / p99 / mean latency and throughput.  Backends whose
client library or endpoint is unavailable are skipped with a clear message.

Usage
-----
    # source the endpoints written by deploy_backends.sh first (optional)
    python3 bench.py                         # run every available approach
    python3 bench.py --approaches shm-copy shm-zerocopy
    python3 bench.py --sizes 64 4096 1048576 --iters 500 --warmup 50
    python3 bench.py --readers 8             # fan-out: 1 put, N gets
    python3 bench.py --csv results.csv

Endpoints are read from backends.env (KEY=VALUE) in this directory if present,
else from the process environment, else from localhost defaults.
"""

import argparse
import mmap
import os
import statistics
import struct
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ENV_FILE = os.path.join(HERE, "backends.env")

# ── Endpoint configuration ────────────────────────────────────────────────────

def load_env():
    """Layer config: backends.env  <  process env  <  built-in defaults."""
    cfg = {
        "REDIS_HOST": "127.0.0.1", "REDIS_PORT": "6379", "REDIS_PASS": "",
        "REDIS_LOCAL_HOST": "127.0.0.1", "REDIS_LOCAL_PORT": "6379",
        "S3_ENDPOINT": "", "S3_ACCESS_KEY": "minioadmin",
        "S3_SECRET_KEY": "minioadmin123", "S3_BUCKET": "statesync",
        "S3_REGION": "us-east-1",
    }
    if os.path.exists(ENV_FILE):
        with open(ENV_FILE) as fh:
            for line in fh:
                line = line.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                k, v = line.split("=", 1)
                cfg[k.strip()] = v.strip()
    cfg.update({k: os.environ[k] for k in cfg if k in os.environ})
    return cfg


# ── Backend interface ─────────────────────────────────────────────────────────

class Backend:
    name = "base"

    def setup(self, max_size: int):
        """Prepare the backend; raise to signal 'skip this approach'."""

    def put(self, key: str, payload):       # producer
        raise NotImplementedError

    def get(self, key: str) -> int:         # consumer; returns bytes delivered
        raise NotImplementedError

    def teardown(self):
        pass


class S3Backend(Backend):
    """External storage, remote — S3-compatible object store via the MinIO client."""
    name = "s3"

    def __init__(self, cfg):
        self.cfg = cfg

    def setup(self, max_size):
        endpoint = self.cfg["S3_ENDPOINT"]
        if not endpoint:
            raise RuntimeError("S3_ENDPOINT not set (run deploy_backends.sh up)")
        try:
            from minio import Minio
        except ImportError as e:
            raise RuntimeError(f"minio not installed ({e}); pip install minio")

        # The MinIO client wants a bare host:port plus a `secure` flag; our
        # backends.env stores a full URL (e.g. http://10.10.1.1:9000).
        secure = endpoint.startswith("https://")
        host = endpoint.split("://", 1)[1] if "://" in endpoint else endpoint

        self.bucket = self.cfg["S3_BUCKET"]
        self.client = Minio(
            host,
            access_key=self.cfg["S3_ACCESS_KEY"],
            secret_key=self.cfg["S3_SECRET_KEY"],
            secure=secure,
        )
        if not self.client.bucket_exists(self.bucket):
            self.client.make_bucket(self.bucket)

    def put(self, key, payload):
        import io
        self.client.put_object(self.bucket, key, io.BytesIO(payload), length=len(payload))

    def get(self, key):
        resp = self.client.get_object(self.bucket, key)
        try:
            return len(resp.read())
        finally:
            resp.close()
            resp.release_conn()


class RedisBackend(Backend):
    """External in-memory KV — Redis (remote or loopback depending on host)."""

    def __init__(self, name, host, port, password=""):
        self.name = name
        self.host, self.port, self.password = host, int(port), password

    def setup(self, max_size):
        try:
            import redis
        except ImportError as e:
            raise RuntimeError(f"redis not installed ({e}); pip install redis")
        self.r = redis.Redis(host=self.host, port=self.port,
                             password=self.password or None, socket_timeout=5)
        self.r.ping()   # raises if unreachable -> approach skipped

    def put(self, key, payload):
        self.r.set(key, payload)

    def get(self, key):
        val = self.r.get(key)
        return len(val) if val else 0

    def teardown(self):
        try:
            self.r.flushdb()
        except Exception:
            pass


class ShmBackend(Backend):
    """Intra-host shared memory over an mmap'd /dev/shm file.

    Layout per key region: [u32 length][payload bytes].  A single fixed region
    is reused (the state-handoff / overwrite pattern), so we model steady-state
    delivery cost without allocator noise.

    zero_copy=False  -> consumer copies the payload into a private bytes object
                        (SerDe-free but still one memcpy: the "container shared
                        memory" approach).
    zero_copy=True   -> consumer returns a memoryview slice with no copy
                        (true zero-copy: our page-chain's in-place read).
    """

    def __init__(self, name, zero_copy, path="/dev/shm/statesync_bench"):
        self.name = name
        self.zero_copy = zero_copy
        self.path = path

    def setup(self, max_size):
        self.region = 8 + max_size            # u32 len + payload (8 for alignment)
        with open(self.path, "wb") as fh:
            fh.truncate(self.region)
        self.fd = os.open(self.path, os.O_RDWR)
        self.mm = mmap.mmap(self.fd, self.region)

    def put(self, key, payload):
        n = len(payload)
        self.mm[0:4] = struct.pack("<I", n)
        self.mm[8:8 + n] = payload

    def get(self, key):
        n = struct.unpack("<I", self.mm[0:4])[0]
        if self.zero_copy:
            view = memoryview(self.mm)[8:8 + n]   # no copy — in-place handle
            return len(view)
        else:
            data = bytes(self.mm[8:8 + n])        # one memcpy into private heap
            return len(data)

    def teardown(self):
        try:
            self.mm.close(); os.close(self.fd); os.remove(self.path)
        except Exception:
            pass


def build_backends(cfg):
    return {
        "s3":           lambda: S3Backend(cfg),
        "redis-remote": lambda: RedisBackend("redis-remote", cfg["REDIS_HOST"],
                                             cfg["REDIS_PORT"], cfg["REDIS_PASS"]),
        "redis-local":  lambda: RedisBackend("redis-local", cfg["REDIS_LOCAL_HOST"],
                                             cfg["REDIS_LOCAL_PORT"], ""),
        "shm-copy":     lambda: ShmBackend("shm-copy", zero_copy=False),
        "shm-zerocopy": lambda: ShmBackend("shm-zerocopy", zero_copy=True),
    }


# ── Measurement ───────────────────────────────────────────────────────────────

def percentile(samples, p):
    if not samples:
        return 0.0
    s = sorted(samples)
    k = max(0, min(len(s) - 1, int(round((p / 100.0) * (len(s) - 1)))))
    return s[k]


def measure(backend, size, iters, warmup, readers):
    """Return a result dict of put/get latency stats (microseconds) for one size."""
    payload = os.urandom(size)
    key = f"statesync/{size}"

    # Warmup (JIT caches, TCP connection, page faults).
    for _ in range(warmup):
        backend.put(key, payload)
        backend.get(key)

    put_us, get_us = [], []
    for _ in range(iters):
        t0 = time.perf_counter_ns()
        backend.put(key, payload)
        t1 = time.perf_counter_ns()
        put_us.append((t1 - t0) / 1000.0)

        # Fan-out: one publish, `readers` consumers each fetch the state.
        t0 = time.perf_counter_ns()
        for _ in range(readers):
            backend.get(key)
        t1 = time.perf_counter_ns()
        get_us.append((t1 - t0) / 1000.0 / readers)   # per-reader

    def stats(xs):
        return {
            "p50": percentile(xs, 50), "p99": percentile(xs, 99),
            "mean": statistics.fmean(xs),
        }

    put_p50 = max(stats(put_us)["p50"], 1e-6)
    get_p50 = max(stats(get_us)["p50"], 1e-6)
    return {
        "size": size,
        "put": stats(put_us), "get": stats(get_us),
        # Throughput from median latency, GiB/s.
        "put_gibps": size / (put_p50 * 1e-6) / (1024 ** 3),
        "get_gibps": size / (get_p50 * 1e-6) / (1024 ** 3),
    }


def fmt_size(n):
    if n < 1024:
        return f"{n}B"
    if n < 1024 ** 2:
        return f"{n//1024}KiB"
    if n < 1024 ** 3:
        return f"{n//1024**2}MiB"
    return f"{n//1024**3}GiB"


# ── Main ──────────────────────────────────────────────────────────────────────

def main():
    cfg = load_env()
    registry = build_backends(cfg)

    ap = argparse.ArgumentParser(description="StateSync micro-benchmark")
    ap.add_argument("--approaches", nargs="+", default=list(registry),
                    choices=list(registry), help="which approaches to run")
    ap.add_argument("--sizes", nargs="+", type=int,
                    default=[16 * 1024, 1024 * 1024, 16 * 1024 * 1024,
                             256 * 1024 * 1024],
                    help="state sizes in bytes")
    ap.add_argument("--iters", type=int, default=200, help="timed iterations")
    ap.add_argument("--warmup", type=int, default=20, help="warmup iterations")
    ap.add_argument("--readers", type=int, default=1,
                    help="consumers per publish (fan-out degree)")
    ap.add_argument("--csv", default="", help="write raw results to this CSV")
    args = ap.parse_args()

    max_size = max(args.sizes)
    print(f"StateSync micro-benchmark")
    print(f"  iters={args.iters} warmup={args.warmup} readers={args.readers}")
    print(f"  sizes={[fmt_size(s) for s in args.sizes]}")
    print()

    csv_rows = []
    header = f"{'approach':<14}{'size':>8}  {'put p50':>10}{'put p99':>10}" \
             f"{'get p50':>10}{'get p99':>10}  {'put GiB/s':>10}{'get GiB/s':>10}"

    for name in args.approaches:
        backend = registry[name]()
        try:
            backend.setup(max_size)
        except Exception as e:
            print(f"[skip] {name}: {e}\n")
            continue

        print(f"=== {name} ===")
        print(header)
        try:
            for size in args.sizes:
                r = measure(backend, size, args.iters, args.warmup, args.readers)
                print(f"{name:<14}{fmt_size(size):>8}  "
                      f"{r['put']['p50']:>10.2f}{r['put']['p99']:>10.2f}"
                      f"{r['get']['p50']:>10.2f}{r['get']['p99']:>10.2f}  "
                      f"{r['put_gibps']:>10.3f}{r['get_gibps']:>10.3f}")
                csv_rows.append((name, size, args.readers,
                                 r['put']['p50'], r['put']['p99'], r['put']['mean'],
                                 r['get']['p50'], r['get']['p99'], r['get']['mean'],
                                 r['put_gibps'], r['get_gibps']))
        finally:
            backend.teardown()
        print()

    if args.csv and csv_rows:
        import csv
        with open(args.csv, "w", newline="") as fh:
            w = csv.writer(fh)
            w.writerow(["approach", "size_bytes", "readers",
                        "put_p50_us", "put_p99_us", "put_mean_us",
                        "get_p50_us", "get_p99_us", "get_mean_us",
                        "put_gibps", "get_gibps"])
            w.writerows(csv_rows)
        print(f"[csv] wrote {len(csv_rows)} rows -> {args.csv}")


if __name__ == "__main__":
    main()
