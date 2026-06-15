# infer_core.py — the EXACT integer forward pass shared by the Cloudburst +
# RMMap baselines. Reproduces, bit-for-bit, the WebAsShared guest
# (Executor/guest/src/workloads/ml_inference.rs): score_c = Σ_j w[c][j]·x[j]
# (i64), predict = argmax (first max wins). Pure integer → the prediction
# checksum is identical across every system and every W (the gate).
import numpy as np


def load_csv(path):
    rows = []
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line or line.startswith('label'):
                continue
            rows.append([int(v) for v in line.split(',')])
    arr = np.asarray(rows, dtype=np.int64)
    return arr[:, 1:].copy(), arr[:, 0].copy()


def load_model(path):
    """Parse 'model,C,F,w…' → (C, F, W[C,F] int64)."""
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if line.startswith('model,'):
                t = line.split(',')
                c, f = int(t[1]), int(t[2])
                w = np.asarray([int(v) for v in t[3:3 + c * f]], dtype=np.int64)
                return c, f, w.reshape(c, f)
    raise ValueError('no model record in ' + path)


def predict(X, W):
    """argmax of integer class scores. numpy argmax returns the FIRST max on
    ties, matching the guest's strict-`>` first-max scan."""
    return (X @ W.T).argmax(axis=1)        # [n] int64


def evaluate(X, y, W):
    """Returns (correct, total, prediction_checksum) — the gate quantities."""
    pred = predict(X, W)
    return int((pred == y).sum()), int(X.shape[0]), int(pred.sum())
