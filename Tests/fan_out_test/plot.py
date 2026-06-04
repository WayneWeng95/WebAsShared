#!/usr/bin/env python3
"""Plot the map fan-out sweep (results_*.csv) — wall time vs fan-out per corpus.

Reads every results_*.csv in this test dir (one series per corpus size) and
renders a single figure: median wall time vs map fan-out. Non-numeric rows
(e.g. a `CRASH` at low fan-out on a large corpus — wc_map OOMs the guest heap)
are drawn as a red x marker on the baseline so the failure boundary is visible.

Default output: PDF into Tests/Graph/.

Usage:
  ./plot.py                                  # all results_*.csv -> Tests/Graph/fan_out_sweep.pdf
  ./plot.py --format png                     # PNG instead
  ./plot.py --outdir /some/dir --csv a.csv b.csv
"""
import argparse, csv, glob, os
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
GRAPH_DIR = os.path.normpath(os.path.join(HERE, "..", "Graph"))

TICK, LABEL, LEG = 15, 15, 14
plt.rcParams.update({"xtick.labelsize": TICK, "ytick.labelsize": TICK,
                     "axes.labelsize": LABEL, "legend.fontsize": LEG})

COLORS = ["#2057c7", "#8c1d40", "#1d7a3a", "#b8860b", "#5a3d8c"]
MARKERS = ["o", "s", "^", "D", "v"]


def human_size(nbytes):
    n = float(nbytes)
    for unit in ("B", "KiB", "MiB", "GiB"):
        if n < 1024 or unit == "GiB":
            return f"{n:.0f} {unit}" if n >= 10 or unit == "B" else f"{n:.1f} {unit}"
        n /= 1024


def load_series(path):
    """Return (label, [(fanout, wall_ms or None)]) sorted by fan-out."""
    rows = list(csv.DictReader(open(path)))
    if not rows:
        return None
    label = human_size(rows[0]["corpus_bytes"])
    pts = []
    for r in rows:
        f = int(r["fanout"])
        raw = r["wall_ms_median"].strip()
        try:
            pts.append((f, float(raw)))
        except ValueError:
            pts.append((f, None))  # CRASH / non-numeric
    pts.sort(key=lambda p: p[0])
    return label, pts


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--csv", nargs="*", default=None,
                    help="explicit CSV files (default: all results_*.csv in this dir)")
    ap.add_argument("--outdir", default=GRAPH_DIR)
    ap.add_argument("--format", default="pdf", choices=["pdf", "png", "svg"])
    ap.add_argument("--name", default="fan_out_sweep")
    args = ap.parse_args()

    paths = args.csv or sorted(glob.glob(os.path.join(HERE, "results_*.csv")))
    if not paths:
        raise SystemExit("no results_*.csv found — run ./run.sh first")

    series = [s for s in (load_series(p) for p in paths) if s]
    if not series:
        raise SystemExit("no data rows in any results CSV")

    # Union of all fan-out values → categorical x axis (even spacing).
    all_fanouts = sorted({f for _, pts in series for f, _ in pts})
    xpos = {f: i for i, f in enumerate(all_fanouts)}

    fig, ax = plt.subplots(figsize=(7, 4.2))
    for i, (label, pts) in enumerate(series):
        c, m = COLORS[i % len(COLORS)], MARKERS[i % len(MARKERS)]
        gx = [xpos[f] for f, w in pts if w is not None]
        gy = [w for _, w in pts if w is not None]
        ax.plot(gx, gy, marker=m, lw=2, color=c, label=label)
        # Mark crashed fan-outs.
        cx = [xpos[f] for f, w in pts if w is None]
        if cx:
            ax.scatter(cx, [0] * len(cx), marker="x", s=80, color="red", zorder=5)

    ax.set_xticks(list(xpos.values()))
    ax.set_xticklabels([str(f) for f in all_fanouts])
    ax.set_xlabel("map fan-out (parallel worker processes)")
    ax.set_ylabel("wall time (ms, median)")
    ax.set_ylim(bottom=0)
    ax.grid(True, axis="y", ls=":", alpha=0.4)
    ax.legend(frameon=False, loc="upper left", title="corpus")
    ax.set_title("Map fan-out sweep — wall time vs worker count\n"
                 "(red x = crash: low fan-out OOMs the guest heap)", fontsize=11)
    fig.tight_layout()

    os.makedirs(args.outdir, exist_ok=True)
    out = os.path.join(args.outdir, f"{args.name}.{args.format}")
    fig.savefig(out, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {out}")
    for label, pts in series:
        cells = ", ".join(f"{f}:{'CRASH' if w is None else int(w)}" for f, w in pts)
        print(f"  {label}: {cells}")


if __name__ == "__main__":
    main()
