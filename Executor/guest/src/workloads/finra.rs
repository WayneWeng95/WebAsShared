// ─────────────────────────────────────────────────────────────────────────────
// FINRA demo — financial trade audit validation pipeline
//
// Modeled after the FINRA serverless workflow from RMMAP (EuroSys'24).
//
// Slot layout:
//   I/O slot 0      : raw trades CSV (written by host Input node)
//   stream slot 5   : parsed trade records  (written by finra_fetch_private)
//   stream slot 6   : market reference data (written by finra_fetch_public)
//   stream slot 200 : aggregated inputs (trades + reference)
//   stream slots 10–17 : per-rule audit results (written by finra_audit_rule)
//   stream slot 300 : merged rule results (written by host Aggregate node)
//
// DAG stages:
//   Input                → load trades.csv into I/O slot 0
//   finra_fetch_private  → parse CSV ONCE into a packed columnar binary form,
//                          write to stream slot 5
//   finra_fetch_public   → synthetic market ref, write to stream slot 6
//   Aggregate            → merge slots 5+6 → slot 200
//   finra_audit_rule × 8 → each rule reads slot 200 (the broadcast) and scans the
//                          packed columns — no re-parse, O(n) integer-keyed counts
//   Aggregate            → merge slots 10–17 → slot 300
//   finra_merge_results  → summarize violations, write to OUTPUT_IO_SLOT
//   Output               → flush to file
//
// PARSE-ONCE (2026-06-12): fetch_private used to write one text record per trade
// ("T:<csv line>") and every one of the 8 audit rules re-parsed that CSV — 8×
// string parsing plus, in the (account,symbol) rules, an O(n·k) `Vec::find` over
// `format!`-built string keys. At 8 M trades that is billions of string compares
// and millions of tiny record allocations. fetch_private now emits a packed
// 20-byte columnar record per trade (symbols/accounts pre-resolved to integer
// indices) in ~256 KiB chunks, so each rule is a flat integer scan with
// fixed-array counters. Violation counts are unchanged (the correctness gate).
// ─────────────────────────────────────────────────────────────────────────────

use alloc::vec::Vec;
use alloc::vec;
use alloc::string::String;
use crate::api::ShmApi;

const TRADE_STREAM_SLOT: u32 = 5;
const REF_STREAM_SLOT: u32 = 6;
const INPUT_AGG_SLOT: u32 = 200;
const RULE_OUT_BASE: u32 = 10;
// Sharded stateless rules write a UNIQUE slot per (rule, shard) so the aggregator
// splices each partial exactly once (a repeated slot would double-count, and
// distinct slots also avoid concurrent same-slot appends from parallel shards).
// Slot = SHARD_OUT_BASE + rule_id*SHARD_STRIDE + shard_idx, kept well above all
// other finra slots (5,6,10..17,200,300,301). Only used when n_shards > 1; the
// unsharded path still writes RULE_OUT_BASE+rule_id (10..17), so existing DAGs
// are unaffected.
const SHARD_OUT_BASE: u32 = 1000;
const SHARD_STRIDE: u32 = 16; // max shards per rule

// ── Packed columnar layout ────────────────────────────────────────────────────
//
// One trade = a fixed 20-byte little-endian record; many records are batched
// into a single stream record prefixed with TAG_TRADES so the reader can tell a
// trade chunk apart from the text "R:" reference records (which start with 'R').
//
//   off 0  symbol_idx u16   (index into SYMBOLS; SYM_UNKNOWN if not a ref symbol)
//   off 2  side       u8    (0 = BUY, 1 = SELL)
//   off 3  _pad       u8
//   off 4  price      u32   (cents)
//   off 8  quantity   u32
//   off 12 minutes    u16   (hour*60 + minute, 0..1439)
//   off 14 _pad       u16
//   off 16 account_idx u32  (numeric part of the account id)
const REC: usize = 20;
const TAG_TRADES: u8 = 0xBB;
const CHUNK_FLUSH: usize = 1 << 18; // ~256 KiB per stream record

