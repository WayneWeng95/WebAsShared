# Comparison-System Changes — Clean-Deployment Record

**Purpose:** record every local change made to the vendored comparison systems under
`/opt/myapp/compare_system`, so the benchmark baselines can be reproduced from scratch on a
new machine. Generated **2026-06-23**.

> **Why this file exists.** Each comparison system is a *pristine upstream git checkout pinned
> to an old commit* — none of them pull upstream changes. All the benchmark ports we wrote sit
> on top as **uncommitted / untracked** local files. That means a `git clean -fdx` or
> `git checkout .` inside any of those repos **deletes our work without warning**, and a fresh
> `git clone` will not contain any of it. This file + the `patches/` and `port_files/`
> directories next to it are the only durable copy.

Companion files in this folder:
- `benchmark_workloads.md` — copy of `compare_system/benchmark_workloads.md` (workload-per-system matrix).
- `patches/` — re-appliable `git diff` patches for every **tracked-file** edit.
- `port_files/` — verbatim copies of every **untracked** new source file (what `git clean` would wipe).

---

## 1. Upstream pins (re-clone targets)

All repos are frozen at these commits; **do not** advance them — the ports assume these exact trees.

| Repo | Upstream remote | Pinned HEAD | Pin date |
|------|-----------------|-------------|----------|
| RMMap (dmerge) | https://github.com/ProjectMitosisOS/dmerge-eurosys24-ae.git | `8eda3806d5bfd55cc118549cfdf4524434bf170e` | 2023-11-07 |
| RTSFaaS | https://github.com/CGCL-codes/RTSFaaS.git | `89d7b65b58cd65b6aa69fe2f429793810e7ba766` | 2025-05-04 |
| cloudburst | https://github.com/hydro-project/cloudburst.git | `07905ae3f489fb2a9b920f701c3023c02ee8b877` | 2020-07-09 |
| faasm | https://github.com/faasm/faasm.git | `15f3f3a6d220f08882ba7e7165e9618bc2a25de8` **(tag `v0.2.4`)** | 2020-07-15 |
| roadrunner | https://github.com/polaris-slo-cloud/roadrunner.git | `b78029d97bd2a05c4155d81570bb2495395074f4` | 2025-10-06 |
| flink-statefun-transactions | https://github.com/delftdata/flink-statefun-transactions.git | `e13327696c1e609730a14c867955385756cda897` | 2021-06-07 |

> **faasm is intentionally pinned to an old tag.** The checkout is a **detached HEAD exactly on
> tag `v0.2.4`** (commit `15f3f3a6`, 2020-07-15) — `git describe --exact-match HEAD` → `v0.2.4`.
> This is the **ATC '20-era release** where the paper's §6 workloads still live in-tree under
> `func/` (`sgd/`, `tf/`, `polybench/`, `python/`, `demo/matrix.cpp`); our WordCount port is added
> on top of that tree. **Do NOT use `main`** — the local `main` branch tracks `origin/main` at the
> much newer **GRANNY-era v0.33.0 line**, which *deleted* `func/` and moved workloads to external
> `experiment-*` repos. (The in-tree `VERSION` file reads `0.2.3`, lagging the tag by one; the tag
> `v0.2.4` is authoritative.)
>
> Re-clone exactly (plain clone — **do NOT init submodules**):
> ```sh
> git clone https://github.com/faasm/faasm.git
> cd faasm && git checkout v0.2.4
> ```
> **Do not run `git submodule update --init --recursive`.** The reference node has
> **none** of faasm's `third-party/*` submodules initialised (`git submodule status`
> shows a leading `-` on all 15). faasm builds its WASM functions via the **faasm Docker
> toolchain** (`faasm/cli` / `faasm/cpp` containers), not by locally building llvm-project,
> tensorflow, cpython, etc. A recursive init also **cannot complete from upstream** anyway —
> two v0.2.4-era submodule mirrors are dead (`https://github.com/Shillaker/eigen-git-mirror`
> and `.../faasm-demo-c` → HTTP 404). So: plain clone, checkout `v0.2.4`, build via Docker.

---

## 2. Change summary (what we touched)

| Repo | Tracked edits | New (untracked) port files | Last touched | Status |
|------|---------------|----------------------------|--------------|--------|
| **RTSFaaS** | `RDMABenchmark.java`, `Metrics.java` | — | **2026-06-22** | live / actively edited |
| **cloudburst** | `server.py` (dispatch) | `wordcount.py`, `terasort.py`, `redis_runner.py`, `redis_kvs.py` | 2026-06-18 | settled |
| **RMMap** | — | `exp/wordcount/app/*`, regenerated `pyx/bindings.cpp` | 2026-06-12 | settled |
| **faasm** | `func/CMakeLists.txt` | `func/wordcount/{wordcount.cpp,CMakeLists.txt}` | 2026-06-10 | settled |
| **roadrunner** | `docs/quick-install.sh` (chmod +x only) | generated `…/input-data/storage/files/` payloads | 2026-06-10 | settled |
| **flink-statefun-transactions** | — | — | — | pristine; not yet ported |

