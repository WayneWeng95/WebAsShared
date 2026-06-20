#!/usr/bin/env python3
"""gen_variants.py — emit a placement SymbolicDag variant (Inter-Node App Benchmark copy).

This is the Inter-Node Application Benchmark's self-contained copy of the
Scheduling-Policy generator, extended to all SIX workloads. Unlike the
Scheduling-Policy experiment (which SWEEPS the policy axis), this benchmark targets
MAKESPAN and fixes a single policy — **balanced** — for every workload, so `policy`
defaults to `balanced` here.

Usage:
    gen_variants.py <workload> [policy] [--nodes N] [--fanout W] [--out PATH]

<workload> ∈ {word_count, finra, ml_training, ml_inference, matrix, terasort}
<policy>   ∈ {pack, balanced, spread, random}   (default: balanced)

Why this exists
---------------
The stock DAGs in DAGs/symbolic_dag/<wl>_auto_placement.json place their heavy
parallel stages with `placement: "all"` — i.e. the partitioner replicates them
on EVERY host regardless of placement_policy — and the few remaining auto-placed
nodes (the convergence tail) get pinned to node 0 by `converge_on_coordinator`.
Net effect: pack / balanced / spread / random all produce the SAME placement, so
the policy has nothing to decide.

To actually exercise the policy we transform two of the three workloads so their
parallel-compute fan is AUTO-placed (the policy decides where each shard lands):

    finra        → strip placement:"all" from the `rule_*` fan   (8 audit rules)
    ml_training  → strip placement:"all" from the `train_*` fan   (8 SGD shards)

and we disable converge_on_coordinator so the tail isn't force-pinned to node 0.

word_count is left STRUCTURALLY UNCHANGED and serves as a CONTROL: its `map_n`
fan is welded to wc_distribute's output-slot layout and cannot be auto-placed
without restructuring, so it is expected to be policy-INSENSITIVE. Seeing it stay
flat while finra/ml_training move is itself a result: workloads dominated by
`placement:"all"` stages don't respond to placement policy.

The emitted variant is still a SymbolicDag. It is pre-partitioned OFFLINE by
prepartition.sh (via the `partition` binary with NO live hints) so the embedded
placement_policy is faithfully realized — on a live cluster the coordinator's
SCX-derived hints would otherwise override it (see splitter.rs:142,
`hints.or(policy_hints)`).
"""
import argparse
import json
import os
import sys

# This copy lives at Tests/Inter-Node Application_Benchmark/scripts/, three levels
# below the repo root (vs the Scheduling-Policy original's two).
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))

# Per-workload parallel "fan" stage to auto-place. None = leave as control.
FAN_PREFIX = {
    "word_count": None,        # control — structurally pinned, cannot auto-place
    "finra": "rule_",          # 8 audit rules
    "ml_training": "train_",   # 8 SGD training shards
    "ml_inference": "predict_",  # W MNIST predict workers (homogeneous, like train_)
    "matrix": "block_",        # r·c SUMMA block workers (block_i_j grid)
    "terasort": None,          # all-to-all shuffle — dedicated builder, not the fan path
}

# Per-machine aggregator that fans IN the parallel stage. When the fan is
# distributed, this MUST collapse from placement:"all" to a single node, else the
# N shards × M per-machine aggregators form an all-to-all cross-node RDMA gather
# that deadlocks the executor. Collapsing makes it a clean N→1 fan-in.
AGGREGATOR = {
    "word_count": None,
    "finra": "aggregate_rules",
    "ml_training": "aggregate_train",
    "ml_inference": "aggregate_predict",
    "matrix": "aggregate",
    "terasort": None,          # N owner-aggregators built by the dedicated builder
}

# Workloads whose fan is placed EXPLICITLY per node (via _fan_placement below)
# rather than through the named placement_policy. For these the pack-cap →
# `weighted` policy rewrite is skipped: their fan already carries an explicit
# node_id, and the weighted policy would also scatter the auto-placed reducer/tail
# off node 0. (finra takes its own hybrid path and returns early, so it isn't here.)
EXPLICIT_FAN = ("ml_training", "ml_inference", "matrix")

POLICIES = ("pack", "balanced", "spread", "random")

