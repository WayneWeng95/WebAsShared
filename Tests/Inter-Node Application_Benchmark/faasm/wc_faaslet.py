#!/usr/bin/env python3
"""wc_faaslet.py — per-node Faasm Faaslet runner for inter-node WordCount.

Launched on a node by that node's agent (../../Inter-Node deployment/faasm/agent.py).
The wc.cwasm Faaslet is a pure stdin→stdout filter (the intra-node baseline protocol:
`map` chunk→`word\\x1fcount` partials, `reduce` partials→merged), so this wrapper
does the Faasm KV (Redis) I/O around it — the serialized cross-node state transfer
WasMem's RDMA page-chain avoids.

  map    <uid> <i>   : read  <uid>_chunk_<i>  → wc.cwasm map    → write <uid>_partial_<i>
  reduce <uid> <n>   : wait for all partials  → wc.cwasm reduce → write <uid>_result

Env: REDIS_HOST/REDIS_PORT, WC_CWASM (abs path to the module), WASMTIME (optional).
"""
import os
import shutil
import subprocess
import sys
import time

import redis


def _wasmtime():
    cand = os.environ.get("WASMTIME") or "wasmtime"
    if os.path.isabs(cand) and os.path.exists(cand):
        return cand
    return shutil.which(cand) or os.path.expanduser("~/.wasmtime/bin/wasmtime")


def _faaslet(mode, data):
    """One fresh WASM instance (Faaslet): stdin=data → stdout."""
    wc = os.environ["WC_CWASM"]
    p = subprocess.run([_wasmtime(), "run", "--allow-precompiled", wc, mode],
                       input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if p.returncode != 0:
        sys.exit(f"faaslet {mode} failed: {p.stderr.decode(errors='replace')[:500]}")
    return p.stdout


def main():
    if len(sys.argv) < 3:
        sys.exit("usage: wc_faaslet.py map|reduce <uid> [i|n]")
    mode, uid = sys.argv[1], sys.argv[2]
    r = redis.Redis(host=os.environ["REDIS_HOST"], port=int(os.environ.get("REDIS_PORT", "6379")))

    if mode == "map":
        i = int(sys.argv[3])
        chunk = r.get(f"{uid}_chunk_{i}")
        if chunk is None:
            sys.exit(f"chunk {i} missing in Redis")
        r.set(f"{uid}_partial_{i}", _faaslet("map", chunk))

    elif mode == "reduce":
        n = int(sys.argv[3])
        # Self-synchronize: wait until every map partial is in the KV, then merge.
        deadline = time.time() + 600
        while time.time() < deadline:
            if all(r.exists(f"{uid}_partial_{i}") for i in range(n)):
                break
            time.sleep(0.05)
        else:
            sys.exit("timeout waiting for map partials")
        partials = b"".join(r.get(f"{uid}_partial_{i}") for i in range(n))
        r.set(f"{uid}_result", _faaslet("reduce", partials))

    else:
        sys.exit(f"unknown mode {mode!r}")


if __name__ == "__main__":
    main()
