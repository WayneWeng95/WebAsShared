#!/usr/bin/env python3
"""PageSize experiment — PUT & GET latency vs transfer size, one line per page size.

Reads results.csv (produced by run.sh, which sweeps shm_statesync across page
sizes).  Renders a combined PUT | GET latency panel in the StateSync style, with
one curve per page size (4 KiB / 64 KiB / 2 MiB).

By default it plots the COPY engine row (`shm-copy-engine[-<page>]`): PUT is the
write (identical for both engine rows) and the copy GET materializes the payload,
so BOTH operations depend on page size — which is the point of this experiment.
The zero-copy splice GET is page-size agnostic; pass --row zerocopy to see it
(its GET panel is ~flat across every page size, by design).

  ./plot_pagesize.py                       # results.csv -> figs/pagesize_put_get.png
  ./plot_pagesize.py --metric mean
  ./plot_pagesize.py --row zerocopy
"""

import argparse
import csv
import os
from collections import defaultdict

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_CSV = os.path.join(HERE, "results.csv")
DEFAULT_BASELINE = os.path.join(HERE, "..", "Micro-Benchmarks", "StateSync-local", "results.csv")

# Page sizes we expect, in order, with styling.
PAGES = [4096, 65536, 2097152]
LABEL = {4096: "WasMem 4 KiB page", 65536: "WasMem 64 KiB page", 2097152: "WasMem 2 MiB page"}
COLOR = {4096: "#2057c7", 65536: "#e8843c", 2097152: "#8c1d40"}
MARKER = {4096: "o", 65536: "s", 2097152: "^"}

# Comparison group: the modelled /dev/shm zero-copy row from StateSync-local.
COMP = "shm-zerocopy"
COMP_LABEL = "RMMAP (Shared memory zero-copy)"
COMP_COLOR = "#1d7a3e"
COMP_MARKER = "*"

TICK_SIZE, LABEL_SIZE, LEGEND_SIZE, YLABEL_SIZE = 16, 16, 16, 14
plt.rcParams.update({"xtick.labelsize": TICK_SIZE, "ytick.labelsize": TICK_SIZE,
                     "axes.labelsize": LABEL_SIZE, "legend.fontsize": LEGEND_SIZE})


def fmt_size(n):
    n = int(n)
    if n < 1024: return f"{n}B"
    if n < 1024 ** 2: return f"{n // 1024}KiB"
    if n < 1024 ** 3: return f"{n // 1024 ** 2}MiB"
    return f"{n // 1024 ** 3}GiB"


def parse_size(s):
    units = {"B": 1, "KiB": 1024, "MiB": 1024 ** 2, "GiB": 1024 ** 3}
    for u in ("KiB", "MiB", "GiB", "B"):
        if s.endswith(u) and s[: -len(u)].isdigit():
            return int(s[: -len(u)]) * units[u]
    return int(s)


def page_of(approach):
    """shm-copy-engine -> 4096; shm-copy-engine-64KiB -> 65536."""
    tail = approach.rsplit("-", 1)[-1]
    if tail[:1].isdigit() and tail[-1:].isalpha():
        return parse_size(tail)
    return 4096


def load(path, base_name, readers=1):
    """Return {page_bytes: {transfer_size: row}} for the chosen engine row family."""
    data = defaultdict(dict)
    for r in csv.DictReader(open(path)):
        a = r["approach"]
        if not a.startswith(base_name):
            continue
        # exclude the other family (copy- vs zerocopy-) by exact base match
        rest = a[len(base_name):]
        if rest and not rest.startswith("-"):
            continue
        if int(r.get("readers", 1)) != readers:
            continue
        data[page_of(a)][int(r["size_bytes"])] = r
    return data


def style(pb):
    return dict(color=COLOR.get(pb, "#444"), marker=MARKER.get(pb, "o"),
                label=LABEL.get(pb, f"{fmt_size(pb)} page"), linewidth=2.0, markersize=8)


