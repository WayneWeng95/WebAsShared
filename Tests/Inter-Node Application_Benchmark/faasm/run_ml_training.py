#!/usr/bin/env python3
"""run_ml_training.py — Faasm inter-node SGD-training driver (runs on node 0).

The counterpart to ../wasmem/run_ml_training.py, measured the SAME way. Synchronous SGD,
ONE epoch (matching the WasMem auto-placement DAG, whose weight_checksum is the one-epoch
gate), as a homogeneous W-wide gradient fan of Faaslets across the cluster, state via Redis:

  1. (untimed) load CSV + init weights on node 0 (sgd_core), split into W disjoint shards.
  2. (timed) build the binary frame per shard (model replicated in + the shard's samples),
     upload to Redis {uid}_frame_{i}; launch W sgd_grad.cwasm Faaslets across nodes — each
     reads its frame, computes its gradient SUM (C*f i64), writes it back.
  3. (timed) aggregate the W gradients → ONE central integer weight step (sgd_core.apply_update,
     identical to the guest) → weight_checksum (the gate, fan-out-invariant) + accuracy.

makespan = mean ± std over reps. t0 is before the frame upload (Faasm pays its KV cost).
sgd_grad.cwasm (reused from the intra baseline) + sgd_faaslet.py are git-synced to every
node. sgd_core's integer kernel is identical to the WasMem guest → identical checksum.

Usage:
  ./run_ml_training.py --data TestData/ml/sgd_600000.csv --workers 60 --nodes 4 --reps 15
"""
import argparse
import os
import struct
import sys
import time
import uuid

import numpy as np
import redis

import faasm_lib as fl

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
SGD_CWASM = os.path.join(ROOT, "Tests", "Intra-Node Application_Benchmark", "ML_training",
                         "baseline", "faasm", "demo", "sgd_grad.cwasm")
SGD_CORE_DIR = os.path.join(ROOT, "Tests", "Intra-Node Application_Benchmark", "ML_training", "baseline")
WRAPPER = os.path.join(HERE, "sgd_faaslet.py")
RESULT = os.path.join(ROOT, "TestOutput", "ml_training_faasm_result.txt")

sys.path.insert(0, SGD_CORE_DIR)
import sgd_core as core  # noqa: E402


def make_frame(Xi, yi, Wt):
    """Binary frame for sgd_grad.cwasm: [n:u32][f:u32] + model (C*f i64) +
    n rows of [label:i32][f features:i32]. Matches the intra-node demo packer."""
    n, f = Xi.shape
    buf = bytearray(struct.pack("<II", n, f))
    buf += Wt.astype("<i8").tobytes()
    rows = np.empty((n, f + 1), dtype="<i4")
    rows[:, 0] = yi.astype("<i4"); rows[:, 1:] = Xi.astype("<i4")
    buf += rows.tobytes()
    return bytes(buf)


