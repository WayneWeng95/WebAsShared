#!/usr/bin/env python3
# gen_data.py — synthetic, learnable, INTEGER classification dataset for the SGD
# benchmark. Frozen shared spec: every system trains on the exact same file.
#
#   line format:  label,f0,f1,...,f{F-1}      (no header)
#   label ∈ 0..C-1 ; features are small ints 0..FEAT_MAX
#
# Each class gets a fixed integer "prototype" vector (seeded, deterministic);
# samples are prototype + bounded integer noise, clamped to [0, FEAT_MAX]. The
# class signal is linearly separable enough that the integer linear classifier
# climbs well above chance (10%), while the small feature range keeps the
# fixed-point gradient arithmetic comfortably inside i64.
#
# Usage: gen_data.py N OUT_PATH [seed]      (N = number of samples)
import sys

C = 10        # classes (matches N_CLASSES in the guest)
F = 16        # features
FEAT_MAX = 4  # feature values in 0..FEAT_MAX

N = int(sys.argv[1])
out = sys.argv[2]
seed = int(sys.argv[3]) if len(sys.argv) > 3 else 1234

# Deterministic LCG (no numpy dependence → identical on any box / Python build).
state = seed & 0xFFFFFFFF
def rnd():
    global state
    state = (1103515245 * state + 12345) & 0x7FFFFFFF
    return state

# Per-class prototypes: class c emphasises a rotating block of features.
prototypes = []
for c in range(C):
    p = [0] * F
    for j in range(F):
        # high value on features "owned" by this class, low elsewhere
        p[j] = FEAT_MAX if (j % C) == c else (FEAT_MAX // 4)
    prototypes.append(p)

with open(out, "w") as fh:
    for i in range(N):
        c = rnd() % C
        proto = prototypes[c]
        feats = []
        for j in range(F):
            noise = (rnd() % 3) - 1          # -1, 0, +1
            v = proto[j] + noise
            if v < 0: v = 0
            if v > FEAT_MAX: v = FEAT_MAX
            feats.append(v)
        fh.write(str(c) + "," + ",".join(str(v) for v in feats) + "\n")

print(f"wrote {N} samples, C={C} F={F} feat_max={FEAT_MAX} → {out}")
