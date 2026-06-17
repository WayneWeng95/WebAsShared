================================================================================
OPEN ITEMS  (updated 2026-06-11)
================================================================================

Resolved items are one-line traces below; full details in git history + README.md
("Recent Improvements"). Open work is in the TODO section at the bottom.

--- RESOLVED (earlier; see README.md) -------------------------------------------
[x] (1) Cross-node aggregate dropped a contribution — recv slot freed before its
        Aggregate consumer ran. Cluster-verified.
[x] (2) Data-parallel input sharding (load_slice) — N nodes give the exact 1x. Cluster.
[x] (3) Per-wave compute timing; (3b) RSS = executor process-tree private RSS.
[x] (6) Capacity-weighted fan-out (fanout:N across cluster); (7) core-budget cap.
[x] Output split_records (record i → paths[i]); dag_demo FileDispatch wasm path fix.
[x] Single-node streaming pass (Tests/Streaming/, 34 checks) — fixed a per-round race:
        host publishes per-slot watermark stream_hi_{slot}; consumers bound reads via
        pipe_read_window. Same fix Python side. img_pipeline_demo un-broken.

--- RESOLVED 2026-06-10/11 (cross-node streaming + fan-out) ----------------------
[x] conn-3/4 streaming control lane — dedicated per-peer TCP streams (BASE_PORT3/4) for
        pipeline rdma_send/recv, so concurrent transfers to the same peer stop crossing
        over (multi-path control-channel collision). RemoteSend/Recv/atomics keep conn-1/2.
        + round-handling fix (reset pipe_cursor_{slot} in the rdma_recv free path).
        Files: connect/src/mesh/{mod,connect}.rs, host pipeline.rs. Loopback + 2-node real
        cluster: 3/3 images byte-identical.
[x] RDMA OUTPUT-RETURN Mode 1 (batch) — Output on node 0 with a cross-machine dep →
        partitioner auto-injects RemoteSend/Recv (splitter.rs cross_edge_slot_kind + wave-0
        guard). DAG: img_pipeline_return.json. Cluster 3/3. 2026-06-10.
[x] (1) Mode 2 — STREAMING per-round output RETURN — new StreamOutput node kind (types.rs /
        pipeline.rs::execute_stream_output): per round rdma_recv the result over conn-4 +
        binary-write to paths[round] (PersistenceWriter::watch_slot_binary). Threaded sink
        (mod.rs 3b/3c) so node 0 is source (conn-3) + sink (conn-4) concurrently. DAGs:
        Tests/Streaming_CrossNode/node{0,1}_mode2.json + verify_mode2.py. Cluster 3/3.
[x] LOCALITY HINT — placer dep-affinity is now DATA-WEIGHTED (placer.rs edge_weight).
        Per-node split:"avoid"(keep local) / "prefer"(safe to cut) / out_weight (default
        1.0); cross-machine cuts fall on the cheapest edges. 4 unit tests +
        Tests/Cluster_Eval/check_locality.py (cut follows the hint, flips when reversed).
        NOTE: only acts when ≥2 hosts have quota (Pack/balanced force a single auto node).
[x] STREAMING FAN-OUT — per-stage WIDTH (Phase 1) + load signals (3) + dynamic autoscaling
        (4). `width`/`max_width` on StreamPipelineStage; a widened stage host-scatters the
        per-tick batch into private sub-slots (stage_fanout.rs) and gathers in order (slots
        are single-owner). width==1 = exact original path. max_width → per-tick active width
        from EMA load (desired_width, ±1/tick hysteresis). Tests/Streaming/width_test.py
        (filter×3 == baseline; autoscale 1→peak 4). Phase 1 cluster-verified via the combo
        DAG; signals/autoscaling local. 2026-06-11.
[x] PHASE 2 — cross-host streaming AUTO-SPLIT + N-WAY / SELF-OPTIMIZING / HINT-AWARE.
        partitioner split_stream_pipelines (splitter.rs): `segments:M` cuts a single
        StreamPipeline into M cross-node segments at the M-1 CHEAPEST stage boundaries
        (choose_cuts: split:"avoid"→∞, "prefer"→0, else out_weight); `cut_after` (int or
        list) = explicit override. Segments chained by embedded rdma + Mode-2 return; placed
        by capacity hint (machine_order); `segments` clamps to online node count. Terminal
        Output → StreamOutput sink. Partitioner-only (executor + types.rs untouched). 22
        partitioner tests; CLUSTER-VERIFIED segments:3 across 3 online nodes (0,1,3 —
        coordinator maps logical→physical) 3/3 == baseline. DAGs: img_pipeline_split.json
        (explicit), img_pipeline_split_auto.json (segments + hints).
        + COORDINATOR FIX (cluster_dag.rs has_remote_nodes): detect EMBEDDED rdma_send/recv
        (not just RemoteSend/Recv nodes) so the per-node rdma block is attached for streaming
        DAGs submitted via node-agent. 2026-06-11.
