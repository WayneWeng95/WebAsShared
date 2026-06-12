# FINRA — WebAsShared vs RMMap / Faasm / Cloudburst

Workload **#3**. A multi-stage financial-compliance audit workflow — the
inter-stage state movement that page-chain splicing targets. Built following
[`../EXPERIMENT_RUNBOOK.md`](../EXPERIMENT_RUNBOOK.md).

```
load_trades → fetch_private (parse) ┐
              fetch_public (ref data)┴→ aggregate(→200)
   → finra_audit_rule × 8  (each reads slot 200 — a BROADCAST) → aggregate(→300)
   → finra_merge_results → save
```

**Frozen shared spec** (the same for every system — *not* RMMap's different
native finra): our **8 audit rules** + the generated `trades_*.csv`. The audit
rules (`Executor/guest/src/workloads/finra.rs`):

| rule | name | condition |
|------|------|-----------|
| 0 | PRICE_OUTLIER | price > 2× reference avg (20-symbol ref table) |
| 1 | LARGE_ORDER | quantity > 5000 |
| 2 | WASH_TRADE | same (account,symbol) both BUY and SELL |
| 3 | SPOOFING | (account,symbol) trade count > 10 |
| 4 | CONCENTRATION | symbol count > total/20 + 100 |
| 5 | AFTER_HOURS | time outside 09:30–16:00 |
| 6 | PENNY_STOCK | price < $5.00 |
| 7 | ROUND_LOT | quantity ≥ 1000 and a multiple of 1000 |

**Correctness gate:** `total_violations` per trades file — deterministic
(`gen_trades.py`, fixed seed) and **identical across every system**. The
canonical sweep is **10 k / 100 k / 1 M** trades:

| trades | total_violations |
|--------|------------------|
| 10 000    | **6 183**   |
| 100 000   | **49 763**  |
| 1 000 000 | **456 574** |

