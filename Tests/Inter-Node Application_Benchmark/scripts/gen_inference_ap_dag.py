#!/usr/bin/env python3
# gen_inference_ap_dag.py — emit the MNIST-inference auto-placement SymbolicDag
# (workload #6), the cross-node analogue of gen_sgd_ap_dag.py.
#
# WHY THIS EXISTS
# ---------------
# The inter-node application benchmark drives our platform through
# gen_variants.py → partition → submit, exactly like ml_training. ml_training's
# fan (the SGD gradient workers) is auto-placed per policy and gathered with the
# per-machine local-combine pattern (one transfer/peer, deadlock-safe). MNIST
# inference has the SAME shape — a homogeneous W-wide `predict_*` fan over a
# broadcast model — so it reuses gen_variants.py's generic fan transform
# UNCHANGED. This generator emits the width-W auto-placement variant; gen_variants
# imports build_dag so a `--fanout W` request regenerates the fan at width W.
#
# WHY THE GATE STILL HOLDS (EXPERIMENT_RUNBOOK §1/§5.2)
# -----------------------------------------------------
# Each test sample is classified INDEPENDENTLY (integer forward pass + argmax), so
# the prediction checksum (Σ predicted labels) and the correct count are
# FAN-OUT-INVARIANT: identical no matter how the test set is sharded across the W
# workers or how those workers are placed across nodes. A plain order-independent
# integer sum.
#
# STRUCTURE (mirrors ml_training_auto_placement.json so gen_variants.py works UNCHANGED)
# -------------------------------------------------------------------------------
#   load_model  (all, replicate) Input slot 5
#   setup_model (all) infer_setup_model arg=5      → broadcast model into slot 1901
#   load_data   (all, replicate) Input slot 0
#   partition   (all) infer_partition wasm_arg=packed(10, W)  → shards 10..10+W
#   predict_i ← FAN (all) infer_predict wasm_arg=packed(10+i, 300+i)  → preds 300..300+W
#                   gen_variants strips placement and places these per policy; each
#                   reads its same-machine shard + the locally-broadcast model.
#   aggregate_predict (all) Aggregate upstream=[300..300+W] downstream=1200  ← AGGREGATOR
#                   gen_variants rewrites this into the per-machine local-combine +
#                   global gather on node 0 (1 transfer/peer, deadlock-safe).
#   aggregate_global  Aggregate {}                                   (auto → node 0)
#   reduce            infer_reduce  (NO arg → partitioner auto-wires it to
#                     aggregate_global's downstream slot, like sgd_update)  (auto → node 0)
#   save              Output → TestOutput/ml_inference_ap_result.txt
#
# Slots match Tests/Intra-Node Application_Benchmark/ML_inference/gen_dag.py and the
# guest (Executor/guest/src/workloads/ml_inference.rs): the model file loads into
# I/O slot 5, the broadcast model stream slot 1901 is hardcoded in the guest, the
# data input is slot 0. Input-prep (partition/setup_model) is placement:"all", so
# every node builds its OWN shards + model copy from a local replica; the only
# partitioner-visible cross-node wiring is the fan→aggregate gather.
#
# Usage:  ./gen_inference_ap_dag.py [W] [MODEL_PATH] [DATA_PATH] \
#             > DAGs/symbolic_dag/ml_inference_auto_placement.json
import json
import sys

# Slot bases spaced wider than the largest fanout W so the per-worker ranges never
# overlap: data shards [PART_BASE, PART_BASE+W), predict outputs [OUT_BASE, OUT_BASE+W).
# Both stay below the guest's hardcoded model stream slot (1901) for W up to ~64.
MODEL_IO = 5         # I/O slot the model file is loaded into (matches the guest)
DATA_IO = 0          # I/O slot the test data is loaded into (INPUT_IO_SLOT)
PART_BASE = 10       # predict-worker shard slots: PART_BASE .. PART_BASE+W
OUT_BASE = 300       # predict outputs: infer_predict(i) writes OUT_BASE+i
AGG_DOWN = 1200      # aggregate_predict gather slot

# Shared with the intra-node ML_inference benchmark: the frozen integer linear
# classifier (gen_model.py) and the synthetic test set (ML_training/gen_data.py,
# "label,f0..f15" per line). The model is keyed to the same per-class prototypes
# as the data, so argmax(Σ w·x) is a meaningful classifier and the prediction
# checksum is exact + fan-out-invariant.
DEFAULT_MODEL = "TestData/ml/infer_model.txt"
DEFAULT_DATA = "TestData/ml/test_50000.csv"


