// ─────────────────────────────────────────────────────────────────────────────
// TeraSort  —  data-parallel sort with an all-to-all shuffle (workload #7)
//
// Unlike WordCount (per-mapper partial maps gathered 1-per-peer), TeraSort moves
// the ENTIRE dataset once across an N×N exchange: every worker routes each of its
// records to the range-owner for that key, so the whole input is repartitioned.
// On our system those records cross the shuffle zero-copy (the host Aggregate
// splices page chains); the KV baselines serialize every record through Redis.
//
// Record format (one per input line, newline already stripped by the host):
//   bytes 0..KEY_LEN   : 10-byte sort key (printable ASCII)
//   bytes KEY_LEN..    : payload (Gensort convention: 100-byte records)
//
// Slot layout (N = fan-out):
//   I/O slot 0                : raw input records           (host Input node)
//   stream slots 10 .. 10+N   : per-worker contiguous shards (ts_distribute)
//   stream slots 100 + i*N+j  : worker i's records bound for owner j (ts_partition)
//   stream slots 400 + j      : owner j's gathered range       (host Aggregate ×N)
//   I/O slots   2 + j         : owner j's per-range summary    (ts_merge → Output)
//
// DAG stages:
//   Input         → load records into I/O slot 0
//   ts_distribute → zero-copy contiguous split into N worker shards (slots 10..)
//   ts_partition×N→ each worker routes its records by key into N owner sub-slots
//   Aggregate ×N  → owner j gathers {100+i*N+j : i} → slot 400+j (zero-copy splice)
//   ts_merge ×N   → owner j sorts its range; emits summary to I/O slot 2+j
//   Output ×N     → flush each range summary to a part file
//
// Range partitioning is BY KEY-PREFIX over a fixed key space — no sampling stage:
// keys are uniform printable ASCII, so owner = (first_byte - LO) * N / SPAN gives
// balanced, contiguous, monotonic ranges.  Concatenating owners 0..N-1 in order
// is therefore globally sorted.  The per-range key checksum (Σ of all key bytes)
// and record count are fan-out-invariant: identical at every N for a given input,
// so any dropped/duplicated record in the shuffle is caught.
// ─────────────────────────────────────────────────────────────────────────────

use crate::api::ShmApi;

/// First stream slot used by the distribute stage (per-worker input shards).
const TS_DIST_BASE: u32 = 10;
/// Base for per-(worker,owner) partition sub-slots: slot = TS_PART_BASE + i*N + j.
const TS_PART_BASE: u32 = 100;
// (The per-owner gathered range slots, base 400 + j, are produced by the host
//  Aggregate nodes and passed to ts_merge via `arg`, so the guest needs no const.)
/// Base I/O slot for per-range summaries (0 = input, 1 = default output).
const TS_OUT_BASE: u32 = 2;

/// Sort-key length in bytes (Gensort: 10-byte key + 90-byte payload).
const KEY_LEN: usize = 10;
/// Printable-ASCII key-space bounds used by `gen_records.py` (inclusive lo..hi).
/// SPAN = 64 (a power of two) so the byte→symbol map is bias-free (256 % 64 == 0)
/// and ranges split evenly for every fan-out N in {1,2,4,8,16}.
const KEY_LO: u32 = 33;    // '!'
const KEY_HI: u32 = 96;    // '`'
const KEY_SPAN: u32 = KEY_HI - KEY_LO + 1; // 64 distinct first-byte values

/// Map a record's first key byte to its range-owner in `[0, n)`.
/// Monotonic in the byte value, so ranges are contiguous in sort order.
#[inline]
fn owner_of(first_byte: u8, n: u32) -> u32 {
    let b = (first_byte as u32).clamp(KEY_LO, KEY_HI) - KEY_LO;
    let o = b * n / KEY_SPAN;
    if o >= n { n - 1 } else { o }
}

