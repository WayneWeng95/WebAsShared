#!/usr/bin/env python3
# gen_dag.py — emit a native single-node WordCount DAG with N parallel map workers.
#
#   load  ──►  wc_distribute(N)  ──►  wc_map_0 ──┐
#                                  ├─ wc_map_1 ──┤
#                                  ├─    ...     ├─►  aggregate ──► wc_reduce ──► save
#                                  └─ wc_map_N ──┘
#
# Mirrors the per-machine fan-out of DAGs/symbolic_dag/word_count_auto_placement
# but as a flat native DAG so it runs via `node-agent run` with no coordinator —
# isolating the intra-node, shared-memory page-chain path. wc_distribute does the
# zero-copy contiguous split; the mapper→reducer partial maps move through SHM
# slots with no serialization (the headline this benchmark measures).
#
# Usage: gen_dag.py N CORPUS_PATH OUT_PATH SHM_PATH
# Env: WC_WASM overrides the guest module path (e.g. a precompiled .cwasm for AOT).
import json
import os
import sys

N = int(sys.argv[1])
corpus = sys.argv[2]
out_path = sys.argv[3]
shm_path = sys.argv[4]
WASM = os.environ.get('WC_WASM',
                      'Executor/target/wasm32-unknown-unknown/release/guest.wasm')

DIST_BASE = 10     # map worker input slots: DIST_BASE .. DIST_BASE+N
MAP_OUT = 100      # wc_map(slot) writes to slot+100
AGG_DOWN = 1000    # merged slot, kept clear of (DIST_BASE+MAP_OUT+N) for large N

nodes = [
    {"id": "load", "deps": [],
     "kind": {"Input": {"path": corpus, "slot": 0, "prefetch": True}}},
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
worker_slots = [DIST_BASE + i for i in range(N)]   # input pages, relinked by distribute
nodes += [
    # Free the per-worker input slots once every mapper has read them, so the
    # original input pages (≈1× input) return to the SHM pool before the
    # aggregate/reduce/output stages instead of leaking for the rest of the run.
    {"id": "free_input", "deps": map_ids,
     "kind": {"FreeSlots": {"stream": worker_slots}}},
    {"id": "aggregate", "deps": map_ids,
     "kind": {"Aggregate": {"upstream": upstream, "downstream": AGG_DOWN}}},
    {"id": "reduce", "deps": ["aggregate"],
     "kind": {"Func": {"func": "wc_reduce", "arg": AGG_DOWN}}},
    {"id": "save", "deps": ["reduce"],
     "kind": {"Output": {"path": out_path}}},
]

print(json.dumps({
    "shm_path": shm_path,
    "wasm_path": WASM,
    "log_level": "warn",
    "nodes": nodes,
}, indent=2))
