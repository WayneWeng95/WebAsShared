================================================================================
OPEN ITEMS  (updated 2026-06-05)
================================================================================

Resolved items have been summarized in README.md ("Recent Improvements") and
removed from this file. One-line trace below; details are in git history.

--- RESOLVED (see README.md "Recent Improvements") ------------------------------
[x] (1) Cross-node aggregate dropped a node's contribution — receiver freed a
        stream RemoteRecv slot before its Aggregate consumer ran. Cluster-verified.
[x] (2) Data-parallel input sharding — each node loads a line-aligned fractional
        slice (load_slice); N nodes give the exact 1x result. Cluster-verified.
[x] (3) Per-wave compute timing ([DAG][timing]); staging timed/excluded in worker.
[x] (3b) RSS metric fixed — now sums the executor process TREE's private RSS
        (host + all wasm-call workers), SHM counted once via shm_bump_offset.
[x] (6) Capacity-weighted fan-out — `fanout: N` = N total workers across the
        cluster, apportioned by per-host capacity (Hamilton). Composes with (2).
[x] (7) Fan-out core-budget cap — clamp N to Σ max(1, cores−reserve) (reserve=2);
        e.g. fanout:50 on 2×16 cores → 28. Works with or without SCX.
[x] Streaming check + fixes — engine verified correct; added Output `split_records`
        (record i → paths[i]) fixing img_pipeline_demo; fixed dag_demo FileDispatch
        wasm path. (StreamPipeline/PyPipeline pipelining itself was already sound.)
        See Tests/Fan_out_remote/ for the fan-out sweep harness + results.

--- TODO ------------------------------------------------------------------------

[ ] Bring comparable frameworks onto the table (benchmark baselines).
    Tracked as a SEPARATE benchmark case. The fan-out sweep harness in
    Tests/Fan_out_remote/ is a starting point for the measurement side.

[ ] Test streaming-processing (StreamPipeline / PyPipeline) workloads.
    So far only smoke-checked on the single-node demos (img_pipeline_demo,
    dag_demo). Need a proper test pass: correctness of the software-pipelined
    rounds (per-round outputs match a non-pipelined baseline), the multi-stage
    slot lifecycle across ticks, reset/multi-run behaviour, and the per-round
    output paths (Output split_records). Cover Rust and Python (PyPipeline).

[ ] Test CROSS-NODE streaming processing.
    StreamPipeline across machines (per-round rdma_recv/rdma_send between stages
    on different nodes) has not been exercised end-to-end on the cluster. Need a
    2-node streaming DAG: verify rounds advance in lockstep across nodes, the
    cross-node hand-off per tick is correct (no dropped/duplicated rounds —
    cf. the (1) merge-drop class of bug), and the final result matches a
    single-node streaming baseline. Build on Tests/Fan_out_remote/ harness style.


after the worker dropped, wait for 30s,if job submitted, excluding it from calculated the job partitioning. after 30s not responses make it out from the cluster for scheduling decision. (Do it later, not important now)