# ── finra hybrid sharding ────────────────────────────────────────────────────
# finra has 8 FIXED, heterogeneous audit rules (rule_0..7), so its fan can't be a
# homogeneous {32,48} like word_count's map workers. Instead we shard only the 5
# STATELESS per-record rules (each shard scans a disjoint 1/S of the trades and
# the merge SUMS the partials → exact total) while the 3 STATEFUL rules
# (wash/spoofing/concentration) stay single full-data nodes (their cross-record
# state can't be reconstructed from a slice). Effective fan = 3 + 5·S, so a
# requested fanout F → S = round((F-3)/5): F=32 → S=6 (33 workers), F=48 → S=9.
# The guest (Executor/guest/src/workloads/finra.rs) reads rule_id (bits 0..3),
# shard_idx (bits 3..7), n_shards (bits 7..11) from its single packed arg and
# writes a UNIQUE output slot per (rule,shard) so the aggregator splices each
# partial once. The bit layout is deliberately SMALL: the partitioner's
# collect_slots treats every integer in a node's kind as a candidate slot, so a
# large packed arg (or output slot) would overflow STREAM_SLOT_COUNT (2048).
FINRA_STATEFUL = (2, 3, 4)            # WASH_TRADE, SPOOFING, CONCENTRATION
FINRA_STATELESS = (0, 1, 5, 6, 7)     # PRICE_OUTLIER, LARGE_ORDER, AFTER_HOURS, PENNY_STOCK, ROUND_LOT
FINRA_RULE_OUT_BASE = 10              # stateful rule i → slot 10+i (matches guest)
FINRA_SHARD_OUT_BASE = 400           # stateless shard → 400 + rule_id*16 + shard_idx (matches guest)
FINRA_SHARD_STRIDE = 16
# Max rule-workers a WORKER node may run (the executor's per-node stage-width cap;
# a worker beyond this fails the job). Node 0 (local executor) is exempt — see
# _fan_placement. Empirically 16 runs, 17 fails on workers.
FINRA_MAX_WORKER_RULES = 16


def _finra_pack_arg(rule_id: int, shard_idx: int, n_shards: int) -> int:
    """Pack (rule_id, shard_idx, n_shards) into one small u32 matching the guest's
    bit layout (rule_id:0..3, shard_idx:3..7, n_shards:7..11). Kept < 2048 so the
    partitioner's collect_slots doesn't mistake it for a high slot number."""
    return (rule_id & 0x7) | ((shard_idx & 0xF) << 3) | ((n_shards & 0xF) << 7)


def source_dag(workload: str) -> str:
    return os.path.join(ROOT, "DAGs", "symbolic_dag", f"{workload}_auto_placement.json")


def first_fit_weights(fanout: int, nodes: int, cap: int) -> list[int]:
    """Per-node worker counts for first-fit *pack* consolidation: fill node 0 up
    to `cap`, then node 1, etc. e.g. fanout=32, nodes=4, cap=16 → [16,16,0,0];
    fanout=48 → [16,16,16,0]. Used as `weighted` capacity weights: the partitioner's
    Hamilton apportionment (apportion_fanout) reproduces these exact counts."""
    counts = [0] * nodes
    remaining = fanout
    for i in range(nodes):
        take = min(cap, remaining)
        counts[i] = take
        remaining -= take
        if remaining <= 0:
            break
    if remaining > 0:
        # More workers than nodes×cap can hold — top up round-robin so none are lost.
        i = 0
        while remaining > 0:
            counts[i % nodes] += 1
            remaining -= 1
            i += 1
    return counts


