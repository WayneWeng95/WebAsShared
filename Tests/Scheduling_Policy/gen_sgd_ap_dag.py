#!/usr/bin/env python3
# gen_sgd_ap_dag.py — emit the SGD auto-placement SymbolicDag (the maintained
# ML-training pipeline, replacing the orphan PCA+decision-stump one).
#
# WHY THIS EXISTS
# ---------------
# The placement-policy study (gen_variants.py) targets DAGs/symbolic_dag/
# ml_training_auto_placement.json. The old file there used the PCA+decision-stump
# guest pipeline (ml_partition/ml_pca/ml_train/ml_validate), which:
#   (a) is an ORPHAN — nothing in the repo runs it, and it was never validated
#       through the partition→submit path; and
#   (b) is broken-by-construction — its `validate` needs both the trained stumps
#       AND the PCA test features but the DAG only routes the stumps, so it
#       early-returns and writes an empty result file even single-node.
# The benchmark's MAINTAINED pipeline is SGD (sgd_partition/sgd_encode/sgd_grad/
# sgd_update/sgd_validate, Executor/guest/src/workloads/ml_training.rs), with the
# real `weight_checksum` gate (fan-out-invariant: associative integer gradient sum
# + ONE central step). This generator emits an auto-placement variant of THAT.
#
# E=1 (SINGLE EPOCH) — the cross-node-safe choice
# -----------------------------------------------
# With E=1 the gradient workers read NO model (epoch 0 sees zero weights), so there
# is NO per-epoch model broadcast from node 0 back to the workers — the structure
# collapses to finra's single-gather shape (the one that's deadlock-free). E>1 would
# need a cross-node model scatter every epoch (the deferred many-to-many all-reduce;
# see Scheduling_Policy/NEXT_STEPS.md "Dead ends"), so it stays out of scope here.
# The weight_checksum after one epoch is still exact + fan-out-invariant, which is
# all the correctness gate and the placement contrast need.
#
# STRUCTURE (mirrors finra_auto_placement.json so gen_variants.py works UNCHANGED)
# -------------------------------------------------------------------------------
#   load            (all, replicate) Input
#   partition       (all) sgd_partition  arg=packed(10, W)         → text shards 10..10+W
#   encode_i        (all) sgd_encode     arg=packed(10+i, 40+i)    → bin shards 40..40+W
#   train_i  ← FAN  (all) sgd_grad       arg=packed(40+i, 64+i)    → grad   64..64+W
#                   gen_variants strips placement and places these per policy.
#   aggregate_train (all) Aggregate upstream=[64..64+W] downstream=1200   ← AGGREGATOR
#                   gen_variants rewrites this into the per-machine local-combine +
#                   global gather on node 0 (1 transfer/peer, deadlock-safe).
#   aggregate_global      Aggregate {}                              (auto → node 0)
#   update                sgd_update  (no arg → partitioner auto-wires it to
#                         aggregate_global's downstream)            (auto → node 0)
#   validate              sgd_validate arg=packed(40, W)  reads bins 40..40+W (node 0
#                         has them: encode is placement:all) + the model → checksum
#   save                  Output → TestOutput/ml_training_ap_result.txt
#
# Slots match Tests/Inter-Node Application_Benchmark/ML_training/gen_dag.py (E=1):
#   DATA_BASE=10  BIN_BASE=40  GRAD_BASE=64  GSUM_BASE=1200  (model slot 1900 is
#   hardcoded in the guest). Input-prep flows through machine-LOCAL slots (every
#   placement:"all" copy reads/writes the same local SHM slot numbers), so the only
#   partitioner-visible cross-node wiring is the fan→aggregate gather.
#
# Usage:  ./gen_sgd_ap_dag.py [W] > DAGs/symbolic_dag/ml_training_auto_placement.json
import json
import sys

# Slot bases must be spaced wider than the largest fanout W so the per-worker slot
# ranges never overlap: text shards [DATA_BASE, DATA_BASE+W), binary shards
# [BIN_BASE, BIN_BASE+W), grad outputs [GRAD_BASE, GRAD_BASE+W). With W up to ~64
# these stay disjoint and below the guest's hardcoded model slot (1900). (The old
# 40/64 bases collided at W>24: bin 64 == grad 64.)
DATA_BASE = 10
BIN_BASE = 100
GRAD_BASE = 300
GSUM_BASE = 1200


