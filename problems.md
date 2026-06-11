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

PRIORITY ORDER (updated 2026-06-11):
  1. Streaming fan-out + locality awareness (hint mechanism). (active next)
  2. Cross-node streaming test harness (deterministic analytic lockstep + Python).
  3. [FUTURE WORK] Worker-drop scheduling grace period.
  Separate track, ongoing (outside this order): benchmark baselines.
  DONE: Mode 2 — streaming / per-round RDMA output return (cluster-verified 2026-06-11).

[x] RDMA OUTPUT-RETURN, Mode 1 (batch) — DONE + cluster-verified 2026-06-10.
    A worker's output is returned to the coordinator (node 0) over RDMA just by
    placing the Output node on node 0 with a cross-machine dep on the worker's
    producer; the Partitioner auto-injects the RemoteSend/RemoteRecv pair. Two
    splitter.rs fixes: (a) cross_edge_slot_kind() picks slot_kind Io when the source
    is the terminal OUTPUT_IO_SLOT(1) (io_heads, length-prefixed records → Output
    split_records), Stream otherwise; (b) the §5d wave-0 guard now also treats a
    StreamPipeline's embedded rdma_send as a "send to peer P", so the return
    RemoteRecv is sequenced AFTER the local source pipeline (else it sits in wave 0
    and deadlocks — RDMA threads join at end of their own wave, mod.rs:652).
    Also: Partitioner StreamPipeline output-slot registration (slot_assigner.rs) so
    these DAGs partition at all. DAG: DAGs/symbolic_dag/img_pipeline_return.json.
    Verified 3/3 images byte-identical on node 0 (gradient=683910b9,
    checker/rings=c36c7705). NOTE: partitioner placement is RNG-nondeterministic;
    regression-check structure, not bytes.

[x] (1) Mode 2 — STREAMING / per-round output return — DONE + cluster-verified 2026-06-11.
    Worker's final StreamPipeline relays each round's output via embedded rdma_send
    back to node 0; node 0 runs a sink that writes each round as it arrives, ONE FILE
    PER ROUND (paths[round]).
    DESIGN (resolved the open Q toward one-file-per-round + a new node kind, NOT
    extending Output, to keep the race-sensitive execute_stream_pipeline untouched):
      - New `StreamOutput` node kind (types.rs / pipeline.rs::execute_stream_output):
        per round, optional rdma_recv the worker's result over the streaming lane
        (recv_channel_stream / conn-4) then binary-write that round's record to
        paths[round] via the background PersistenceWriter (new watch_slot_binary —
        raw bytes, binary-safe for images; the text watch_stream path is unchanged).
        Covers both coordinator-side remote RETURN (rdma_recv set) and a local
        per-round write (rdma_recv absent).
      - INPUT also streams one-by-one (user choice): node 0's source StreamPipeline
        rdma_sends one decoded image per round on conn-3.
      - node 0 is BOTH source AND sink simultaneously. Enabled WITHOUT the general
        "run StreamPipelines as threads" change: StreamOutput is classified into the
        threaded host group (mod.rs 3b/3c), so the sink receives on conn-4 (its own
        thread) while the source StreamPipeline sends on conn-3 (main thread). Safe
        because the streaming lane is direction-split (ctrl_as_sender_stream /
        ctrl_as_receiver_stream) — no control-channel collision.
      - Worker returns each round via the StreamPipeline's existing embedded rdma_send
        (free_after); a single node-1 StreamPipeline does both rdma_recv (input) and
        rdma_send (output) per round.
    DAGs: Tests/Streaming_CrossNode/node0_mode2.json + node1_mode2.json; verify on
    node 0 with verify_mode2.py (TestOutput/mode2_return_out vs baseline/). 3/3 images
    byte-identical to the single-node baseline, deterministic. Single-node streaming
    regression (Tests/Streaming) 34/34 still pass.