def build(workload: str, policy: str, nodes: int, input_path: str | None = None,
          fanout: int | None = None, pack_cap: int | None = None,
          matrix_n: int | None = None) -> dict:
    if workload not in FAN_PREFIX:
        sys.exit(f"unknown workload {workload!r}; expected one of {list(FAN_PREFIX)}")
    if policy not in POLICIES:
        sys.exit(f"unknown policy {policy!r}; expected one of {POLICIES}")

    # terasort: the all-to-all shuffle is built whole by its dedicated builder (no
    # template DAG). fanout N = partition-worker count (default N = nodes, the
    # canonical one-partition-one-owner-per-node square transpose). Partition
    # workers and range-owners are placed per policy; the builder inserts the
    # per-machine local-combine so each (source, owner) pair sends one transfer.
    if workload == "terasort":
        import gen_terasort_ap_dag
        N = fanout if fanout is not None else nodes
        part_nodes = _fan_placement(N, nodes, policy, pack_cap)
        owner_nodes = _fan_placement(N, nodes, policy, pack_cap)
        records = input_path or "TestData/terasort/records_32mb.txt"
        d = gen_terasort_ap_dag.build_dag(N, part_nodes, owner_nodes, records)
        d["total_nodes"] = nodes
        d["placement_policy"] = policy
        return d

    with open(source_dag(workload)) as f:
        d = json.load(f)

    # Strip the verbose _comment block so generated files stay readable.
    d.pop("_comment", None)

    d["total_nodes"] = nodes
    d["placement_policy"] = policy

    # Optional input-size override: rewrite every Input node's `path`. The
    # partitioner re-derives shared_inputs from these, so the override propagates
    # to RDMA staging automatically. Used by the word_count size sweep.
    if input_path is not None:
        touched = 0
        for n in d.get("nodes", []):
            k = n.get("kind", {})
            if isinstance(k, dict) and "Input" in k and "path" in k["Input"]:
                k["Input"]["path"] = input_path
                touched += 1
        if touched == 0:
            sys.exit(f"[gen] no Input node with a 'path' in {workload} to override")

    # finra hybrid sharding takes over entirely when a fanout is requested: it
    # shards the stateless rules and does its own per-policy placement, so the
    # word_count-style fanout/pack-cap/fan blocks below don't apply.
    if workload == "finra" and fanout is not None:
        _build_finra_hybrid(d, policy, nodes, fanout, pack_cap)
        return d

    # ml_training: the SGD fan is HOMOGENEOUS (W identical sgd_grad workers, each on
    # a disjoint shard), so a fanout request regenerates the whole auto-placement DAG
    # at width W = fanout (W encode + W train nodes) via gen_sgd_ap_dag, then the
    # generic fan-placement below assigns the train_* fan per policy with pack_cap —
    # the SGD analogue of finra's fanout sweep, minus the heterogeneous-rule hybrid.
    if workload == "ml_training" and fanout is not None:
        import gen_sgd_ap_dag
        d = gen_sgd_ap_dag.build_dag(fanout)
        d["total_nodes"] = nodes
        d["placement_policy"] = policy
        if input_path is not None:
            for n in d.get("nodes", []):
                k = n.get("kind", {})
                if isinstance(k, dict) and "Input" in k and "path" in k["Input"]:
                    k["Input"]["path"] = input_path

    # ml_inference: the predict fan is HOMOGENEOUS (W identical infer_predict workers
    # over disjoint test shards + a broadcast model), the exact shape of ml_training.
    # A fanout request regenerates the auto-placement DAG at width W via
    # gen_inference_ap_dag; the generic fan-placement below then assigns the
    # predict_* fan per policy. `--input` overrides the TEST DATA path (the larger,
    # swept input); the model path is fixed.
    if workload == "ml_inference" and fanout is not None:
        import gen_inference_ap_dag
        d = gen_inference_ap_dag.build_dag(
            fanout, data_path=input_path or gen_inference_ap_dag.DEFAULT_DATA)
        d["total_nodes"] = nodes
        d["placement_policy"] = policy

    # matrix: a fanout request sets the SUMMA block count W = r·c and regenerates
    # the block_i_j grid via gen_matrix_ap_dag at matrix dimension N (from --matrix-n,
    # which must match the staged A/B inputs). The generic fan-placement below then
    # assigns the block_* fan per policy. `--input` is not used (binary A/B inputs
    # are named by the DAG); size is swept via --matrix-n.
    if workload == "matrix" and fanout is not None:
        import gen_matrix_ap_dag
        if matrix_n is None:
            sys.exit("[gen] matrix --fanout requires --matrix-n (the N×N dimension)")
        d = gen_matrix_ap_dag.build_dag(fanout, matrix_n)
        d["total_nodes"] = nodes
        d["placement_policy"] = policy

    # Optional fanout override: set the parallel stage's `fanout`. On a
    # placement:"all" node this is the TOTAL worker count split across the
    # cluster (e.g. fanout 64 on 4 nodes → 16 workers/node). The EXPLICIT_FAN
    # workloads already regenerated their fan above (no `fanout` field), so they
    # skip this.
    if fanout is not None and workload not in EXPLICIT_FAN:
        touched = 0
        for n in d.get("nodes", []):
            if "fanout" in n:
                n["fanout"] = fanout
                touched += 1
        if touched == 0:
            sys.exit(f"[gen] no node with a 'fanout' field in {workload} to override")

    # First-fit pack consolidation: turn the named `pack` policy into an explicit
    # `weighted` policy that fills nodes one-at-a-time up to `pack_cap` workers
    # (= physical cores/node), so pack uses ceil(fanout/cap) nodes instead of
    # spreading thin. `balanced` and the others keep their named behaviour.
    # (EXPLICIT_FAN workloads use explicit per-node placement via _fan_placement
    # below, so they do NOT take the weighted-policy route — that would also
    # distribute the auto-placed reducer/validate/tail nodes off node 0.)
    if pack_cap and policy == "pack" and workload not in EXPLICIT_FAN:
        if fanout is None:
            sys.exit("[gen] --pack-cap requires --fanout (needs the worker count)")
        weights = first_fit_weights(fanout, nodes, pack_cap)
        d["placement_policy"] = {"type": "weighted", "weights": [float(w) for w in weights]}

    fan = FAN_PREFIX[workload]
    if fan is not None:
        # Collect fan nodes sorted by their numeric suffix.
        fan_nodes = []
        for n in d.get("nodes", []):
            if n.get("id", "").startswith(fan):
                n.pop("placement", None)
                try:
                    idx = int(n["id"][len(fan):])
                except ValueError:
                    idx = len(fan_nodes)
                fan_nodes.append((idx, n))
        if not fan_nodes:
            sys.exit(f"[gen] no '{fan}*' nodes found in {workload} — DAG layout changed?")
        fan_nodes.sort(key=lambda t: t[0])

        # ── Local-combine gather (mirrors word_count's working cross-node path) ──
        # Place each fan node EXPLICITLY per policy, then insert a PER-MACHINE local
        # aggregate so each worker sends exactly ONE combined transfer to node 0 —
        # not one per rule. This is the crucial difference: word_count gathers 1
        # transfer/peer (it combines locally first) and works; sending 2+ raw
        # transfers/peer desyncs the per-peer control stream and deadlocks. The
        # per-machine aggregate is an empty `{Aggregate:{}}` whose upstream the
        # partitioner derives from its local-rule deps (same mechanism word_count's
        # per-machine `aggregate` uses).
        node_ids = _fan_placement(len(fan_nodes), nodes, policy, pack_cap)
        for (_, n), nid in zip(fan_nodes, node_ids):
            n["node_id"] = nid

        # Co-locate each fan worker's per-shard PRODUCER (an `encode_*` node) on the
        # SAME machine as the worker, instead of leaving it placement:"all". Without
        # this every node parses ALL W shards — a fixed W-wide cost that swamps the
        # placement signal (e.g. at W=32 it makes balanced and pack converge on the
        # encode cost rather than the gather). With co-location a node only parses the
        # shards it trains on; the cheap split (`partition`) stays placement:"all", so
        # the same-machine text shard is present for the placed encoder to read. This
        # mirrors the other workloads, which never replicate input-prep W times.
        # (Only the SGD ml_training fan has such producers; finra rules read a shared
        # input, so this loop is a no-op there.)
        id_to_obj = {n["id"]: n for n in d.get("nodes", [])}
        for (_, fn), nid in zip(fan_nodes, node_ids):
            for dep in fn.get("deps", []):
                if dep.startswith("encode_") and dep in id_to_obj:
                    prod = id_to_obj[dep]
                    prod.pop("placement", None)
                    prod["node_id"] = nid

        # Each rule's hardcoded guest output slot (RULE_OUT_BASE + i) — take it from
        # the original aggregator's upstream list so the partitioner knows where the
        # guest writes and can derive the local aggregate's inputs.
        agg_node = next((n for n in d.get("nodes", []) if n.get("id") == AGGREGATOR[workload]), None)
        if agg_node is None:
            sys.exit(f"[gen] aggregator '{AGGREGATOR[workload]}' not found in {workload}")
        agg_inner = agg_node["kind"]["Aggregate"]
        slots = list(agg_inner.get("upstream", []))
        if len(slots) < len(fan_nodes):
            sys.exit(f"[gen] aggregator has {len(slots)} upstream slots < {len(fan_nodes)} fan nodes")
        for (_, n), slot in zip(fan_nodes, slots):
            n["output_slot"] = slot

        # Group rules by machine and build one local aggregate per machine.
        by_machine = {}
        for (_, n), nid in zip(fan_nodes, node_ids):
            by_machine.setdefault(nid, []).append(n["id"])
        local_aggs = []
        for m in sorted(by_machine):
            lid = f"{AGGREGATOR[workload]}_local_{m}"
            local_aggs.append(lid)
            d["nodes"].append({
                "id": lid, "node_id": m, "deps": by_machine[m],
                "kind": {"Aggregate": {}},
            })

        # The global aggregator (node 0) now gathers the per-machine LOCALS — one
        # transfer per machine — instead of the raw rules.
        agg_node.pop("placement", None)
        agg_node["node_id"] = 0
        agg_node["deps"] = local_aggs
        agg_inner.pop("upstream", None)
        agg_inner["upstream_nodes"] = local_aggs

        # Keep input-prep placement:"all" (each node computes its OWN inputs from a
        # local copy — a placed worker reads the same-machine copy via the
        # partitioner's same-machine wiring), and mark Input nodes `replicate` so
        # every node loads the FULL file. Task-parallel rules each need the whole
        # dataset; slicing it would make stateful rules (spoofing, wash) wrong.
        for n in d.get("nodes", []):
            k = n.get("kind", {})
            if isinstance(k, dict) and "Input" in k:
                k["Input"]["replicate"] = True
        d["converge_on_coordinator"] = True
    return d


