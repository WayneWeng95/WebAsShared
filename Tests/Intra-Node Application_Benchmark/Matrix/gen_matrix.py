#!/usr/bin/env python3
# gen_matrix.py — generate the shared matrix-multiply inputs for the cross-system
# benchmark. Both A and B are N×N with **small integer entries** (0..9, seeded),
# stored as raw little-endian float64 (row-major). Integer entries keep every
# product exact (< 2^53), so a checksum of C = A·B matches bit-for-bit across all
# four systems — the correctness gate (same determinism trick as the
# all-lowercase WordCount corpus).
#
# Usage: gen_matrix.py N OUT_A.bin OUT_B.bin [seed]
#
# Prints the expected checksum = int(sum(A·B)). Computed in O(N^2) via
#   sum(A@B) = sum_k (sum_i A[i,k]) * (sum_j B[k,j]) = colsum(A) . rowsum(B)
# so we never materialize the full product just to gate it.
import sys
import numpy as np

N = int(sys.argv[1])
out_a = sys.argv[2]
out_b = sys.argv[3]
seed = int(sys.argv[4]) if len(sys.argv) > 4 else 12345

rng = np.random.default_rng(seed)
# Distinct seeds per matrix so A != B; entries 0..9 (inclusive).
A = rng.integers(0, 10, size=(N, N), dtype=np.int64)
B = np.random.default_rng(seed + 1).integers(0, 10, size=(N, N), dtype=np.int64)

# Store as float64 (the wire/SHM format the guest and baselines read).
A.astype(np.float64).tofile(out_a)
B.astype(np.float64).tofile(out_b)

checksum = int(A.sum(axis=0) @ B.sum(axis=1))  # == int((A @ B).sum())
print(f"N={N} seed={seed} bytes_each={N*N*8} checksum={checksum}")
