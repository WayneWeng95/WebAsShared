#!/usr/bin/env python3
"""Plot the HARNESS-ONLY page-size sweep: engine PUT write bandwidth vs page size.

The real engine uses 4 KiB pages.  shm_statesync's --page-bytes lets us rewrite
the SAME page-chain PUT with larger pages (the engine itself is untouched) to show
that the PUT gap to a flat contiguous memcpy is purely a page/chunk-size effect —
PUT climbs toward the flat baseline as pages grow, while the zero-copy GET splice
stays flat (page-size agnostic).

Reads results_pagesweep.csv (rows whose approach is shm-zerocopy-engine[-<page>]),
plus the modelled flat-memcpy PUT from StateSync-local/results.csv as the baseline.

  ./plot_pagesweep.py                       # results_pagesweep.csv -> figs/put_page_sweep.png
  ./plot_pagesweep.py --size 134217728
"""

import argparse
import csv
import os

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_SWEEP = os.path.join(HERE, "results.csv")
DEFAULT_BASELINE = os.path.join(HERE, "..", "Micro-Benchmarks", "StateSync-local", "results.csv")

TICK_SIZE, LABEL_SIZE, LEGEND_SIZE = 16, 16, 15
plt.rcParams.update({"xtick.labelsize": TICK_SIZE, "ytick.labelsize": TICK_SIZE,
                     "axes.labelsize": LABEL_SIZE, "legend.fontsize": LEGEND_SIZE})


def parse_size(s):
    s = s.strip()
    units = {"B": 1, "KiB": 1024, "MiB": 1024 ** 2, "GiB": 1024 ** 3}
    for u in ("KiB", "MiB", "GiB", "B"):
        if s.endswith(u) and s[: -len(u)].isdigit():
            return int(s[: -len(u)]) * units[u]
    return int(s)


def fmt_size(n):
    n = int(n)
    if n < 1024: return f"{n}B"
    if n < 1024 ** 2: return f"{n // 1024}KiB"
    if n < 1024 ** 3: return f"{n // 1024 ** 2}MiB"
    return f"{n // 1024 ** 3}GiB"


def page_of(approach):
    """shm-zerocopy-engine -> 4096; shm-zerocopy-engine-64KiB -> 65536."""
    tail = approach.rsplit("-", 1)[-1]
    if tail[:1].isdigit() and tail[-1:].isalpha():
        return parse_size(tail)
    return 4096


def load_sweep(path, size, row_prefix="shm-zerocopy-engine"):
    pts = []
    for r in csv.DictReader(open(path)):
        if int(r["size_bytes"]) != size:
            continue
        if not r["approach"].startswith(row_prefix):
            continue
        pts.append((page_of(r["approach"]), float(r["put_gibps"])))
    pts.sort()
    return pts


def flat_baseline(path, size):
    """Modelled flat-memcpy PUT bandwidth (shm-zerocopy row) at `size`, GiB/s."""
    if not os.path.exists(path):
        return None
    for r in csv.DictReader(open(path)):
        if r["approach"] == "shm-zerocopy" and int(r["size_bytes"]) == size:
            return float(r["put_gibps"])
    return None


def main():
    ap = argparse.ArgumentParser(description="Plot engine PUT bandwidth vs page size")
    ap.add_argument("--csv", default=DEFAULT_SWEEP)
    ap.add_argument("--baseline-csv", default=DEFAULT_BASELINE)
    ap.add_argument("--size", type=int, default=128 * 1024 * 1024)
    ap.add_argument("--outdir", default=os.path.normpath(os.path.join(HERE, "..", "Figures")))
    ap.add_argument("--format", default="pdf", choices=["png", "pdf", "svg"])
    ap.add_argument("--figsize", default="6.5,4")
    args = ap.parse_args()

    pts = load_sweep(args.csv, args.size)
    if not pts:
        raise SystemExit(f"[plot] no shm-zerocopy-engine rows for size {args.size} in {args.csv}")
    flat = flat_baseline(args.baseline_csv, args.size)

    xs = list(range(len(pts)))
    pages = [p for p, _ in pts]
    gibps = [g for _, g in pts]

    fig, ax = plt.subplots(figsize=tuple(float(x) for x in args.figsize.split(",")))
    ax.plot(xs, gibps, color="#2057c7", marker="o", markersize=9, linewidth=2.2,
            label="Engine page-chain PUT")
    # Annotate the engine's real page size (4 KiB).
    if 4096 in pages:
        i = pages.index(4096)
        ax.annotate("engine\n(4 KiB)", (xs[i], gibps[i]), textcoords="offset points",
                    xytext=(8, 10), fontsize=12, color="#2057c7")
    if flat:
        ax.axhline(flat, color="#1d7a3e", ls="--", linewidth=2.0,
                   label=f"Flat memcpy (model) ≈ {flat:.1f} GiB/s")

    ax.set_xticks(xs)
    ax.set_xticklabels([fmt_size(p) for p in pages], fontsize=TICK_SIZE)
    ax.set_xlabel("page size (harness experiment)")
    ax.set_ylabel("PUT bandwidth (GiB/s)")
    ax.set_ylim(bottom=0)
    ax.grid(True, axis="y", ls=":", alpha=0.4)
    ax.legend(frameon=False, loc="lower right")
    ax.set_title(f"PUT @ {fmt_size(args.size)} — bigger pages recover the flat-copy gap",
                 fontsize=13)
    fig.tight_layout()
    out = os.path.join(args.outdir, f"put_page_sweep.{args.format}")
    os.makedirs(args.outdir, exist_ok=True)
    fig.savefig(out, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {out}")

    print(f"\nEngine PUT bandwidth vs page size @ {fmt_size(args.size)}:")
    for p, g in pts:
        print(f"  {fmt_size(p):>8}: {g:6.2f} GiB/s")
    if flat:
        print(f"  flat memcpy (model): {flat:.2f} GiB/s")


if __name__ == "__main__":
    main()
