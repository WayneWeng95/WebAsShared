// ─────────────────────────────────────────────────────────────────────────────
// Word count demo  —  parallel map-reduce (10 workers)
//
// Slot layout:
//   I/O  slot 0          : raw input lines (written by host Input node)
//   stream slots 10–19   : per-worker line partitions (written by wc_distribute)
//   stream slots 110–119 : per-worker word-frequency records (written by wc_map)
//   stream slot  200     : merged mapper output (written by host Aggregate node)
//
// DAG stages:
//   Input          → load corpus into I/O slot 0
//   wc_distribute  → fan lines round-robin into stream slots 10–19
//   wc_map × 10   → each worker counts words in its slot, writes to slot+100
//   Aggregate      → merge stream slots 110–119 → slot 200
//   wc_reduce      → aggregate per-word counts from slot 200, write to output
//   Output         → flush output I/O slot to file
//
// Record formats:
//   wc_map  emits : "word=<w>\x1f<count>" (0x1F unit-separator)
//   wc_reduce emits: "<word>: <count>"  (one line per unique word, sorted)
//                    plus a header record "=== word_count ==="
// ─────────────────────────────────────────────────────────────────────────────

use alloc::vec::Vec;
use crate::api::ShmApi;

/// First stream slot used by the distribute stage.
const WC_DIST_BASE: u32 = 10;
/// Map output base: wc_map(slot) writes to stream slot `slot + WC_MAP_OUT_BASE`.
const WC_MAP_OUT_BASE: u32 = 100;

/// Distribute: read every line from the default input I/O slot and append it
/// round-robin to one of the `n_workers` stream slots starting at `WC_DIST_BASE`.
#[no_mangle]
pub extern "C" fn wc_distribute(n_workers: u32) {
    let lines = ShmApi::read_all_inputs();
    for (i, (_origin, line)) in lines.iter().enumerate() {
        let slot = WC_DIST_BASE + (i as u32 % n_workers);
        ShmApi::append_stream_data(slot, line);
    }
}

/// Map: count occurrences of every unique word in stream slot `slot`.
/// Emits one `"word=<w>\x1f<count>"` record per unique word to stream slot
/// `slot + WC_MAP_OUT_BASE`.
#[no_mangle]
pub extern "C" fn wc_map(slot: u32) {
    let mut counts: Vec<(alloc::string::String, u64)> = Vec::new();

    let records = ShmApi::read_all_stream_records(slot);
    for (_origin, rec) in &records {
        let line = core::str::from_utf8(rec).unwrap_or("");
        for token in line.split_whitespace() {
            let word: alloc::string::String = token
                .chars()
                .filter(|c| c.is_alphabetic())
                .map(|c| {
                    if c >= 'A' && c <= 'Z' { (c as u8 + 32) as char } else { c }
                })
                .collect();
            if word.is_empty() { continue; }
            match counts.iter_mut().find(|(w, _)| w == &word) {
                Some((_, n)) => *n += 1,
                None => counts.push((word, 1)),
            }
        }
    }

    let out_slot = slot + WC_MAP_OUT_BASE;
    for (word, count) in &counts {
        let rec = alloc::format!("word={}\x1f{}", word, count);
        ShmApi::append_stream_data(out_slot, rec.as_bytes());
    }
}

/// Reduce: read all `"word=<w>\x1f<count>"` records from `stream_slot`,
/// merge counts, sort alphabetically, and write to `OUTPUT_IO_SLOT`.
#[no_mangle]
pub extern "C" fn wc_reduce(stream_slot: u32) {
    let mut totals: Vec<(alloc::string::String, u64)> = Vec::new();

    let records = ShmApi::read_all_stream_records(stream_slot);
    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("");
        let body = match s.strip_prefix("word=") {
            Some(b) => b,
            None => continue,
        };
        let sep = match body.find('\x1f') {
            Some(i) => i,
            None => continue,
        };
        let word = &body[..sep];
        let count: u64 = body[sep + 1..].parse().unwrap_or(0);
        if word.is_empty() { continue; }
        match totals.iter_mut().find(|(w, _)| w == word) {
            Some((_, n)) => *n += count,
            None => totals.push((alloc::string::String::from(word), count)),
        }
    }

    totals.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));

    let unique = totals.len();
    let total: u64 = totals.iter().map(|(_, n)| n).sum();

    ShmApi::write_output_str("=== word_count ===");
    ShmApi::write_output_str(&alloc::format!("map_records_received={}", records.len()));
    ShmApi::write_output_str(&alloc::format!("unique_words={}", unique));
    ShmApi::write_output_str(&alloc::format!("total_occurrences={}", total));
    for (word, count) in &totals {
        ShmApi::write_output_str(&alloc::format!("{}: {}", word, count));
    }
}