> **Fixes (2026-06-12):**
> 1. `parse_time` only handled ISO `…T10:30:00`, so every `HH:MM` trade parsed to
>    `(0,0)` and AFTER_HOURS flagged all of them. Fixed to parse plain `HH:MM`.
> 2. **Parse-once rule_0 bug.** When the audit rules were switched to the packed
>    columnar form (below), rule_0 (PRICE_OUTLIER) re-derived its reference-price
>    table from the 20 `R:` records the aggregate carried into slot 200. At large
>    scale the aggregate proved unreliable at carrying those 20 tiny records
>    behind tens of MB of trade chunks, silently zeroing the table and **inflating
>    rule_0** (e.g. 980 605 vs 80 427 at 2 M). Fixed by resolving the reference
>    price from a **static in-rule constant** (`REF_CENTS`, indexed by
>    `symbol_idx`) — the same fixed table every baseline already hardcodes
>    (Faasm's `ref_cents()` is the identical `match`). The Python `finra_rules.py`
>    port matches; the gates above are post-fix and agree across all four systems.

## Input

`gen_trades.py N OUT.csv [seed]` → `TestData/finra/trades_<N>.csv` (the 20
reference symbols, field distributions that trigger all 8 rules at stable rates).

```bash
for n in 10000 100000 1000000; do
  python3 gen_trades.py $n ../../../TestData/finra/trades_$n.csv; done
```

## Parse-once / columnar broadcast (the optimization)

Originally `finra_fetch_private` wrote one **text** record per trade (`T:<csv>`)
and **each of the 8 rules re-parsed that CSV** from slot 200 — 8× string parsing,
plus the (account,symbol) rules did an **O(n·k) `Vec::find`** over `format!`-built
string keys. At 80 k trades the 8-rule wave was **92 % of runtime** (1235 ms);
at larger sizes the O(n·k) keyed rules would never finish.

`fetch_private` now parses the CSV **once** into a packed **20-byte columnar
record** per trade (symbols/accounts pre-resolved to integer indices), batched
into ~256 KiB stream chunks. Each rule becomes a flat integer scan with
**fixed-array counters** (`[u8; ACCT*SYM]` for wash/spoofing — dense because the
generator uses 100 accounts × 20 symbols). Effect at 80 k (AOT, per-wave):

| stage | before | after |
|-------|--------|-------|
| 8-rule wave (wave 3) | **1235 ms** | **9.2 ms** (134×) |
| TOTAL compute | 1340 ms | **178 ms** (7.5×) |

The bottleneck now moves to the single-threaded `fetch_private` parse stage —
i.e. real parsing work, not orchestration overhead. The 8-rule broadcast is now
nearly free.

## WebAsShared (ours) — native, DONE

`gen_dag.py <trades.csv> <out> <shm>` emits the fixed 8-rule DAG (env `WC_WASM`
→ AOT `.cwasm`). `run.sh` sweeps trade count and writes the shared columns
`size_trades,topo,compute_ms,throughput_trades_s,peak_mem_mb,reps,total_violations`.

```bash
./run.sh "10000 100000 1000000" 3                                  # JIT → results.csv
WC_WASM=Executor/target/wasm32-unknown-unknown/release/guest.cwasm \
  WC_CSV="$PWD/results_aot.csv" ./run.sh "10000 100000 1000000" 3   # AOT → results_aot.csv
```

Results (compute ms / throughput trades·s⁻¹, 3 reps, this box):

| trades | JIT ms | AOT ms | AOT trades/s | peak MB (AOT) |
|--------|--------|--------|--------------|---------------|
| 10 000    | 612  | **34**   | 292 826 | 72  |
| 100 000   | 790  | **259**  | 386 324 | 95  |
| 1 000 000 | 2337 | **1753** | 570 308 | 267 |

`results_aot.csv` is the fair line for the cross-system plot. AOT helps most at
small N (it removes the per-worker guest JIT of the 10 short-lived workers — 18×
at 10 k); at 1 M the run is dominated by the single parse stage so the gap shrinks.

## Baselines (same frozen spec, same trades files)

Shared 8-rule logic: `finra_rules.py` (a faithful Python port of `finra.rs`,
validated against the native counts) — reused by the Python baselines.

| System | Status | Approach |
|--------|--------|----------|
| **Cloudburst** | ✅ | 8-rule DAG over the Redis runner (`baseline/cloudburst/`). Trades broadcast to all 8 rules through Redis; rules sequential in one process. |
| **RMMap-ES** | ✅ | same 8 rules over Redis+pickle, rules as **parallel processes** (≈ pods), py3.7 image (`baseline/rmmap/`). |
| **Faasm-like** | ✅ | 8 rules → **wasm32-wasip1** (`finra_wasi.rs`), each rule a fresh **wasmtime** Faaslet, trades via Redis (`baseline/faasm/demo/`). |

**All four reproduce the gate** (6183/49763/456574). Cross-system at 1 M trades
(AOT line for ours, throughput trades·s⁻¹ / e2e latency):

| System | 1 M throughput (k trades/s) | 1 M latency (ms) |
|--------|----------------------------|------------------|
| **WasMem** (AOT) | **570** | **1753** |
| Faasm-like | 194 | 5155 |
| RMMap-ES   | 104 | 9573 |
| Cloudburst | 61  | 16506 |

`plot.py` → [`figs/finra_bars.pdf`](./figs/finra_bars.pdf): one panel per trade
count (10k / 100k / 1M), each the four systems' end-to-end latency (ms), bars in
legend order Cloudburst→WasMem; StateSync palette.

> **The result, and the history.** *Before* parse-once, FINRA was
> overhead-dominated: the 8 rules re-parsed the CSV and the run was 92 % rule
> wave, so at 80 k WasMem was actually **slowest** (the tiny broadcast gave the
> zero-copy SHM nothing to win, while our DAG paid per-stage worker bring-up).
> *After* parse-once (the broadcast is now nearly free) **plus AOT** (no per-worker
> guest JIT), WasMem is **fastest at every size** — ~2.3–3× Faasm and ~9×
> Cloudburst at 1 M. Two reasons it pulls further ahead with scale: (1) the 8-rule
> broadcast is O(n) integer scans we parse **once**; (2) the baselines pay
> **per-stage KV serialization** that grows with the data — at 1 M Cloudburst
> pushes **157 MB put + 721 MB get** through Redis, and Faasm re-parses the CSV in
> 8 separate Faaslets, while WasMem moves the broadcast **zero-copy** through the
> page chain. Same axis as WordCount: our edge scales with state volume.
