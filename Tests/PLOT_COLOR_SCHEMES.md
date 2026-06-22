# Plot Label & Color Schemes — Unified Palette

The single source of truth for colors, labels, and markers across the **10 paper
figures** under `Tests/`. Design goal: **mute the baselines into one harmonious
low-saturation "wave," and give the hero (WasMem) the only high-saturation color
so it pops.** Markers are aligned by role across every line chart.

> Companion visualization: `PLOT_COLOR_SCHEMES_by_group.png` (rendered by
> `viz_color_schemes.py`). Generated figures are collected in `Tests/Figures/`.

---

## The unified palette (by semantic role)

| Role | Hex | Swatch idea | Marker (line charts) |
|---|---|---|---|
| **WasMem / ours — POP** | `#3a5fc4` | saturated blue | `o` |
| WasMem light (sync / balanced / largest page) | `#8aa8f0` | light blue | `o` |
| WasMem mid (64 KiB) | `#4f74e8` | mid blue | `o` |
| WasMem dark (4 KiB) | `#1e3db0` | dark blue | `o` |
| Faasm / shm-copy (copy-based) | `#6fa8a0` | muted teal | `D` |
| RMMap / zero-copy baselines (shm-zerocopy, rdma-shm) | `#86b07d` | muted sage | `*` |
| Cloudburst / redis-local (warm) | `#e0be72` | muted gold | `v` |
| cloudburst-cold (greyed gold) | `#cbb78c` | gold-grey | `X` |
| redis-remote / Flink StateFun | `#d69a60` | muted orange | `^` |
| s3 | `#c97b7b` | muted red | `s` |
| s3-disk | `#9d5a60` | maroon | `P` |
| Roadrunner (rr-embedded) | `#a17a52` | brown | `P` |
| RTSFaaS (RDMA shared memory) | `#b07ba6` | mauve | `D` |

**Why it works:** WasMem is the *only* high-saturation color; every baseline sits
at ~40–55% saturation, so the eye lands on WasMem first. Saturation does the
popping, not lightness.

**Marker rule (aligned across figures):** ours = `o`, Faasm/copy = `D`,
RMMap/zerocopy = `*`, s3 = `s`, s3-disk / Roadrunner = `P`, redis-remote = `^`,
redis-local / cloudburst-warm = `v`, cloudburst-cold = `X`, RTSFaaS = `D`.
(`P`/`D` are reused only across figures that never co-occur.)

---

## Per-figure mapping (the 10 PDFs)

### 1. `latency_put_get.pdf` — `Micro-Benchmarks/StateSync-local/plot.py`
Line panels (Read|Write). `shm-zerocopy` is *ours* here.
| Label | Color | Marker |
|---|---|---|
| s3-disk | `#9d5a60` | `P` |
| s3 | `#c97b7b` | `s` |
| redis-remote | `#d69a60` | `^` |
| redis-local | `#e0be72` | `v` |
| shm-copy | `#6fa8a0` | `D` |
| **shm-zerocopy (ours)** | `#3a5fc4` | `o` |

### 2. `compare_put_get_mean.pdf` — `StateSync/plot_compare.py`
| Label | Color | Marker |
|---|---|---|
| s3 | `#c97b7b` | `s` |
| redis-local | `#e0be72` | `v` |
| rr-embedded (Roadrunner) | `#a17a52` | `P` |
| shm-copy | `#6fa8a0` | `D` |
| shm-zerocopy (baseline) | `#86b07d` | `*` |
| **shm-zerocopy-engine (ours)** | `#3a5fc4` | `o` |

### 3. `latency_throughput_remote.pdf` — `Micro-Benchmarks/StateSync-remote/plot_remote.py`
Broken-axis latency + throughput. **No WasMem series** in this figure; `rdma-shm`
is RTSFaaS. (`s3-disk` and `cloudburst-cold` are commented out in the script, so
the live PDF shows the 4 active rows.)
| Label | Color | Marker |
|---|---|---|
| s3-disk *(disabled)* | `#9d5a60` | `P` |
| s3 | `#c97b7b` | `s` |
| redis-remote | `#d69a60` | `^` |
| cloudburst-cold *(disabled)* | `#cbb78c` | `X` |
| cloudburst-warm | `#e0be72` | `v` |
| rdma-shm = **RTSFaaS** | `#b07ba6` | `D` |

### 4. `remote_redis_vs_rdma.pdf` — `StateSync/plot_remote_compare.py`
The one figure where RTSFaaS and WasMem RDMA sit side by side.
| Label | Color | Marker |
|---|---|---|
| redis-remote | `#d69a60` | `^` |
| cloudburst-warm | `#e0be72` | `v` |
| rdma-shm = **RTSFaaS** | `#b07ba6` | `D` |
| **rdma-shm-ours = WasMem** | `#3a5fc4` | `o` |

