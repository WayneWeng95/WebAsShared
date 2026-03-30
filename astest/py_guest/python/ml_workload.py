"""ML Training workload: image classification pipeline.

Modeled after the ML training serverless workflow from RMMAP (EuroSys'24).
The original uses MNIST + PCA + LightGBM random forest.  We implement a
pure-Python equivalent (no numpy/sklearn) that preserves the same DAG shape:

  1. ml_partition       — split dataset into N shards
  2. ml_pca × 2         — feature extraction via power-iteration PCA
  3. ml_train × 8       — train decision stumps on PCA features
  4. ml_validate        — ensemble stumps, compute accuracy

Dataset: mnist_features.csv  (10K rows, 28 features, label 0-9)
Record format through the pipeline:
  Raw:      "label,f0,f1,...,f27"
  Post-PCA: "label,pc0,pc1,...,pc7"   (projected to 8 principal components)
  Stump:    "stump:feat_idx,threshold,left_label,right_label"
"""

import shm
import struct

# ── Helpers ───────────────────────────────────────────────────────────────────

def _parse_dataset(records):
    """Parse CSV records into (labels, features) where features is list of list[int]."""
    labels = []
    features = []
    for _origin, rec in records:
        s = rec.decode('utf-8', errors='replace').strip()
        if not s or s.startswith('label'):
            continue
        parts = s.split(',')
        if len(parts) < 2:
            continue
        labels.append(int(parts[0]))
        features.append([int(x) for x in parts[1:]])
    return labels, features


def _dot(a, b):
    """Dot product of two lists."""
    return sum(x * y for x, y in zip(a, b))


def _vec_sub(a, b):
    return [x - y for x, y in zip(a, b)]


def _vec_scale(a, s):
    return [x * s for x in a]


def _vec_norm(a):
    s = sum(x * x for x in a)
    if s == 0:
        return 1.0
    # Integer sqrt approximation
    r = s
    for _ in range(30):
        r = (r + s / max(r, 1)) / 2
        if r * r <= s < (r + 1) * (r + 1):
            break
    return max(float(r), 1.0)


# ── Stage 1: Partition ────────────────────────────────────────────────────────

def ml_partition(n_shards, base_slot):
    """Read dataset from I/O slot 0, distribute rows round-robin to
    stream slots base_slot .. base_slot + n_shards - 1."""
    records = shm.read_all_inputs()
    idx = 0
    for _origin, rec in records:
        s = rec.decode('utf-8', errors='replace').strip()
        if not s or s.startswith('label'):
            continue
        shm.append_stream_data(base_slot + (idx % n_shards), s.encode())
        idx += 1


def ml_redistribute(in_stream_slot, n_shards):
    """Read PCA-projected records from stream slot `in_stream_slot`,
    distribute round-robin to stream slots n_shards .. n_shards + n_shards - 1.
    Note: `n_shards` doubles as both the count and the base_slot for simplicity
    with the 2-arg PyFunc interface.  Use base_slot = n_shards (e.g., 8 → slots 8..15
    won't work).  Instead we use a fixed base of 30 and pass n_shards as arg."""
    # For the DAG, arg=in_stream_slot, arg2=n_shards.  Base slot is fixed at 30.
    base = 30
    records = shm.read_all_stream_records(in_stream_slot)
    for idx, (_origin, rec) in enumerate(records):
        shm.append_stream_data(base + (idx % n_shards), rec)


# ── Stage 2: PCA (power iteration) ───────────────────────────────────────────

N_COMPONENTS = 8

def ml_pca(in_slot, out_slot):
    """Compute PCA on records in stream slot `in_slot` using power iteration.
    Projects features to `N_COMPONENTS` dimensions.
    Writes projected records to stream slot `out_slot`."""
    labels, features = _parse_dataset(shm.read_all_stream_records(in_slot))
    if not features:
        return

    n_samples = len(features)
    n_feat = len(features[0])

    # Compute mean
    mean = [0.0] * n_feat
    for row in features:
        for j in range(n_feat):
            mean[j] += row[j]
    mean = [m / n_samples for m in mean]

    # Center data
    centered = []
    for row in features:
        centered.append([row[j] - mean[j] for j in range(n_feat)])

    # Power iteration to find top N_COMPONENTS principal directions
    components = []
    n_comp = min(N_COMPONENTS, n_feat)

    for _ in range(n_comp):
        # Initialize random-ish vector
        vec = [1.0 / (j + 1) for j in range(n_feat)]
        norm = _vec_norm(vec)
        vec = [v / norm for v in vec]

        # Power iteration: v = (X^T X) v, normalized
        for _iter in range(20):
            # new_vec = X^T (X v)
            new_vec = [0.0] * n_feat
            for row in centered:
                proj = sum(row[j] * vec[j] for j in range(n_feat))
                for j in range(n_feat):
                    new_vec[j] += row[j] * proj

            norm = _vec_norm(new_vec)
            vec = [v / norm for v in new_vec]

        components.append(vec)

        # Deflate: remove this component from data
        for i, row in enumerate(centered):
            proj = sum(row[j] * vec[j] for j in range(n_feat))
            for j in range(n_feat):
                centered[i][j] -= proj * vec[j]

    # Project all samples
    for i in range(n_samples):
        row = features[i]
        centered_row = [row[j] - mean[j] for j in range(n_feat)]
        projections = []
        for comp in components:
            projections.append(int(sum(centered_row[j] * comp[j] for j in range(n_feat))))
        rec = str(labels[i]) + ',' + ','.join(str(p) for p in projections)
        shm.append_stream_data(out_slot, rec.encode())


