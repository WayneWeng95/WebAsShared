//! Native (non-WASM) split & merge for job-level divide-and-merge (Goal 1).
//!
//! These run as ordinary host-side processes — no WASM guest, so no 4 GiB
//! wasm32 address-space cap. A large input that can't be loaded into a single
//! executor's SHM window is split into `N` shard files here; each shard is then
//! processed by its own WASM executor (its own SHM region); finally the smaller
//! partial outputs are folded back together here.
//!
//! Subcommands (see `main.rs`):
//!   host split <input> <N> <out_prefix>        → out_prefix.0 .. out_prefix.{N-1}
//!   host merge <reducer> <output> <partial...> → folded final output
//!
//! P1 keeps the split record/line-oriented (works for text corpora such as the
//! WordCount inputs) and ships the `wordcount` reducer plus the trivially-general
//! `concat` and `counters` reducers. Size-based auto-split and a workload-supplied
//! native merge source are deferred (see `Dev_docs/parallel_jobs.md`).

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};

/// `host split <input> <N> <out_prefix>`
///
/// Splits `input` into `N` shards on line boundaries, so no record is torn
/// across a shard. Shards are balanced by byte size: each line is appended to
/// the shard whose running byte total is currently smallest is *not* used —
/// instead we cut on cumulative byte thresholds so shard order matches input
/// order (cheaper, and order-preserving for merges that care).
///
/// Shards are written as `{out_prefix}.0 .. {out_prefix}.{N-1}`. Empty trailing
/// shards can occur if the input has fewer lines than `N`; they are still
/// created (empty) so downstream code can rely on exactly `N` files existing.
pub fn run_split(input: &str, n: usize, out_prefix: &str) -> Result<()> {
    if n == 0 {
        bail!("split: N must be >= 1");
    }

    let total_bytes = std::fs::metadata(input)
        .with_context(|| format!("stat input file: {}", input))?
        .len();

    // Byte threshold at which we advance to the next shard. Cutting on cumulative
    // bytes (snapped to the next line boundary) keeps shards contiguous and in
    // input order, and balances them by size regardless of line-length variance.
    let per_shard = (total_bytes as usize).div_ceil(n).max(1);

    let f = File::open(input).with_context(|| format!("open input file: {}", input))?;
    let mut reader = BufReader::new(f);

    let mut writers: Vec<BufWriter<File>> = (0..n)
        .map(|i| {
            let path = format!("{}.{}", out_prefix, i);
            File::create(&path)
                .map(BufWriter::new)
                .with_context(|| format!("create shard file: {}", path))
        })
        .collect::<Result<_>>()?;

    let mut shard = 0usize;
    let mut written_in_shard = 0usize;
    let mut line = Vec::<u8>::new();
    loop {
        line.clear();
        // read_until keeps the trailing '\n' so shards can be concatenated to
        // reconstruct the original byte-for-byte.
        let n_read = reader.read_until(b'\n', &mut line).context("read input line")?;
        if n_read == 0 {
            break; // EOF
        }
        // Advance to the next shard once this one has met its byte quota — but
        // never past the last shard, and never leave the current shard empty
        // (avoids a torn first line landing alone).
        if shard + 1 < n && written_in_shard >= per_shard {
            shard += 1;
            written_in_shard = 0;
        }
        writers[shard].write_all(&line).context("write shard line")?;
        written_in_shard += n_read;
    }

    for (i, w) in writers.iter_mut().enumerate() {
        w.flush().with_context(|| format!("flush shard {}", i))?;
    }

    let sizes: Vec<u64> = (0..n)
        .map(|i| std::fs::metadata(format!("{}.{}", out_prefix, i)).map(|m| m.len()).unwrap_or(0))
        .collect();
    println!(
        "[split] {} ({} bytes) → {} shards: {:?}",
        input, total_bytes, n, sizes
    );
    Ok(())
}