### 5. `pagesize_put_get_zerocopy.pdf` — `PageSize/plot_pagesize.py`
WasMem blue ramp — **darkest = smallest page**.
| Label | Color | Marker |
|---|---|---|
| WasMem 4 KiB | `#1e3db0` | `o` |
| WasMem 64 KiB | `#4f74e8` | `o` |
| WasMem 2 MiB | `#8aa8f0` | `o` |
| RMMAP zero-copy (dashed) | `#86b07d` | `*` |

### 6. `policy_grid.pdf` — `Scheduling_Policy/analysis/plot_policy_grid.py`
Grouped bars (both are our scheduler). Opaque two-tone bars: saturated base
(policy color) = makespan, lighter tint above (`lighten(policy color)`) = total
execution time. Makespan value labels are **white** on both policies. The legend
sits in a slim band **above** the plots: the shade explanation on top ("full bar
= total execution time · saturated base = makespan") with the pack/balanced
swatches (saturated policy color = makespan) in a row beneath it.
| Label | Color |
|---|---|
| pack | `#8aa8f0` |
| balanced | `#3a5fc4` |

### 7. `all_workloads_bars.pdf` — `Intra-Node Application_Benchmark/analysis/plot_bars_grid.py`
3×6 grouped-bar grid. Draw/legend order: Cloudburst → RMMap → Faasm → WasMem.
| Label | Color |
|---|---|
| **WasMem** | `#3a5fc4` |
| Faasm | `#6fa8a0` |
| RMMap | `#86b07d` |
| Cloudburst | `#e0be72` |

### 8. `mem_largest_load.pdf` — `Intra-Node Application_Benchmark/analysis/plot_mem_largest.py`
| Label | Color |
|---|---|
| **WasMem (AOT)** | `#3a5fc4` |
| RMMap | `#86b07d` |
| Faasm | `#6fa8a0` |
| Cloudburst | `#e0be72` |

### 9. `streaming_overlay.pdf` — `Streaming Application_Benchmark/intra-node/plot.py`
Broken-axis grouped bars. RTSFaaS is mauve (distinct from Faasm's teal).
| Label | Color |
|---|---|
| Flink StateFun | `#d69a60` |
| RTSFaaS | `#b07ba6` |
| WasMem (sync) | `#8aa8f0` |
| **WasMem (async)** | `#3a5fc4` |

### 10. `inter_node_bars.pdf` — `Inter-Node Application_Benchmark/analysis/plot_inter_bars.py`
Single panel, two-level broken linear y-axis (lower `0–70 s`, upper `350–600 s`).
Six workload groups, two two-tone bars each: full bar = total execution time
(lighter `lighten()` tint), saturated base = makespan. Largest input per workload;
the size sits under each group (bold workload name + normal-weight size line, like
`mem_largest_load.pdf`). Faasm/Finra shows a makespan-only bar (no `total_job`
column in `faasm/results_finra.csv` yet).
| Label | Color |
|---|---|
| Faasm | `#6fa8a0` |
| **WasMem (ours)** | `#3a5fc4` |

---

## Output locations

The shared `Tests/Graph/` folder was removed. The five StateSync/PageSize line
scripts now write **directly** into `Tests/Figures/`; the bar/scheduling/streaming
scripts write to their own local `figs/` and a copy is collected in `Tests/Figures/`.

| Figure | Written to |
|---|---|
| latency_put_get / compare_put_get_mean / latency_throughput_remote / remote_redis_vs_rdma / pagesize_put_get_zerocopy | `Tests/Figures/` (default `--outdir`) |
| policy_grid | `Scheduling_Policy/analysis/figs/` → copied to `Tests/Figures/` |
| all_workloads_bars / mem_largest_load | `Intra-Node .../analysis/figs/` → copied to `Tests/Figures/` |
| streaming_overlay | `Streaming .../intra-node/figs/` → copied to `Tests/Figures/` |
| inter_node_bars | `Inter-Node .../analysis/figs/` → copied to `Tests/Figures/` |

Regenerate all 10 with their plot scripts (most default to `--format pdf`).

---

## Notes / known nuances

- **`rdma-shm` is RTSFaaS, not ours.** In `plot_remote_compare.py`, `rdma-shm-ours`
  is the WasMem variant; keep the two visually split (mauve `D` vs blue `o`).
- **RTSFaaS (mauve) vs Roadrunner (brown)** are deliberately different hues even
  though they never share a figure.
- The per-workload app-benchmark plots (`WordCount/plot.py`, etc.) still carry the
  older vivid 4-system palette; they are **not** among the 9 paper figures (the
  combined `all_workloads_bars.pdf` replaces them). Update them too if they ever
  re-enter the paper.
