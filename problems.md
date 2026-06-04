
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

[ ] (1) CROSS-NODE AGGREGATE DROPS A NODE'S CONTRIBUTION  — BLOCKER
    Symptom: 2-node word_count_auto_placement returns 1x (single-node) count,
    not the sum of both nodes. map_records_received=193 (= one node's 8 maps;
    both would be ~384). Per-word counts == single-node exactly.
    Isolated: node 2 gets the full corpus via RDMA staging (logs confirm) AND
    its local pipeline produces the full count in slot 159 (reproduced
    standalone). So both pipelines are fine; the loss is purely in the
    cross-node path: node 2 aggregate_1 -> RemoteSend(slot 159) -> node 0
    RemoteRecv(slot 161) -> aggregate_global. Node 0's aggregate_global sees
    only its own local slot 158; slot 161 arrives empty.
    NOTE: present in ALL runs (incl. before the distribute fix) — NOT caused by
    the distribute/packing work. RDMA threads ARE joined per-wave (mod.rs:618),
    and timing suggests node 0's recv did block ~2.1s for node 2, so it likely
    received something — need the real byte counts.
    LEADING HYPOTHESIS: slot 159 on node 2 is reclaimed after aggregate_1's wave
    BEFORE RemoteSend reads it, so it sends 0 bytes. build_slot_refcounts /
    node_owned_slots may not count a RemoteSend as a *reader* of its input slot
    (only producers/owners are counted). The local repro kept 159 alive only
    because I added a wc_reduce reader. CHECK: does reclamation keep a slot
    alive for a RemoteSend that reads it? (plan.rs build_slot_refcounts +
    node_owned_slots / node_routed_upstream_slots, and the post-wave reclaim in
    mod.rs ~635).
    NEED: executor stdout is hidden in multi-node (captured). To confirm, make
    the coordinator/worker spawn the executor with live output (ExecutorHandle::
    spawn live flag) OR log RemoteSend/Recv byte counts to a file, then re-run
    and read "[RemoteSend-SI] slot 159: N bytes" / "[RemoteRecv-SI] slot 161:
    N bytes". N==0 on send => reclamation/pipeline; N>0 but 161 empty => link/
    merge bug.

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

