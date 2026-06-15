// ─────────────────────────────────────────────────────────────────────────────
// ML inference (MNIST-style) — distributed batch inference, integer-exact gate.
//
// Workload #6 of the Application_Benchmark. A PRE-TRAINED integer linear
// classifier (one weight row per class) is applied to a held-out test set:
// partition the test data across W workers, each runs the forward pass
// (argmax over class scores) on its shard, the partial counts are aggregated,
// and accuracy + a prediction checksum are reported.
//
// Why the gate holds (EXPERIMENT_RUNBOOK §1/§5.2):
//   • Each sample is classified INDEPENDENTLY, so the set of predictions — and
//     hence Σ predicted labels (the checksum) and the correct count — is
//     identical no matter how the test set is sharded across workers
//     (FAN-OUT-INVARIANT). It's a plain integer sum: order-independent.
//   • The forward pass is integer `score_c = Σ_j w[c][j]·x[j]` + argmax (first
//     max wins), no transcendental functions → reproducible to the bit in any
//     language (CROSS-SYSTEM-EXACT).
// Gate = `prediction_checksum` (Σ predicted labels over the test set).
//
// DAG (gen_dag.py):
//   load_model(io slot M) → infer_setup_model → model stream slot
//   load_data(slot 0) → infer_partition(W) → shards
//   infer_predict ×W (read shard + broadcast model) → Aggregate → infer_reduce → save
//
// Added ADDITIVELY (new module, new #[no_mangle] fns) — touches no existing stage.
// ─────────────────────────────────────────────────────────────────────────────

use alloc::vec::Vec;
use crate::api::ShmApi;

const INF_CLASSES: usize = 10;          // matches the model / dataset
const INF_MODEL_SLOT: u32 = 1901;       // broadcast model (distinct from SGD's 1900)
const INF_PART_BASE: u32 = 10;

/// Parse a "label,f0,f1,..." data line. `None` for blank / header / non-data.
fn inf_parse(rec: &[u8]) -> Option<(i64, Vec<i64>)> {
    let s = core::str::from_utf8(rec).unwrap_or("").trim();
    if s.is_empty() || s.starts_with("label") || s.starts_with("model")
        || s.starts_with("pred") { return None; }
    let mut it = s.split(',');
    let label: i64 = it.next()?.parse().ok()?;
    let feats: Vec<i64> = it.map(|v| v.parse().unwrap_or(0)).collect();
    if feats.is_empty() { return None; }
    Some((label, feats))
}

/// Read the broadcast model record: (classes, features, weights).
fn inf_read_model() -> Option<(usize, usize, Vec<i64>)> {
    let (_o, rec) = ShmApi::read_latest_stream_data(INF_MODEL_SLOT)?;
    let s = core::str::from_utf8(&rec).unwrap_or("").trim();
    let mut it = s.split(',');
    if it.next()? != "model" { return None; }
    let c: usize = it.next()?.parse().ok()?;
    let f: usize = it.next()?.parse().ok()?;
    let w: Vec<i64> = it.map(|v| v.parse().unwrap_or(0)).collect();
    if w.len() != c * f { return None; }
    Some((c, f, w))
}

/// Setup: copy the "model,C,F,w…" record from io slot `arg` (loaded by Input)
/// into the broadcast model stream slot. Runs once before the predict workers;
/// all W workers then non-destructively broadcast-read it (one slot fans out).
#[no_mangle]
pub extern "C" fn infer_setup_model(arg: u32) {
    for (_o, rec) in &ShmApi::read_all_inputs_from(arg) {
        let s = core::str::from_utf8(rec).unwrap_or("").trim();
        if s.starts_with("model,") {
            ShmApi::append_stream_data(INF_MODEL_SLOT, s.as_bytes());
            return;
        }
    }
}

/// Partition: zero-copy contiguous split of input slot 0 into `W` shards at
/// `base` (arg packs `base | (W<<16)`). The sole reader of slot 0.
#[no_mangle]
pub extern "C" fn infer_partition(arg: u32) {
    let (base, n) = super::unpack_fanout_arg(arg, INF_PART_BASE);
    if n == 0 { return; }
    ShmApi::split_input_contiguous(common::INPUT_IO_SLOT, base, n);
}

/// Predict: read this worker's shard + the broadcast model, run the integer
/// forward pass (argmax of `Σ w·x`) per sample, emit `pred,correct,total,predsum`.
/// `arg` packs `data_slot | (out_slot << 16)`.
#[no_mangle]
pub extern "C" fn infer_predict(arg: u32) {
    let data_slot = arg & 0xFFFF;
    let out_slot = arg >> 16;
    let (c, f, w) = match inf_read_model() { Some(m) => m, None => return };
    let mut correct: i64 = 0;
    let mut total: i64 = 0;
    let mut predsum: i64 = 0;
    for (_o, rec) in &ShmApi::read_all_stream_records(data_slot) {
        let (label, feats) = match inf_parse(rec) { Some(s) => s, None => continue };
        if feats.len() != f { continue; }
        let mut best = 0usize;
        let mut best_val = i64::MIN;
        for cls in 0..c {
            let mut score: i64 = 0;
            for j in 0..f { score += w[cls * f + j] * feats[j]; }
            if score > best_val { best_val = score; best = cls; }
        }
        total += 1;
        predsum += best as i64;
        if best as i64 == label { correct += 1; }
    }
    ShmApi::append_stream_data(out_slot,
        alloc::format!("pred,{},{},{}", correct, total, predsum).as_bytes());
}

/// Reduce: sum the per-worker `pred,…` records (aggregated into `arg`'s slot),
/// report accuracy + the prediction checksum (the gate).
#[no_mangle]
pub extern "C" fn infer_reduce(arg: u32) {
    let mut correct: i64 = 0;
    let mut total: i64 = 0;
    let mut predsum: i64 = 0;
    for (_o, rec) in &ShmApi::read_all_stream_records(arg) {
        let s = core::str::from_utf8(rec).unwrap_or("").trim();
        if !s.starts_with("pred,") { continue; }
        let mut it = s["pred,".len()..].split(',');
        correct += it.next().and_then(|t| t.parse().ok()).unwrap_or(0);
        total += it.next().and_then(|t| t.parse().ok()).unwrap_or(0);
        predsum += it.next().and_then(|t| t.parse().ok()).unwrap_or(0);
    }
    let _ = INF_CLASSES;
    let acc = if total > 0 { correct * 10000 / total } else { 0 };
    ShmApi::write_output_str("=== mnist_inference_results ===");
    ShmApi::write_output_str(&alloc::format!("test_samples={}", total));
    ShmApi::write_output_str(&alloc::format!("correct={}", correct));
    ShmApi::write_output_str(&alloc::format!("prediction_checksum={}", predsum));
    ShmApi::write_output_str(&alloc::format!("accuracy={}.{}%", acc / 100, acc % 100));
}
