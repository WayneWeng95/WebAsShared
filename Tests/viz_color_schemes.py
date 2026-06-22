#!/usr/bin/env python3
"""Render the current Tests/ plot palettes as color swatches.

Produces one figure:
  PLOT_COLOR_SCHEMES_by_group.png  — every test group, its labels + colors
"""
import matplotlib.pyplot as plt
from matplotlib.patches import Rectangle

# ── PROPOSED UNIFIED PALETTE (Proposal A: muted wave + WasMem pop) ─────────────
# One saturated hero blue for our system; everything else a low-saturation wave.
POP        = "#3a5fc4"   # WasMem / ours — the pop (high saturation)
POP_LIGHT  = "#8aa8f0"   # secondary WasMem variant (sync / balanced / 4KiB)
POP_MID    = "#4f74e8"   # WasMem mid (64 KiB)
POP_DARK   = "#1e3db0"   # WasMem dark (2 MiB)
TEAL       = "#6fa8a0"   # Faasm / shm-copy (copy-based)
SAGE       = "#86b07d"   # RMMap / zero-copy baselines (shm-zerocopy, rdma-shm)
GOLD       = "#e0be72"   # Cloudburst / redis-local (warm)
GOLD_GREY  = "#cbb78c"   # cloudburst-cold (cool/greyed gold)
ORANGE     = "#d69a60"   # redis-remote / Flink StateFun
RED        = "#c97b7b"   # s3
MAROON     = "#9d5a60"   # s3-disk
SIENNA     = "#a17a52"   # Roadrunner (rr-embedded) — brown, kept clear of RTSFaaS
MAUVE      = "#b07ba6"   # RTSFaaS (RDMA shared memory) — own hue, ≠ Faasm teal & ≠ Roadrunner

# One panel per ACTUAL paper PDF (the 9 figures the user keeps).
# (PDF filename, source script, graph type, [(label, hex, marker), ...]).
# Unified markers: ours=o, copy/Faasm=D, RMMap/zerocopy=*, s3=s, s3-disk/RR=P,
# redis-remote=^, redis-local/cloudburst-warm=v, cloudburst-cold=X.
GROUPS = [
    ("latency_put_get.pdf",
     "src: Micro-Benchmarks/StateSync-local/plot.py",
     "line panels (Read|Write)", [
        ("s3-disk", MAROON, "P"), ("s3", RED, "s"), ("redis-remote", ORANGE, "^"),
        ("redis-local", GOLD, "v"), ("shm-copy", TEAL, "D"), ("shm-zerocopy (ours)", POP, "o"),
    ]),
    ("compare_put_get_mean.pdf",
     "src: StateSync/plot_compare.py",
     "line panels (Read|Write)", [
        ("s3", RED, "s"), ("redis-local", GOLD, "v"), ("rr-embedded (Roadrunner)", SIENNA, "P"),
        ("shm-copy", TEAL, "D"), ("shm-zerocopy", SAGE, "*"), ("shm-zerocopy-engine (ours)", POP, "o"),
    ]),
    ("latency_throughput_remote.pdf",
     "src: Micro-Benchmarks/StateSync-remote/plot_remote.py",
     "broken-axis latency + throughput", [
        ("s3-disk", MAROON, "P"), ("s3", RED, "s"), ("redis-remote", ORANGE, "^"),
        ("cloudburst-cold", GOLD_GREY, "X"), ("cloudburst-warm", GOLD, "v"), ("rdma-shm = RTSFaaS", MAUVE, "D"),
    ]),
    ("remote_redis_vs_rdma.pdf",
     "src: StateSync/plot_remote_compare.py",
     "broken-axis latency + throughput", [
        ("redis-remote", ORANGE, "^"), ("cloudburst-warm", GOLD, "v"),
        ("rdma-shm = RTSFaaS", MAUVE, "D"), ("rdma-shm-ours = WasMem", POP, "o"),
    ]),
    ("pagesize_put_get_zerocopy.pdf",
     "src: PageSize/plot_pagesize.py",
     "line panels (Read|Write) — WasMem blue ramp", [
        ("WasMem 4 KiB", POP_DARK, "o"), ("WasMem 64 KiB", POP_MID, "o"),
        ("WasMem 2 MiB", POP_LIGHT, "o"), ("RMMAP zero-copy (dashed)", SAGE, "*"),
    ]),
    ("policy_grid.pdf",
     "src: Scheduling_Policy/analysis/plot_policy_grid.py",
     "grouped bars w/ makespan shadow (bars only)", [
        ("pack", POP_LIGHT, None), ("balanced", POP, None),
    ]),
    ("all_workloads_bars.pdf",
     "src: Intra-Node .../analysis/plot_bars_grid.py",
     "3x6 grouped-bar grid (bars only)", [
        ("WasMem", POP, None), ("Faasm", TEAL, None),
        ("RMMap", SAGE, None), ("Cloudburst", GOLD, None),
    ]),
    ("mem_largest_load.pdf",
     "src: Intra-Node .../analysis/plot_mem_largest.py",
     "mem-saving grouped bars (bars only)", [
        ("WasMem (AOT)", POP, None), ("RMMap", SAGE, None),
        ("Faasm", TEAL, None), ("Cloudburst", GOLD, None),
    ]),
    ("streaming_overlay.pdf",
     "src: Streaming Application_Benchmark/intra-node/plot.py",
     "broken-axis grouped bars (bars only)", [
        ("Flink StateFun", ORANGE, None), ("RTSFaaS", MAUVE, None),
        ("WasMem (sync)", POP_LIGHT, None), ("WasMem (async)", POP, None),
    ]),
]