[~] (2) STREAMING FAN-OUT + LOCALITY AWARENESS — locality hint DONE, streaming scenarios next.
    Streaming-workload fan-out scenarios + a user hint mechanism: let the user hint
    which edges transfer less data so the partitioner breaks the topology there for
    multi-node deployment.
    [x] LOCALITY HINT + CUT HEURISTIC (locally tested 2026-06-11). The placer's
        dep-affinity is now DATA-WEIGHTED (placer.rs assign_nodes + edge_weight): a
        consumer is pulled toward the host of its strongest producer, so when the
        cluster must split, cross-machine cuts fall on the safest/lightest edges.
        User-facing hint = per-node breakability intent (each node controls its OWN
        output edge):
          - `split: "avoid"`  → don't break here; keep the consumer local (maps to a
            large finite weight — strong preference, yields only to capacity limits).
          - `split: "prefer"` → safe place to break; cuts gravitate here (weight 0).
          - omitted → neutral.
        e.g. chain A→B→C with `B: "prefer"` cuts B→C; `A: "avoid"` keeps A→B local.
        Advanced numeric override `out_weight: Option<f64>` (used only when `split`
        absent; default 1.0). Backward-compat by construction — neutral nodes resolve
        to 1.0 = original count-based affinity (same last-maximal tie-break). 4 unit
        tests (split_avoid_keeps_consumer_local, split_prefer_becomes_the_cut_point,
        out_weight_pulls_consumer_to_heaviest_producer, equal_weights_reproduce_count_
        affinity); all 20 partitioner tests pass, full build.sh green. Design: per-NODE
        hint (fits the bare-id `deps` schema), greedy weighted-affinity (not full
        min-cut), strong-preference (not hard guarantee — pin node_id for that).
    [~] STREAMING FAN-OUT (unified per-stage width + dynamic autoscaling). User wants
        a unified mechanism: scale a hot stage (more workers in a stage) AND replicate
        full pipelines (all stages wide), driven by dynamic autoscaling on load.
        Unifying primitive = per-stage WIDTH. Phased; the static width mechanism is the
        actuator the autoscaler will drive. Plan: ~/.claude/plans/encapsulated-splashing-fairy.md.
        [x] PHASE 1 — static per-stage width, single host (locally tested 2026-06-11).
            `width`/`max_width` added to StreamPipelineStage (types.rs). A widened stage
            runs W persistent workers; each tick the host SCATTERS the per-tick input
            batch round-robin into W private sub-slots (reserved band 1792.., dag_runner/
            stage_fanout.rs), runs the workers, and GATHERS their outputs back into the
            stage's slot in record order. Needed because slots are single-owner
            (read_next load-then-fetch_add + chain append are not concurrency-safe).
            `width==1` keeps the EXACT original path → existing DAGs byte-for-byte
            unchanged (Tests/Streaming 34/34 still pass). New Tests/Streaming/width_test.py:
            word-count filter×3+transform×2 == width:1 == analytic + deterministic
            (correct sum proves all records were distributed across workers), image
            widened-stage byte-identical. build.sh green.
        [ ] PHASE 2 — cross-host width via partitioner (expand width → placed workers,
            use split hint for cut points, RDMA scatter/gather + Mode-2 return). cluster.
        [ ] PHASE 3 — load signals: per-stage backlog (count_stream_records) + per-tick
            stage time into metrics / [DAG][timing]. locally testable.
        [ ] PHASE 4 — dynamic autoscaling: in-executor control loop pre-spawns max_width
            and gates active width per tick from Phase-3 backlog (policy + hysteresis).

[ ] Benchmark baselines — SEPARATE TRACK, ongoing. Bring comparable frameworks onto
    the table. Tests/Fan_out_remote/ is the starting point for the measurement side.

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
    [ ] (3) Cross-node streaming TEST HARNESS (Tests/Streaming, 2-node loopback):
        lockstep rounds, per-tick hand-off correctness (no dropped/duplicated
        rounds), result matches single-node baseline. Rust + Python.
        PARTIAL: Tests/Streaming_CrossNode/ has the hand-split node0/node1 image
        DAGs + verify.py (byte-compare vs single-node baseline). Still want the
        deterministic analytic (pipeline_source→…→sink) lockstep version + Python.
    [x] Real-cluster (not loopback) end-to-end verification — DONE 2026-06-10.
        Two physical nodes (node-0 10.10.1.2, node-1 10.10.1.1, mlx4_0). Image
        pipeline split across nodes (node0 decode → RDMA → node1 rotate/gray/
        equalize/blur/export): 3/3 images byte-identical to the single-node
        baseline (gradient=683910b9, checker/rings=c36c7705). conn-3/4 streaming
        lanes + round-handling confirmed on real RDMA, not just loopback.
        GOTCHAS hit (operational, not code): (a) both nodes must be on the SAME
        commit + rebuilt — a node-1 binary predating the conn-3/4 commit (d52167e)
        dials only conn-1/2 and deadlocks node-0's mesh setup; (b) a multi-path
        Output needs `split_records: true` or it writes all N records into paths[0].
        NOTE: the committed img DAGs (rdma_img_pipeline_node1, rdma_py_img_pipeline_
        node1, img_pipeline_auto_placement save_images, pipeline_routing save_images)
        all still MISSING split_records — latent same-bug.


[ ] (4) [FUTURE WORK] Worker-drop scheduling grace period.
    After a worker drops, wait 30s: if a job was submitted, exclude that worker from
    job partitioning; after 30s with no response, remove it from the cluster for
    scheduling decisions. (Deferred — not important now.)