def load_comparison(path, readers=1):
    """Modelled shm-zerocopy rows {transfer_size: row} from StateSync-local."""
    out = {}
    if not os.path.exists(path):
        print(f"[plot] note: baseline {path} not found — skipping comparison group")
        return out
    for r in csv.DictReader(open(path)):
        if r["approach"] == COMP and int(r.get("readers", 1)) == readers:
            out[int(r["size_bytes"])] = r
    return out


def main():
    ap = argparse.ArgumentParser(description="PageSize PUT/GET latency vs transfer size")
    ap.add_argument("--csv", default=DEFAULT_CSV)
    ap.add_argument("--baseline-csv", default=DEFAULT_BASELINE)
    ap.add_argument("--no-baseline", action="store_true",
                    help="omit the modelled shared-memory zero-copy comparison line")
    ap.add_argument("--outdir", default=os.path.normpath(os.path.join(HERE, "..", "Graph")))
    ap.add_argument("--format", default="pdf", choices=["png", "pdf", "svg"])
    ap.add_argument("--metric", choices=["p50", "mean"], default="mean")
    ap.add_argument("--row", choices=["copy", "zerocopy"], default="zerocopy",
                    help="which engine GET path to plot (default copy — page-dependent)")
    ap.add_argument("--figsize", default="9,3")
    args = ap.parse_args()

    base = "shm-copy-engine" if args.row == "copy" else "shm-zerocopy-engine"
    data = load(args.csv, base)
    pages = [p for p in PAGES if p in data] + [p for p in data if p not in PAGES]
    if not pages:
        raise SystemExit(f"[plot] no '{base}' rows found in {args.csv}")
    sizes = sorted({s for p in data for s in data[p]})
    idx = {s: i for i, s in enumerate(sizes)}
    m = args.metric
    stat = "Mean" if m == "mean" else "Median"
    comp = {} if args.no_baseline else load_comparison(args.baseline_csv)

    figsize = tuple(float(x) for x in args.figsize.split(","))
    fig, axes = plt.subplots(1, 2, figsize=figsize)
    for ax, op in zip(axes, ("put", "get")):
        for pb in pages:
            xs = [idx[s] for s in sorted(data[pb])]
            ys = [float(data[pb][s][f"{op}_{m}_us"]) for s in sorted(data[pb])]
            ax.plot(xs, ys, **style(pb))
        # Comparison group: modelled shared-memory zero-copy (dashed).
        if comp:
            cs = sorted(s for s in comp if s in idx)
            if cs:
                ax.plot([idx[s] for s in cs], [float(comp[s][f"{op}_{m}_us"]) for s in cs],
                        color=COMP_COLOR, marker=COMP_MARKER, ls="--", linewidth=2.0,
                        markersize=10, label=COMP_LABEL)
        ax.set_yscale("log")
        ax.set_xticks(range(len(sizes)))
        ax.set_xticklabels([fmt_size(s) for s in sizes], fontsize=TICK_SIZE)
        ax.set_xlabel("transfer size")
        ax.set_ylabel(f"{stat} {op.upper()} latency (µs, log)", fontsize=YLABEL_SIZE)
        ax.grid(True, which="both", ls=":", alpha=0.4)
    fig.tight_layout()
    handles, labels = axes[0].get_legend_handles_labels()
    fig.legend(handles, labels, loc="lower center", ncol=2, fontsize=LEGEND_SIZE,
               frameon=False, bbox_to_anchor=(0.5, 1.0))
    tag = "" if m == "mean" else f"_{m}"   # mean is canonical (no suffix); p50 -> _p50
    rtag = "" if args.row == "copy" else f"_{args.row}"
    out = os.path.join(args.outdir, f"pagesize_put_get{rtag}{tag}.{args.format}")
    os.makedirs(args.outdir, exist_ok=True)
    fig.savefig(out, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {out}  ({base}, {m})")


if __name__ == "__main__":
    main()
