#!/usr/bin/env python3
# gen_model.py — emit a frozen, PRE-TRAINED integer linear classifier for the
# inference benchmark. Deterministic (no RNG) → identical on any box, and shared
# verbatim by every system so the prediction checksum is exactly reproducible.
#
# Model file (one line):  model,C,F,w0,w1,...,w{C*F-1}      (row-major, i64)
#
# The weights are the per-class prototypes the dataset is built around
# (gen_data.py: feature j is "owned" by class j%C, value FEAT_MAX, else FEAT_MAX//4),
# so argmax(Σ w[c]·x) is an accurate nearest-prototype linear classifier on the
# prototype+noise samples — a meaningful "trained" model without needing a real
# training run.
#
# Usage: gen_model.py OUT_PATH
import sys

C = 10
F = 16
FEAT_MAX = 4

out = sys.argv[1]

w = []
for c in range(C):
    for j in range(F):
        w.append(FEAT_MAX if (j % C) == c else FEAT_MAX // 4)

with open(out, "w") as fh:
    fh.write("model," + str(C) + "," + str(F) + "," + ",".join(str(v) for v in w) + "\n")

print(f"wrote model C={C} F={F} ({C*F} weights) → {out}")
