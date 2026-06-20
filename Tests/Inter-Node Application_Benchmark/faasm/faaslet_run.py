#!/usr/bin/env python3
"""faaslet_run.py — GENERIC per-node Faaslet runner (git-synced to every node).

Reads input blob(s) from Redis, pipes them through a wasmtime module (stdin→stdout),
writes the result blob to Redis. The driver pre-stages each worker's input under a
Redis key and collects its output key, so this one wrapper serves every fan-style
Faasm workload (FINRA rules, ML-inference predict, Matrix blocks, WordCount map, …).

  faaslet_run.py <module> <in_keys_csv> <out_key> [-- <faaslet_args>...]
    <in_keys_csv>  one or more Redis keys (comma-separated); their values are
                   concatenated in order and fed on stdin (empty/missing → b"").
    <faaslet_args> positional args passed to the module (e.g. a FINRA rule id).

env: REDIS_HOST/REDIS_PORT, WASMTIME (optional).
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
    argv = sys.argv[1:]
    faaslet_args = []
    if "--" in argv:
        i = argv.index("--")
        faaslet_args = argv[i + 1:]
        argv = argv[:i]
    if len(argv) < 3:
        sys.exit("usage: faaslet_run.py <module> <in_keys_csv> <out_key> [-- args...]")
    module, in_keys, out_key = argv[0], argv[1], argv[2]

    r = redis.Redis(host=os.environ["REDIS_HOST"], port=int(os.environ.get("REDIS_PORT", "6379")))
    blob = b"".join((r.get(k) or b"") for k in in_keys.split(","))
    p = subprocess.run([_wasmtime(), "run", "--allow-precompiled", module] + faaslet_args,
                       input=blob, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if p.returncode != 0:
        sys.exit(f"faaslet failed: {p.stderr.decode(errors='replace')[:500]}")
    r.set(out_key, p.stdout)


if __name__ == "__main__":
    main()
