// ─────────────────────────────────────────────────────────────────────────────
// ML Training demo — image classification pipeline
//
// Modeled after the ML training serverless workflow from RMMAP (EuroSys'24).
// The original uses MNIST + PCA + LightGBM random forest.  We implement a
// pure no_std equivalent: power-iteration PCA + decision stump ensemble.
//
// Slot layout:
//   I/O slot 0          : raw CSV (label,f0,f1,...,f27)
//   stream slots 10–11  : partitioned dataset (2 PCA shards)
//   stream slots 110–111: PCA-projected features
//   stream slot 200     : aggregated PCA output
//   stream slots 30–37  : re-distributed training shards (8 workers)
//   stream slots 130–137: trained stump models
//   stream slot 300     : aggregated stumps + test data
//
// DAG stages:
//   Input            → load mnist_features.csv into I/O slot 0
//   ml_partition(2)  → split dataset round-robin to slots 10–11
//   ml_pca × 2       → each reads slot N, projects to 8 PCs, writes slot N+100
//   Aggregate        → merge PCA outputs → slot 200
//   ml_redistribute  → split slot 200 round-robin to slots 30–37
//   ml_train × 8     → each reads slot N, trains stumps, writes slot N+100
//   Aggregate        → merge stumps + PCA data → slot 300
//   ml_validate      → ensemble stumps, compute accuracy, write OUTPUT_IO_SLOT
// ─────────────────────────────────────────────────────────────────────────────

use alloc::vec::Vec;
use crate::api::ShmApi;

const PARTITION_BASE: u32 = 10;
const PCA_OUT_OFFSET: u32 = 100;
const TRAIN_BASE: u32 = 30;
const TRAIN_OUT_OFFSET: u32 = 100;
const N_COMPONENTS: usize = 8;
const N_CLASSES: usize = 10;
const N_TRAIN_SHARDS: u32 = 8;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn parse_records(slot: u32) -> (Vec<u32>, Vec<Vec<i32>>) {
    let mut labels = Vec::new();
    let mut features = Vec::new();
    let records = ShmApi::read_all_stream_records(slot);
    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("");
        let s = s.trim();
        if s.is_empty() || s.starts_with("label") || s.starts_with("stump") {
            continue;
        }
        let parts: Vec<&str> = s.split(',').collect();
        if parts.len() < 2 { continue; }
        let label: u32 = parts[0].parse().unwrap_or(0);
        let feats: Vec<i32> = parts[1..].iter()
            .map(|v| v.parse().unwrap_or(0))
            .collect();
        labels.push(label);
        features.push(feats);
    }
    (labels, features)
}

fn vec_norm_sq(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum()
}

// ── Exported functions ───────────────────────────────────────────────────────

/// Partition: split dataset from I/O slot 0 round-robin to `n_shards` stream
/// slots starting at `base`.
///
/// `arg` packs the partitioner's slot layout: `base | (n_shards << 16)` (see
/// `partitioner::slot_assigner`).  Legacy single-node DAGs (high bits clear)
/// pass a bare shard count and default the base to `PARTITION_BASE`.
#[no_mangle]
pub extern "C" fn ml_partition(arg: u32) {
    let (base, n_shards) = super::unpack_fanout_arg(arg, PARTITION_BASE);
    if n_shards == 0 { return; }
    let records = ShmApi::read_all_inputs();
    let mut idx: u32 = 0;
    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("").trim();
        if s.is_empty() || s.starts_with("label") { continue; }
        ShmApi::append_stream_data(base + (idx % n_shards), rec);
        idx += 1;
    }
}

/// Redistribute: read PCA output from stream slot, distribute to 8 training slots.
/// The arg is the source aggregated PCA slot (e.g., 200).
#[no_mangle]
pub extern "C" fn ml_redistribute(in_slot: u32) {
    let records = ShmApi::read_all_stream_records(in_slot);
    let mut idx: u32 = 0;
    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("").trim();
        if s.is_empty() || s.starts_with("label") || s.starts_with("stump") { continue; }
        ShmApi::append_stream_data(TRAIN_BASE + (idx % N_TRAIN_SHARDS), rec);
        idx += 1;
    }
}

