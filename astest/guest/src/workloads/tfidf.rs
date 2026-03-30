// ─────────────────────────────────────────────────────────────────────────────
// TF-IDF demo  —  distributed feature extraction (8 workers)
//
// Slot layout:
//   I/O  slot 0          : raw input lines (written by host Input node)
//   stream slots 10–17   : per-worker line partitions (reuses wc_distribute)
//   stream slots 110–117 : per-worker TF/DF records   (written by tfidf_map)
//   stream slot  200     : merged mapper output (written by host Aggregate node)
//
// DAG stages:
//   Input           → load corpus into I/O slot 0
//   wc_distribute   → fan lines round-robin into stream slots 10–17  (8 workers)
//   tfidf_map × 8   → each worker emits TF/DF/docs records to slot+100
//   Aggregate       → merge stream slots 110–117 → slot 200
//   tfidf_reduce    → compute TF-IDF scores, write top-50 to OUTPUT_IO_SLOT
//   Output          → flush output I/O slot to file
//
// Record formats emitted by tfidf_map:
//   "docs=<N>"                          — doc count for this shard
//   "tf\x1f<word>\x1f<count>"           — raw term frequency
//   "df\x1f<word>\x1f<count>"           — document frequency (# docs with word)
//
// IDF approximation:
//   IDF(w) = floor_log2(total_docs / df(w) + 1)
//   This avoids f64::ln which is unavailable in no_std.  The relative ranking
//   of high-IDF (rare) vs low-IDF (common) terms is preserved.
// ─────────────────────────────────────────────────────────────────────────────

use alloc::vec::Vec;
use crate::api::ShmApi;

const TFIDF_MAP_OUT_BASE: u32 = 100;
const MIN_WORD_LEN: usize = 3;
const TOP_K: usize = 50;

/// Integer log2 floor: returns 0 for n ≤ 1, otherwise floor(log2(n)).
#[inline]
fn floor_log2(n: u32) -> u32 {
    if n <= 1 { 0 } else { 31 - n.leading_zeros() }
}

/// Map: compute term frequency (TF) and document frequency (DF) for all
/// records in stream slot `slot`.  Emits docs/TF/DF records to slot+100.
#[no_mangle]
pub extern "C" fn tfidf_map(slot: u32) {
    let mut tf: Vec<(alloc::string::String, u64)> = Vec::new();
    let mut df: Vec<(alloc::string::String, u64)> = Vec::new();
    let mut doc_count: u64 = 0;

    let records = ShmApi::read_all_stream_records(slot);
    for (_origin, rec) in &records {
        let line = core::str::from_utf8(rec).unwrap_or("");
        let mut seen_in_doc: Vec<alloc::string::String> = Vec::new();

        for token in line.split_whitespace() {
            let word: alloc::string::String = token
                .chars()
                .filter(|c| c.is_alphabetic())
                .map(|c| if c >= 'A' && c <= 'Z' { (c as u8 + 32) as char } else { c })
                .collect();
            if word.len() < MIN_WORD_LEN { continue; }

            match tf.iter_mut().find(|(w, _)| w == &word) {
                Some((_, n)) => *n += 1,
                None => tf.push((word.clone(), 1)),
            }
            if !seen_in_doc.iter().any(|w| w == &word) {
                seen_in_doc.push(word);
            }
        }

        for word in seen_in_doc {
            match df.iter_mut().find(|(w, _)| w == &word) {
                Some((_, n)) => *n += 1,
                None => df.push((word, 1)),
            }
        }
        doc_count += 1;
    }

    let out_slot = slot + TFIDF_MAP_OUT_BASE;
    ShmApi::append_stream_data(out_slot,
        alloc::format!("docs={}", doc_count).as_bytes());
    for (word, count) in &tf {
        ShmApi::append_stream_data(out_slot,
            alloc::format!("tf\x1f{}\x1f{}", word, count).as_bytes());
    }
    for (word, count) in &df {
        ShmApi::append_stream_data(out_slot,
            alloc::format!("df\x1f{}\x1f{}", word, count).as_bytes());
    }
}

/// Reduce: merge all TF/DF/docs records from stream slot `stream_slot`,
/// compute TF-IDF scores, sort descending, write top-50 to OUTPUT_IO_SLOT.
#[no_mangle]
pub extern "C" fn tfidf_reduce(stream_slot: u32) {
    let mut tf_total: Vec<(alloc::string::String, u64)> = Vec::new();
    let mut df_total: Vec<(alloc::string::String, u64)> = Vec::new();
    let mut total_docs: u64 = 0;

    let records = ShmApi::read_all_stream_records(stream_slot);
    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("");
        if let Some(rest) = s.strip_prefix("docs=") {
            total_docs += rest.parse::<u64>().unwrap_or(0);
            continue;
        }
        let parts: Vec<&str> = s.splitn(3, '\x1f').collect();
        if parts.len() != 3 { continue; }
        let (kind, word, val_str) = (parts[0], parts[1], parts[2]);
        let val: u64 = val_str.parse().unwrap_or(0);
        let table = match kind {
            "tf" => &mut tf_total,
            "df" => &mut df_total,
            _    => continue,
        };
        match table.iter_mut().find(|(w, _)| w == word) {
            Some((_, n)) => *n += val,
            None => table.push((alloc::string::String::from(word), val)),
        }
    }

    if total_docs == 0 { return; }

    // score(w) = tf(w) * floor_log2(total_docs / df(w) + 1)
    let mut scores: Vec<(alloc::string::String, u64)> = Vec::new();
    for (word, tf) in &tf_total {
        let df = df_total.iter()
            .find(|(w, _)| w == word)
            .map(|(_, n)| *n)
            .unwrap_or(1)
            .max(1);
        let idf = floor_log2((total_docs / df + 1) as u32);
        scores.push((word.clone(), tf * idf as u64));
    }

    scores.sort_by(|(_, a), (_, b)| b.cmp(a));
    scores.truncate(TOP_K);

    ShmApi::write_output_str("=== tfidf_results ===");
    ShmApi::write_output_str(&alloc::format!("total_docs={}", total_docs));
    ShmApi::write_output_str(&alloc::format!("unique_terms={}", tf_total.len()));
    ShmApi::write_output_str("--- top_50_tfidf_terms ---");
    for (word, score) in &scores {
        ShmApi::write_output_str(&alloc::format!("{}: {}", word, score));
    }
}