def packed(lo, hi):
    return (lo & 0xFFFF) | ((hi & 0xFFFF) << 16)


def build_dag(W):
    """Return the SGD auto-placement SymbolicDag dict for `W` gradient workers.
    Imported by gen_variants.py so a `--fanout W` request regenerates the fan at
    width W (the SGD analogue of finra's fanout sweep)."""
    # IMPORTANT — packed args go in `wasm_arg`, NOT `arg`.
    # The guest functions decode their slots from packed(lo, hi) = lo | (hi<<16),
    # a HUGE integer (hi<<16 ≥ 65536). The partitioner's recv-slot allocator
    # (slot.rs::collect_slots → splitter.rs next_slot) scans every integer in the
    # node kinds to find the max slot in use; a packed value in `arg` poisons that
    # scan and pushes RemoteRecv slots out of range, so the cross-node gather
    # deadlocks. finra's convention avoids this: `arg` is a small REAL slot (used by
    # the partitioner for wiring); the guest-side parameter lives in `wasm_arg`
    # (translate_func_kind forwards wasm_arg to the guest). collect_slots skips
    # `wasm_arg` (never a slot). Keep `arg` = the small input slot.
    nodes = [
        {"id": "load", "placement": "all", "deps": [],
         "kind": {"Input": {"path": "TestData/ml/sgd_100000.csv", "prefetch": True}}},
        # Input-prep (replicated per machine): split the FULL local copy into W
        # shards, then parse each text shard once into a compact binary blob. Both
        # flow through machine-local slots; deps enforce ordering.
        {"id": "partition", "placement": "all", "deps": ["load"],
         "kind": {"Func": {"func": "sgd_partition", "arg": 0,
                           "wasm_arg": packed(DATA_BASE, W)}}},
    ]

    for i in range(W):
        nodes.append({"id": f"encode_{i}", "placement": "all", "deps": ["partition"],
                      "kind": {"Func": {"func": "sgd_encode", "arg": DATA_BASE + i,
                                        "wasm_arg": packed(DATA_BASE + i, BIN_BASE + i)}}})

    # The FAN: one gradient worker per shard. placement:"all" here is a placeholder —
    # gen_variants.py strips it and assigns an explicit node_id per placement policy.
    for i in range(W):
        nodes.append({"id": f"train_{i}", "placement": "all", "deps": [f"encode_{i}"],
                      "kind": {"Func": {"func": "sgd_grad", "arg": BIN_BASE + i,
                                        "wasm_arg": packed(BIN_BASE + i, GRAD_BASE + i)}}})

    # AGGREGATOR: the upstream list declares each worker's grad output slot (64+i) so
    # the partitioner/gen_variants know where the guest writes; downstream is the
    # gather slot the reducer reads.
    nodes.append({"id": "aggregate_train", "placement": "all",
                  "deps": [f"train_{i}" for i in range(W)],
                  "kind": {"Aggregate": {"upstream": [GRAD_BASE + i for i in range(W)],
                                         "downstream": GSUM_BASE}}})

    nodes += [
        {"id": "aggregate_global", "deps": ["aggregate_train"],
         "kind": {"Aggregate": {}}},
        # Reducer: sum every worker's gradient, take ONE central step, append the
        # model. No arg → the partitioner auto-wires its input to aggregate_global's
        # downstream (exactly like finra's `merge`).
        {"id": "update", "deps": ["aggregate_global"],
         "kind": {"Func": {"func": "sgd_update"}}},
        # Validate reads all W binary shards (node 0 has them — encode is
        # placement:all) plus the freshly-written model, and emits the
        # weight_checksum gate + accuracy.
        {"id": "validate", "deps": ["update"],
         "kind": {"Func": {"func": "sgd_validate", "arg": BIN_BASE,
                           "wasm_arg": packed(BIN_BASE, W)}}},
        {"id": "save", "deps": ["validate"],
         "kind": {"Output": {"path": "TestOutput/ml_training_ap_result.txt"}}},
    ]

    return {
        "shm_path_prefix": "/dev/shm/rdma_sgd_ap",
        "log_level": "info",
        "total_nodes": 3,
        "transfer": True,
        "placement_policy": "pack",
        "nodes": nodes,
    }


if __name__ == "__main__":
    W = int(sys.argv[1]) if len(sys.argv) > 1 else 8
    print(json.dumps(build_dag(W), indent=2))