/// PCA: power-iteration feature extraction.
/// Reads from stream slot `slot`, writes N_COMPONENTS-dim projections to slot+100.
#[no_mangle]
pub extern "C" fn ml_pca(slot: u32) {
    let (labels, features) = parse_records(slot);
    if features.is_empty() { return; }

    let n_samples = features.len();
    let n_feat = features[0].len();
    let n_comp = if N_COMPONENTS < n_feat { N_COMPONENTS } else { n_feat };

    // Compute mean (fixed-point: sum / n)
    let mut mean = alloc::vec![0i64; n_feat];
    for row in &features {
        for j in 0..n_feat {
            mean[j] += row[j] as i64;
        }
    }
    let mean_f: Vec<f32> = mean.iter().map(|m| *m as f32 / n_samples as f32).collect();

    // Center data
    let mut centered: Vec<Vec<f32>> = Vec::with_capacity(n_samples);
    for row in &features {
        centered.push(row.iter().enumerate()
            .map(|(j, &v)| v as f32 - mean_f[j]).collect());
    }

    // Power iteration for top components
    let mut components: Vec<Vec<f32>> = Vec::new();
    for _comp in 0..n_comp {
        let mut vec: Vec<f32> = (0..n_feat).map(|j| 1.0 / (j as f32 + 1.0)).collect();
        let norm = vec_norm_sq(&vec).sqrt();
        if norm > 0.0 { for v in &mut vec { *v /= norm; } }

        for _iter in 0..20 {
            let mut new_vec = alloc::vec![0.0f32; n_feat];
            for row in &centered {
                let proj: f32 = row.iter().zip(vec.iter()).map(|(a, b)| a * b).sum();
                for j in 0..n_feat {
                    new_vec[j] += row[j] * proj;
                }
            }
            let norm = vec_norm_sq(&new_vec).sqrt();
            if norm > 0.0 { for v in &mut new_vec { *v /= norm; } }
            vec = new_vec;
        }

        // Deflate
        for row in &mut centered {
            let proj: f32 = row.iter().zip(vec.iter()).map(|(a, b)| a * b).sum();
            for j in 0..n_feat {
                row[j] -= proj * vec[j];
            }
        }
        components.push(vec);
    }

    // Project and emit
    let out_slot = slot + PCA_OUT_OFFSET;
    for i in 0..n_samples {
        let row = &features[i];
        let centered_row: Vec<f32> = row.iter().enumerate()
            .map(|(j, &v)| v as f32 - mean_f[j]).collect();
        let mut parts = alloc::vec![alloc::format!("{}", labels[i])];
        for comp in &components {
            let proj: f32 = centered_row.iter().zip(comp.iter())
                .map(|(a, b)| a * b).sum();
            parts.push(alloc::format!("{}", proj as i32));
        }
        ShmApi::append_stream_data(out_slot, parts.join(",").as_bytes());
    }
}

/// Train: fit decision stumps on PCA features.
/// Reads from stream slot `slot`, writes stump definitions to slot+100.
#[no_mangle]
pub extern "C" fn ml_train(slot: u32) {
    let (labels, features) = parse_records(slot);
    if features.is_empty() { return; }

    let n_feat = features[0].len();
    let out_slot = slot + TRAIN_OUT_OFFSET;

    // For each class, find best single-feature threshold split
    for cls in 0..N_CLASSES {
        let mut best_feat: usize = 0;
        let mut best_thresh: i32 = 0;
        let mut best_score: u32 = 0;

        for f in 0..n_feat {
            // Collect unique sorted values
            let mut vals: Vec<i32> = features.iter().map(|r| r[f]).collect();
            vals.sort();
            vals.dedup();
            if vals.len() < 2 { continue; }

            // Sample thresholds
            let step = if vals.len() > 10 { vals.len() / 10 } else { 1 };
            let mut ti = 0;
            while ti < vals.len() - 1 {
                let thresh = (vals[ti] + vals[ti + 1]) / 2;
                let mut correct: u32 = 0;
                for i in 0..labels.len() {
                    let predicted = features[i][f] <= thresh;
                    let actual = labels[i] == cls as u32;
                    if predicted == actual { correct += 1; }
                }
                if correct > best_score {
                    best_score = correct;
                    best_feat = f;
                    best_thresh = thresh;
                }
                ti += step;
            }
        }

        ShmApi::append_stream_data(out_slot,
            alloc::format!("stump:{},{},{},{}", best_feat, best_thresh, cls, best_score)
                .as_bytes());
    }
}

