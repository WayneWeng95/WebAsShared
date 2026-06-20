#!/usr/bin/env python3
"""distribute.py — push a LARGE local file to every node's Faasm agent.

git can't carry large inputs (corpora, .bin, records), so this streams a file from
node 0 to each node's agent `/upload` (raw body, 1 MiB chunks — no base64, no full
load into memory on either side). By default the file lands at the SAME absolute
path on every node, so a workload that reads it by path finds it identically.

Usage:
  ./distribute.py <local_file> [dst_path] [--workers-only]
    <local_file>     file on this node (e.g. ../../../TestData/corpus_xlarge.txt)
    [dst_path]       where to write on each node (default: same absolute path)
    --workers-only   skip the coordinator (it already has the file)

Examples:
  ./distribute.py ../../../TestData/corpus_xlarge.txt --workers-only
  ./distribute.py /opt/.../A_512.bin /opt/.../A_512.bin
"""
import os
import sys
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))


def load_env():
    env = {}
    with open(os.path.join(HERE, "cluster.env")) as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if "=" in line:
                k, v = line.split("=", 1)
                env[k.strip()] = v.strip().strip('"')
    return env, [env["COORD_IP"]] + env.get("WORKER_IPS", "").split()


def upload(host, port, local, dst):
    size = os.path.getsize(local)
    with open(local, "rb") as f:
        req = urllib.request.Request(
            f"http://{host}:{port}/upload", data=f, method="POST",
            headers={"X-Dest-Path": dst, "Content-Length": str(size),
                     "Content-Type": "application/octet-stream"})
        with urllib.request.urlopen(req, timeout=7200) as r:
            return r.read()


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    workers_only = "--workers-only" in sys.argv
    if not args:
        sys.exit("usage: distribute.py <local_file> [dst_path] [--workers-only]")
    local = os.path.abspath(args[0])
    if not os.path.isfile(local):
        sys.exit(f"not a file: {local}")
    dst = args[1] if len(args) > 1 else local

    env, nodes = load_env()
    port = int(env.get("AGENT_PORT", "9600"))
    coord = env["COORD_IP"]
    targets = [n for n in nodes if not (workers_only and n == coord)]
    mb = os.path.getsize(local) / (1024 * 1024)
    print(f"[distribute] {local} ({mb:.1f} MiB) → {dst} on {targets}")
    for n in targets:
        try:
            upload(n, port, local, dst)
            print(f"  ✓ {n}")
        except Exception as ex:
            print(f"  ✗ {n}: {ex}")
            sys.exit(1)
    print("[distribute] done")


if __name__ == "__main__":
    main()
