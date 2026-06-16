# Faasm WordCount port

Map-reduce WordCount as a Faaslet (WASM) function — the Faasm baseline for
[`../../README.md`](../../README.md). **This is a new port** (Faasm shipped no WordCount).

## Files (canonical copies; installed into the Faasm tree)

| File | Installed to (in `../../../../../../compare_system/faasm/`) |
|------|------------------------------------------------------------|
| `wordcount.cpp`   | `func/wordcount/wordcount.cpp` |
| `CMakeLists.txt`  | `func/wordcount/CMakeLists.txt` |
| —                 | wired via `add_subdirectory(wordcount)` in `func/CMakeLists.txt` |

One WASM module, three funcs selected by idx (Faasm's `FAASM_FUNC(name, idx)`):

| idx | func | role |
|-----|------|------|
| 0 | `faasmMain` | splitter/driver: read `corpus` state → N newline-aligned `chunk_<i>` → chain N mappers → await → chain reducer |
| 1 | `wc_mapper` | read `chunk_<i>`, count words, write `partial_<i>` |
| 2 | `wc_reducer`| merge `partial_0..partial_{N-1}` → `result` |

Fan-out is `faasmChainThisInput`; state moves through the Faasm KV
(`faasmReadState`/`faasmWriteState`). The serialized `partial_<i>` blobs are the
inter-stage transfer we measure against our zero-copy path.

## Build

```bash
cd /opt/myapp/compare_system/faasm
# inside the Faasm toolchain container / env (see faasm docs/getting_started)
inv compile wordcount wordcount            # FAASM_USER=wordcount, func=wordcount
#   → wasm at wasm/wordcount/wordcount/function.wasm
```

The pure word-counting/serialization logic is also covered by a standalone host
test (`g++ -std=c++17`) — run during development to validate tokenization and
the partial→merge round-trip without the toolchain.

## Run

```bash
# 1. upload the corpus as Faasm state under user=wordcount, key=corpus
inv state.set wordcount corpus --path /path/to/corpus.txt      # (or the driver/ upload pattern)
# 2. invoke; the string input = number of mappers N
inv invoke wordcount wordcount --input 8
#   driver prints: "WordCount done: 8 mappers + reduce over <bytes> bytes"
#   reducer prints: "WordCount reduce: <unique> unique words across 8 partials"
#   final counts in state key "result"
```

> Use the **same corpus and the same N** as the other three systems. Metric:
> Faasm's billable memory + per-function exec time; the `partial_<i>` writes are
> the serialized state-transfer cost (contrast: ours = 0).

## Reproduction status (2026-06-12) — BLOCKED on this dev box (images removed upstream)

Tried to stand Faasm up here. **The pinned v0.2.3 Docker images required to compile
and run are no longer on Docker Hub**, so neither the compile path nor the runtime
stack can come up on this box:

| Image (needed for) | Status on Docker Hub |
|--------------------|----------------------|
| `faasm/toolchain:0.2.3` (compile C++→WASM via `bin/cli.sh`) | **repo deleted** — `object not found` |
| `faasm/knative-worker:0.2.3` (the `worker` in `docker-compose.yml`) | **repo deleted** — `object not found` |
| `faasm/cpp:0.2.3` | **repo deleted** — `object not found` |
| `faasm/cli:0.2.3` (all-in-one dev container) | **no such tag** — the `cli` repo only starts at `0.4.x` |
| `faasm/upload:0.2.3`, `faasm/redis:0.2.3` | still present (but useless without toolchain + worker) |

Root cause: the ATC'20-era (0.2.x) images were retired when the project moved to the
GRANNY-era (`main`, 0.33.x) — same reason `func/` was removed upstream
(see `Benchmarks/benchmark_workloads.md`). The local `faasm/` submodules
(`third-party/WAVM`, `llvm-project`, …) are **not checked out**, and `/` has only
~16 GB free, so a from-source toolchain/LLVM build isn't viable here either.

**Paths to a real Faasm number (all off this box):**
1. **Eval cluster / a provisioned Faasm host** — build the v0.2.x toolchain from source
   (submodules + LLVM + wasm sysroot, needs disk), `inv compile wordcount wordcount`,
   `docker-compose up`, then the Run recipe above. *(Phase 0.)*
2. **Port the func to a current Faasm** (`faasm/cli:0.9.4` exists) — but the GRANNY-era
   host interface differs from the `faasm/faasm.h` API this `wordcount.cpp` uses
   (`FAASM_FUNC`, `faasmChainThisInput`, `faasmReadState/WriteState`), so it's a real
   port, not a recompile. Only worth it if v0.2.x can't be rebuilt.

The port itself is complete and its word-count/serialization logic is host-compile
verified; only the Faasm runtime is blocked here.

### Could the new GRANNY-era Faasm (0.32/0.33) work instead? — investigated 2026-06-12

Checked, because the old 0.2.x images are gone. Findings:

- **Images exist** on **ghcr.io** (not Docker Hub): `ghcr.io/faasm/{worker,planner,upload,
  minio,redis,cli,cpp-sysroot}` — pinned by `faasmctl` (`FAASM_VERSION=0.32/0.33`,
  `CPP_VERSION=0.8.0`, `FAABRIC/planner 0.22.0`). `pip install faasmctl` works.
- **The cluster DOES deploy here.** `faasmctl deploy.compose --workers 1` brought up
  planner + worker + upload + minio + redis successfully (worker `faasm-…-worker-1`).
- **BUT it exhausts the disk.** The GRANNY image set + worker filled `/` to **100% (109 MB
  left)** on this box (63 GB disk, ~87% used by other project data). Compiling our function
  needs the `cpp-sysroot` toolchain + a **conan/boost** build on top — several more GB that
  don't fit. Torn back down and reclaimed.
- **And the port would need rewriting** to the GRANNY host interface (cpp `0.8.0`): the ATC'20
  `FAASM_FUNC` / `faasmChainThisInput` / `faasmReadState/WriteState` API this `wordcount.cpp`
  uses changed in the GRANNY line — a real port, not a recompile.

**Verdict: ABORT on this box.** GRANNY is viable *in principle* (images current, cluster
deploys) but not here — no disk headroom for the toolchain/conan build, and the func needs an
API port. Right home is the **eval cluster** (provision disk, deploy the 0.32/0.33 cluster,
port `wordcount.cpp` to the cpp-0.8 interface, compile + sweep). Left the box clean (images
removed).
