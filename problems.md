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
[x] Streaming check + fixes — added Output `split_records` (record i → paths[i]);
        fixed dag_demo FileDispatch wasm path.
        See Tests/Fan_out_remote/ for the fan-out sweep harness + results.

[x] Single-node streaming test pass (StreamPipeline / PyPipeline) — Tests/Streaming/.
        FOUND + FIXED a per-round race: in the pipelined wave schedule, stage S
        (producing round R) and stage S+1 (consuming round R-1) run concurrently
        and share a fixed stream slot; consumers read "all since cursor" and so
        raced into the next round's concurrently-appended records — per-round
        batching was non-deterministic (only the cumulative total survived; the
        img pipeline was safe only because its stages read ONE record/tick).
        Fix: the host publishes a per-slot read watermark `stream_hi_{slot}` (the
        pre-tick committed count) before each tick's scatter; consumers bound
        their read to it (`pipe_read_window`). Cursors are keyed by input slot so
        they reset cleanly between runs. Same fix on the Python side (shm.py
        named atomics + pyp_* workload). Harness asserts per-round correctness vs
        analytic AND non-pipelined (WasmGrouping/PyGrouping) baselines,
        determinism, slot lifecycle, reset/multi-run, Output split_records, and
        Rust/Python parity (34 checks). Also un-broke img_pipeline_demo.json (it
        used a `Pipeline` node kind that no longer exists → now StreamPipeline;
        pipelined output verified byte-identical to the serial baseline).
        NOTE: the img workload yields a different result for 1 image vs 3 images
        (reproduces in serial too — a multi-image workload quirk, NOT a
        pipelining bug); left as-is, out of streaming-correctness scope.

--- TODO ------------------------------------------------------------------------

[ ] Bring comparable frameworks onto the table (benchmark baselines).
    Tracked as a SEPARATE benchmark case. The fan-out sweep harness in
    Tests/Fan_out_remote/ is a starting point for the measurement side.

[~] CROSS-NODE streaming processing — IN PROGRESS (loopback-verified single-path;
    multi-path fix landing).

    Done so far (2-node loopback, real mlx4_0 device):
    [x] Round-handling bug FIXED. A watermark consumer (pipe_read_window) reading
        an rdma_recv slot produced only round 0 then empties: rdma_recv frees +
        refills the recv slot each round and reset the read_next cursor
        (stream_cursor_{slot}) but NOT the watermark cursor (pipe_cursor_{slot}),
        so after round 0 the cursor stayed at 10 while the slot count reset to 10
        → every later round read an empty window. Fix: the host also resets
        pipe_cursor_{slot} in the rdma_recv pre-fetch/free path (both
        execute_stream_pipeline and execute_py_pipeline). cursor_idx map added.
    [x] Single-path cross-node streaming VERIFIED. Minimal deterministic DAG
        (node0 source+filter --rdma_send--> node1 rdma_recv+sink) gives the exact
        analytic summaries (90,10090,...,40090), 5/5, deterministic over 5 runs.
        Image pipeline alone across 2 nodes: 3/3 images, byte-identical to the
        single-node baseline, deterministic.

    Root-caused but NOT a streaming bug — multi-path RDMA control-channel
    collision (this is what made img_pipeline_auto_placement drop round 0):
        When two transfers run CONCURRENTLY between the same node pair — e.g.
        Path A (StreamPipeline rdma_send, slot 40) and Path B (RemoteSend, slot
        520) — they share the single per-peer control TCP stream. The SI
        handshake carries only total_bytes (no transfer identity) and receivers
        are matched by ARRIVAL ORDER, so they cross over: the 372-byte Path-B
        payload landed in the pipeline's slot 40 and the 65 KB image landed in
        Path-B's slot 600. Proven from the byte sizes in the logs.
    [x] FIX IMPLEMENTED + VERIFIED — a dedicated per-peer *streaming* control
        channel (conn-3 / conn-4 on BASE_PORT3 / BASE_PORT4) used only by
        pipeline rdma_send / rdma_recv. RemoteSend/Recv, atomics, StateSync keep
        conn-1 / conn-2 unchanged → no protocol change for non-streaming paths.
        No serialization: the two paths run on separate TCP streams concurrently.
        Chosen over (a) serializing whole transfers and (b) tagged
        multiplexing/demux, because it leaves the non-streaming wire path
        untouched.
        Files: connect/src/mesh/{mod.rs (PeerLink + BASE_PORT3/4 + send/recv_
        channel_stream), connect.rs (conn-3/4 setup)}; host pipeline.rs rdma_send/
        recv now use *_channel_stream.
        Verified on 2-node loopback (mlx4_0): auto_placement now delivers 3/3
        images, all correct (gradient=c2d6a0e7, checker/rings=29c99210),
        deterministic over 3 runs. Minimal deterministic stream (5/5) and
        image-only (3/3) still pass after moving streaming to its own lane.

    [x] Non-streaming RDMA unchanged — VERIFIED. (a) Clean 2-node RemoteSend/Recv
        DAG (total=2): correct + deterministic. (b) In auto_placement, Path B
        (RemoteSend 520 → RemoteRecv 600 → agg) now counts 18 (was a corrupt 7
        before the fix when the image crossed into slot 600). So conn-1/conn-2
        carry non-streaming transfers exactly as before.
        NOTE: demo_auto_placement.sh times out, but that is PRE-EXISTING and
        unrelated: word_count_auto.json declares total_nodes:3 (partitions to
        hosts 0,1,2) while the script's run_pair launches only nodes 0 and 1, so
        the mesh blocks waiting for the never-started node 2. Not a regression
        from the conn-3/4 change (which only adds connections between the nodes
        that DO run).

    Still TODO:
    [ ] Cross-node streaming TEST HARNESS (Tests/Streaming, 2-node loopback):
        lockstep rounds, per-tick hand-off correctness (no dropped/duplicated
        rounds), result matches single-node baseline. Rust + Python.
    [ ] Real-cluster (not loopback) end-to-end verification.


after the worker dropped, wait for 30s,if job submitted, excluding it from calculated the job partitioning. after 30s not responses make it out from the cluster for scheduling decision. (Do it later, not important now)

Additional:

Steaming workload fan-out scenarios. and how to deal with the locality awareness (maybe adding a hint mechanism so the user could give hints on which part may transfer less data and its better to consider break the topology here for multi-node deployment) for dealing with steam pipelines. 