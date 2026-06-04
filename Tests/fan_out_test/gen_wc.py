#!/usr/bin/env python3
# Emit a native single-node word-count DAG with N parallel map-worker processes.
#
# Mirrors the per-machine fan-out of DAGs/symbolic_dag/word_count_auto_placement
# (load -> distribute -> map_n x N -> aggregate -> reduce -> save) but as a flat
# native DAG so it runs via `node-agent run` with no coordinator — isolating the
# process-per-worker fan-out cost.
#
# Usage: gen_wc.py N CORPUS_PATH OUT_PATH SHM_PATH
import json, sys

N        = int(sys.argv[1])
corpus   = sys.argv[2]
out_path = sys.argv[3]
shm_path = sys.argv[4]

DIST_BASE = 10     # map worker input slots: DIST_BASE .. DIST_BASE+N
MAP_OUT   = 100    # wc_map(slot) writes to slot+100
AGG_DOWN  = 1000   # merged slot, kept clear of (DIST_BASE+MAP_OUT+N) for large N

nodes = [
    {"id": "load", "deps": [],
     "kind": {"Input": {"path": corpus, "slot": 0, "prefetch": True}}},
    # wc_distribute(N): zero-copy contiguous split of the input into N segments.
    {"id": "distribute", "deps": ["load"],
     "kind": {"Func": {"func": "wc_distribute", "arg": N, "arg2": N}}},
]

map_ids = []
for i in range(N):
    mid = f"map_{i}"
    map_ids.append(mid)
    nodes.append({"id": mid, "deps": ["distribute"],
                  "kind": {"Func": {"func": "wc_map",
                                    "arg": DIST_BASE + i,
                                    "arg2": DIST_BASE + i + MAP_OUT}}})

upstream = [DIST_BASE + i + MAP_OUT for i in range(N)]
nodes += [
    {"id": "aggregate", "deps": map_ids,
     "kind": {"Aggregate": {"upstream": upstream, "downstream": AGG_DOWN}}},
    {"id": "reduce", "deps": ["aggregate"],
     "kind": {"Func": {"func": "wc_reduce", "arg": AGG_DOWN}}},
    {"id": "save", "deps": ["reduce"],
     "kind": {"Output": {"path": out_path}}},
]

print(json.dumps({
    "shm_path": shm_path,
    "wasm_path": "Executor/target/wasm32-unknown-unknown/release/guest.wasm",
    "log_level": "warn",
    "nodes": nodes,
}, indent=2))