def text_color(hexc):
    """Black or white label depending on swatch luminance."""
    r, g, b = (int(hexc[i:i+2], 16) for i in (1, 3, 5))
    return "white" if (0.299*r + 0.587*g + 0.114*b) < 140 else "black"


# ── Figure 1: by group ────────────────────────────────────────────────────────
ncol = 3
nrow = -(-len(GROUPS) // ncol)
fig, axes = plt.subplots(nrow, ncol, figsize=(16, 3.6 * nrow))
axes = axes.ravel()
SW_H = 1.0
HEADER = 1.7   # vertical room reserved above the swatches for output/type lines
for ax, (title, out, gtype, items) in zip(axes, GROUPS):
    n = len(items)
    top = n + HEADER
    ax.set_title(title, fontsize=11, fontweight="bold", loc="left")
    # Output file(s) + graph type sit just under the title, above the swatches.
    ax.text(0, n + 0.9, "→ " + out, ha="left", va="center",
            fontsize=7.5, color="#1a5", family="monospace")
    ax.text(0, n + 0.35, "type: " + gtype, ha="left", va="center",
            fontsize=8, style="italic", color="#555")
    for i, (label, hexc, marker) in enumerate(items):
        y = n - 1 - i
        # Swatch (the legend block) carries the label name + hex inside it.
        ax.add_patch(Rectangle((0, y), 3.2, SW_H, facecolor=hexc, edgecolor="#222", lw=0.5))
        tc = text_color(hexc)
        ax.text(0.12, y + 0.5, label, ha="left", va="center",
                fontsize=8.5, fontweight="bold", color=tc)
        ax.text(3.08, y + 0.5, hexc, ha="right", va="center",
                fontsize=7, color=tc, family="monospace", alpha=0.9)
        # Marker glyph used in the line charts (in the swatch's own color), drawn
        # just right of the swatch; bar-only plots show a dash instead.
        if marker:
            ax.scatter(3.55, y + 0.5, marker=marker, s=90, c=hexc,
                       edgecolors="black", linewidths=0.6, zorder=5)
        else:
            ax.text(3.55, y + 0.5, "—", ha="center", va="center",
                    fontsize=10, color="#999")
        # Also repeat the name to the right of the marker for quick scanning.
        ax.text(3.85, y + 0.5, label, ha="left", va="center", fontsize=9)
    ax.set_xlim(0, 8.5)
    ax.set_ylim(0, top)
    ax.axis("off")
for ax in axes[len(GROUPS):]:
    ax.axis("off")
fig.suptitle("PROPOSED unified palette — muted wave + WasMem pop (early look, 9 PDFs)",
             fontsize=15, fontweight="bold")
fig.tight_layout(rect=[0, 0, 1, 0.97])
fig.savefig("/home/weikang/WebAsShared/Tests/PLOT_COLOR_SCHEMES_by_group.png", dpi=130, bbox_inches="tight")
plt.close(fig)

print("wrote PLOT_COLOR_SCHEMES_by_group.png")
