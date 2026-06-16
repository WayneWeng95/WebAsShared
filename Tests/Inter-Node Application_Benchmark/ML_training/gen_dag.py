#!/usr/bin/env python3
# gen_dag.py — emit a native single-node SGD DAG with E epochs unrolled and W
# parallel gradient workers per epoch.
#
#   load(slot 0) → sgd_init → sgd_partition(W) → shards[10..10+W]
#   epoch e:  sgd_grad ×W (read shard + latest model)  ┐
#                                                       ├─ Aggregate → sgd_update
#             (model appended to slot 1900; broadcast-read by next epoch)
#   … ×E …  → sgd_validate → save
#
# Each gradient worker is a separate OS process; per-epoch gradient SUMS move
# through the SHM page-chain with ZERO serialization, and the model is broadcast
# by a single appended slot record (non-destructive reads fan out to all W). This
# is the per-iteration model/gradient state transfer the baselines pay Redis for.
#
# Usage: gen_dag.py W E DATA_PATH OUT_PATH SHM_PATH
# Env: ML_WASM overrides the guest module path (e.g. a precompiled .cwasm for AOT).
import json
import os
import sys

W = int(sys.argv[1])          # gradient workers per epoch
E = int(sys.argv[2])          # epochs (unrolled)
data = sys.argv[3]
out_path = sys.argv[4]
shm_path = sys.argv[5]
WASM = os.environ.get('ML_WASM',
                      'Executor/target/wasm32-unknown-unknown/release/guest.wasm')

DATA_BASE = 10                # text shard slots: DATA_BASE .. DATA_BASE+W
BIN_BASE = 40                 # binary shard slots: BIN_BASE .. BIN_BASE+W (parse-once)
GRAD_BASE = 64                # per-epoch grad slots: GRAD_BASE + e*GRAD_STRIDE + i
GRAD_STRIDE = 32              # ≥ max W; keeps each epoch's grad slots disjoint
GSUM_BASE = 1200              # per-epoch aggregated-gradient slot: GSUM_BASE + e
# (guest hardcodes the model slot at 1900; stays clear of all of the above)

def packed(lo, hi):           # base|count<<16 packing the guest unpacks
    return (lo & 0xFFFF) | ((hi & 0xFFFF) << 16)

nodes = [
    {"id": "load", "deps": [],
     "kind": {"Input": {"path": data, "slot": 0, "prefetch": True}}},
    # partition is the SOLE reader of slot 0 (the input is consumed once). It
    # zero-copy splits into W shards; sgd_update bootstraps n_samples/lr_den from
    # the worker-emitted shard counts, so nothing needs to re-read the input.
    {"id": "partition", "deps": ["load"],
     "kind": {"Func": {"func": "sgd_partition", "arg": packed(DATA_BASE, W)}}},
]

# One-time encode: parse each text shard → compact binary blob ONCE, so the
# per-epoch gradient workers skip text parsing (the dominant unrolled-epoch cost).
encode_ids = []
for i in range(W):
    eid = f"encode_{i}"
    encode_ids.append(eid)
    nodes.append({"id": eid, "deps": ["partition"],
                  "kind": {"Func": {"func": "sgd_encode",
                                    "arg": packed(DATA_BASE + i, BIN_BASE + i)}}})

prev = None
for e in range(E):
    grad_ids = []
    for i in range(W):
        gid = f"grad_{e}_{i}"
        grad_ids.append(gid)
        bin_slot = BIN_BASE + i
        grad_out = GRAD_BASE + e * GRAD_STRIDE + i
        # epoch 0 reads its own encoded shard; later epochs wait on the model update.
        dep = encode_ids[i] if prev is None else prev
        nodes.append({"id": gid, "deps": [dep],
                      "kind": {"Func": {"func": "sgd_grad",
                                        "arg": packed(bin_slot, grad_out)}}})
    gsum = GSUM_BASE + e
    nodes.append({"id": f"agg_{e}", "deps": grad_ids,
                  "kind": {"Aggregate": {
                      "upstream": [GRAD_BASE + e * GRAD_STRIDE + i for i in range(W)],
                      "downstream": gsum}}})
    nodes.append({"id": f"update_{e}", "deps": [f"agg_{e}"],
                  "kind": {"Func": {"func": "sgd_update", "arg": gsum}}})
    prev = f"update_{e}"

nodes += [
    {"id": "validate", "deps": [prev],
     "kind": {"Func": {"func": "sgd_validate", "arg": packed(BIN_BASE, W)}}},
    {"id": "save", "deps": ["validate"],
     "kind": {"Output": {"path": out_path}}},
]

print(json.dumps({
    "shm_path": shm_path,
    "wasm_path": WASM,
    "log_level": "warn",
    "nodes": nodes,
}, indent=2))