# ── Stage 3: Train decision stumps ───────────────────────────────────────────

def ml_train(in_slot, out_slot):
    """Train a set of decision stumps on PCA features from stream slot `in_slot`.
    Each stump picks the best (feature, threshold) split for one class.
    Writes stump definitions to stream slot `out_slot`."""
    labels, features = _parse_dataset(shm.read_all_stream_records(in_slot))
    if not features:
        return

    n_feat = len(features[0])
    n_classes = 10

    # For each class, find best single-feature threshold split (one-vs-rest)
    for cls in range(n_classes):
        best_feat = 0
        best_thresh = 0
        best_score = -1

        for f in range(n_feat):
            # Sort values for this feature
            vals = sorted(set(row[f] for row in features))
            if len(vals) < 2:
                continue

            for ti in range(0, len(vals) - 1, max(1, len(vals) // 10)):
                thresh = (vals[ti] + vals[ti + 1]) // 2
                # Count correct predictions
                correct = 0
                for i in range(len(labels)):
                    predicted_cls = cls if features[i][f] <= thresh else -1
                    actual = 1 if labels[i] == cls else 0
                    if (predicted_cls == cls) == (actual == 1):
                        correct += 1
                if correct > best_score:
                    best_score = correct
                    best_feat = f
                    best_thresh = thresh

        # Emit stump: feature_index, threshold, positive_class
        stump = 'stump:%d,%d,%d,%d' % (best_feat, best_thresh, cls, best_score)
        shm.append_stream_data(out_slot, stump.encode())


# ── Stage 4: Validate ────────────────────────────────────────────────────────

def ml_validate(in_slot):
    """Read all stumps + PCA-projected test data from aggregated stream slot.
    Ensemble stumps via voting, compute accuracy.
    Writes results to I/O slot 1."""
    stumps = []
    labels = []
    features = []

    for _origin, rec in shm.read_all_stream_records(in_slot):
        s = rec.decode('utf-8', errors='replace').strip()
        if s.startswith('stump:'):
            parts = s[6:].split(',')
            if len(parts) >= 4:
                stumps.append({
                    'feat': int(parts[0]),
                    'thresh': int(parts[1]),
                    'cls': int(parts[2]),
                    'score': int(parts[3]),
                })
        else:
            parts = s.split(',')
            if len(parts) >= 2:
                labels.append(int(parts[0]))
                features.append([int(x) for x in parts[1:]])

    if not features or not stumps:
        return

    # Predict using stump ensemble: each stump votes for its class
    correct = 0
    for i in range(len(labels)):
        votes = [0] * 10
        for st in stumps:
            f_idx = st['feat']
            if f_idx < len(features[i]):
                if features[i][f_idx] <= st['thresh']:
                    votes[st['cls']] += 1
        predicted = max(range(10), key=lambda c: votes[c])
        if predicted == labels[i]:
            correct += 1

    accuracy = 100.0 * correct / max(len(labels), 1)

    shm.write_output(b'=== ml_training_results ===')
    shm.write_output(('total_stumps=%d' % len(stumps)).encode())
    shm.write_output(('test_samples=%d' % len(labels)).encode())
    shm.write_output(('correct=%d' % correct).encode())
    shm.write_output(('accuracy=%.2f%%' % accuracy).encode())
    shm.write_output(b'--- stump_details ---')
    for st in stumps:
        shm.write_output(
            ('class_%d: feat=%d thresh=%d train_score=%d' % (
                st['cls'], st['feat'], st['thresh'], st['score'])).encode())
