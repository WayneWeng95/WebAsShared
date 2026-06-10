#!/usr/bin/env python3
"""
Generate an intra-node image-fanout DAG for a given fanout degree N.

Shape (one-to-many, zero-copy delivery):

    load (Input, binary)            image bytes -> I/O slot 10
      -> ingest (StreamPipeline)    img_ingest: I/O 10 -> stream slot 20   (staging, excluded)
      -> fanout (Shuffle/Broadcast) splice stream 20 -> N downstream slots 100..100+N-1
      -> w0..w{N-1} (WasmVoid)      img_noop|img_resize on each downstream slot  (the wave we time)

The Shuffle{Broadcast} splices the image page chain into every downstream slot
WITHOUT copying the bytes — the host runs the N worker subprocesses as one
parallel wave, and the DAG runner reports that wave's compute-only time.

NOTE: there is intentionally NO FreeSlots node. Broadcast makes all downstream
slots (and the source slot 20) share ONE physical page chain (chain_onto sets
each downstream head to the source's pages — that's the zero-copy win). Freeing
more than one of those slots would double-free the shared pages and deadlock.
This matches the existing convention (DAGs/demo_dag/dag_demo.json broadcasts and
frees nothing). Cleanup is instead handled by run.sh giving each run a fresh SHM
file (rm before each rep), so nothing accumulates across repetitions.

Usage:
    python3 gen_dag.py --n 100 --func noop --image TestData/fanout_img/10MB.img
    python3 gen_dag.py --n 10 --func resize --out /tmp/img_fanout_10.json

Prints the path of the written DAG (also honoured via --out).
"""
import argparse, json, pathlib, sys

ROOT = pathlib.Path(__file__).resolve().parents[2]   # WebAsShared/
HERE = pathlib.Path(__file__).resolve().parent
IO_SLOT = 10
STREAM_SLOT = 20
WORKER_BASE = 100
WASM_DIR = "Executor/target/wasm32-unknown-unknown/release"

def default_wasm() -> str:
    """Prefer the AOT-precompiled guest.cwasm (no per-subprocess JIT) when it
    exists; otherwise fall back to the JIT'd guest.wasm."""
    cwasm = ROOT / WASM_DIR / "guest.cwasm"
    return f"{WASM_DIR}/guest.cwasm" if cwasm.exists() else f"{WASM_DIR}/guest.wasm"

def build(n: int, func: str, image: str, shm: str, wasm: str) -> dict:
    worker_slots = [WORKER_BASE + i for i in range(n)]
    nodes = [
        {"id": "load", "deps": [],
         "kind": {"Input": {"paths": [image], "slot": IO_SLOT, "binary": True}}},
        {"id": "ingest", "deps": ["load"],
         "kind": {"StreamPipeline": {"rounds": 1,
                  "stages": [{"func": "img_ingest", "arg0": IO_SLOT, "arg1": STREAM_SLOT}]}}},
        {"id": "fanout", "deps": ["ingest"],
         "kind": {"Shuffle": {"upstream": [STREAM_SLOT], "downstream": worker_slots,
                  "policy": {"type": "Broadcast"}}}},
    ]
    worker_func = "img_resize" if func == "resize" else "img_noop"
    for i, slot in enumerate(worker_slots):
        nodes.append({"id": f"w{i}", "deps": ["fanout"],
                      "kind": {"WasmVoid": {"func": worker_func, "arg": slot}}})
    # No FreeSlots: broadcast shares one page chain across all worker slots, so
    # freeing >1 of them double-frees. run.sh uses a fresh SHM file per run instead.
    return {
        "shm_path": shm,
        "wasm_path": wasm,
        "log_level": "info",
        "runs": 1,
        "nodes": nodes,
    }

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, required=True, help="fanout degree (worker count)")
    ap.add_argument("--func", choices=["noop", "resize"], default="noop")
    ap.add_argument("--image", default="TestData/fanout_img/10MB.img",
                    help="image payload path (project-root-relative)")
    ap.add_argument("--out", default=None, help="output DAG path (default: Tests folder)")
    ap.add_argument("--shm", default=None, help="shm_path (default derived from n/func)")
    ap.add_argument("--wasm", default=None,
                    help="guest module path (default: guest.cwasm if built, else guest.wasm)")
    args = ap.parse_args()

    if args.n < 1:
        sys.exit("--n must be >= 1")
    shm = args.shm or f"/dev/shm/img_fanout_{args.func}_{args.n}"
    dag = build(args.n, args.func, args.image, shm, args.wasm or default_wasm())
    out = pathlib.Path(args.out) if args.out else HERE / f"_dag_{args.func}_n{args.n}.json"
    out.write_text(json.dumps(dag, indent=2))
    print(out)

if __name__ == "__main__":
    main()