/// `host merge <reducer> <output> <partial...>`
///
/// Folds the partial outputs of the shard executors into a single final file
/// using the named reducer.
pub fn run_merge(reducer: &str, output: &str, partials: &[String]) -> Result<()> {
    if partials.is_empty() {
        bail!("merge: need at least one partial file");
    }
    // Reducers may carry an argument after ':' — e.g. `topk:100`.
    let (name, arg) = match reducer.split_once(':') {
        Some((n, a)) => (n, Some(a)),
        None => (reducer, None),
    };
    let merged: Vec<u8> = match name {
        "wordcount" => merge_wordcount(partials)?.into_bytes(),
        "counters" => merge_counters(partials)?.into_bytes(),
        "concat" => merge_concat(partials)?,
        "sum" => merge_sum(partials)?.into_bytes(),
        "topk" => {
            let k: usize = arg
                .ok_or_else(|| anyhow::anyhow!("merge: topk needs a count, e.g. `topk:100`"))?
                .parse()
                .context("merge: topk count must be a non-negative integer")?;
            merge_topk(partials, k)?.into_bytes()
        }
        other => bail!(
            "merge: unknown reducer '{}' (known: wordcount, counters, concat, sum, topk:K)",
            other
        ),
    };
    std::fs::write(output, &merged)
        .with_context(|| format!("write merged output: {}", output))?;
    println!(
        "[merge] reducer={} {} partial(s) → {} ({} bytes)",
        reducer, partials.len(), output, merged.len()
    );
    Ok(())
}

/// WordCount reducer.
///
/// Each partial is a WordCount `Output` file of the form:
/// ```text
/// === word_count ===
/// map_records_received=NNN
/// unique_words=NNN
/// total_occurrences=NNN
/// <word>: <count>
/// ...
/// ```
/// We sum the per-word counts across all partials, recompute the header
/// aggregates, and emit the same format. `map_records_received` /
/// `total_occurrences` are summed; `unique_words` is recomputed from the merged
/// map. Any header line we don't recognise is ignored (the body is the source of
/// truth). Word lines are `"<word>: <count>"`.
fn merge_wordcount(partials: &[String]) -> Result<String> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut map_records_received: u64 = 0;

    for path in partials {
        let f = File::open(path).with_context(|| format!("open partial: {}", path))?;
        for line in BufReader::new(f).lines() {
            let line = line.with_context(|| format!("read partial line: {}", path))?;
            let line = line.trim();
            if line.is_empty() || line.starts_with("=== ") {
                continue;
            }
            // Header meta lines: key=value.
            if let Some((k, v)) = line.split_once('=') {
                // Only when it looks like a header counter (no ": " word body).
                if !line.contains(": ") {
                    if k.trim() == "map_records_received" {
                        map_records_received += v.trim().parse::<u64>().unwrap_or(0);
                    }
                    // unique_words / total_occurrences are recomputed below.
                    continue;
                }
            }
            // Body line: "<word>: <count>".
            if let Some((word, cnt)) = line.rsplit_once(':') {
                let word = word.trim();
                if let Ok(c) = cnt.trim().parse::<u64>() {
                    *counts.entry(word.to_string()).or_insert(0) += c;
                    continue;
                }
            }
            // Unrecognised line — skip quietly (partials only contain the above).
        }
    }

    let total_occurrences: u64 = counts.values().sum();
    let unique_words = counts.len();

    let mut out = String::new();
    out.push_str("=== word_count ===\n");
    out.push_str(&format!("map_records_received={}\n", map_records_received));
    out.push_str(&format!("unique_words={}\n", unique_words));
    out.push_str(&format!("total_occurrences={}\n", total_occurrences));
    for (word, count) in &counts {
        out.push_str(&format!("{}: {}\n", word, count));
    }
    Ok(out)
}