def main():
    env, nodes, root = fl.load_env()
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", default=os.path.join(root, "TestData", "ml", "sgd_600000.csv"))
    ap.add_argument("--workers", type=int, default=60, help="sgd_grad fan width")
    ap.add_argument("--nodes", type=int, default=4, help="spread Faaslets over the first K nodes")
    ap.add_argument("--reps", type=int, default=15)
    ap.add_argument("--expect", type=int, default=None, help="gate weight_checksum against this")
    ap.add_argument("--csv", default=os.path.join(HERE, "results_ml_training.csv"))
    args = ap.parse_args()

    nodes = nodes[:max(1, min(args.nodes, len(nodes)))]
    W = args.workers
    port = int(env.get("AGENT_PORT", "9600"))
    rh, rp = env["REDIS_HOST"], int(env.get("REDIS_PORT", "6379"))
    fenv = {"REDIS_HOST": rh, "REDIS_PORT": str(rp), "SGD_CWASM": SGD_CWASM,
            "WASMTIME": env.get("WASMTIME", "wasmtime")}

    for f in (args.data, SGD_CWASM):
        if not os.path.exists(f):
            sys.exit(f"[ml_training] missing on node 0: {f}")
    rds = redis.Redis(host=rh, port=rp)
    try:
        rds.ping()
    except Exception as ex:
        sys.exit(f"[ml_training] Redis unreachable at {rh}:{rp} ({ex})")
    fl.health_check(nodes, port)

    # OFFLINE (untimed): load data, init weights, split into W disjoint shards.
    X, y, F = core.load_csv(args.data)
    N = X.shape[0]
    lr_den = N * core.SGD_LR_K
    bounds = [(k * N // W, (k + 1) * N // W) for k in range(W)]

    new = not os.path.exists(args.csv)
    fcsv = open(args.csv, "a")
    if new:
        fcsv.write("samples,workers,nodes_used,makespan_mean_ms,makespan_std_ms,total_job_mean_ms,"
                   "accuracy_pct,weight_checksum,expect,success,reps\n")

    print(f"[ml_training] samples={N} workers={W} nodes={len(nodes)} reps={args.reps} (1 epoch)")
    ms_list, job_list, cks_seen, acc_seen, ok_all = [], [], None, 0.0, True
    for rep in range(1, args.reps + 1):
        uid = uuid.uuid4().hex
        rds.flushdb()                                   # cold Redis — no warm state across reps
        Wt = core.init_weights(F)                       # fresh initial model each rep
        t0 = time.time()                                # end-to-end clock incl. the frame upload (Faasm's KV cost)
        for i, (lo, hi) in enumerate(bounds):           # build + upload each shard's frame (model replicated in)
            rds.set(f"{uid}_frame_{i}", make_frame(X[lo:hi], y[lo:hi], Wt))

        specs = [{"node": nodes[i % len(nodes)], "tag": f"train_{i}",
                  "cmd": ["python3", WRAPPER, uid, str(i)], "env": fenv} for i in range(W)]
        _, ok, per = fl.run_faaslets(specs, port, t0=t0)

        gsum = np.zeros((core.N_CLASSES, F), dtype=np.int64)   # aggregate the W gradients
        for i in range(W):
            gb = rds.get(f"{uid}_g_{i}")
            if gb is None or len(gb) < core.N_CLASSES * F * 8:
                ok = False
                continue
            gsum += np.frombuffer(gb, dtype=np.int64).reshape(core.N_CLASSES, F)
        Wt = core.apply_update(Wt, gsum, lr_den)        # ONE central integer step (= guest sgd_update)
        cks = int(core.checksum(Wt))
        correct, total = core.accuracy(Wt, X, y)
        acc_seen = (100.0 * correct / total) if total else 0.0
        makespan = int((time.time() - t0) * 1000)        # clock stops after the update
        job_ms = sum(per.values())

        os.makedirs(os.path.dirname(RESULT), exist_ok=True)
        with open(RESULT, "w") as f:
            f.write(f"=== sgd_training ===\nsamples={N}\nweight_checksum={cks}\n"
                    f"train_correct={correct}\ntrain_total={total}\naccuracy={acc_seen:.2f}%\n")

        gate = args.expect if args.expect is not None else (cks_seen if cks_seen is not None else cks)
        cks_seen = cks
        success = ok and cks == gate
        ok_all = ok_all and success
        ms_list.append(makespan)
        job_list.append(job_ms)
        print(f"[ml_training] rep {rep}: makespan={makespan}ms total_job={job_ms}ms "
              f"weight_checksum={cks} gate={gate} ok={success}")

    mean_ms, std_ms = fl.mean_std(ms_list)
    mean_ms, std_ms = int(mean_ms), round(std_ms, 1)
    job_mean = int(sum(job_list) / len(job_list))
    gate = args.expect if args.expect is not None else cks_seen
    fcsv.write(f"{N},{W},{len(nodes)},{mean_ms},{std_ms},{job_mean},"
               f"{round(acc_seen, 2)},{cks_seen},{gate},{ok_all},{args.reps}\n")
    fcsv.close()
    print(f"[ml_training] mean makespan={mean_ms}±{std_ms}ms total_job={job_mean}ms "
          f"accuracy={acc_seen:.2f}% weight_checksum={cks_seen} success={ok_all} → {args.csv}")
    print(f"RESULT weight_checksum={cks_seen} makespan_ms={mean_ms} std={std_ms} success={ok_all}")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
