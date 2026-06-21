#!/usr/bin/env python3
"""infer_faaslet.py — per-node Faasm Faaslet runner for inter-node MNIST inference.

Launched on a node by that node's agent. infer_predict.cwasm is the intra-node inference
Faaslet (wasm32-wasip1 stdin→stdout, reused verbatim): stdin = a binary frame
([n][f] + model + n samples), stdout = [correct][total][predsum] (3× i64 le). This
wrapper does the Faasm KV (Redis) I/O around it. The driver pre-builds the full frame per
shard (model replicated in), so this wrapper just pipes it.

  <uid> <i> : read {uid}_frame_{i} → infer_predict.cwasm → write {uid}_r_{i} (24-byte result).

Env: REDIS_HOST/REDIS_PORT, INFER_CWASM (abs path to the module), WASMTIME (optional).
"""
import os
import shutil
import subprocess
import sys

import redis


def _wasmtime():
    cand = os.environ.get("WASMTIME") or "wasmtime"
    if os.path.isabs(cand) and os.path.exists(cand):
        return cand
    return shutil.which(cand) or os.path.expanduser("~/.wasmtime/bin/wasmtime")


def main():
    if len(sys.argv) < 3:
        sys.exit("usage: infer_faaslet.py <uid> <i>")
    uid, i = sys.argv[1], sys.argv[2]
    r = redis.Redis(host=os.environ["REDIS_HOST"], port=int(os.environ.get("REDIS_PORT", "6379")))

    frame = r.get(f"{uid}_frame_{i}")
    if frame is None:
        sys.exit(f"frame {i} missing in Redis")
    module = os.environ["INFER_CWASM"]
    p = subprocess.run([_wasmtime(), "run", "--allow-precompiled", module],
                       input=frame, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if p.returncode != 0:
        sys.exit(f"infer_predict failed: {p.stderr.decode(errors='replace')[:500]}")
    r.set(f"{uid}_r_{i}", p.stdout)


if __name__ == "__main__":
    main()
