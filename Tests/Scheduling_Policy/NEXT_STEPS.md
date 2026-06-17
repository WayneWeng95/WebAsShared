# Scheduling_Policy — status & next steps

_Snapshot of where the placement-policy experiment stands and what to do next._

## Current state (what works)

### word_count — DONE
- Sweep: input size {50, 500, 1000 MB} × policy {pack, balanced} × fanout, via
  `wc_size_sweep.sh`. Metrics: **makespan** (latency) and **total_exec** (Σ busy
  time over working nodes, idle excluded) — see `analyze_exec.py`.
- Result: balanced wins on makespan (more nodes), pack wins on total_exec
  (less coordination); gap converges at scale. word_count is **bandwidth-bound**.
- `pack` consolidation is first-fit via `--pack-cap` (e.g. cap 16 → fanout 32 fills
  2 nodes [16,16,0,0], fanout 48 fills 3 nodes).

### finra — FIXED, all 4 policies correct + deadlock-free
- All policies (pack/balanced/spread/random) on 4 nodes return `Success: true` and
  `total_violations = 1952`, **matching the single-node ground truth** (incl.
  stateful rules: spoofing 245, wash 250, concentration 5).
- Timings (single runs): pack 924ms < spread 1264 < balanced 1278 < random 1330.
  finra is **coordination-bound** (192 KB input, light compute) → pack/locality
  wins — the *opposite* of word_count. This is the workload-dependent story.

## Next steps (in priority order)

1. **Build a finra policy sweep** (`finra_sweep.sh`), analogous to `wc_size_sweep.sh`
   but no size/fanout axis (fixed trades.csv, 8 rules):
   - axes: policy {pack, balanced, spread, random} × N reps (median).
   - metrics: makespan + total_exec (working nodes = those with `finra_audit_rule`;
     idle excluded), throughput (trades/s), violations (correctness gate = 1952).
   - reuse the submit/timing/median logic from `wc_size_sweep.sh`.
   - lock in the pack-vs-others numbers properly (current ones are single runs).

2. **Commit the framework fixes + harness** (they're validated; 23/23 partitioner
   lib tests pass, word_count untouched). Suggested grouping:
   - Partitioner: cycle fix, `wasm_arg` forwarding, same-machine dep wiring,
     `replicate` no-slice. Regression test: `sandwich_placement_is_acyclic`.
   - `Tests/Scheduling_Policy/` harness (gen_variants, sweeps, analyzers, README).
   - End commit messages with the Co-Authored-By trailer.

3. **Multi-node Faasm baseline** for word_count (reuse
   `Tests/Inter-Node Application_Benchmark/WordCount/baseline/faasm/demo/`):
   shared Redis on node-0 + distribute mapper Faaslets across nodes. See the
   "Baseline" section in `README.md`. Same CSV columns so rows line up.

4. **ml_training** — same shape as finra (train_* fan, aggregate_train). The
   gen_variants transform already handles it; validate it the same way finra was
   (ground truth vs distributed). Likely works now via local-combine.

## How finra was fixed (so it isn't re-broken)

The distributed finra needed FIVE fixes — the partitioner ones are framework-level,
the rest live in `gen_variants.py`:

| # | Fix | Where |
|---|-----|-------|
| 1 | Cycle in deadlock-prevention pass (update `succ_map` incrementally) | `splitter.rs` |
| 2 | `translate_func_kind` forwards `wasm_arg` (rule_id) not the input slot `arg` | `splitter.rs` |
| 3 | Placed node reads SAME-MACHINE copy of a `placement:"all"` producer (gated on `node_id`) | `splitter.rs` `expand_all_placements` |
| 4 | `replicate: true` Input → no slicing (every node loads full file) | `splitter.rs` `assign_input_slices` |
| 5 | **Local-combine**: per-machine `aggregate_rules_local` (empty `{Aggregate:{}}`) → 1 transfer/peer | `gen_variants.py` |

**The key lesson (fix #5):** a node receiving **>1 cross-node transfer from the same
peer** desyncs the per-peer control stream and deadlocks (sender-initiated). The
working pattern (word_count, the benchmark) does a **per-machine local aggregate
first** so each peer sends exactly ONE transfer. Always combine locally before the
cross-node hop. Verify with: `node0 receives 1 transfer/peer` (see the topology
check in the git history of this work).

**Dead ends (don't retry):**
- **Receiver-initiated (RI) protocol** — tried (`remote_protocol` field), broke the
  deadlock but the gather still failed/hung; reverted. Not the answer.
- Auto-placing the fan with the per-machine `placement:"all"` aggregator → N×M
  all-to-all gather (deadlocks). Must collapse to local-combine + single global.

## Gotchas / environment
- Distributing-policy finra used to **wedge the whole cluster** on deadlock; that's
  fixed, but if a future change regresses it, a hung job needs a cluster reset
  (kill stray `host` executors on workers + restart coordinator).
- Build with `./build.sh` (not bare cargo). It can't overwrite `node-agent` while
  the coordinator is running ("Text file busy") — stop the coordinator first.
- Experiment submits **pre-partitioned** ClusterDags (via the standalone `partition`
  binary) so the embedded `placement_policy` is authoritative; the coordinator
  passes them through. Offline pre-partitioning is in `wc_size_sweep.sh` / by hand.
- Ground-truth check for finra correctness: partition the stock
  `DAGs/symbolic_dag/finra_auto_placement.json` with `total_nodes=1` and submit →
  must report `total_violations=1952`.