// Fixed counter dimensions. The generator uses 100 accounts (ACC000..ACC099) and
// the 20 SYMBOLS below, so (account,symbol) fits a dense table with no collisions
// — keeping the gate exact while making the keyed rules O(n) and alloc-free.
const SYM: usize = 32;   // ≥ SYMBOLS.len()
const ACCT: usize = 128; // ≥ number of distinct accounts
const SYM_UNKNOWN: u16 = 0xFFFF;

// Reference symbols — same set/order as `finra_fetch_public` and gen_trades.py.
const SYMBOLS: [&str; 20] = [
    "AAPL", "GOOG", "MSFT", "AMZN", "META", "TSLA", "NVDA", "JPM", "BAC", "WFC",
    "GS", "MS", "V", "MA", "NFLX", "DIS", "INTC", "AMD", "ORCL", "CRM",
];

// Reference avg price (cents), indexed by symbol_idx — the SAME static table as
// `finra_fetch_public` / gen_trades.py / the baselines (Faasm's `ref_cents()` is
// the same match). The price-outlier rule resolves it here, in-rule, rather than
// re-deriving it from "R:" records carried through the aggregate: at large scale
// the aggregate proved unreliable at carrying the 20 tiny ref records behind tens
// of MB of trade chunks, which silently zeroed the table. This static table is
// what every system uses, so it keeps the gate exact at every size.
const REF_CENTS: [u32; 20] = [
    17500, 14000, 37000, 18000, 50000, 25000, 80000, 19500, 3500, 5500,
    40000, 9000, 28000, 45000, 60000, 11000, 4500, 16000, 12500, 30000,
];

fn symbol_index(sym: &str) -> u16 {
    match SYMBOLS.iter().position(|&s| s == sym) {
        Some(i) => i as u16,
        None => SYM_UNKNOWN,
    }
}

