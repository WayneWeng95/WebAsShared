# sgd_core.py — the EXACT integer SGD shared by the Cloudburst + RMMap baselines.
#
# Reproduces, bit-for-bit, the WebAsShared guest kernel
# (Executor/guest/src/workloads/ml_training.rs, the sgd_* functions): a
# multi-class linear classifier trained by full-batch gradient descent on a
# least-squares one-hot objective, in pure int64 arithmetic. Because every step
# is integer (associative sum + ONE central toward-zero division), all systems —
# Rust-WASM, numpy, whatever — land on the IDENTICAL final weight checksum, which
# is the cross-system correctness gate (EXPERIMENT_RUNBOOK §1/§5.2).
#
# Frozen constants — MUST equal the guest's:
#   N_CLASSES = 10 ; SGD_TARGET = 1024 ; SGD_LR_K = 16 ; lr_den = n_samples*LR_K
import numpy as np

N_CLASSES = 10
SGD_TARGET = 1024
SGD_LR_K = 16


def load_csv(path):
    """Load 'label,f0,..' integer CSV → (X int64 [N,F], y int64 [N], F)."""
    rows = []
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line or line.startswith('label'):
                continue
            rows.append([int(v) for v in line.split(',')])
    arr = np.asarray(rows, dtype=np.int64)
    y = arr[:, 0].copy()
    X = arr[:, 1:].copy()
    return X, y, X.shape[1]


def trunc_div(a, d):
    """Element-wise toward-zero integer division (matches Rust i64 `/`)."""
    if d == 0:
        return np.zeros_like(a)
    return np.sign(a) * (np.abs(a) // d)


def grad_sum(X, y, W):
    """Integer gradient SUM over the given samples for current weights W.
    Returns gsum [C,F] int64. Summing grad_sum over disjoint shards == grad_sum
    over their union (integer add is associative) → fan-out invariance."""
    C, F = W.shape
    pred = X @ W.T                       # [n, C] int64
    T = np.zeros((X.shape[0], C), dtype=np.int64)
    T[np.arange(X.shape[0]), y] = SGD_TARGET
    err = pred - T                       # [n, C] int64
    return err.T @ X                     # [C, F] int64


def apply_update(W, gsum_total, lr_den):
    """One central toward-zero step on the aggregated gradient sum."""
    return W - trunc_div(gsum_total, lr_den)


def init_weights(F):
    return np.zeros((N_CLASSES, F), dtype=np.int64)


def checksum(W):
    return int(W.sum())


def accuracy(W, X, y):
    pred = X @ W.T
    return int((pred.argmax(axis=1) == y).sum()), X.shape[0]