def packed(lo, hi):
    return (lo & 0xFFFF) | ((hi & 0xFFFF) << 16)


def build_dag(W, model_path=DEFAULT_MODEL, data_path=DEFAULT_DATA):
    """Return the MNIST-inference auto-placement SymbolicDag dict for `W` predict
    workers. Imported by gen_variants.py so a `--fanout W` request regenerates the
    fan at width W (the inference analogue of ml_training's gen_sgd_ap_dag)."""
    # IMPORTANT — packed args go in `wasm_arg`, NOT `arg`.
    # infer_partition / infer_predict decode their slots from packed(lo, hi) =
    # lo | (hi<<16), a HUGE integer. The partitioner's recv-slot allocator
    # (slot.rs::collect_slots → splitter.rs next_slot) scans every integer in the
    # node kinds to find the max slot in use; a packed value in `arg` poisons that
    # scan and pushes RemoteRecv slots out of range, deadlocking the gather.
    # collect_slots SKIPS `wasm_arg`, so the packed guest parameter lives there;
    # `arg` stays a small REAL slot the partitioner can safely account for.
    nodes = [
        # Broadcast model: load the pre-trained weights (replicated per machine) and
        # publish them into the hardcoded broadcast stream slot once per node. All W
        # predict workers on that node non-destructively broadcast-read it.
        {"id": "load_model", "placement": "all", "deps": [],
         "kind": {"Input": {"path": model_path, "slot": MODEL_IO,
                            "prefetch": True, "replicate": True}}},
        {"id": "setup_model", "placement": "all", "deps": ["load_model"],
         "kind": {"Func": {"func": "infer_setup_model", "arg": MODEL_IO}}},
        # Test data (replicated per machine): zero-copy contiguous split of the FULL
        # local copy into W shards. The split is cheap (no parsing); the per-sample
        # forward pass happens in the placed predict workers, so leaving partition
        # placement:"all" costs only the shard bookkeeping, not W× the parse.
        {"id": "load_data", "placement": "all", "deps": [],
         "kind": {"Input": {"path": data_path, "slot": DATA_IO,
                            "prefetch": True, "replicate": True}}},
        {"id": "partition", "placement": "all", "deps": ["load_data"],
         "kind": {"Func": {"func": "infer_partition", "arg": 0,
                           "wasm_arg": packed(PART_BASE, W)}}},
    ]

    # The FAN: one predict worker per shard, each reads its same-machine shard +
    # the locally-broadcast model. placement:"all" here is a placeholder —
    # gen_variants.py strips it and assigns an explicit node_id per placement policy.
    for i in range(W):
        nodes.append({"id": f"predict_{i}", "placement": "all",
                      "deps": ["partition", "setup_model"],
                      "kind": {"Func": {"func": "infer_predict", "arg": PART_BASE + i,
                                        "wasm_arg": packed(PART_BASE + i, OUT_BASE + i)}}})

    # AGGREGATOR: upstream declares each worker's prediction output slot (OUT_BASE+i)
    # so gen_variants/the partitioner know where the guest writes; downstream is the
    # gather slot the reducer reads.
    nodes.append({"id": "aggregate_predict", "placement": "all",
                  "deps": [f"predict_{i}" for i in range(W)],
                  "kind": {"Aggregate": {"upstream": [OUT_BASE + i for i in range(W)],
                                         "downstream": AGG_DOWN}}})

    nodes += [
        {"id": "aggregate_global", "deps": ["aggregate_predict"],
         "kind": {"Aggregate": {}}},
        # Reducer: sum the per-worker `pred,correct,total,predsum` records, report
        # accuracy + the prediction checksum (the gate). NO arg — the partitioner
        # auto-wires it to aggregate_global's downstream slot (same as sgd_update).
        {"id": "reduce", "deps": ["aggregate_global"],
         "kind": {"Func": {"func": "infer_reduce"}}},
        {"id": "save", "deps": ["reduce"],
         "kind": {"Output": {"path": "TestOutput/ml_inference_ap_result.txt"}}},
    ]

    return {
        "shm_path_prefix": "/dev/shm/rdma_mnist_ap",
        "log_level": "info",
        "total_nodes": 3,
        "transfer": True,
        "placement_policy": "balanced",
        "nodes": nodes,
    }


if __name__ == "__main__":
    W = int(sys.argv[1]) if len(sys.argv) > 1 else 8
    model = sys.argv[2] if len(sys.argv) > 2 else DEFAULT_MODEL
    data = sys.argv[3] if len(sys.argv) > 3 else DEFAULT_DATA
    print(json.dumps(build_dag(W, model, data), indent=2))
