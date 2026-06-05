
--- The testing file size is too small and unable to showcase the fan-out performance benefits, also the measuring need to check

--- bring the comparable frames onto the table


The client pre-partitions (main.rs:222), so it sends a node_dags ClusterDag and the coordinator's resolve_dag uses it as-is. That means all my coordinator-side logic (live-node scaling, SCX clamping, converge) has been dead code — the client partitions blindly with no live-cluster knowledge. Removing total_nodes exposed this: detection broke, and even fixed, the client can't partition without live info.

The fix: client sends the raw SymbolicDag; the coordinator partitions it (which is where live-scaling/SCX/converge live). The partitioner already does Func→WasmVoid for Rust, so no client transform is needed for the symbolic path.


Found the root cause: the client detects SymbolicDag by the literal string "total_nodes" (main.rs:219) — which I just removed from every DAG. So word_count.json now falls through to the "treat as ClusterDag" branch → missing field node_dags.

But there's a deeper issue I need to check: the client partitions client-side (line 222), which means it can't do live-node scaling (the client doesn't know how many nodes are online — only the coordinator does). Let me see how the transform + coordinator path fit together before fixing


================================================================================
STATUS / OPEN ITEMS  (updated 2026-06-04)
================================================================================

--- DONE (fixed & verified) -----------------------------------------------------

[x] Client/coordinator partitioning dead-code: client now ships the raw
    SymbolicDag; coordinator partitions server-side with live SCX hints, then
    transforms (Func→WasmVoid / PyFunc). Protocol carries python opts.
[x] Non-contiguous live nodes: coordinator uses live_node_ids (coordinator +
    ANY connected workers, e.g. {0,2}) and remaps a dense logical 0..N space to
    physical nodes for split / mesh IPs / DAG delivery. Verified on cluster
    (node 0 + node 2, node 1 absent).
[x] Zero-copy wc_distribute: contiguous record-aligned page-chain split
    (relink + ≤1-page seam copy per cut) instead of a full per-line copy. Peak
    SHM ~1x input instead of 2x; the 1 GiB single-node corpus that used to
    crash now runs. Output byte-identical to the old copy path.
[x] MR-src-ext local-protection (WC status=4): collect_src_sges no longer grows
    the ext MR mid-collection (stale lkeys). Grow once to full SHM capacity,
    stamp one lkey into all ext SGEs.  (Built; not yet exercised by a transfer
    large enough to cross MR1 on the cluster.)
[x] Fan-out distribute slot mismatch (counted only 8/50 of data): wc_distribute
    treated arg as the worker count + hardcoded base=10, while the partitioner
    assigned maps at out_base=50. Now the partitioner packs arg = base |
    (n_consumers<<16) in the FINAL Func→WasmVoid step (translate_func_kind, NOT
    slot assignment — packing during assignment polluted collect_slots and blew
    next_slot past STREAM_SLOT_COUNT). Guest unpack_fanout_arg() splits base..
    base+n; legacy (high bits 0) falls back to count + default base. Applied to
    wc_distribute AND ml_partition (same bug). Verified on cluster: per-word
    counts now exactly match the single-node baseline.
    See: Tests/fan_out_test/ for the fan-out sweep harness + findings.


--- TODO (next, in order) -------------------------------------------------------

[x] (1) CROSS-NODE AGGREGATE DROPS A NODE'S CONTRIBUTION  — FIXED & VERIFIED
        (verified on the live 2-node cluster 2026-06-05: 2-node count is now
         EXACTLY 2x the single-node corpus_xlarge baseline, per-word and in
         total_occurrences 89,403,388 -> 178,806,776. Was 1x before the fix.)
    Symptom: 2-node word_count_auto_placement returns 1x (single-node) count,
    not the sum of both nodes. map_records_received=193 (= one node's 8 maps;
    both would be ~384). Per-word counts == single-node exactly.

    ROOT CAUSE (was the OPPOSITE end from the old hypothesis — it's the
    RECEIVER, node 0, not the sender):
    The post-wave "RemoteRecv with no downstream consumers: free immediately"
    branch in mod.rs gated on `remote_recv_dep_counts.contains_key(id)`, but
    that map is built ONLY for Io-kind recvs (stream recvs deliberately
    excluded, lines ~329-349). So for the STREAM recv `rr_aggregate_1_from_1`
    (slot 161) the map never contains it -> the "no consumers" branch always
    fired -> free_stream_slot(161) ran one wave AFTER recv, zeroing
    writer_heads[161] and freeing the pages. Then aggregate_global's
    chain_onto(160, 161) saw writer_heads[161]==0 and silently skipped it
    (chain_splicer.rs:43) -> only slot 158 merged -> 1x. The 2.1s recv block
    was REAL (node 0 did receive node 1's bytes); node 0 then threw them away.
    The old "sender reclaims slot 159 / RemoteSend not counted as reader"
    hypothesis was wrong: slot 159 is the Aggregate downstream, nothing in
    node_owned_slots frees it, so it survives to RemoteSend intact.

    FIX (mod.rs): added `remote_recv_has_consumer: HashSet<String>` over ALL
    recv nodes (Stream + Io) with >=1 dep-consumer, and gated the immediate-free
    branch on `!remote_recv_has_consumer.contains(id)` instead of the Io-only
    map. A stream recv with a consumer is now left for that consumer to reclaim
    (StreamPipeline via build_slot_refcounts, or Aggregate via
    node_routed_upstream_slots -> clear_stream_slot after the merge). This also
    fixes the latent stream-recv -> StreamPipeline case (it would have been
    freed prematurely too).

    VERIFIED 2026-06-05 on the live cluster (coordinator=node 0=receiver had the
    fix; worker=node 1=pure sender, unaffected by the changed branch). Submitted
    DAGs/symbolic_dag/word_count_auto_placement.json over corpus_xlarge (500 MiB).
    Result EXACTLY 2x the single-node baseline (each node processes the full
    corpus under placement:"all"; point (2) sharding then brings this back to 1x).
    Baseline saved at TestOutput/wc_singlenode_xlarge.txt.

[x] (2) DATA-PARALLEL SHARDING — DONE & VERIFIED ON THE CLUSTER
        (commit d61b850. 2-node run of word_count_auto_placement now returns
         EXACTLY 1x the single-node baseline: total_occurrences 89,403,388 and
         all 25 per-word counts match exactly — down from the 2x measured after
         the (1) merge fix, confirming each node processed only its half-slice.
         Runtime also dropped 11.8s -> 7.1s. NOTE: slices are weighted by
         per-machine map count, but the placement:"all" path always replicates
         the fanout uniformly, so real runs today are always an even split; see
         the per-machine-fanout-scaling caveat under (6).)
    placement:"all" makes every node load + count the FULL corpus, so a working
    merge would give Nx (double/triple count), not 1x. We do NOT want Nx; the
    correct distributed result is 1x produced by N nodes each doing a slice.

    IMPLEMENTED:
    - host: SlotLoader::load_slice(path, slot, lo, hi) + prefetch_slice — loads
      only the line-aligned byte window [lo*len, hi*len) using the MapReduce
      split rule (a node owns the lines that BEGIN in its window; start advances
      past the next \n unless lo*len==0, end advances past the next \n unless
      EOF). Adjacent nodes share the boundary (hi_i == lo_{i+1}) so every line
      is read by exactly one node — no splits/gaps/overlaps. New
      InputParams.slice: Option<[f64;2]>, wired into the Input dispatch (prefetch
      AND plain paths). slot_loader.rs / types.rs / dispatch.rs.
    - partitioner: expand_fanout now reports the fanout map-worker ids;
      assign_input_slices() (splitter.rs, after assign_nodes) stamps each
      replicated shared-input `load_{m}` with slice [Σw<m, Σw<=m]/W where
      weight_m = map workers on machine m (falls back to equal weights if no
      fanout). Only placement:"all" inputs (path in shared_inputs) are sliced;
      single-machine and chunked/binary inputs load whole. total_nodes<=1 → no
      slice. The `slice` field survives transform_cluster_dag (Input untouched).
    - Verified on word_count_auto_placement (2 nodes, 8 maps each): emits
      machine 0 -> [0.0,0.5], machine 1 -> [0.5,1.0].
    - LOCAL EXACT VALIDATION (single-node, this machine): running the baseline
      DAG twice with slice [0,0.5] then [0.5,1.0] and summing == the full
      single-node count for EVERY word incl. the boundary edge word `ga`, and
      total_occurrences 89,403,388 exactly. So load_slice + dispatch + the
      partitioner fractions are correct.

    PENDING (user runs the cluster): submit word_count_auto_placement; expect
    EXACTLY 1x the single-node baseline (total_occurrences 89,403,388), down from
    the 2x measured after the (1) merge fix, with each node's log showing it
    loaded only its half ("slice 0.0000..0.5000" / "0.5000..1.0000").
    DEPLOY NOTE: the WORKER node (10.10.1.1) must `git pull` + `./build.sh` to
    get the slice-aware host binary (old host ignores `slice` via serde default
    and would load the full file -> 1.5x), and restart its node-agent daemon to
    reconnect after the coordinator was bounced. Coordinator (node 0) already
    rebuilt + restarted.
    PLAN (agreed with user): keep replicating the file to every node (staging
    unchanged — simple), but each node PROCESSES only its slice via the existing
    host load_chunk(offset,len) (line-aligned). Bonus: only each node's slice
    enters SHM, so per-node SHM ~ corpus/N (relieves the 1.5 GiB window for big
    corpora).
    - Slice size must be WEIGHTED BY THE NUMBER OF EXECUTORS (map workers) placed
      on each node, NOT 50/50: weight_i = maps_on_node_i / total_maps. Partitioner
      knows the per-node map count from placement; emit each node's `load` a
      FRACTIONAL range [Σw<i, Σw<=i) and let the host resolve to byte offsets
      against the actual file size + line alignment at load time.
    - Depends on (1): with slicing, a broken merge would yield a TOO-LOW count
      (only one node's slice), so the merge must be fixed first.

[x] (3) MEASUREMENT / METRICS — per-wave timing added (staging excluded)
    - host dag_runner now times every wave and prints a per-run breakdown:
      "[DAG][timing] wave N: X ms (Y%) K node(s) [ids]" + TOTAL compute ms,
      also emitted to the HostLogger. Staging (cross-node file replication)
      happens in the node-agent worker BEFORE the executor is spawned, so it is
      inherently excluded from this compute total.
    - node-agent worker now logs staging separately: "staged N file(s), M MiB
      in T ms (excluded from compute)".
    - Verified single-node xlarge: wave 2 (10 maps) = 71.6%, distribute wave
      (folds the input prefetch-join) = 27.4%, reduce = 1.0%, TOTAL 6013 ms.
    NOTE: input-load time currently shows up inside the first CONSUMER wave
    (the prefetch is joined when distribute runs), not wave 0 (load). Per-NODE
    timing within a wave isn't broken out because a wave's nodes run
    concurrently (subprocess/threads) — wave granularity bounds the slowest node.
    Multi-node: the breakdown is in each executor's stdout/log (captured); a
    future step could thread it into the JobCompleted summary.

[x] (3b) RSS METRIC MEASURED WRONG PROCESS — FIXED
    Symptom (user): RSS doesn't change during worker/coordinator execution.
    Cause: metrics::sample_rss read /proc/SELF/status — the node-agent DAEMON's
    RSS (~4 MiB, constant) — not the executor subprocess that mmaps the SHM and
    runs the workloads. Coordinator also hardcoded executor_running=false AND its
    main loop blocks in handle.wait() during its local run, so it sampled nothing
    during execution.
    Fix: sample() takes executor_pid; sample_rss reads /proc/<pid>/status (falls
    back to self when idle/exited). Worker passes exec.pid(); `run` passes
    handle.pid(); coordinator's local-executor section replaced the blocking
    wait() with a try_wait() poll loop that samples (pid + shm_path) every
    metrics interval and feeds scx_view. Touches metrics.rs, worker.rs, main.rs,
    coordinator.rs (+ extract_shm_path made pub(crate)).
    REFINED: rss_bytes now sums the PRIVATE resident memory (VmRSS − RssShmem)
    of the WHOLE executor process TREE — host + every fanned-out `wasm-call`
    worker (each carries its own wasmtime runtime + JIT'd guest + guest heap,
    which dominate: measured ~985 MiB private across 10 workers vs ~76 MiB for
    the host alone). SHM is subtracted from every process so it's counted ZERO
    times here and ONCE in shm_bump_offset → total job memory = rss_bytes +
    shm_bump_offset, no double-count. sample_rss walks /proc, builds the ppid
    tree, BFS from executor_pid. (metrics.rs)
    VERIFIED (single-node run, 524 MiB, 10 workers): rss_bytes grows 387→531 MiB
    during the run (was flat ~4 MiB before any fix, 76 MiB with host-only),
    shm_bump 549 MiB, total ~1.0–1.5 GiB. Worker/coordinator use the same code;
    live-cluster confirmation pending node-agent restart (user).

--- FAN-OUT SWEEP EXPERIMENT (2026-06-05) → Tests/Fan_out_remote/ -------------
Evaluated fanout {5,10,15,20} × corpus {52MB,524MB,1GB} on the 2-node cluster
(16 cores/node, new cluster-total fanout + cap=28). Correctness: total_occ
constant across fanouts per corpus (1x). Perf: optimal fanout grows with input
size — 52MB peaks at fanout 10 then regresses (spawn overhead), 524MB plateaus
after 10, 1GB keeps improving through 20 (14.1s→11.6s). Matches the (5) ceiling
intuition. Harness + README + results.csv under Tests/Fan_out_remote/.

[ ] (4) bring comparable frameworks onto the table (benchmark baselines).
    (user: tracked as a SEPARATE benchmark case.)

--- STREAMING WORKLOADS CHECK (2026-06-05) ----------------------------------
The StreamPipeline / PyPipeline streaming MECHANISM works correctly: software-
pipelining across rounds executes in the right tick order and produces correct
results. Verified:
  - img_pipeline_demo: 3 images software-pipelined through load→rotate→grayscale
    →equalize→blur→export across 8 ticks, all stages ran in order.
  - dag_demo stream_pipeline_demo: 5 rounds through source→filter→transform→sink
    over 8 ticks → batch_count=29, value_sum=88270 (correct).
Two PERIPHERAL issues found (NOT the streaming engine):
  (a) img_pipeline_demo Output: a single-run StreamPipeline emits N round-results
      into one slot, but an Output node with multiple `paths` only writes
      paths[run_index % len] = the FIRST file per run → checkerboard/rings come
      out 0 bytes (only gradient written). Output cycles paths per RUN, but the
      pipeline does N ROUNDS in one run. Demo/Output-multipath mismatch.
  (b) dag_demo dispatch_file_equal: a FileDispatch node hard-codes a relative
      wasm path "../target/wasm32-.../guest.wasm" that doesn't resolve from the
      run CWD → mmap fails, aborting the (already-finished) pipeline's run.
      Demo path bug.

[ ] (5) (optional perf) fan-out doesn't scale past ~core count: each map is a
    separate process (fork + wasmtime JIT + SHM mmap), wave spawns all N
    uncapped, workload is memory-bandwidth-bound, and wc_map's read_all OOMs the
    ~0.5 GiB guest heap at low fan-out on large inputs. Fixes: stream wc_map
    (chain_for_each not read_all), reuse one wasmtime engine/threads, cap
    concurrency at ~cores. See Tests/fan_out_test/README.md.

[x] (6) PER-MACHINE FANOUT SCALING — DONE (makes the (2) slice weighting bite)
    SEMANTICS (user choice): `fanout: N` on a placement:"all" node = N TOTAL
    workers across the cluster, apportioned by per-host capacity (Hamilton /
    largest-remainder so parts sum to exactly N). Equal capacity → N/nodes each;
    this DOES reduce the old per-machine count (word_count fanout:8 on 2 nodes:
    was 8+8=16, now 4+4=8) — accepted tradeoff for cluster-total semantics.
    IMPL (splitter.rs): partition() resolves effective_hints BEFORE expansion;
    expand_all_placements(hints) calls apportion_fanout(n, total_nodes, hints)
    and stamps each machine's "all" copy with its share. expand_fanout then
    expands per-machine; assign_input_slices (2) counts the apportioned maps so
    the input slice automatically tracks the map share.
    VERIFIED (partitioner): capacity 0.8/0.2, fanout:8 → maps 6/2, slices
    [0,0.75]/[0.75,1.0]; uniform → maps 4/4, slices [0,0.5]/[0.5,1.0]. A host
    with ~0 capacity gets 0 maps + an empty slice (handled by empty-aggregate /
    0-byte RemoteSend). Unit tests in splitter.rs still green (15 pass).
    NOTE: live cluster uses the coordinator's SCX/metrics-derived capacity hints;
    standalone CLI with no hints → uniform. Cluster run pending (user runs it;
    needs node-agent rebuilt+restarted so the in-process partitioner is current).

[x] (7) FANOUT CORE-BUDGET CAP — DONE (clamp over-large fanout to the cluster)
    A requested `fanout: N` is clamped to the cluster's usable core budget
    BEFORE capacity apportionment (6): budget = Σ_hosts max(1, cores_host −
    reserve), reserve = PARTITIONER_FANOUT_CORES_RESERVED_PER_HOST (=2, leaves
    headroom for the control plane / RDMA threads / OS). So fanout:50 on a
    2×16-core cluster → capped to 28, not 50.
    DATA PATH: each node already reports cpu_cores in its metrics → scx_view.
    New scheduler::host_cores(view) (raw per-host cores, no busy discount) →
    new PlacementHints.cores field → coordinator populates it (and remaps it to
    logical ids in resolve_dag); the in-process partitioner reads it. Hints are
    now kept even when SCX capacity is empty as long as cores are known, so the
    cap works without SCX scheduling. Standalone CLI (no cores) → no cap.
    IMPL: cap_fanout(n, total_nodes, hints) in splitter.rs, applied in
    expand_all_placements before apportion_fanout; logs "fanout '<id>' capped
    A → B" to stderr. Touches: NodeAgent/common (const), scheduler (host_cores +
    export), placer (cores field), coordinator (populate + remap + keep-hints),
    splitter (cap_fanout). Unit test fanout_capped_to_core_budget green.
    VERIFIED (partitioner): cores 16/16 reserve 2, fanout:50 → 28 (14/14);
    cores 16/8 + cap 0.66/0.34, fanout:50 → 20 (maps 13/7, slices
    [0,0.65]/[0.65,1.0]). Composes with (6) and (2). Cluster run pending (user).

--- OLD (6) DESCRIPTION (kept for context) -----------------------------------
    The (2) slice code weights each node's shard by its map-worker count
    (weight_m = maps_on_m / total_maps) and is UNIT-TESTED for the unbalanced
    case: 8 maps on node 0 + 2 on node 1 -> slices [0,0.8] / [0.8,1.0]
    (splitter.rs tests input_slices_weighted_by_map_count / _balanced_when_maps_even).
    BUT the slicing path only fires for placement:"all" inputs, and "all" today
    replicates the fanout UNIFORMLY (expand_all_placements clones the node with
    the same `fanout`, expand_fanout splits it evenly), so every machine gets the
    SAME map count -> the weighting always yields an even split (50/50 on 2 nodes).
    The framework also forbids cross-node Input deps (splitter.rs ~165), so an
    auto-placed fanout reading a replicated load can't be expressed — the only
    sliceable shape is the symmetric "all" one.
    TO MAKE 8/2 REACHABLE: scale each machine's replicated map fanout by its
    capacity weight in expand_all_placements (e.g. fanout_m = round(fanout *
    cap_m / Σcap)), using the same live SCX/capacity hints the placer already
    consumes. Then node 0 gets 8 maps + a 0.8 slice and node 1 gets 2 + 0.2,
    end-to-end. The (2) host load_slice + the weighting are already ready for it;
    only the per-machine fanout count needs to become capacity-aware.