/// Distribute the input across `n` worker shards starting at `TS_DIST_BASE`,
/// **zero-copy** (contiguous, record-aligned page-chain relink — only the ≤1-page
/// seam at each cut is copied). Sort is a permutation, so contiguous shards yield
/// the same global result; only which worker first sees which records changes.
///
/// `arg` packs `base | (n << 16)` (partitioner form); the legacy bare-count form
/// (high bits clear) falls back to `TS_DIST_BASE`.
#[no_mangle]
pub extern "C" fn ts_distribute(arg: u32) {
    let (out_base, n) = super::unpack_fanout_arg(arg, TS_DIST_BASE);
    if n == 0 { return; }
    ShmApi::split_input_contiguous(common::INPUT_IO_SLOT, out_base, n);
}

/// Partition: worker `i` reads its shard (stream slot `in_slot`) and routes every
/// record to its range-owner's sub-slot `TS_PART_BASE + i*N + owner`.
///
/// `arg` packs `in_slot | (N << 16)`. The worker index is recovered as
/// `i = in_slot - TS_DIST_BASE`, matching `ts_distribute`'s contiguous layout.
#[no_mangle]
pub extern "C" fn ts_partition(arg: u32) {
    let in_slot = arg & 0xFFFF;
    let n = arg >> 16;
    if n == 0 { return; }
    let i = in_slot.saturating_sub(TS_DIST_BASE);
    let out_base = TS_PART_BASE + i * n;

    // STREAM the shard (bounded heap = one record): the records the host
    // Aggregate later splices into each owner's range come from these sub-slots,
    // so the only data that must reside is the routed copy — not a second full
    // heap copy of the shard. The host frees this shard (per-shard FreeSlots)
    // right after, so the input pages don't coexist with the routed copies.
    ShmApi::for_each_stream_record(in_slot, |_origin, rec| {
        if rec.is_empty() { return; }
        let owner = owner_of(rec[0], n);
        ShmApi::append_stream_data(out_base + owner, rec);
    });
}

/// Merge: owner `j` reads its gathered range (stream slot `in_slot`), sorts it by
/// the 10-byte key, and emits a single summary record describing the range. The
/// sorted bytes never leave SHM — for the benchmark we only need the correctness
/// gate, not a 1 GB sorted file. The summary goes to I/O slot `TS_OUT_BASE + j`.
///
/// `arg` packs `in_slot | (j << 16)` (j = owner index, for the output slot).
/// Summary line:
///   `ts_range range=<j> records=<n> sorted=<0|1> keysum=<dec> first=<hex10> last=<hex10>`
#[no_mangle]
pub extern "C" fn ts_merge(arg: u32) {
    let in_slot = arg & 0xFFFF;
    let j = arg >> 16;

    // STREAM the aggregated range and collect KEYS ONLY (KEY_LEN bytes each), so
    // the merge heap is ~KEY_LEN/record-size (~10%) of the range instead of a
    // second full copy — the record bytes stay in the SHM page chain the host
    // Aggregate spliced in (zero-copy). Sorting keys is the TeraSort order.
    let mut keys: alloc::vec::Vec<[u8; KEY_LEN]> = alloc::vec::Vec::new();
    let mut keysum: u64 = 0;
    ShmApi::for_each_stream_record(in_slot, |_origin, rec| {
        if rec.is_empty() { return; }
        let mut k = [0u8; KEY_LEN];
        let klen = KEY_LEN.min(rec.len());
        k[..klen].copy_from_slice(&rec[..klen]);
        for &b in &k[..klen] { keysum = keysum.wrapping_add(b as u64); }
        keys.push(k);
    });
    keys.sort_unstable();

    let n = keys.len();
    let sorted = 1u32; // sorted by construction (we just sorted the keys)
    let first = keys.first().map(|k| hex_key(k)).unwrap_or_default();
    let last  = keys.last().map(|k| hex_key(k)).unwrap_or_default();

    let summary = alloc::format!(
        "ts_range range={} records={} sorted={} keysum={} first={} last={}",
        j, n, sorted, keysum, first, last,
    );
    ShmApi::write_output_str_to(TS_OUT_BASE + j, &summary);
}

