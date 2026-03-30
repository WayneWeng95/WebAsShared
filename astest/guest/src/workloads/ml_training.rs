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
use alloc::string::String;
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

/// Partition: split dataset from I/O slot 0 round-robin to stream slots 10..10+n.
#[no_mangle]
pub extern "C" fn ml_partition(n_shards: u32) {
    let records = ShmApi::read_all_inputs();
    let mut idx: u32 = 0;
    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("").trim();
        if s.is_empty() || s.starts_with("label") { continue; }
        ShmApi::append_stream_data(PARTITION_BASE + (idx % n_shards), rec);
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