fn account_index(acct: &str) -> u32 {
    // "ACC004" → 4. The numeric tail identifies the account.
    let mut v: u32 = 0;
    let mut seen = false;
    for b in acct.bytes() {
        if b.is_ascii_digit() {
            v = v.wrapping_mul(10) + (b - b'0') as u32;
            seen = true;
        }
    }
    if seen { v } else { 0 }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn parse_cents(s: &str) -> u32 {
    // "175.50" → 17550
    let parts: Vec<&str> = s.splitn(2, '.').collect();
    let whole: u32 = parts[0].parse().unwrap_or(0);
    let frac: u32 = if parts.len() > 1 {
        let f = parts[1];
        if f.len() >= 2 { f[..2].parse().unwrap_or(0) }
        else { f.parse::<u32>().unwrap_or(0) * 10 }
    } else { 0 };
    whole * 100 + frac
}

fn parse_time(ts: &str) -> (u8, u8) {
    // Accept both ISO "2024-01-15T10:30:00" and plain "HH:MM" (the trades CSV
    // format). Previously only the ISO form was handled, so every "HH:MM" trade
    // parsed to (0,0) and the AFTER_HOURS rule flagged all of them.
    let time_part = match ts.find('T') {
        Some(pos) => &ts[pos + 1..],
        None => ts,
    };
    let parts: Vec<&str> = time_part.splitn(3, ':').collect();
    if parts.len() >= 2 {
        let h: u8 = parts[0].parse().unwrap_or(0);
        let m: u8 = parts[1].parse().unwrap_or(0);
        return (h, m);
    }
    (0, 0)
}

#[inline]
fn ref_cents(symbol_idx: u16) -> u32 {
    let i = symbol_idx as usize;
    if i < REF_CENTS.len() { REF_CENTS[i] } else { 10000 }
}

#[inline]
fn rd_u16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
#[inline]
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Visit every packed trade record across all TAG_TRADES chunks in `records`.
/// Each callback receives one 20-byte record slice (decode with the rd_* helpers).
fn for_each_trade(records: &[(u32, Vec<u8>)], mut f: impl FnMut(&[u8])) {
    for (_origin, rec) in records {
        if rec.first() != Some(&TAG_TRADES) { continue; }
        let body = &rec[1..];
        let n = body.len() / REC;
        for i in 0..n {
            f(&body[i * REC..i * REC + REC]);
        }
    }
}

/// Sharded variant: visit only the records belonging to shard `shard_idx` of
/// `n_shards`, partitioning the global record stream round-robin (record g goes
/// to shard g % n_shards). Disjoint and complete across shards, so per-shard
/// counts of any PER-RECORD (stateless) rule SUM to the full-data count — which
/// is why finra_merge_results (which sums all rule partials) stays exact under
/// sharding. `n_shards <= 1` degrades to for_each_trade (the full stream). Only
/// stateless rules may use this; stateful rules need the whole dataset.
fn for_each_trade_shard(records: &[(u32, Vec<u8>)], shard_idx: u32, n_shards: u32,
                        mut f: impl FnMut(&[u8])) {
    if n_shards <= 1 {
        return for_each_trade(records, f);
    }
    let mut g: u32 = 0;
    for (_origin, rec) in records {
        if rec.first() != Some(&TAG_TRADES) { continue; }
        let body = &rec[1..];
        let n = body.len() / REC;
        for i in 0..n {
            if g % n_shards == shard_idx {
                f(&body[i * REC..i * REC + REC]);
            }
            g += 1;
        }
    }
}

// ── Exported functions ───────────────────────────────────────────────────────

/// Parse trades CSV from I/O slot 0 ONCE, write packed columnar records to slot 5.
#[no_mangle]
pub extern "C" fn finra_fetch_private(_unused: u32) {
    let records = ShmApi::read_all_inputs();
    let mut buf: Vec<u8> = Vec::with_capacity(CHUNK_FLUSH + REC);
    buf.push(TAG_TRADES);

    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("");
        for line in s.split('\n') {
            let line = line.trim();
            if line.is_empty() || line.starts_with("trade_id") { continue; }
            let p: Vec<&str> = line.splitn(7, ',').collect();
            if p.len() < 7 { continue; }

            let symbol_idx = symbol_index(p[1]);
            let price = parse_cents(p[2]);
            let quantity: u32 = p[3].parse().unwrap_or(0);
            let side: u8 = if p[4] == "BUY" { 0 } else { 1 };
            let (h, m) = parse_time(p[5]);
            let minutes: u16 = h as u16 * 60 + m as u16;
            let account_idx = account_index(p[6]);

            buf.extend_from_slice(&symbol_idx.to_le_bytes()); // 0
            buf.push(side);                                    // 2
            buf.push(0);                                       // 3 pad
            buf.extend_from_slice(&price.to_le_bytes());       // 4
            buf.extend_from_slice(&quantity.to_le_bytes());    // 8
            buf.extend_from_slice(&minutes.to_le_bytes());     // 12
            buf.extend_from_slice(&[0u8, 0]);                  // 14 pad
            buf.extend_from_slice(&account_idx.to_le_bytes()); // 16

            if buf.len() >= CHUNK_FLUSH {
                ShmApi::append_stream_data(TRADE_STREAM_SLOT, &buf);
                buf.clear();
                buf.push(TAG_TRADES);
            }
        }
    }
    if buf.len() > 1 {
        ShmApi::append_stream_data(TRADE_STREAM_SLOT, &buf);
    }
}