/// Validate: ensemble stumps and compute accuracy on PCA test data.
/// Reads everything from aggregated stream slot `agg_slot`, writes to OUTPUT_IO_SLOT.
#[no_mangle]
pub extern "C" fn ml_validate(agg_slot: u32) {
    let mut stumps: Vec<(usize, i32, usize, u32)> = Vec::new(); // (feat, thresh, cls, score)
    let mut labels: Vec<u32> = Vec::new();
    let mut features: Vec<Vec<i32>> = Vec::new();

    let records = ShmApi::read_all_stream_records(agg_slot);
    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("").trim();
        if s.starts_with("stump:") {
            let parts: Vec<&str> = s[6..].split(',').collect();
            if parts.len() >= 4 {
                stumps.push((
                    parts[0].parse().unwrap_or(0),
                    parts[1].parse().unwrap_or(0),
                    parts[2].parse().unwrap_or(0),
                    parts[3].parse().unwrap_or(0),
                ));
            }
        } else if !s.is_empty() && !s.starts_with("label") {
            let parts: Vec<&str> = s.split(',').collect();
            if parts.len() >= 2 {
                labels.push(parts[0].parse().unwrap_or(0));
                features.push(parts[1..].iter()
                    .map(|v| v.parse().unwrap_or(0))
                    .collect());
            }
        }
    }

    if features.is_empty() || stumps.is_empty() { return; }

    let mut correct: u32 = 0;
    for i in 0..labels.len() {
        let mut votes = [0u32; N_CLASSES];
        for &(feat, thresh, cls, _) in &stumps {
            if feat < features[i].len() && features[i][feat] <= thresh {
                votes[cls] += 1;
            }
        }
        let predicted = votes.iter().enumerate()
            .max_by_key(|&(_, v)| v)
            .map(|(c, _)| c as u32)
            .unwrap_or(0);
        if predicted == labels[i] { correct += 1; }
    }

    let acc_pct = if !labels.is_empty() {
        correct as u64 * 10000 / labels.len() as u64
    } else { 0 };

    ShmApi::write_output_str("=== ml_training_results ===");
    ShmApi::write_output_str(&alloc::format!("total_stumps={}", stumps.len()));
    ShmApi::write_output_str(&alloc::format!("test_samples={}", labels.len()));
    ShmApi::write_output_str(&alloc::format!("correct={}", correct));
    ShmApi::write_output_str(&alloc::format!("accuracy={}.{}%", acc_pct / 100, acc_pct % 100));
    ShmApi::write_output_str("--- stump_details ---");
    for &(feat, thresh, cls, score) in &stumps {
        ShmApi::write_output_str(&alloc::format!(
            "class_{}: feat={} thresh={} train_score={}", cls, feat, thresh, score));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Synchronous data-parallel SGD — fixed-point, integer-exact, fan-out-invariant.
//
// Added ADDITIVELY for the Application_Benchmark (workload #5). It does NOT touch
// the PCA/decision-stump stages above; it implements the "ML training (SGD)"
// workload the cross-system suite actually compares (RMMap ml-pipeline / Faasm
// HOGWILD! / Cloudburst) with a runbook-style correctness gate.
//
// Model: a multi-class LINEAR classifier (one weight row per class) trained by
// full-batch gradient descent on a least-squares (one-hot target) objective —
// no transcendental functions, so it is reproducible to the bit in ANY language.
//
// Why the gate holds (EXPERIMENT_RUNBOOK §1/§5.2):
//   • The per-epoch gradient is a SUM of per-sample contributions. Integer
//     addition is associative+commutative, so Σ over the whole dataset is
//     identical no matter how samples are sharded across workers → the result
//     is FAN-OUT-INVARIANT (same checksum at every W).
//   • The single learning-rate division is applied ONCE, centrally, to the
//     aggregated gradient sum (never per-worker), with toward-zero truncation
//     (`i64 /`) — replicated bit-for-bit by the Python/WASM baselines → the
//     result is CROSS-SYSTEM-EXACT.
//   • All arithmetic is integer i64 → no float drift between Rust-WASM and numpy.
// Gate value: `weight_checksum` = Σ of all model weights after E epochs.
//
// DAG (gen_dag.py unrolls E epochs):
//   Input(slot 0) → sgd_init → sgd_partition(W) → shards[10..10+W]
//   per epoch e:  sgd_grad × W (read shard + latest model) → Aggregate → sgd_update
//   → sgd_validate → Output
//
// Model lives in ONE stream slot (SGD_MODEL_SLOT); sgd_update APPENDS the new
// model each epoch and every worker broadcast-reads the latest (reads are
// non-destructive pointer traversals, so one slot fans out to all workers).
// ─────────────────────────────────────────────────────────────────────────────

const SGD_MODEL_SLOT: u32 = 1900;  // single slot; new model appended each epoch
const SGD_TARGET: i64 = 1024;      // fixed-point one-hot target for the true class
const SGD_LR_K: i64 = 16;          // LR_DEN = n_samples * SGD_LR_K (size-independent step)

/// Toward-zero integer division — matches Rust `i64 /` and the baselines' helper.
#[inline]
fn sgd_trunc_div(n: i64, d: i64) -> i64 { if d == 0 { 0 } else { n / d } }

/// Parse a "label,f0,f1,..." data line into (label, features). `None` for
/// blank / header / non-data lines.
fn sgd_parse_sample(rec: &[u8]) -> Option<(i64, Vec<i64>)> {
    let s = core::str::from_utf8(rec).unwrap_or("").trim();
    if s.is_empty() || s.starts_with("label") || s.starts_with("model")
        || s.starts_with("grad") { return None; }
    let mut it = s.split(',');
    let label: i64 = it.next()?.parse().ok()?;
    let feats: Vec<i64> = it.map(|v| v.parse().unwrap_or(0)).collect();
    if feats.is_empty() { return None; }
    Some((label, feats))
}

/// Read the latest model record: returns (classes, features, n_samples, lr_den, weights).
/// `None` before the first sgd_update has written one (epoch 0 sees no model).
fn sgd_read_model() -> Option<(usize, usize, i64, i64, Vec<i64>)> {
    let (_o, rec) = ShmApi::read_latest_stream_data(SGD_MODEL_SLOT)?;
    let s = core::str::from_utf8(&rec).unwrap_or("").trim();
    let mut it = s.split(',');
    if it.next()? != "model" { return None; }
    let c: usize = it.next()?.parse().ok()?;
    let f: usize = it.next()?.parse().ok()?;
    let n: i64 = it.next()?.parse().ok()?;
    let lr_den: i64 = it.next()?.parse().ok()?;
    let w: Vec<i64> = it.map(|v| v.parse().unwrap_or(0)).collect();
    if w.len() != c * f { return None; }
    Some((c, f, n, lr_den, w))
}

fn sgd_write_model(c: usize, f: usize, n: i64, lr_den: i64, w: &[i64]) {
    let mut parts = alloc::vec![
        alloc::string::String::from("model"),
        alloc::format!("{}", c), alloc::format!("{}", f),
        alloc::format!("{}", n), alloc::format!("{}", lr_den),
    ];
    for v in w { parts.push(alloc::format!("{}", v)); }
    ShmApi::append_stream_data(SGD_MODEL_SLOT, parts.join(",").as_bytes());
}

/// Partition: zero-copy contiguous split of input slot 0 into `W` shards at
/// `base` (arg packs `base | (W<<16)`). The SOLE reader of slot 0 (the input
/// is consumed once, like word_count's wc_distribute). Same SHM page-chain split.
#[no_mangle]
pub extern "C" fn sgd_partition(arg: u32) {
    let (base, n) = super::unpack_fanout_arg(arg, PARTITION_BASE);
    if n == 0 { return; }
    ShmApi::split_input_contiguous(common::INPUT_IO_SLOT, base, n);
}

// ── Binary shard encoding (parse-once optimization) ───────────────────────────
// The gradient workers run E times (epochs are unrolled), so parsing the TEXT
// shard every epoch is the dominant per-epoch cost. `sgd_encode` parses each
// text shard ONCE into a compact little-endian binary blob; the per-epoch
// gradient workers then skip text parsing entirely (a flat byte→i32 read, no
// per-sample String/Vec allocation). One-time work hoisted out of the E-loop —
// the same trick the baselines get for free by unpickling a binary numpy array.
//
// Binary block record: [count:u32][f:u32] then count×([label:i32][f×i32]) (LE).
// Features are stored as full i32 — the natural width for arbitrary integer
// feature data. At the largest sweep sizes this pushes total SHM past the
// 64 MiB initial pool, which is now handled correctly by the host's per-wave
// mapping re-sync (dag_runner::sync_mapping_if_grown); see problems.md. The old
// i8 packing was a workaround for a latent SHM-growth bug and is no longer
// needed.
const SGD_BIN_BLK: usize = 4096;

#[inline]
fn rd_i32(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn sgd_flush_block(bin_slot: u32, body: &mut Vec<u8>, count: u32, f: usize) {
    if count == 0 { return; }
    let mut rec = Vec::with_capacity(8 + body.len());
    rec.extend_from_slice(&count.to_le_bytes());
    rec.extend_from_slice(&(f as u32).to_le_bytes());
    rec.extend_from_slice(body);
    ShmApi::append_stream_data(bin_slot, &rec);
    body.clear();
}

/// Encode: parse this worker's TEXT shard ONCE into compact binary blocks at
/// `bin_slot`. `arg` packs `text_slot | (bin_slot << 16)`. Runs once (a wave
/// after partition), so per-epoch gradient workers never re-parse text.
#[no_mangle]
pub extern "C" fn sgd_encode(arg: u32) {
    let text_slot = arg & 0xFFFF;
    let bin_slot = arg >> 16;
    let mut f: usize = 0;
    let mut body: Vec<u8> = Vec::new();
    let mut count: u32 = 0;
    for (_o, rec) in &ShmApi::read_all_stream_records(text_slot) {
        let (label, feats) = match sgd_parse_sample(rec) { Some(s) => s, None => continue };
        if f == 0 { f = feats.len(); }
        if feats.len() != f { continue; }
        body.extend_from_slice(&(label as i32).to_le_bytes());
        for &v in &feats { body.extend_from_slice(&(v as i32).to_le_bytes()); }   // i32 feature
        count += 1;
        if count as usize >= SGD_BIN_BLK { sgd_flush_block(bin_slot, &mut body, count, f); count = 0; }
    }
    sgd_flush_block(bin_slot, &mut body, count, f);
}

/// Read F (feature count) from the first binary block's header, or 0 if empty.
fn sgd_bin_features(recs: &[(u32, Vec<u8>)]) -> usize {
    recs.iter().find_map(|(_o, b)| if b.len() >= 8 { Some(rd_i32(b, 4) as usize) } else { None })
        .unwrap_or(0)
}

/// Gradient worker: read this worker's BINARY shard + the latest model (zero
/// weights on epoch 0, before any model exists), accumulate the integer gradient
/// SUM over the shard, emit `grad,count,F,g0,…`. `arg` packs
/// `bin_slot | (grad_out_slot << 16)`. The shard count + F let sgd_update
/// bootstrap n_samples / lr_den without re-reading the (consumed) input slot.
#[no_mangle]
pub extern "C" fn sgd_grad(arg: u32) {
    let bin_slot = arg & 0xFFFF;
    let grad_out = arg >> 16;
    let recs = ShmApi::read_all_stream_records(bin_slot);

    // Feature count + classes: from the model if present, else from the shard header.
    let model = sgd_read_model();
    let c = match &model { Some((c, ..)) => *c, None => N_CLASSES };
    let f = match &model { Some((_, f, ..)) => *f, None => sgd_bin_features(&recs) };
    if f == 0 { return; }
    let zero;
    let w: &[i64] = match &model { Some((.., w)) => w, None => { zero = alloc::vec![0i64; c * f]; &zero } };

    let mut g = alloc::vec![0i64; c * f];
    let mut feats = alloc::vec![0i64; f];
    let mut count: i64 = 0;
    for (_o, b) in &recs {
        if b.len() < 8 { continue; }
        let n = rd_i32(b, 0) as usize;
        if rd_i32(b, 4) as usize != f { continue; }
        let mut off = 8;
        for _ in 0..n {
            let label = rd_i32(b, off) as i64; off += 4;
            for j in 0..f { feats[j] = rd_i32(b, off) as i64; off += 4; }
            count += 1;
            for cls in 0..c {
                let mut pred: i64 = 0;
                for j in 0..f { pred += w[cls * f + j] * feats[j]; }
                let target = if cls as i64 == label { SGD_TARGET } else { 0 };
                let err = pred - target;
                for j in 0..f { g[cls * f + j] += err * feats[j]; }
            }
        }
    }
    let mut parts = alloc::vec![
        alloc::string::String::from("grad"),
        alloc::format!("{}", count), alloc::format!("{}", f),
    ];
    for v in &g { parts.push(alloc::format!("{}", v)); }
    ShmApi::append_stream_data(grad_out, parts.join(",").as_bytes());
}

/// Update: sum every worker's gradient record (aggregated into `arg`'s slot),
/// take ONE central toward-zero step, append the new model. On epoch 0 (no model
/// yet) it bootstraps the model: n_samples = Σ worker counts, lr_den = n*SGD_LR_K,
/// initial weights = 0. The element-wise integer sum + single division is what
/// makes the result fan-out-invariant.
#[no_mangle]
pub extern "C" fn sgd_update(arg: u32) {
    let recs = ShmApi::read_all_stream_records(arg);

    // First pass: discover F and the total sample count from the grad records.
    let mut f: usize = 0;
    let mut n: i64 = 0;
    for (_o, rec) in &recs {
        let s = core::str::from_utf8(rec).unwrap_or("").trim();
        if !s.starts_with("grad,") { continue; }
        let mut it = s["grad,".len()..].split(',');
        let cnt: i64 = it.next().and_then(|t| t.parse().ok()).unwrap_or(0);
        let ff: usize = it.next().and_then(|t| t.parse().ok()).unwrap_or(0);
        n += cnt;
        if f == 0 { f = ff; }
    }
    if f == 0 { return; }

    let (c, n_model, lr_den, mut w) = match sgd_read_model() {
        Some((c, _f, nm, lr, w)) => (c, nm, lr, w),
        None => (N_CLASSES, n, n * SGD_LR_K, alloc::vec![0i64; N_CLASSES * f]),
    };

    let mut gsum = alloc::vec![0i64; c * f];
    for (_o, rec) in &recs {
        let s = core::str::from_utf8(rec).unwrap_or("").trim();
        if !s.starts_with("grad,") { continue; }
        // skip the two header fields (count, F), then the c*f gradient values
        for (k, tok) in s["grad,".len()..].split(',').skip(2).enumerate() {
            if k < c * f { gsum[k] += tok.parse::<i64>().unwrap_or(0); }
        }
    }
    for k in 0..c * f { w[k] -= sgd_trunc_div(gsum[k], lr_den); }
    sgd_write_model(c, f, n_model, lr_den, &w);
}

/// Validate: read all `W` BINARY shards + the final model, compute the weight
/// checksum (the gate) and train accuracy. `arg` packs `bin_base | (W<<16)`.
#[no_mangle]
pub extern "C" fn sgd_validate(arg: u32) {
    let (base, w_count) = super::unpack_fanout_arg(arg, PARTITION_BASE);
    let (c, f, n, _lr, w) = match sgd_read_model() { Some(m) => m, None => return };
    let mut checksum: i64 = 0;
    for v in &w { checksum += *v; }

    let mut total: i64 = 0;
    let mut correct: i64 = 0;
    let mut feats = alloc::vec![0i64; f];
    for shard in 0..w_count {
        for (_o, b) in &ShmApi::read_all_stream_records(base + shard) {
            if b.len() < 8 || rd_i32(b, 4) as usize != f { continue; }
            let nrec = rd_i32(b, 0) as usize;
            let mut off = 8;
            for _ in 0..nrec {
                let label = rd_i32(b, off) as i64; off += 4;
                for j in 0..f { feats[j] = rd_i32(b, off) as i64; off += 4; }
                let mut best = 0usize;
                let mut best_val = i64::MIN;
                for cls in 0..c {
                    let mut pred: i64 = 0;
                    for j in 0..f { pred += w[cls * f + j] * feats[j]; }
                    if pred > best_val { best_val = pred; best = cls; }
                }
                total += 1;
                if best as i64 == label { correct += 1; }
            }
        }
    }
    let acc = if total > 0 { correct * 10000 / total } else { 0 };
    ShmApi::write_output_str("=== sgd_training_results ===");
    ShmApi::write_output_str(&alloc::format!("classes={} features={} samples={}", c, f, n));
    ShmApi::write_output_str(&alloc::format!("weight_checksum={}", checksum));
    ShmApi::write_output_str(&alloc::format!("train_correct={}", correct));
    ShmApi::write_output_str(&alloc::format!("train_total={}", total));
    ShmApi::write_output_str(&alloc::format!("accuracy={}.{}%", acc / 100, acc % 100));
}
