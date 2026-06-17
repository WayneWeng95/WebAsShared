#!/usr/bin/env python3
"""gen_variants.py — emit a policy-sensitive SymbolicDag variant.

Usage:
    gen_variants.py <workload> <policy> [--nodes N] [--out PATH]

<workload> ∈ {word_count, finra, ml_training}
<policy>   ∈ {pack, balanced, spread, random}

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

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))

# Per-workload parallel "fan" stage to auto-place. None = leave as control.
FAN_PREFIX = {
    "word_count": None,        # control — structurally pinned, cannot auto-place
    "finra": "rule_",          # 8 audit rules
    "ml_training": "train_",   # 8 SGD training shards
}

# Per-machine aggregator that fans IN the parallel stage. When the fan is
# distributed, this MUST collapse from placement:"all" to a single node, else the
# N shards × M per-machine aggregators form an all-to-all cross-node RDMA gather
# that deadlocks the executor. Collapsing makes it a clean N→1 fan-in.
AGGREGATOR = {
    "word_count": None,
    "finra": "aggregate_rules",
    "ml_training": "aggregate_train",
}

POLICIES = ("pack", "balanced", "spread", "random")


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
          fanout: int | None = None, pack_cap: int | None = None) -> dict:
    if workload not in FAN_PREFIX:
        sys.exit(f"unknown workload {workload!r}; expected one of {list(FAN_PREFIX)}")
    if policy not in POLICIES:
        sys.exit(f"unknown policy {policy!r}; expected one of {POLICIES}")

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

    # Optional fanout override: set the parallel stage's `fanout`. On a
    # placement:"all" node this is the TOTAL worker count split across the
    # cluster (e.g. fanout 64 on 4 nodes → 16 workers/node).
    if fanout is not None:
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
    if pack_cap and policy == "pack":
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

        # ── Option 2: pure-gather placement ──────────────────────────────────────
        # Place each fan node EXPLICITLY per policy (node_id), and keep the
        # input-prep stages replicated on every node so each worker reads its LOCAL
        # input. Only the fan OUTPUTS cross the network, gathering to node 0 → a
        # pure N→1 gather. No node both sends and receives across peers, so the
        # bidirectional scatter-gather hub deadlock can't occur.
        node_ids = _fan_placement(len(fan_nodes), nodes, policy, pack_cap)
        for (_, n), nid in zip(fan_nodes, node_ids):
            n["node_id"] = nid

        # Collapse the per-machine aggregator to a SINGLE fan-in node on node 0 with
        # ID-based upstream (so cross-node fan outputs resolve to their RemoteRecv
        # slots instead of being dropped). Each fan node gets the matching
        # output_slot so the partitioner knows where the guest writes.
        agg_node = next((n for n in d.get("nodes", []) if n.get("id") == AGGREGATOR[workload]), None)
        if agg_node is None:
            sys.exit(f"[gen] aggregator '{AGGREGATOR[workload]}' not found in {workload}")
        agg_inner = agg_node["kind"]["Aggregate"]
        slots = list(agg_inner.get("upstream", []))
        if len(slots) < len(fan_nodes):
            sys.exit(f"[gen] aggregator has {len(slots)} upstream slots < {len(fan_nodes)} fan nodes")
        for (_, n), slot in zip(fan_nodes, slots):
            n["output_slot"] = slot
        agg_inner.pop("upstream", None)
        agg_inner["upstream_nodes"] = [n["id"] for _, n in fan_nodes]
        agg_node.pop("placement", None)
        agg_node["node_id"] = 0

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
        # Use the receiver-initiated handshake for the gather cross-edges: node 0
        # receives many transfers from many peers, and the sender-initiated
        # rendezvous stalls there. RI ("receiver announces first") avoids it.
        d["remote_protocol"] = "receiver_init"
    return d


def _fan_placement(n_fan: int, nodes: int, policy: str, pack_cap: int | None) -> list[int]:
    """node_id per fan node (index order) for a given policy.
    pack: fill node 0 (up to pack_cap, default all) then spill; balanced/spread:
    round-robin across nodes; random: seeded uniform (reproducible)."""
    if policy == "pack":
        cap = pack_cap if pack_cap else n_fan
        ids, node, cnt = [], 0, 0
        for _ in range(n_fan):
            if cnt >= cap and node < nodes - 1:
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
    ap.add_argument("policy", choices=POLICIES)
    ap.add_argument("--nodes", type=int, default=4)
    ap.add_argument("--input", default=None, help="override Input node path (size sweep)")
    ap.add_argument("--fanout", type=int, default=None, help="override parallel-stage fanout (total workers across cluster)")
    ap.add_argument("--pack-cap", type=int, default=None, help="per-node worker cap for first-fit pack consolidation (e.g. 16 = cores/node)")
    ap.add_argument("--out", default=None, help="output path (default: stdout)")
    args = ap.parse_args()

    d = build(args.workload, args.policy, args.nodes, args.input, args.fanout, args.pack_cap)
    text = json.dumps(d, indent=2)
    if args.out:
        with open(args.out, "w") as f:
            f.write(text + "\n")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
