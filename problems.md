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

[x] [PERF] Memory cost on WordCount + Matrix — was a MEASUREMENT BUG in the sweep sampler,
    NOT a real framework cost (2026-06-17). The out-of-band peak sampler in WordCount/run.sh
    and Matrix/run.sh summed raw `VmRSS` across the host process tree AND added the SHM file
    size (`stat -c%s`) on top. But VmRSS includes RssShmem, so the shared SHM segment was
    counted once per worker that had it resident PLUS once more as the file add — heavy
    double-counting, concentrated exactly where SHM is large and fan-out is wide (WordCount,
    Matrix). It also explained the "grows with worker count where baselines stay flat" symptom.
    Our own ground truth (NodeAgent/agent/src/metrics.rs) already defines footprint correctly:
    private RSS = `VmRSS − RssShmem` per process, plus the SHM bump offset (superblock LE u32
    at byte 4) counted ONCE. Fix: made both samplers match metrics.rs (subtract RssShmem; read
    the bump offset via `od -An -tu4 -j4 -N4` instead of file size). Re-ran all four sweeps
    (checksums/occurrences fan-out-invariant — correctness unaffected); old CSVs kept as
    *_old_20260617.csv. Results now competitive: WordCount AOT 1001MB/16w 4005→2476 MB
    (Faasm 2313), 500MB/16w 2358→1316 (Faasm 1312, ~tied); Matrix AOT 2048/16w 1088→669
    (RMMap 430 / Faasm 404 — closer, no longer an outlier), 512/16w now lowest of all systems.
    Regenerated mem_footprint*.md/csv + all figs (WordCount/Matrix plot.py, analysis/
    plot_bars_grid.py + plot_mem_largest.py). Baseline CSVs untouched (bug was ours only).

[x] [BASELINE] Faasm peak memory was FLAT in fan-out — measurement bug in the demo drivers
    (2026-06-17). WordCount/Matrix/Finra faasm/demo/driver.py recorded `max` of the per-Faaslet
    RSS (each `wasmtime run` timed by /usr/bin/time -v), but the map workers run CONCURRENTLY
    (ThreadPoolExecutor), so N Faaslets are co-resident — the footprint is their SUM, not the
    max of one. Symptom: WordCount faasm peak ~19 MB flat across every fan-out AND size; Matrix
    even DECREASED with fan-out (bigger grid → smaller blocks → smaller single Faaslet). Fix:
    sum the co-resident Faaslet RSS for the concurrent phase (reduce stays a separate max).
    Removed the now-redundant post-hoc `N×` Faasm multiplier in analysis/mem_footprint.py (the
    driver measures concurrency directly now); collapsed the two output variants into one
    (mem_footprint.{md,csv}); repointed plot_mem_largest.py to it; deleted mem_footprint_raw.*.
    Re-ran the 3 faasm sweeps (redis needs --proto-max-bulk-len ≥3gb for the WordCount 1001MB/1w
    single SET; old CSVs kept as *_old_20260617.csv). New faasm grows with fan-out as expected
    and lands near the old N×max estimate (per-Faaslet RSS ~uniform), so our AOT lead holds/
    improves: WordCount 500/16w faasm 1450 vs AOT 1316 (we now win); Matrix 2048/16w faasm 1183
    vs AOT 669. Regenerated footprint tables + figs (WordCount/Matrix/Finra plot.py + analysis).

[x] [BASELINE] RMMap peak memory was FLAT in fan-out — same class of bug, plus the metric was
    unified across per-process baselines (2026-06-17). All RMMap drivers run workers as separate
    processes (multiprocessing.Pool, ES/Redis pickle path — no shared mapping), yet measured:
    Matrix `max` of one worker; ML_training/ML_inference parent RUSAGE_SELF only (workers
    UNCOUNTED); Finra parent + RUSAGE_CHILDREN.ru_maxrss (largest SINGLE child of 8). All
    undercounted the concurrent procs. Fix: each worker reports private (Private_Clean+Dirty)
    and shared (Shared_*) RSS from /proc/self/smaps_rollup at its high-water; footprint =
    Σ private + shared-once (max). DECISION (user): use this private-RSS-+-shared-once metric —
    the same one WasMem uses — and apply it to Faasm too (re-measured the 3 faasm drivers the
    same way: sample the wasmtime child's smaps_rollup via Popen+poll instead of /usr/bin/time
    full RSS; supersedes the "sum full RSS" of the entry above). WordCount RMMap LEFT AS-IS
    (mappers run sequentially by default → flat is correct). Also fixed a stale run.sh path in
    ML_training/ML_inference rmmap (Tests/Application_Benchmark → Tests/Inter-Node
    Application_Benchmark) so missing ML test data regenerates; ran Finra rmmap NATIVELY (its
    run.sh wraps the driver in a dmerge-redis docker container not set up here — the ES driver
    is pure python+redis). Old CSVs kept as *_old_20260617.csv. RMMap now grows with fan-out:
    Matrix 512 62→218 / 2048 390→1140; Finra 244/624/4158 (8-pod broadcast); ML_inference
    73→187 (3.2MB). Regenerated mem_footprint.{md,csv} + every fig. Caveat: summing full
    per-process RSS would double-count each pod's shared runtime; private+shared-once avoids
    that. NOTE residual inconsistency — our own WasMem sampler sums VmRSS−RssShmem (counts
    shared libs per worker), so baselines now get slightly MORE generous shared-dedup than we
    give ourselves; acceptable (conservative toward competitors).

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