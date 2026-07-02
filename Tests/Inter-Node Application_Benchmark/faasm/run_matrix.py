#!/usr/bin/env python3
"""run_matrix.py — Faasm inter-node Matrix (SUMMA) experiment driver (runs on node 0).

The counterpart to ../wasmem/run_matrix.py, measured the SAME way. Matrix multiply
C = A·B as an r·c block grid of Faaslets across the cluster, all state through Redis:

  1. tile (node 0)   : A → r row-panels (br×N), B → c col-panels (N×bc) → Redis
                       {uid}_a_{i} / {uid}_b_{j}.
  2. block wave      : r·c block Faaslets across nodes — each reads its A_i + B_j panels,
                       runs matblock.cwasm (naive ikj kernel, same as the WasMem guest),
                       writes C_ij → {uid}_c_{i}_{j}.
  3. assemble (node 0): read every C block, fold the f64 checksum (Σ of all entries) —
                       the fan-out-invariant gate (exact integer, order-independent).

makespan = t0 (before the panel upload) → last block complete + checksum assembled — the
same partitioner-free window the WasMem driver reports. matblock.cwasm (reused from the
intra-node Matrix baseline) + matblock_faaslet.py are git-synced to every node. The
compute is N³-bound (naive, not BLAS — fairness §5), inputs tiny.

Usage:
  ./run_matrix.py --matrix-n 512 --workers 4 --nodes 4 --reps 3
  ./run_matrix.py --matrix-n 512 --workers 4 --expect 2722562338
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
MATBLOCK_CWASM = os.path.join(ROOT, "Tests", "Intra-Node Application_Benchmark", "Matrix",
                              "baseline", "faasm", "demo", "matblock.cwasm")
WRAPPER = os.path.join(HERE, "matblock_faaslet.py")
RESULT = os.path.join(ROOT, "TestOutput", "matrix_faasm_result.txt")


def grid(workers):
    """Balanced r×c factorization (r ≤ c, r·c == workers) — matches gen_matrix_ap_dag."""
    r, k = 1, 1
    while k * k <= workers:
        if workers % k == 0:
            r = k
        k += 1
    return r, workers // r


def main():
    env, nodes, root = fl.load_env()
    ap = argparse.ArgumentParser()
    ap.add_argument("--matrix-n", type=int, default=512, help="N×N dimension (needs A_<N>.bin/B_<N>.bin)")
    ap.add_argument("--workers", type=int, default=4, help="block count r·c (1,2,4,8,16)")
    ap.add_argument("--nodes", type=int, default=4, help="spread block Faaslets over the first K nodes")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--expect", type=int, default=None, help="gate checksum against this value")
    ap.add_argument("--csv", default=os.path.join(HERE, "results_matrix.csv"))
    args = ap.parse_args()

    nodes = fl.map_nodes(nodes, args.nodes)   # compute faaslets on WORKER nodes only (node-0 = coordinator)
    N, W = args.matrix_n, args.workers
    R, C = grid(W)
    if N % R or N % C:
        sys.exit(f"[matrix] N={N} not divisible by grid {R}x{C}")
    br, bc = N // R, N // C
    port = int(env.get("AGENT_PORT", "9600"))
    rh, rp = env["REDIS_HOST"], int(env.get("REDIS_PORT", "6379"))
    fenv = {"REDIS_HOST": rh, "REDIS_PORT": str(rp), "MATBLOCK_CWASM": MATBLOCK_CWASM,
            "WASMTIME": env.get("WASMTIME", "wasmtime")}

    a_path = os.path.join(root, "TestData", "matrix", f"A_{N}.bin")
    b_path = os.path.join(root, "TestData", "matrix", f"B_{N}.bin")
    for f in (a_path, b_path, MATBLOCK_CWASM):
        if not os.path.exists(f):
            sys.exit(f"[matrix] missing on node 0: {f}")

    rds = redis.Redis(host=rh, port=rp)
    try:
        rds.ping()
    except Exception as ex:
        sys.exit(f"[matrix] Redis unreachable at {rh}:{rp} ({ex})")
    fl.health_check(nodes, port)

    A = np.fromfile(a_path).reshape(N, N)
    B = np.fromfile(b_path).reshape(N, N)
    cells = [(i, j) for i in range(R) for j in range(C)]

    new = not os.path.exists(args.csv)
    fcsv = open(args.csv, "a")
    if new:
        fcsv.write("matrix_n,workers,nodes_used,makespan_mean_ms,makespan_std_ms,total_job_mean_ms,"
                   "gflops,checksum,expect,success,reps\n")

    print(f"[matrix] N={N} grid={R}x{C} workers={W} nodes={len(nodes)} reps={args.reps}")
    ms_list, job_list, cks_seen, ok_all = [], [], None, True
    for rep in range(1, args.reps + 1):
        uid = uuid.uuid4().hex
        rds.flushdb()                                       # cold Redis — no warm state across reps
        t0 = time.time()                                    # end-to-end clock: tile/upload → blocks → assemble
        for i in range(R):                                  # tile A rows + B cols → KV (timed window)
            rds.set(f"{uid}_a_{i}", np.ascontiguousarray(A[i * br:(i + 1) * br, :]).tobytes())
        for j in range(C):
            rds.set(f"{uid}_b_{j}", np.ascontiguousarray(B[:, j * bc:(j + 1) * bc]).tobytes())

        specs = [{"node": nodes[(i * C + j) % len(nodes)], "tag": f"block_{i}_{j}",
                  "cmd": ["python3", WRAPPER, "block", uid, str(i), str(j), str(br), str(bc), str(N)],
                  "env": fenv} for (i, j) in cells]
        _, ok, per = fl.run_faaslets(specs, port, t0=t0)

        checksum = 0                                        # assemble: fold Σ of all C entries
        for (i, j) in cells:
            cbuf = rds.get(f"{uid}_c_{i}_{j}")
            if cbuf is None:
                ok = False
                continue
            checksum += int(np.frombuffer(cbuf, dtype=np.float64).sum())
        makespan = int((time.time() - t0) * 1000)           # clock stops after assemble
        job_ms = sum(per.values())

        os.makedirs(os.path.dirname(RESULT), exist_ok=True)
        with open(RESULT, "w") as f:
            f.write(f"=== matrix ===\nmatrix_n={N}\nworkers={W}\nnodes_used={len(nodes)}\n"
                    f"checksum={checksum}\n")

        gate = args.expect if args.expect is not None else (cks_seen if cks_seen is not None else checksum)
        cks_seen = checksum
        success = ok and checksum == gate
        ok_all = ok_all and success
        ms_list.append(makespan)
        job_list.append(job_ms)
        print(f"[matrix] rep {rep}: makespan={makespan}ms total_job={job_ms}ms "
              f"checksum={checksum} gate={gate} ok={success}")

    mean_ms, std_ms = fl.mean_std(ms_list)
    mean_ms, std_ms = int(mean_ms), round(std_ms, 1)
    job_mean = int(sum(job_list) / len(job_list))
    gate = args.expect if args.expect is not None else cks_seen
    gflops = round(2.0 * N ** 3 / (mean_ms / 1000.0) / 1e9, 3) if mean_ms > 0 else 0
    fcsv.write(f"{N},{W},{len(nodes)},{mean_ms},{std_ms},{job_mean},{gflops},{cks_seen},{gate},{ok_all},{args.reps}\n")
    fcsv.close()
    print(f"[matrix] mean makespan={mean_ms}±{std_ms}ms total_job={job_mean}ms gflops={gflops} "
          f"checksum={cks_seen} success={ok_all} → {args.csv}")
    print(f"RESULT checksum={cks_seen} makespan_ms={mean_ms} std={std_ms} gflops={gflops} success={ok_all}")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
