#!/usr/bin/env python3
# gen_dag.py — emit a native single-node MNIST-style inference DAG with W parallel
# predict workers.
#
#   load_model(io slot 5) → setup_model ──┐
#   load_data(slot 0) → partition(W) ──────┼─► predict_0 ──┐
#                                          │   predict_i ──┼─► Aggregate → reduce → save
#                                          └─► predict_W ──┘
#
# Each predict worker is a separate OS process; the test shard + the broadcast
# model move through the SHM page-chain with ZERO serialization (the headline).
# A single forward pass per sample (argmax of integer class scores) → the
# prediction checksum is fan-out-invariant and integer-exact (the gate).
#
# Usage: gen_dag.py W MODEL_PATH DATA_PATH OUT_PATH SHM_PATH
# Env: MLI_WASM overrides the guest module path (e.g. a precompiled .cwasm for AOT).
import json
import os
import sys

W = int(sys.argv[1])
model = sys.argv[2]
data = sys.argv[3]
out_path = sys.argv[4]
shm_path = sys.argv[5]
WASM = os.environ.get('MLI_WASM') or \
       'Executor/target/wasm32-unknown-unknown/release/guest.wasm'

MODEL_IO = 5                  # io slot the model file is loaded into
PART_BASE = 10                # predict-worker shard slots: PART_BASE .. PART_BASE+W
OUT_OFFSET = 100              # predict(slot) writes to slot+OUT_OFFSET
AGG_DOWN = 1000               # merged slot

def packed(lo, hi):
    return (lo & 0xFFFF) | ((hi & 0xFFFF) << 16)

nodes = [
    {"id": "load_model", "deps": [],
     "kind": {"Input": {"path": model, "slot": MODEL_IO, "prefetch": True}}},
    {"id": "setup_model", "deps": ["load_model"],
     "kind": {"Func": {"func": "infer_setup_model", "arg": MODEL_IO}}},
    {"id": "load_data", "deps": [],
     "kind": {"Input": {"path": data, "slot": 0, "prefetch": True}}},
    {"id": "partition", "deps": ["load_data"],
     "kind": {"Func": {"func": "infer_partition", "arg": packed(PART_BASE, W)}}},
]

predict_ids = []
for i in range(W):
    pid = f"predict_{i}"
    predict_ids.append(pid)
    data_slot = PART_BASE + i
    out_slot = PART_BASE + i + OUT_OFFSET
    nodes.append({"id": pid, "deps": ["partition", "setup_model"],
                  "kind": {"Func": {"func": "infer_predict",
                                    "arg": packed(data_slot, out_slot)}}})

nodes += [
    {"id": "aggregate", "deps": predict_ids,
     "kind": {"Aggregate": {"upstream": [PART_BASE + i + OUT_OFFSET for i in range(W)],
                            "downstream": AGG_DOWN}}},
    {"id": "reduce", "deps": ["aggregate"],
     "kind": {"Func": {"func": "infer_reduce", "arg": AGG_DOWN}}},
    {"id": "save", "deps": ["reduce"],
     "kind": {"Output": {"path": out_path}}},
]

print(json.dumps({
    "shm_path": shm_path,
    "wasm_path": WASM,
    "log_level": "warn",
    "nodes": nodes,
}, indent=2))
