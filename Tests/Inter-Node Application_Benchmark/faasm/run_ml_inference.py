#!/usr/bin/env python3
"""run_ml_inference.py — Faasm inter-node MNIST-inference driver (runs on node 0).

The counterpart to ../wasmem/run_ml_inference.py, measured the SAME way. Inference is a
homogeneous W-wide predict fan of Faaslets across the cluster, all state through Redis:

  1. (untimed) load model + test CSV on node 0 (infer_core), split into W disjoint shards.
  2. (timed) build the binary frame per shard (model replicated in + the shard's samples),
     upload to Redis {uid}_frame_{i}; launch W infer_predict.cwasm Faaslets across nodes —
     each reads its frame, runs the integer forward pass, writes [correct][total][predsum].
  3. (timed) aggregate the per-shard results → accuracy + prediction_checksum (the gate,
     fan-out-invariant: each sample classified independently → Σ predicted labels is exact).

makespan = mean ± std over reps. t0 is before the frame upload (Faasm pays its KV
serialization, per the methodology). infer_predict.cwasm (reused from the intra baseline) +
infer_faaslet.py are git-synced to every node. Same integer kernel as the WasMem guest.

Usage:
  ./run_ml_inference.py --data TestData/ml/test_600000.csv --workers 60 --nodes 4 --reps 15
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
INFER_CWASM = os.path.join(ROOT, "Tests", "Intra-Node Application_Benchmark", "ML_inference",
                           "baseline", "faasm", "demo", "infer_predict.cwasm")
INFER_CORE_DIR = os.path.join(ROOT, "Tests", "Intra-Node Application_Benchmark", "ML_inference", "baseline")
WRAPPER = os.path.join(HERE, "infer_faaslet.py")
RESULT = os.path.join(ROOT, "TestOutput", "ml_inference_faasm_result.txt")
DEFAULT_MODEL = os.path.join(ROOT, "TestData", "ml", "infer_model.txt")

sys.path.insert(0, INFER_CORE_DIR)
import infer_core as core  # noqa: E402


def make_frame(Xi, yi, Wt):
    """Binary frame for infer_predict.cwasm: [n:u32][f:u32] + model (C*f i64) +
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
    ap.add_argument("--data", default=os.path.join(root, "TestData", "ml", "test_600000.csv"))
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument("--workers", type=int, default=60, help="predict-worker fan width")
    ap.add_argument("--nodes", type=int, default=4, help="spread Faaslets over the first K nodes")
    ap.add_argument("--reps", type=int, default=15)
    ap.add_argument("--expect", type=int, default=None, help="gate prediction_checksum against this")
    ap.add_argument("--csv", default=os.path.join(HERE, "results_ml_inference.csv"))
    args = ap.parse_args()

    nodes = nodes[:max(1, min(args.nodes, len(nodes)))]
    W = args.workers
    port = int(env.get("AGENT_PORT", "9600"))
    rh, rp = env["REDIS_HOST"], int(env.get("REDIS_PORT", "6379"))
    fenv = {"REDIS_HOST": rh, "REDIS_PORT": str(rp), "INFER_CWASM": INFER_CWASM,
            "WASMTIME": env.get("WASMTIME", "wasmtime")}

    for f in (args.data, args.model, INFER_CWASM):
        if not os.path.exists(f):
            sys.exit(f"[ml_inference] missing on node 0: {f}")
    rds = redis.Redis(host=rh, port=rp)
    try:
        rds.ping()
    except Exception as ex:
        sys.exit(f"[ml_inference] Redis unreachable at {rh}:{rp} ({ex})")
    fl.health_check(nodes, port)

    # OFFLINE (untimed): load model + test set, split into W disjoint shards.
    X, y = core.load_csv(args.data)
    _, F, Wt = core.load_model(args.model)
    N = X.shape[0]
    bounds = [(k * N // W, (k + 1) * N // W) for k in range(W)]
    # numpy ground-truth checksum (sanity / default gate)
    gt_correct, gt_total, gt_predsum = core.evaluate(X, y, Wt)

    new = not os.path.exists(args.csv)
    fcsv = open(args.csv, "a")
    if new:
        fcsv.write("samples,workers,nodes_used,makespan_mean_ms,makespan_std_ms,total_job_mean_ms,"
                   "accuracy_pct,prediction_checksum,expect,success,reps\n")

    print(f"[ml_inference] samples={N} workers={W} nodes={len(nodes)} reps={args.reps} "
          f"(gt checksum={gt_predsum} acc={100.0*gt_correct/gt_total:.2f}%)")
    ms_list, job_list, cks_seen, acc_seen, ok_all = [], [], None, 0.0, True
    for rep in range(1, args.reps + 1):
        uid = uuid.uuid4().hex
        rds.flushdb()                                   # cold Redis — no warm state across reps
        t0 = time.time()                                # end-to-end clock incl. the frame upload (Faasm's KV cost)
        for i, (lo, hi) in enumerate(bounds):           # build + upload each shard's frame
            rds.set(f"{uid}_frame_{i}", make_frame(X[lo:hi], y[lo:hi], Wt))

        specs = [{"node": nodes[i % len(nodes)], "tag": f"predict_{i}",
                  "cmd": ["python3", WRAPPER, uid, str(i)], "env": fenv} for i in range(W)]
        _, ok, per = fl.run_faaslets(specs, port, t0=t0)

        correct = total = predsum = 0                   # aggregate the per-shard results
        for i in range(W):
            rb = rds.get(f"{uid}_r_{i}")
            if rb is None or len(rb) < 24:
                ok = False
                continue
            c2, t2, p2 = struct.unpack("<qqq", rb[:24])
            correct += c2; total += t2; predsum += p2
        makespan = int((time.time() - t0) * 1000)        # clock stops after aggregate
        job_ms = sum(per.values())
        acc_seen = (100.0 * correct / total) if total else 0.0

        os.makedirs(os.path.dirname(RESULT), exist_ok=True)
        with open(RESULT, "w") as f:
            f.write(f"=== mnist_inference ===\ntest_samples={total}\ncorrect={correct}\n"
                    f"prediction_checksum={predsum}\naccuracy={acc_seen:.2f}%\n")

        gate = args.expect if args.expect is not None else (cks_seen if cks_seen is not None else predsum)
        cks_seen = predsum
        success = ok and predsum == gate
        ok_all = ok_all and success
        ms_list.append(makespan)
        job_list.append(job_ms)
        print(f"[ml_inference] rep {rep}: makespan={makespan}ms total_job={job_ms}ms "
              f"checksum={predsum} gate={gate} ok={success}")

    mean_ms, std_ms = fl.mean_std(ms_list)
    mean_ms, std_ms = int(mean_ms), round(std_ms, 1)
    job_mean = int(sum(job_list) / len(job_list))
    gate = args.expect if args.expect is not None else cks_seen
    fcsv.write(f"{N},{W},{len(nodes)},{mean_ms},{std_ms},{job_mean},"
               f"{round(acc_seen, 2)},{cks_seen},{gate},{ok_all},{args.reps}\n")
    fcsv.close()
    print(f"[ml_inference] mean makespan={mean_ms}±{std_ms}ms total_job={job_mean}ms "
          f"accuracy={acc_seen:.2f}% checksum={cks_seen} success={ok_all} → {args.csv}")
    print(f"RESULT checksum={cks_seen} makespan_ms={mean_ms} std={std_ms} success={ok_all}")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
