# Faasm-like WordCount demo (Faaslet-lite)

Real Faasm can't be stood up on this box (the v0.2.x images are gone; the GRANNY
0.32/0.33 stack deploys but exhausts the disk and needs a host-interface port —
see [`../README.md`](../README.md)). So this **abstracts Faasm's mechanism for
WordCount** into a runnable demo, the same way the Cloudburst and RMMap baselines
reproduce their data paths rather than their full stacks.

## What it abstracts from Faasm

| Faasm | This demo |
|-------|-----------|
| Functions compiled to **WASM**, run as **Faaslets** in a lightweight runtime (WAVM/WAMR) with SFI isolation | `wc.rs` → **wasm32-wasip1**, AOT-compiled to `wc.cwasm`, run as a fresh **wasmtime** instance per call |
| **`faasmReadState`/`faasmWriteState`** move corpus chunks + `partial_<i>` through a distributed KV (Redis-backed), **serialized** | the host (`driver.py`) writes `chunk_<i>` / reads `partial_<i>` in **Redis** — the same serialized KV state transfer |
| **`faasmChainThisInput`** fans out to N mappers | `driver.py` runs N mapper Faaslets **concurrently** |
| **Billable memory** / small per-function footprint vs containers | per-Faaslet **peak RSS** (measured with `/usr/bin/time`) |

Same tokenization (split on non-alpha, lowercase) and wire format
(`"word\x1fcount\n"`) as the Faasm C++ port (`../wordcount.cpp`), so the state
bytes are comparable. The map streams stdin in 64 KB blocks (a single
`read_to_end` into wasm32 linear memory caps out ~128 MB) — which also keeps the
Faaslet footprint **tiny and constant (~19 MB)**, independent of corpus size.

**Faithful:** WASM isolation, serialized KV state, the *partitioned* read pattern
of the real `wordcount.cpp` (each mapper reads only its `chunk_<i>` — 1× corpus,
unlike RMMap-ES's broadcast), chaining shape, lightweight footprint.
**Dropped (honest scope):** Faasm's scheduler, Proto-Faaslet snapshots, WAVM
specifics, cold-start/snapshot story, the real billable-memory accounting. This
is a Faasm-*like* demo, **not** the Faasm runtime.

## Run

```bash
# needs: rustc + `rustup target add wasm32-wasip1`, wasmtime, Redis on :6379, python3-redis
./run.sh "50 500 1000" "1 2 4 8 16"      # builds wc.cwasm, sweeps → results.csv
```

Columns: `size_mb,workers,topo,e2e_ms,throughput_mb_s,words_per_s,peak_mem_mb,state_kv_mb,total_occurrences`.

- `peak_mem_mb` — largest single Faaslet RSS (~19 MB, constant).
- `state_kv_mb` — bytes through Redis (≈ 2× corpus: chunks written + read, partitioned; partials negligible). Our zero-copy page-chain = 0.

## Notes

- Compute is **compiled Rust→WASM**, so it's much faster than the Python
  Cloudburst/RMMap baselines — the demo shows Faasm's WASM-compute advantage
  *and* its serialized-KV-state cost in one number.
- Counts validated against every other system (`8940339 / 89403388 / 179060000`).
- Don't compare against Faasm's published numbers (different runtime, snapshots,
  cluster).