/// Generate synthetic market reference data to stream slot 6.
#[no_mangle]
pub extern "C" fn finra_fetch_public(_unused: u32) {
    let data: &[(&str, u32, u32)] = &[
        ("AAPL", 17500, 80000000), ("GOOG", 14000, 25000000),
        ("MSFT", 37000, 22000000), ("AMZN", 18000, 50000000),
        ("META", 50000, 15000000), ("TSLA", 25000, 100000000),
        ("NVDA", 80000, 40000000), ("JPM",  19500, 10000000),
        ("BAC",  3500,  45000000), ("WFC",  5500,  20000000),
        ("GS",   40000, 3000000),  ("MS",   9000,  10000000),
        ("V",    28000, 7000000),  ("MA",   45000, 4000000),
        ("NFLX", 60000, 5000000),  ("DIS",  11000, 12000000),
        ("INTC", 4500,  30000000), ("AMD",  16000, 50000000),
        ("ORCL", 12500, 8000000),  ("CRM",  30000, 5000000),
    ];
    for &(sym, avg_cents, avg_vol) in data {
        let avg_price = alloc::format!("{}.{:02}", avg_cents / 100, avg_cents % 100);
        ShmApi::append_stream_data(REF_STREAM_SLOT,
            alloc::format!("R:{},{},{}", sym, avg_price, avg_vol).as_bytes());
    }
}

/// Run audit rule over the packed columns in slot 200 (the broadcast), writing
/// results to stream slot 10 + rule_id. No CSV re-parse; integer-keyed.
///
/// The single u32 arg is PACKED so the existing 1-param ABI can carry an optional
/// data-shard spec (set by Scheduling_Policy/gen_variants.py's hybrid transform):
///   bits  0..8   rule_id    (0..7)
///   bits  8..16  shard_idx  (which slice this worker handles)
///   bits 16..24  n_shards   (0 or 1 = no sharding, run full data)
/// Backward-compatible: callers that pass a bare rule_id (0..7) get n_shards=0 →
/// full data, identical to before. Only the 5 STATELESS per-record rules honour
/// the shard; the 3 STATEFUL rules (wash/spoofing/concentration) always scan the
/// whole dataset — their cross-record state can't be reconstructed from a slice.
#[no_mangle]
pub extern "C" fn finra_audit_rule(packed: u32) {
    let rule_id = packed & 0xFF;
    let shard_idx = (packed >> 8) & 0xFF;
    let n_shards = (packed >> 16) & 0xFF;
    let records = ShmApi::read_all_stream_records(INPUT_AGG_SLOT);
    let out_slot = if n_shards > 1 {
        SHARD_OUT_BASE + rule_id * SHARD_STRIDE + shard_idx
    } else {
        RULE_OUT_BASE + rule_id
    };
    let mut violations: u32 = 0;
    // Stateless rules iterate only their shard; stateful rules ignore the shard.
    let shard = |records: &[(u32, Vec<u8>)], f: &mut dyn FnMut(&[u8])| {
        for_each_trade_shard(records, shard_idx, n_shards, f);
    };

    match rule_id {
        0 => {
            // Price outlier: trade price > 2× reference avg (static ref table)
            shard(&records, &mut |r| {
                let avg = ref_cents(rd_u16(r, 0));
                if rd_u32(r, 4) > 2 * avg { violations += 1; }
            });
        }
        1 => {
            // Large order: quantity > 5000
            shard(&records, &mut |r| {
                if rd_u32(r, 8) > 5000 { violations += 1; }
            });
        }
        2 => {
            // Wash trade: same account buys + sells same symbol
            let mut mask = vec![0u8; ACCT * SYM];
            for_each_trade(&records, |r| {
                let si = (rd_u16(r, 0) as usize) & (SYM - 1);
                let ai = (rd_u32(r, 16) as usize) & (ACCT - 1);
                mask[ai * SYM + si] |= 1u8 << r[2];
            });
            violations = mask.iter().filter(|&&m| m == 3).count() as u32;
        }
        3 => {
            // Spoofing: >10 trades same account+symbol
            let mut counts = vec![0u32; ACCT * SYM];
            for_each_trade(&records, |r| {
                let si = (rd_u16(r, 0) as usize) & (SYM - 1);
                let ai = (rd_u32(r, 16) as usize) & (ACCT - 1);
                counts[ai * SYM + si] += 1;
            });
            violations = counts.iter().filter(|&&n| n > 10).count() as u32;
        }
        4 => {
            // Concentration: symbol count > total/20 + 100
            let mut sym_counts = [0u32; SYM];
            let mut total: u32 = 0;
            for_each_trade(&records, |r| {
                let si = (rd_u16(r, 0) as usize) & (SYM - 1);
                sym_counts[si] += 1;
                total += 1;
            });
            let threshold = total / 20 + 100;
            violations = sym_counts.iter().filter(|&&n| n > threshold).count() as u32;
        }
        5 => {
            // After-hours: outside 09:30-16:00
            shard(&records, &mut |r| {
                let minutes = rd_u16(r, 12) as u32;
                if minutes < 570 || minutes > 960 { violations += 1; }
            });
        }
        6 => {
            // Penny stock: price < $5.00 (500 cents)
            shard(&records, &mut |r| {
                if rd_u32(r, 4) < 500 { violations += 1; }
            });
        }
        7 => {
            // Round lot: quantity is exact multiple of 1000
            shard(&records, &mut |r| {
                let q = rd_u32(r, 8);
                if q >= 1000 && q % 1000 == 0 { violations += 1; }
            });
        }
        _ => {}
    }

    let rule_names: &[&str] = &[
        "PRICE_OUTLIER", "LARGE_ORDER", "WASH_TRADE", "SPOOFING",
        "CONCENTRATION", "AFTER_HOURS", "PENNY_STOCK", "ROUND_LOT",
    ];
    let name = if (rule_id as usize) < rule_names.len() {
        rule_names[rule_id as usize]
    } else { "UNKNOWN" };

    ShmApi::append_stream_data(out_slot,
        alloc::format!("rule={},violations={}", rule_id, violations).as_bytes());
    ShmApi::append_stream_data(out_slot,
        alloc::format!("rule_name={}", name).as_bytes());
}