[x] Combined cross-node test (Tests/Streaming_CrossNode/node{0,1}_combo.json + verify_combo.py):
        streaming input + cross-node RDMA + per-stage width + Mode-2 return in one run.
        Cluster 3/3 == baseline.
[x] RDMA input staging — coordinator RDMA-stages shared_inputs to workers
        (stage_shared_inputs_rdma, TCP fallback). word-count auto-placement runs distributed
        (Tests/Cluster_Eval/). NOTE: bare `host dag` does NOT stage inputs; node-agent does.
[x] split_records latent bug — added "split_records": true to rdma_img_pipeline_node1.json
        and rdma_py_img_pipeline_node1.json (were writing all 3 images into paths[0]).
        img_pipeline_auto_placement already had it; pipeline_routing has no multi-path Output.

================================================================================
TODO  (open)
================================================================================

All feature + correctness work is done and cluster-verified. Cluster e2e of the built
features is covered (locality hint via the auto-split-with-hints run; word-count
loopback==baseline + cluster success; autoscaling is a per-node loop with nothing
cross-node-specific to test). Only deferred work + the benchmark track remain:

[ ] [FUTURE] Peer-failure robustness — a RemoteRecv whose peer dies segfaults (seen when a
    node lacked its local input under bare `host dag`). Pre-existing, not hardened.

[ ] [FUTURE] Worker-drop scheduling grace period — after a worker drops, wait 30s then
    exclude it from job partitioning; remove from the cluster after no response. Deferred.

[ ] Benchmark baselines — SEPARATE TRACK, ongoing. Bring comparable frameworks onto the
    table; Tests/Fan_out_remote/ is the measurement starting point.

[ ] [PERF] Memory cost on WordCount + Matrix — investigate why our peak memory (incl. KV)
    is uncompetitive on these two workloads while we lead on Finra/ML_training/ML_inference.
    At the largest load (WordCount 1001MB/16w, Matrix 2048/16w) WasMem-AOT is NOT the lowest:
    WordCount AOT 4005 MB vs Faasm 2022; Matrix AOT 1088 MB vs RMMap 430 / Faasm 404. Our
    footprint also grows with worker count (per-worker SHM/RSS) where baselines stay flat.
    Data + chart: Tests/Inter-Node Application_Benchmark/analysis/ (mem_footprint*.md/csv,
    figs/mem_largest_load.pdf).

[x] [FRAMEWORK] SHM dynamic-growth past INITIAL_SHM_SIZE (64 MiB) FIXED (2026-06-16). Root
    cause was NOT the grow/remap handshake but a STALE MAPPING in the main DAG-runner process:
    host-side nodes (Aggregate, Output, Input) and post-wave reclamation walk SHM page chains
    IN the main process, which maps INITIAL_SHM_SIZE at startup and only re-synced its mapping
    in chunked mode (dag_runner mod.rs:442/867). In a one-shot/unrolled DAG a worker subprocess
    grows the file via host_remap, but the main process's mapping stays at 64 MiB — so a later
    wave's Aggregate/Output reads land in the zero-filled wasm-reserved region past the stale
    VMA → gradients/model silently read as zeros (validate emits nothing). Happens at W=1 too
    (cross-wave staleness, not a concurrent race). Fix: shm::sync_mapping_if_grown — a guarded
    re-sync (one atomic compare; mmap only when global_capacity outgrew our VMA, tracked in
    MAPPED_CAP) called once per wave after subprocess join, before reclamation (the one-shot
    analogue of the chunked-loop sync). i8 feature workaround in ML_training reverted to i32.
    A/B verified: fix OFF → 600k W=1 grows to 128 MiB, "success", but 0-byte validate output;
    fix ON → weight_checksum=852 @97.97%, fan-out-invariant across W=1..16; ML_inference 600k
    likewise invariant (checksum=1861022). Build: ./build.sh.
    Official ML_training results regenerated with i32 (full sweep, E=10, 3 reps): checksums
    831/841/828 and accuracy UNCHANGED (i32≡i8 numerically — features promote to i64), so the
    i8 packing only ever shrank the footprint. peak_mem rose where i32 crosses 64 MiB (AOT 600k
    +28–33%; lowest-peak-mem claim still holds, 331 vs 356–435 MB) and latency ~+5–11% at the
    larger sizes (4× bigger binary shard to scan); cross-system wins intact (latency margin now
    ~1.5×→2.6×). README + figs (plot.py) updated. i8 was the odd one out: faasm uses i32,
    rmmap/cloudburst int64 — none used i8, so i32 is the fair representation. ML_inference never
    packed features (text→i64), so nothing to revert there; it only needed the host-side fix.



    Also adding a single process local node testing for the base performance comparison. 


One more thing: In the partitioner, we can adding an auto snapshot-resume point part which can automatically trigger our key point snapshot-resume feature to asynchronize offload state checkpoints. 