
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

[ ] (2) DATA-PARALLEL SHARDING (replication double-counts today)
    placement:"all" makes every node load + count the FULL corpus, so a working
    merge would give Nx (double/triple count), not 1x. We do NOT want Nx; the
    correct distributed result is 1x produced by N nodes each doing a slice.
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

[ ] (3) MEASUREMENT / METRICS still need checking (per-node, per-wave timing;
    exclude staging). Original note above.

[ ] (4) bring comparable frameworks onto the table (benchmark baselines).

[ ] (5) (optional perf) fan-out doesn't scale past ~core count: each map is a
    separate process (fork + wasmtime JIT + SHM mmap), wave spawns all N
    uncapped, workload is memory-bandwidth-bound, and wc_map's read_all OOMs the
    ~0.5 GiB guest heap at low fan-out on large inputs. Fixes: stream wc_map
    (chain_for_each not read_all), reuse one wasmtime engine/threads, cap
    concurrency at ~cores. See Tests/fan_out_test/README.md.