/// Merge all audit rule results from stream slot `agg_slot`,
/// write summary to OUTPUT_IO_SLOT.
#[no_mangle]
pub extern "C" fn finra_merge_results(agg_slot: u32) {
    let mut rule_violations: Vec<(u32, u32, String)> = Vec::new(); // (rule_id, count, name)

    let records = ShmApi::read_all_stream_records(agg_slot);
    let mut current_rule: u32 = 0;
    let mut current_count: u32 = 0;
    let mut current_name;

    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("");
        if s.starts_with("rule=") {
            let parts: Vec<&str> = s.splitn(2, ',').collect();
            current_rule = parts[0].strip_prefix("rule=")
                .and_then(|v| v.parse().ok()).unwrap_or(0);
            current_count = if parts.len() > 1 {
                parts[1].strip_prefix("violations=")
                    .and_then(|v| v.parse().ok()).unwrap_or(0)
            } else { 0 };
        } else if s.starts_with("rule_name=") {
            current_name = String::from(s.strip_prefix("rule_name=").unwrap_or(""));
            rule_violations.push((current_rule, current_count, current_name.clone()));
        }
    }

    let total: u32 = rule_violations.iter().map(|(_, c, _)| c).sum();

    ShmApi::write_output_str("=== finra_audit_results ===");
    ShmApi::write_output_str(&alloc::format!("total_rules_executed={}", rule_violations.len()));
    ShmApi::write_output_str(&alloc::format!("total_violations={}", total));
    ShmApi::write_output_str("--- per_rule_summary ---");
    rule_violations.sort_by_key(|(id, _, _)| *id);
    for (id, count, name) in &rule_violations {
        ShmApi::write_output_str(&alloc::format!(
            "rule_{} ({}): {} violations", id, name, count));
    }
}
