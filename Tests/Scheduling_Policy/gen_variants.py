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

POLICIES = ("pack", "balanced", "spread", "random")


def source_dag(workload: str) -> str:
    return os.path.join(ROOT, "DAGs", "symbolic_dag", f"{workload}_auto_placement.json")


def build(workload: str, policy: str, nodes: int, input_path: str | None = None) -> dict:
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

    fan = FAN_PREFIX[workload]
    if fan is not None:
        # Auto-place the fan stage: drop placement:"all" so the policy decides
        # where each shard lands. Disable converge so the tail isn't pinned.
        d["converge_on_coordinator"] = False
        touched = 0
        for n in d.get("nodes", []):
            if n.get("id", "").startswith(fan) and "placement" in n:
                n.pop("placement")
                touched += 1
        if touched == 0:
            sys.exit(f"[gen] no '{fan}*' nodes found in {workload} — DAG layout changed?")
    return d


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("workload", choices=list(FAN_PREFIX))
    ap.add_argument("policy", choices=POLICIES)
    ap.add_argument("--nodes", type=int, default=4)
    ap.add_argument("--input", default=None, help="override Input node path (size sweep)")
    ap.add_argument("--out", default=None, help="output path (default: stdout)")
    args = ap.parse_args()

    d = build(args.workload, args.policy, args.nodes, args.input)
    text = json.dumps(d, indent=2)
    if args.out:
        with open(args.out, "w") as f:
            f.write(text + "\n")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