def _build_finra_hybrid(d: dict, policy: str, nodes: int, fanout: int,
                        pack_cap: int | None) -> None:
    """Mutate `d` in place into the hybrid-sharded finra variant.

    Stateless rules (per-record) are split into S shards each; stateful rules
    stay single full-data nodes. Effective fan = 3 + 5·S with S = round((F-3)/5).
    All fan nodes are placed per `policy` (pack-cap first-fit) and gathered via
    the same per-machine local-combine word_count uses (one transfer/peer). Each
    fan node carries the exact `output_slot` the guest writes (stateful → 10+i;
    shard → 1000 + i*16 + shard) so the partitioner derives DISTINCT per-machine
    aggregate inputs (no slot collision, no double-count)."""
    n_state, n_stateless = len(FINRA_STATEFUL), len(FINRA_STATELESS)
    # Distribute EXACTLY (fanout - n_state) stateless shards across the 5 rules so
    # the total fan == fanout exactly (32 → 29 stateless = [6,6,6,6,5]; 48 →
    # [9,9,9,9,9]). With pack_cap=16 this lets pack fit 32 into [16,16] and 48 into
    # [16,16,16] — EVERY node, including coordinator node 0, at ≤16 workers, so the
    # placement never relies on node 0's worker-cap exemption. Different rules may
    # get different shard counts; each rule's shards still tile its own data
    # disjointly (n_shards is per-rule), so per-rule partials sum exactly.
    total_stateless = max(n_stateless, fanout - n_state)
    base, rem = divmod(total_stateless, n_stateless)
    shard_counts = [base + (1 if idx < rem else 0) for idx in range(n_stateless)]

    # Grab the original rule template (deps → aggregate_inputs) then drop the
    # stock rule_0..7 nodes; we re-emit the fan below.
    orig = {n["id"]: n for n in d["nodes"] if n.get("id", "").startswith("rule_")}
    if "rule_0" not in orig:
        sys.exit("[gen] finra DAG layout changed — no 'rule_*' nodes to shard")
    rule_deps = orig["rule_0"].get("deps", ["aggregate_inputs"])
    d["nodes"] = [n for n in d["nodes"] if not n.get("id", "").startswith("rule_")]

    def rule_node(node_id, wasm_arg, output_slot):
        return {"id": node_id, "deps": list(rule_deps), "output_slot": output_slot,
                "kind": {"Func": {"func": "finra_audit_rule", "arg": 200, "wasm_arg": wasm_arg}}}

    # Build the fan in placement order: stateful rules first (full data), then the
    # stateless shards. Composition per machine doesn't affect correctness — only
    # the per-node COUNT drives the pack/balanced placement we're measuring.
    fan = [rule_node(f"rule_{i}", i, FINRA_RULE_OUT_BASE + i) for i in FINRA_STATEFUL]
    for idx, i in enumerate(FINRA_STATELESS):
        sr = shard_counts[idx]
        for k in range(sr):
            fan.append(rule_node(f"rule_{i}_s{k}", _finra_pack_arg(i, k, sr),
                                  FINRA_SHARD_OUT_BASE + i * FINRA_SHARD_STRIDE + k))

    node_ids = _fan_placement(len(fan), nodes, policy, pack_cap)
    for n, nid in zip(fan, node_ids):
        n["node_id"] = nid
    d["nodes"].extend(fan)

    # Per-machine local-combine: one aggregate per machine over its fan nodes
    # (empty Aggregate → partitioner derives upstream from the deps' distinct
    # output_slots), then a single global aggregate gathers the per-machine locals.
    by_machine = {}
    for n, nid in zip(fan, node_ids):
        by_machine.setdefault(nid, []).append(n["id"])
    local_aggs = []
    for m in sorted(by_machine):
        lid = f"aggregate_rules_local_{m}"
        local_aggs.append(lid)
        d["nodes"].append({"id": lid, "node_id": m, "deps": by_machine[m],
                           "kind": {"Aggregate": {}}})

    agg_node = next((n for n in d["nodes"] if n.get("id") == "aggregate_rules"), None)
    if agg_node is None:
        sys.exit("[gen] finra DAG missing 'aggregate_rules'")
    agg_inner = agg_node["kind"]["Aggregate"]
    agg_node.pop("placement", None)
    agg_node["node_id"] = 0
    agg_node["deps"] = local_aggs
    agg_inner.pop("upstream", None)
    agg_inner["upstream_nodes"] = local_aggs

    # Every node loads the FULL trades (replicate): stateful rules need the whole
    # dataset, and stateless shards select their 1/S slice in-guest by record index.
    for n in d["nodes"]:
        k = n.get("kind", {})
        if isinstance(k, dict) and "Input" in k:
            k["Input"]["replicate"] = True
    d["converge_on_coordinator"] = True