/// Hex-encode the leading key bytes of a record (for cross-range order checks).
fn hex_key(rec: &[u8]) -> alloc::string::String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let klen = KEY_LEN.min(rec.len());
    let mut s = alloc::string::String::with_capacity(klen * 2);
    for &b in &rec[..klen] {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

// ─── Inter-node (cluster) tail: single-output range-summary + finalize ────────
//
// The intra-node path writes ONE output file per range (ts_merge → I/O slot 2+j).
// The cluster partitioner, however, gives every workload a SINGLE Output at the
// reserved OUTPUT_IO_SLOT (1) and treats a terminal Func as writing there — so the
// per-range I/O slots are never flushed and the multi-Output DAG stalls. The
// inter-node DAG therefore funnels the N range merges into ONE reducer, mirroring
// the fan workloads (per-worker partial → gather → single reduce → one Output):
//   ts_range_summary ×N  → emit each range's (records, keysum, sorted) as a STREAM
//                          record so the host Aggregate can gather them
//   ts_finalize          → sum the gathered summaries → ONE output record (slot 1)
// Both are ADDITIVE (new #[no_mangle] fns); ts_merge and the intra path are untouched.

/// Range summary (cluster path): sort range `in_slot` by key and emit its
/// `(records, sorted, keysum)` as a STREAM record on `out_slot` (so a final
/// reducer can gather every range into one output). `arg` packs
/// `in_slot | (out_slot << 16)`. Same sort + checksum as `ts_merge`, but the
/// summary goes to a stream slot instead of a per-range I/O file.
#[no_mangle]
pub extern "C" fn ts_range_summary(arg: u32) {
    let in_slot = arg & 0xFFFF;
    let out_slot = arg >> 16;
    let mut keys: alloc::vec::Vec<[u8; KEY_LEN]> = alloc::vec::Vec::new();
    let mut keysum: u64 = 0;
    ShmApi::for_each_stream_record(in_slot, |_origin, rec| {
        if rec.is_empty() { return; }
        let mut k = [0u8; KEY_LEN];
        let klen = KEY_LEN.min(rec.len());
        k[..klen].copy_from_slice(&rec[..klen]);
        for &b in &k[..klen] { keysum = keysum.wrapping_add(b as u64); }
        keys.push(k);
    });
    keys.sort_unstable();
    let sorted = keys.windows(2).all(|w| w[0] <= w[1]) as u32;
    ShmApi::append_stream_data(out_slot, alloc::format!(
        "ts_range records={} sorted={} keysum={}", keys.len(), sorted, keysum).as_bytes());
}

/// Finalize (cluster path): sum every gathered `ts_range …` summary (aggregated
/// into `arg`'s slot) into ONE output record — total records, total keysum, range
/// count, and whether all ranges were sorted. The fan-out-invariant gate: total
/// records == input count and total keysum identical at every fan-out/placement.
#[no_mangle]
pub extern "C" fn ts_finalize(arg: u32) {
    let mut records: u64 = 0;
    let mut keysum: u64 = 0;
    let mut ranges: u64 = 0;
    let mut all_sorted: u32 = 1;
    for (_origin, rec) in &ShmApi::read_all_stream_records(arg) {
        let s = core::str::from_utf8(rec).unwrap_or("").trim();
        if !s.starts_with("ts_range") { continue; }
        ranges += 1;
        for tok in s.split_whitespace() {
            if let Some(v) = tok.strip_prefix("records=") { records += v.parse().unwrap_or(0); }
            else if let Some(v) = tok.strip_prefix("keysum=") { keysum = keysum.wrapping_add(v.parse().unwrap_or(0)); }
            else if let Some(v) = tok.strip_prefix("sorted=") { if v != "1" { all_sorted = 0; } }
        }
    }
    ShmApi::write_output_str("=== terasort_results ===");
    ShmApi::write_output_str(&alloc::format!("ranges={}", ranges));
    ShmApi::write_output_str(&alloc::format!("records={}", records));
    ShmApi::write_output_str(&alloc::format!("keysum={}", keysum));
    ShmApi::write_output_str(&alloc::format!("sorted={}", all_sorted));
}
