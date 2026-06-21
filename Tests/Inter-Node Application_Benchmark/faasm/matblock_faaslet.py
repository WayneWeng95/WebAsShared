#!/usr/bin/env python3
"""matblock_faaslet.py — per-node Faasm Faaslet runner for inter-node Matrix (SUMMA).

Launched on a node by that node's agent (../../Inter-Node deployment/faasm/agent.py).
matblock.cwasm is the intra-node block-multiply Faaslet (wasm32-wasip1 stdin→stdout,
reused verbatim): stdin = [br:u32][bc:u32][n:u32] + A row-panel (br×n f64) + B col-panel
(n×bc f64); stdout = C block (br×bc f64). This wrapper does the Faasm KV (Redis) I/O
around it — the serialized A/B panels + C blocks crossing the network WasMem moves
zero-copy through the SHM page-chain.

  block <uid> <i> <j> <br> <bc> <n> :
    read {uid}_a_{i} (A_i row-panel) + {uid}_b_{j} (B_j col-panel) → matblock.cwasm
    → write {uid}_c_{i}_{j} (C_ij block).

Env: REDIS_HOST/REDIS_PORT, MATBLOCK_CWASM (abs path to the module), WASMTIME (optional).
"""
import os
import shutil
import struct
import subprocess
import sys

import redis


def _wasmtime():
    cand = os.environ.get("WASMTIME") or "wasmtime"
    if os.path.isabs(cand) and os.path.exists(cand):
        return cand
    return shutil.which(cand) or os.path.expanduser("~/.wasmtime/bin/wasmtime")


def main():
    if len(sys.argv) < 7 or sys.argv[1] != "block":
        sys.exit("usage: matblock_faaslet.py block <uid> <i> <j> <br> <bc> <n>")
    uid, i, j, br, bc, n = sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), \
        int(sys.argv[5]), int(sys.argv[6]), int(sys.argv[7])
    r = redis.Redis(host=os.environ["REDIS_HOST"], port=int(os.environ.get("REDIS_PORT", "6379")))

    a_buf = r.get(f"{uid}_a_{i}")
    b_buf = r.get(f"{uid}_b_{j}")
    if a_buf is None or b_buf is None:
        sys.exit(f"panel missing in Redis: a_{i}={a_buf is not None} b_{j}={b_buf is not None}")

    frame = struct.pack("<III", br, bc, n) + a_buf + b_buf
    module = os.environ["MATBLOCK_CWASM"]
    p = subprocess.run([_wasmtime(), "run", "--allow-precompiled", module],
                       input=frame, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if p.returncode != 0:
        sys.exit(f"matblock failed: {p.stderr.decode(errors='replace')[:500]}")
    r.set(f"{uid}_c_{i}_{j}", p.stdout)


if __name__ == "__main__":
    main()
