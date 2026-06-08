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

use core::sync::atomic::{AtomicU64, Ordering};
use alloc::vec::Vec;
use crate::api::ShmApi;

const PIPELINE_BATCH: u32 = 20;

/// Per-stage read window for a consumer stage keyed by its input slot.
///
/// Returns `(cursor, start, end)`: the stage should process records
/// `[start..end]` of `read_all_stream_records(in_slot)` and then store `end`
/// into `cursor`.  `end` is bounded by the host-published watermark
/// `stream_hi_{in_slot}` (sentinel: a raw value of 0 means "unset" → unbounded).
///
/// Under `StreamPipeline` the host refreshes the watermark every tick to the
/// slot's pre-tick committed count, so this bound stops a consumer of round R
/// from racing into round R+1's records that the producer appends concurrently
/// in the same tick.  Under serial execution (`WasmGrouping`) the watermark is
/// never set, so the read is unbounded — which is also correct there because
/// the producer has fully finished before the consumer runs.  Cursors are keyed
/// by input slot so the host can reset them between runs (Reset mode).
fn pipe_read_window(in_slot: u32, total: usize) -> (&'static AtomicU64, usize, usize) {
    let cursor = ShmApi::get_named_atomic(&alloc::format!("pipe_cursor_{}", in_slot));
    let start  = cursor.load(Ordering::Acquire) as usize;
    let raw    = ShmApi::get_named_atomic(&alloc::format!("stream_hi_{}", in_slot))
        .load(Ordering::Acquire) as usize;
    let end    = if raw == 0 { total } else { (raw - 1).min(total) };
    (cursor, start.min(end), end)
}

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
    let all = ShmApi::read_all_stream_records(in_slot);
    let (cursor, start, end) = pipe_read_window(in_slot, all.len());
    let new_recs = &all[start..end];
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
    cursor.store(end as u64, Ordering::Release);
}

/// Stage 3 — transform: reads new records from `in_slot` (cursor: "pipe_transform_cursor"),
/// appends the "|T" transformation marker, writes results to `out_slot`.
#[no_mangle]
pub extern "C" fn pipeline_transform(in_slot: u32, out_slot: u32) {
    let all = ShmApi::read_all_stream_records(in_slot);
    let (cursor, start, end) = pipe_read_window(in_slot, all.len());
    let new_recs = &all[start..end];
    for (_origin, rec) in new_recs {
        let mut out = Vec::with_capacity(rec.len() + 2);
        out.extend_from_slice(rec);
        out.extend_from_slice(b"|T");
        ShmApi::append_stream_data(out_slot, &out);
    }
    cursor.store(end as u64, Ordering::Release);
}

/// Stage 4 — sink: reads new records from `in_slot` (cursor: "pipe_sink_cursor"),
/// sums their value fields, appends a per-batch summary to `summary_slot`.
/// Summary format: "batch_count={N},value_sum={S}"
#[no_mangle]
pub extern "C" fn pipeline_sink(in_slot: u32, summary_slot: u32) {
    let all = ShmApi::read_all_stream_records(in_slot);
    let (cursor, start, end) = pipe_read_window(in_slot, all.len());
    let new_recs = &all[start..end];
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
    cursor.store(end as u64, Ordering::Release);
}

/// Alternate sink that writes each round's summary to **I/O slot** `out_io_slot`
/// (one record per round) instead of a stream slot.  Used by the
/// `Output { split_records: true }` test path: a StreamPipeline emits N records
/// (one per round) and the Output node fans record *i* to `paths[i]`.
/// Same per-round window/cursor/watermark discipline as `pipeline_sink`.
#[no_mangle]
pub extern "C" fn pipeline_io_sink(in_slot: u32, out_io_slot: u32) {
    let all = ShmApi::read_all_stream_records(in_slot);
    let (cursor, start, end) = pipe_read_window(in_slot, all.len());
    let new_recs = &all[start..end];
    let count = new_recs.len() as u32;
    if count > 0 {
        let mut value_sum: u64 = 0;
        for (_origin, rec) in new_recs {
            if let Ok(s) = core::str::from_utf8(rec) {
                if let Some(v_str) = s.split(',').nth(2).and_then(|p| p.split('=').nth(1)) {
                    if let Ok(v) = v_str.trim_end_matches("|T").parse::<u64>() {
                        value_sum += v;
                    }
                }
            }
        }
        let summary = alloc::format!("batch_count={},value_sum={}", count, value_sum);
        ShmApi::write_output_to(out_io_slot, summary.as_bytes());
    }
    cursor.store(end as u64, Ordering::Release);
}