def _fan_placement(n_fan: int, nodes: int, policy: str, pack_cap: int | None) -> list[int]:
    """node_id per fan node (index order) for a given policy.
    pack: fill node 0 (up to pack_cap, default all) then spill; balanced/spread:
    round-robin across nodes; random: seeded uniform (reproducible).

    WORKER nodes (id ≥ 1) are hard-capped at FINRA_MAX_WORKER_RULES: a worker that
    runs more than that many parallel rule-workers exceeds the executor's per-node
    stage-width limit and the job fails (the gather stalls). The COORDINATOR (node
    0) runs its rules through the in-process local executor and isn't subject to
    that limit, so it may hold up to pack_cap. e.g. 33 → [17,16] (2 nodes),
    48 → [17,16,15] (3 nodes) — pack stays consolidated without overloading a worker.
    Only `pack` can exceed the cap (balanced/spread/random round-robin, so their
    per-node count is ⌈n_fan/nodes⌉ ≤ 16 for the sizes we sweep)."""
    if policy == "pack":
        base_cap = pack_cap if pack_cap else n_fan
        cap_for = lambda nid: base_cap if nid == 0 else min(base_cap, FINRA_MAX_WORKER_RULES)
        ids, node, cnt = [], 0, 0
        for _ in range(n_fan):
            while node < nodes - 1 and cnt >= cap_for(node):
                node += 1
                cnt = 0
            ids.append(node)
            cnt += 1
        return ids
    if policy == "random":
        import random
        rng = random.Random(1234)
        return [rng.randrange(nodes) for _ in range(n_fan)]
    # balanced / spread → round-robin
    return [i % nodes for i in range(n_fan)]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("workload", choices=list(FAN_PREFIX))
    # This benchmark targets makespan → balanced for every workload (see module
    # docstring). policy is optional and defaults to balanced.
    ap.add_argument("policy", nargs="?", default="balanced", choices=POLICIES)
    ap.add_argument("--nodes", type=int, default=4)
    ap.add_argument("--input", default=None, help="override Input node path (size sweep)")
    ap.add_argument("--fanout", type=int, default=None, help="override parallel-stage fanout (total workers across cluster)")
    ap.add_argument("--pack-cap", type=int, default=None, help="per-node worker cap for first-fit pack consolidation (e.g. 16 = cores/node)")
    ap.add_argument("--matrix-n", type=int, default=None, help="matrix only: the N×N dimension (must match the staged A/B inputs)")
    ap.add_argument("--out", default=None, help="output path (default: stdout)")
    args = ap.parse_args()

    d = build(args.workload, args.policy, args.nodes, args.input, args.fanout,
              args.pack_cap, args.matrix_n)
    text = json.dumps(d, indent=2)
    if args.out:
        with open(args.out, "w") as f:
            f.write(text + "\n")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
