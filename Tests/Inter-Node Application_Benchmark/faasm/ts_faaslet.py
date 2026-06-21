#!/usr/bin/env python3
"""ts_faaslet.py — per-node Faasm Faaslet runner for inter-node TeraSort.

Launched on a node by that node's agent (../../Inter-Node deployment/faasm/agent.py).
ts.cwasm is the intra-node TeraSort Faaslet (a wasm32-wasip1 stdin→stdout filter,
reused verbatim — `partition` tags each record with its range-owner; `merge` sorts a
range's keys and emits a summary). This wrapper does the Faasm KV (Redis) I/O around
it — the serialized cross-node shuffle WasMem's RDMA page-chain moves zero-copy.

  partition <uid> <i> <N> : read  {uid}_chunk_{i} → ts.cwasm partition N →
                            demux the tagged stream into N buckets, write
                            {uid}_bucket_{i}_{j} for j in 0..N (the shuffle scatter).
  merge     <uid> <j> <N> : read  {uid}_bucket_{*}_{j} (this owner's column, the
                            shuffle gather) → ts.cwasm merge → write {uid}_summary_{j}
                            = "records=… sorted=… keysum=… first=… last=…".

Env: REDIS_HOST/REDIS_PORT, TS_CWASM (abs path to the module), WASMTIME (optional).
"""
import os
import shutil
import subprocess
import sys
import time

import redis

OWNER_TAG_BASE = 128  # must match ts.rs


def _wasmtime():
    cand = os.environ.get("WASMTIME") or "wasmtime"
    if os.path.isabs(cand) and os.path.exists(cand):
        return cand
    return shutil.which(cand) or os.path.expanduser("~/.wasmtime/bin/wasmtime")


def _faaslet(args, data):
    """One fresh WASM instance (Faaslet): stdin=data → stdout."""
    ts = os.environ["TS_CWASM"]
    p = subprocess.run([_wasmtime(), "run", "--allow-precompiled", ts] + args,
                       input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if p.returncode != 0:
        sys.exit(f"faaslet {args} failed: {p.stderr.decode(errors='replace')[:500]}")
    return p.stdout


def main():
    if len(sys.argv) < 5:
        sys.exit("usage: ts_faaslet.py partition|merge <uid> <idx> <N>")
    mode, uid, idx, N = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
    r = redis.Redis(host=os.environ["REDIS_HOST"], port=int(os.environ.get("REDIS_PORT", "6379")))

    if mode == "partition":
        i = idx
        chunk = r.get(f"{uid}_chunk_{i}")
        if chunk is None:
            sys.exit(f"chunk {i} missing in Redis")
        tagged = _faaslet(["partition", str(N)], chunk)
        # Each tagged line is (128 + owner) + record; +128 keeps the tag out of '\n'
        # and printable bytes, so we can split on '\n' and read owner from byte 0.
        buckets = [[] for _ in range(N)]
        for ln in tagged.split(b"\n"):
            if not ln:
                continue
            buckets[ln[0] - OWNER_TAG_BASE].append(ln[1:])
        for j in range(N):
            r.set(f"{uid}_bucket_{i}_{j}", b"\n".join(buckets[j]))

    elif mode == "merge":
        j = idx
        # The driver barriers on all partition Faaslets before launching merges, so
        # every {uid}_bucket_{i}_{j} is present; gather this owner's whole column.
        cols = [r.get(f"{uid}_bucket_{i}_{j}") for i in range(N)]
        blob = b"\n".join(c for c in cols if c)
        r.set(f"{uid}_summary_{j}", _faaslet(["merge"], blob))

    else:
        sys.exit(f"unknown mode {mode!r}")


if __name__ == "__main__":
    main()
