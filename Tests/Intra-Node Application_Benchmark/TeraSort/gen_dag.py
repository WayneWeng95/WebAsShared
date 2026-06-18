#!/usr/bin/env python3
# gen_dag.py — emit a native single-node TeraSort DAG with N-way all-to-all shuffle.
#
#   load ─► ts_distribute(N) ─► ts_partition_0 ─┐                 ┌─► ts_merge_0 ─► save_0
#                              ├─ ts_partition_1 ┼─ aggregate_j ──┼─► ts_merge_1 ─► save_1
#                              ├─     ...        │  (per owner j)  ├─►   ...
#                              └─ ts_partition_N ┘                 └─► ts_merge_N ─► save_N
#
# ts_distribute splits the input into N contiguous worker shards (zero-copy).
# Each ts_partition routes its records by key-prefix into N per-owner sub-slots
# (100 + i*N + j). The host Aggregate for owner j splices the N sub-slots
# {100+i*N+j : i} into slot 400+j — the zero-copy page-chain SHUFFLE, the headline
# this benchmark measures (baselines serialize every record through a KV instead).
# ts_merge sorts owner j's range and emits a one-line summary (records / keysum /
# sorted flag) to its own output file; run.sh sums them into the fan-out-invariant
# correctness gate.
#
# Usage: gen_dag.py N RECORDS_PATH OUT_PREFIX SHM_PATH
# Env: TS_WASM overrides the guest module (e.g. a precompiled .cwasm for AOT).
import json
import os
import sys

N = int(sys.argv[1])
records = sys.argv[2]
out_prefix = sys.argv[3]   # per-range files: <out_prefix>.part<j>
shm_path = sys.argv[4]
WASM = os.environ.get('TS_WASM',
                      'Executor/target/wasm32-unknown-unknown/release/guest.wasm')

DIST_BASE = 10      # per-worker shard slots: DIST_BASE .. DIST_BASE+N
PART_BASE = 100     # ts_partition(i) writes owner j to PART_BASE + i*N + j
MERGE_IN = 400      # host Aggregate gathers owner j into MERGE_IN + j
OUT_BASE = 2        # ts_merge(j) writes its summary to I/O slot OUT_BASE + j

# The routed copies must total ~1x input to feed the Aggregate, so that 1x is the
# SHM floor. The danger is the input shards ALSO living at once: if all N
# partitions run concurrently, every shard (another 1x input) coexists with the
# copies → 2x SHM, which overflows the 2 GiB SHM window for big inputs. So for
# large inputs we cap how many partitions run at once (PART_GROUP), freeing each
# group's shards before the next group starts. Small inputs keep full concurrency.
try:
    _size_mb = os.path.getsize(records) / (1024 * 1024)
except OSError:
    _size_mb = 0
_GROUP_THRESHOLD_MB = float(os.environ.get('TS_GROUP_THRESHOLD_MB', '600'))
if 'TS_PART_GROUP' in os.environ:
    PART_GROUP = max(1, int(os.environ['TS_PART_GROUP']))
elif _size_mb > _GROUP_THRESHOLD_MB:
    # Large input: the routed copies already cost ~1x the (framed) input in SHM,
    # so run partitions serially and free each shard before the next — the freed
    # input pages are recycled into the copies, keeping SHM near 1x instead of 2x.
    PART_GROUP = 1
else:
    PART_GROUP = N                # small input: full concurrency


def pack(low, high):
    return (low & 0xFFFF) | (high << 16)


nodes = [
    {"id": "load", "deps": [],
     "kind": {"Input": {"path": records, "slot": 0, "prefetch": True}}},
    {"id": "distribute", "deps": ["load"],
     "kind": {"Func": {"func": "ts_distribute", "arg": pack(DIST_BASE, N)}}},
]

# ── partition workers ────────────────────────────────────────────────────────
# Each ts_partition streams its shard (bounded heap) and routes records into its
# owner sub-slots. Its shard is freed IMMEDIATELY AFTER (per-shard FreeSlots,
# dep'd only on that partition), so the input pages don't coexist with the routed
# copies across the whole partition wave — the SHM high-water stays near 1× input
# instead of 2×, which is what lets the largest inputs fit the 2 GiB SHM window.
part_ids = []
prev_group_frees = []   # all free_shard ids of the previous group (gate the next)
for g0 in range(0, N, PART_GROUP):
    group = range(g0, min(g0 + PART_GROUP, N))
    # Every partition in this group waits on `distribute` plus the previous
    # group's frees, so at most PART_GROUP shards are resident at once.
    deps = ["distribute"] + prev_group_frees
    this_group_frees = []
    for i in group:
        pid = f"part_{i}"
        part_ids.append(pid)
        nodes.append({"id": pid, "deps": deps,
                      "kind": {"Func": {"func": "ts_partition",
                                        "arg": pack(DIST_BASE + i, N)}}})
        nodes.append({"id": f"free_shard_{i}", "deps": [pid],
                      "kind": {"FreeSlots": {"stream": [DIST_BASE + i]}}})
        this_group_frees.append(f"free_shard_{i}")
    prev_group_frees = this_group_frees

# ── per-owner shuffle (Aggregate) + merge + save ─────────────────────────────
for j in range(N):
    agg_id = f"agg_{j}"
    merge_id = f"merge_{j}"
    save_id = f"save_{j}"
    upstream = [PART_BASE + i * N + j for i in range(N)]
    nodes.append({"id": agg_id, "deps": part_ids,
                  "kind": {"Aggregate": {"upstream": upstream,
                                         "downstream": MERGE_IN + j}}})
    nodes.append({"id": merge_id, "deps": [agg_id],
                  "kind": {"Func": {"func": "ts_merge",
                                    "arg": pack(MERGE_IN + j, j)}}})
    nodes.append({"id": save_id, "deps": [merge_id],
                  "kind": {"Output": {"path": f"{out_prefix}.part{j}",
                                      "slot": OUT_BASE + j}}})

print(json.dumps({
    "shm_path": shm_path,
    "wasm_path": WASM,
    "log_level": "warn",
    "nodes": nodes,
}, indent=2))
