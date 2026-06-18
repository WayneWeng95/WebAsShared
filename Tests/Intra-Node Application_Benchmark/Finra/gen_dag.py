#!/usr/bin/env python3
# gen_dag.py — emit the native FINRA audit DAG, parametrized on the trades file.
#
#   load → fetch_private / fetch_public → aggregate_inputs(→200)
#        → finra_audit_rule × 8 (each reads 200, writes 10+i)   [broadcast stage]
#        → aggregate_rules(→300) → finra_merge_results → save
#
# The 8 rules are fixed (distinct audit logic), so only the input/output paths
# vary across the sweep. Env WC_WASM overrides the guest module (.cwasm for AOT).
import json
import os
import sys

trades = sys.argv[1]
out_path = sys.argv[2]
shm_path = sys.argv[3]
WASM = os.environ.get('WC_WASM',
                      'Executor/target/wasm32-unknown-unknown/release/guest.wasm')

RULE_OUT_BASE = 10
nodes = [
    {"id": "load_trades", "deps": [],
     "kind": {"Input": {"path": trades, "slot": 0, "prefetch": True}}},
    {"id": "fetch_private", "deps": ["load_trades"],
     "kind": {"Func": {"func": "finra_fetch_private", "arg": 0, "arg2": 5}}},
    {"id": "fetch_public", "deps": [],
     "kind": {"Func": {"func": "finra_fetch_public", "arg": 6, "wasm_arg": 0}}},
    {"id": "aggregate_inputs", "deps": ["fetch_private", "fetch_public"],
     "kind": {"Aggregate": {"upstream": [5, 6], "downstream": 200}}},
]

rule_ids = []
for i in range(8):
    rid = f"rule_{i}"
    rule_ids.append(rid)
    nodes.append({"id": rid, "deps": ["aggregate_inputs"],
                  "kind": {"Func": {"func": "finra_audit_rule",
                                    "arg": 200, "arg2": i, "wasm_arg": i}}})

nodes += [
    {"id": "aggregate_rules", "deps": rule_ids,
     "kind": {"Aggregate": {"upstream": [RULE_OUT_BASE + i for i in range(8)],
                            "downstream": 300}}},
    {"id": "merge", "deps": ["aggregate_rules"],
     "kind": {"Func": {"func": "finra_merge_results", "arg": 300}}},
    {"id": "save", "deps": ["merge"],
     "kind": {"Output": {"path": out_path}}},
]

print(json.dumps({
    "shm_path": shm_path,
    "wasm_path": WASM,
    "log_level": "warn",
    "nodes": nodes,
}, indent=2))