/// Generic counter reducer for `key=value` outputs (e.g. MediaReview's
/// `total_events=…`, `login_ok=…`). Sums every numeric `key=value` line across
/// partials; the first `=== header ===` line seen is preserved. Non-numeric
/// values are kept from the last partial (last-write-wins).
fn merge_counters(partials: &[String]) -> Result<String> {
    let mut header: Option<String> = None;
    let mut order: Vec<String> = Vec::new();
    let mut sums: BTreeMap<String, u64> = BTreeMap::new();
    let mut raw: BTreeMap<String, String> = BTreeMap::new();

    for path in partials {
        let f = File::open(path).with_context(|| format!("open partial: {}", path))?;
        for line in BufReader::new(f).lines() {
            let line = line.with_context(|| format!("read partial line: {}", path))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with("=== ") {
                header.get_or_insert_with(|| line.to_string());
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim().to_string(), v.trim().to_string());
                if !sums.contains_key(&k) && !raw.contains_key(&k) {
                    order.push(k.clone());
                }
                match v.parse::<u64>() {
                    Ok(n) => *sums.entry(k).or_insert(0) += n,
                    Err(_) => {
                        raw.insert(k, v);
                    }
                }
            }
        }
    }

    let mut out = String::new();
    if let Some(h) = header {
        out.push_str(&h);
        out.push('\n');
    }
    for k in &order {
        if let Some(n) = sums.get(k) {
            out.push_str(&format!("{}={}\n", k, n));
        } else if let Some(v) = raw.get(k) {
            out.push_str(&format!("{}={}\n", k, v));
        }
    }
    Ok(out)
}

/// Parse a keyed-numeric line into `(key, count)`. Accepts the three shapes the
/// workloads emit — `key: N` (wordcount body), `key=N` (counters), and
/// `key<whitespace>N` — and ignores blank / `=== header ===` lines and any line
/// whose value isn't a non-negative integer. The separator is the LAST one on
/// the line so keys may themselves contain the separator char.
fn parse_key_num(line: &str) -> Option<(String, u64)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with("=== ") {
        return None;
    }
    for sep in [':', '='] {
        if let Some((k, v)) = line.rsplit_once(sep) {
            if let Ok(n) = v.trim().parse::<u64>() {
                return Some((k.trim().to_string(), n));
            }
        }
    }
    // Fall back to whitespace: "key   N".
    if let Some((k, v)) = line.rsplit_once(char::is_whitespace) {
        if let Ok(n) = v.trim().parse::<u64>() {
            return Some((k.trim().to_string(), n));
        }
    }
    None
}

/// Fold every keyed-numeric line across the partials, summing by key. This is the
/// generic associative reducer for any workload whose partial is a set of
/// `key <sep> count` tallies (the divide-and-merge contract). Output is sorted
/// `key: count`.
fn merge_sum(partials: &[String]) -> Result<String> {
    let counts = fold_key_nums(partials)?;
    let mut out = String::new();
    for (k, v) in &counts {
        out.push_str(&format!("{}: {}\n", k, v));
    }
    Ok(out)
}

/// Like `merge_sum`, but emit only the `k` highest-count keys, ordered by count
/// descending (ties broken by key ascending, so the output is deterministic).
fn merge_topk(partials: &[String], k: usize) -> Result<String> {
    let counts = fold_key_nums(partials)?;
    let mut ranked: Vec<(String, u64)> = counts.into_iter().collect();
    // Sort by count desc, then key asc.
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(k);
    let mut out = String::new();
    for (key, count) in &ranked {
        out.push_str(&format!("{}: {}\n", key, count));
    }
    Ok(out)
}

/// Shared fold used by `sum` and `topk`: sum every parseable keyed-numeric line
/// across all partials into a sorted map.
fn fold_key_nums(partials: &[String]) -> Result<BTreeMap<String, u64>> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for path in partials {
        let f = File::open(path).with_context(|| format!("open partial: {}", path))?;
        for line in BufReader::new(f).lines() {
            let line = line.with_context(|| format!("read partial line: {}", path))?;
            if let Some((k, v)) = parse_key_num(&line) {
                *counts.entry(k).or_insert(0) += v;
            }
        }
    }
    Ok(counts)
}

/// Trivial reducer: concatenate partials in argument order, byte-for-byte.
fn merge_concat(partials: &[String]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for path in partials {
        let mut f = File::open(path).with_context(|| format!("open partial: {}", path))?;
        f.read_to_end(&mut out).with_context(|| format!("read partial: {}", path))?;
    }
    Ok(out)
}
