// ─────────────────────────────────────────────────────────────────────────────
// 4-stage streaming pipeline
//
// The host's `StreamPipeline` DAG node drives these functions in a loop.
// Each round the source appends a fresh batch; downstream stages advance their
// own per-stage cursors (stored as named SHM atomics) so they consume only
// records that arrived since the last call.
//
// Slot layout (configurable via JSON):
//   source_slot    (200): raw batch records from the source
//   filter_slot    (201): even-item records kept by the filter stage
//   transform_slot (202): filtered records with "|T" appended
//   summary_slot   (203): one summary record per round from the sink
// ─────────────────────────────────────────────────────────────────────────────

use core::sync::atomic::Ordering;
use alloc::vec::Vec;
use crate::api::ShmApi;

const PIPELINE_BATCH: u32 = 20;

/// Stage 1 — source: appends `PIPELINE_BATCH` records to `out_slot`.
/// Record format: "r={round},i={item:02},v={value:05}" where value = round*1000+item.
#[no_mangle]
pub extern "C" fn pipeline_source(out_slot: u32, round: u32) {
    for i in 0..PIPELINE_BATCH {
        let v = round * 1000 + i;
        let rec = alloc::format!("r={},i={:02},v={:05}", round, i, v);
        ShmApi::append_stream_data(out_slot, rec.as_bytes());
    }
}

/// Stage 2 — filter: reads new records from `in_slot` (cursor: "pipe_filter_cursor"),
/// keeps only those with an even item index, appends surviving records to `out_slot`.
#[no_mangle]
pub extern "C" fn pipeline_filter(in_slot: u32, out_slot: u32) {
    let cursor = ShmApi::get_named_atomic("pipe_filter_cursor");
    let start = cursor.load(Ordering::Acquire) as usize;
    let all = ShmApi::read_all_stream_records(in_slot);
    let new_recs = &all[start.min(all.len())..];
    for (_origin, rec) in new_recs {
        let keep = core::str::from_utf8(rec).ok()
            .and_then(|s| s.split(',').nth(1))
            .and_then(|p| p.split('=').nth(1))
            .and_then(|n| n.parse::<u32>().ok())
            .map(|n| n % 2 == 0)
            .unwrap_or(false);
        if keep {
            ShmApi::append_stream_data(out_slot, rec);
        }
    }
    cursor.store((start + new_recs.len()) as u64, Ordering::Release);
}

/// Stage 3 — transform: reads new records from `in_slot` (cursor: "pipe_transform_cursor"),
/// appends the "|T" transformation marker, writes results to `out_slot`.
#[no_mangle]
pub extern "C" fn pipeline_transform(in_slot: u32, out_slot: u32) {
    let cursor = ShmApi::get_named_atomic("pipe_transform_cursor");
    let start = cursor.load(Ordering::Acquire) as usize;
    let all = ShmApi::read_all_stream_records(in_slot);
    let new_recs = &all[start.min(all.len())..];
    for (_origin, rec) in new_recs {
        let mut out = Vec::with_capacity(rec.len() + 2);
        out.extend_from_slice(rec);
        out.extend_from_slice(b"|T");
        ShmApi::append_stream_data(out_slot, &out);
    }
    cursor.store((start + new_recs.len()) as u64, Ordering::Release);
}

/// Stage 4 — sink: reads new records from `in_slot` (cursor: "pipe_sink_cursor"),
/// sums their value fields, appends a per-batch summary to `summary_slot`.
/// Summary format: "batch_count={N},value_sum={S}"
#[no_mangle]
pub extern "C" fn pipeline_sink(in_slot: u32, summary_slot: u32) {
    let cursor = ShmApi::get_named_atomic("pipe_sink_cursor");
    let start = cursor.load(Ordering::Acquire) as usize;
    let all = ShmApi::read_all_stream_records(in_slot);
    let new_recs = &all[start.min(all.len())..];
    let count = new_recs.len() as u32;
    if count > 0 {
        let mut value_sum: u64 = 0;
        for (_origin, rec) in new_recs {
            if let Ok(s) = core::str::from_utf8(rec) {
                if let Some(v_part) = s.split(',').nth(2) {
                    if let Some(v_str) = v_part.split('=').nth(1) {
                        let v_clean = v_str.trim_end_matches("|T");
                        if let Ok(v) = v_clean.parse::<u64>() {
                            value_sum += v;
                        }
                    }
                }
            }
        }
        let summary = alloc::format!("batch_count={},value_sum={}", count, value_sum);
        ShmApi::append_stream_data(summary_slot, summary.as_bytes());
    }
    cursor.store((start + new_recs.len()) as u64, Ordering::Release);
}
