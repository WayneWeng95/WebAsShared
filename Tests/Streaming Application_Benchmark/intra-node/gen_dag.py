#!/usr/bin/env python3
# gen_dag.py — emit the single-node streaming DAG for a MediaReview/SocialNetwork
# run, parametrized on the events file, partition count and seed-table size.
#
#   Input(events) ─┐
#   <p>_seed       ┴→ <p>_parse (partition BY KEY into slots 10..10+n)
#   → <p>_apply × n (read slot 10+i, write partial tally 110+i)
#   → Aggregate(110..110+n → 300) → <p>_summary → Output
#
# Slot/arg conventions follow word_count.json (the proven single-node fan):
#   parse : arg = n_partitions (the guest writes slot 10 + key%n), arg2 = 10 (base)
#   apply : arg = 10+i (input slot), arg2 = 110+i (declared output → guest writes
#           in_slot+100 internally)
#   seed  : large user count rides in wasm_arg so the partitioner's slot scan
#           (which treats `arg` as a slot candidate, cap 2048) never sees it.
import argparse
import json
import os

WASM = os.environ.get("WC_WASM",
                      "Executor/target/wasm32-unknown-unknown/release/guest.wasm")

PREFIX = {"mediareview": "mr", "socialnetwork": "sn"}

ap = argparse.ArgumentParser()
ap.add_argument("workload", choices=list(PREFIX))
ap.add_argument("events")
ap.add_argument("out_path")
ap.add_argument("shm_path")
ap.add_argument("--partitions", type=int, default=4)
ap.add_argument("--users", type=int, default=10000)
args = ap.parse_args()

p = PREFIX[args.workload]
n = max(1, args.partitions)
BASE, OUT = 10, 110

nodes = [
    {"id": "load", "node_id": 0, "deps": [],
     "kind": {"Input": {"path": args.events, "slot": 0, "prefetch": True}}},
    {"id": "seed", "node_id": 0, "deps": [],
     "kind": {"Func": {"func": f"{p}_seed", "arg": 0, "wasm_arg": args.users}}},
    {"id": "parse", "node_id": 0, "deps": ["load"],
     "kind": {"Func": {"func": f"{p}_parse", "arg": n, "arg2": BASE}}},
]

apply_ids = []
for i in range(n):
    aid = f"apply_{i}"
    apply_ids.append(aid)
    nodes.append({"id": aid, "node_id": 0, "deps": ["parse", "seed"],
                  "kind": {"Func": {"func": f"{p}_apply",
                                    "arg": BASE + i, "arg2": OUT + i}}})

# Fault-tolerance offload (Persist node = snapshot SHM transaction memory to FS).
#   WC_PERSIST=<dir>       async: leaf at end, background write joined at run end.
#   WC_PERSIST_SYNC=<dir>  restricted/synchronous: a BARRIER between apply and
#                          aggregate — the durable (fsync'd) write completes
#                          before the next stage (aggregate) starts.
persist_async = os.environ.get("WC_PERSIST")
persist_sync = os.environ.get("WC_PERSIST_SYNC")

agg_deps = apply_ids
if persist_sync:
    # Insert a synchronous checkpoint of the apply stage's SHM working set
    # (parse partitions BASE..BASE+n that apply read + apply outputs OUT..OUT+n)
    # and make aggregate depend on it, so aggregate cannot start until the
    # checkpoint is durably on disk.
    barrier_slots = [BASE + i for i in range(n)] + [OUT + i for i in range(n)]
    nodes.append({"id": "persist", "node_id": 0, "deps": apply_ids,
                  "kind": {"Persist": {"output_dir": persist_sync,
                                       "atomics": True,
                                       "stream_slots": barrier_slots,
                                       "shared_state": True,
                                       "sync": True}}})
    agg_deps = ["persist"]

nodes += [
    {"id": "aggregate", "node_id": 0, "deps": agg_deps,
     "kind": {"Aggregate": {"upstream": [OUT + i for i in range(n)],
                            "downstream": 300}}},
    {"id": "summary", "node_id": 0, "deps": ["aggregate"],
     "kind": {"Func": {"func": f"{p}_summary", "arg": 300}}},
    {"id": "save", "node_id": 0, "deps": ["summary"],
     "kind": {"Output": {"path": args.out_path}}},
]

if persist_async and not persist_sync:
    # Async offload of the full pipeline's SHM stream slots (input, parse, apply,
    # aggregate) as a leaf — background write, joined at end of run.
    persist_slots = ([0] + [BASE + i for i in range(n)]
                     + [OUT + i for i in range(n)] + [300])
    nodes.append({"id": "persist", "node_id": 0, "deps": ["summary"],
                  "kind": {"Persist": {"output_dir": persist_async,
                                       "atomics": True,
                                       "stream_slots": persist_slots,
                                       "shared_state": True,
                                       "sync": False}}})

print(json.dumps({
    "shm_path": args.shm_path,
    "wasm_path": WASM,
    "log_level": "warn",
    "total_nodes": 1,
    "nodes": nodes,
}, indent=2))