Build artifacts (`*.o .so .pyc .class .jar`, `target/`, `build/`, `__pycache__/`) are also dirty
in several repos but are **regenerated on build** — not recorded here.

---

## 3. Per-repo detail + restore steps

### RMMap (dmerge) — WordCount port
- **New:** `exp/wordcount/app/{functions.py, util.py, bindings.pyx, setup.py, native/wrapper.h}`
- **Regenerated:** `pyx/bindings.cpp` + `pyx/build/` (Cython build output — rebuild, don't copy).
- **Restore:** `cp -r port_files/RMMap_exp_wordcount_app/* <repo>/exp/wordcount/app/`, then rebuild
  the pyx bindings (`cd pyx && python setup.py build_ext --inplace` per the repo's build flow).

### RTSFaaS — single-node baseline keep-alive + metrics buffer  *(LIVE — most recent change)*
- **`morph-clients/.../benchmark/rdma/RDMABenchmark.java`** (+1 line): added
  `Thread.currentThread().join();` after DB start so the JVM stays alive to serve RDMA in the
  single-node baseline.
- **`morph-core/.../profiler/Metrics.java`** (~30 lines): every `new DescriptiveStatistics()` →
  `new DescriptiveStatistics(10000)` — bounds the rolling stats window to the last 10k samples
  (prevents unbounded growth / OOM during long metric runs).
- **Restore:** `git apply patches/RTSFaaS_rdma_metrics.patch` from the repo root, then rebuild jars
  (`mvn package` / the repo's build). Note jars under `morph-core/target/` are rebuilt artifacts.

### cloudburst — WordCount + TeraSort on Redis
- **`cloudburst/server/benchmarks/server.py`** (tracked): import `wordcount` + add a
  `bname == 'wordcount'` dispatch branch.
- **New:** `benchmarks/wordcount.py`, `benchmarks/terasort.py`, `benchmarks/redis_runner.py`,
  `shared/redis_kvs.py` (Redis-backed KVS shim — Anna is deliberately **not** used).
- **Restore:** `git apply patches/cloudburst_server.py.patch`; then
  `cp port_files/cloudburst_benchmarks/{wordcount.py,terasort.py,redis_runner.py} <repo>/cloudburst/server/benchmarks/`
  and `cp port_files/cloudburst_benchmarks/redis_kvs.py <repo>/cloudburst/shared/redis_kvs.py`.

### faasm — WordCount Faaslet
- **`func/CMakeLists.txt`** (tracked, +1 line): `add_subdirectory(wordcount)`.
- **New:** `func/wordcount/{wordcount.cpp, CMakeLists.txt}`.
- **Restore:** `git apply patches/faasm_func_CMakeLists.txt.patch`; then
  `cp -r port_files/faasm_func_wordcount <repo>/func/wordcount`. Build with the faasm wasm toolchain
  (`inv compile wordcount`-style flow); AOT-compile to `wc.cwasm` for the benchmark.

### roadrunner — payload generation only
- **`docs/quick-install.sh`** (tracked): mode change `100644 → 100755` (chmod +x). No content change.
- **Generated:** `experiments/evaluation/input-data/storage/files/` (1M–500M payloads) + a built
  `storage` warp file-server under `…/storage/target/release/`. **Not archived** (large, regenerable):
  rebuild via `experiments/evaluation/input-data/make-payloads.sh` and `cargo build --release` in
  `…/input-data/storage/`.
- **Restore:** `chmod +x docs/quick-install.sh` (or `git apply patches/roadrunner_quick-install.sh.patch`),
  then regenerate payloads.

### flink-statefun-transactions
- No local changes. Pristine upstream. Listed here only as a candidate transactional baseline not
  yet ported.

---

## 4. Clean-deployment checklist (new machine)

1. Clone each repo to the pin in §1 (`git clone <url> && git -C <repo> checkout <HEAD>`).
   **Plain clone for faasm too — do NOT recurse submodules** (see the faasm note in §1).
2. Re-apply tracked edits: `git apply patches/<repo>_*.patch` from each repo root.
3. Restore untracked port files from `port_files/` (paths in §3).
4. Regenerate build artifacts / payloads (RMMap pyx, RTSFaaS jars, faasm wasm, roadrunner payloads).
5. Cross-check against `../../Benchmarks/<system>/` mirrors — the intra-node README states the
   faasm/cloudburst WordCount ports were *also* copied there; reconcile if they diverge.

> ⚠️ **Two methodology caveats carried from `../Inter-Node Application_Benchmark/STATUS.md`:**
> (1) cloudburst runs on **Redis, not Anna** — locality numbers differ from published.
> (2) Faasm makespan **includes** its Redis input upload; WasMem **excludes** RDMA input staging.

---

*Snapshot taken 2026-06-23. If you edit a baseline after this date, re-export its patch
(`git -C <repo> diff -- <file> > patches/…`) and refresh `port_files/` so this record stays current.*
