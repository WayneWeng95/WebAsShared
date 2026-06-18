// ─────────────────────────────────────────────────────────────────────────────
// MediaReview — RTSFaaS stateful event-stream workload, re-implemented on our
// stream-processing model. (Workload #6; spec frozen under
// Tests/Streaming Application_Benchmark/baseline/RTSFaaS/MediaReview/.)
//
// RTSFaaS runs this as a transactional job (TSTREAM CC + TiKV + Java). We do NOT
// reproduce that stack — we re-implement the *application semantics* as a stateful
// streaming DAG and compare on throughput / latency / SHM-billable-memory / RDMA
// bytes (NOT transactional isolation; abortRatio=0 in the app config anyway).
//
// Three keyed state tables, held as keyed SHM state (insert_shared_data /
// read_shared_chain), partitioned across workers by the Shuffle:
//
//   table 0  user_pwd      login  : READ  password, compare to the attempt
//   table 1  movie_rating  rate   : WRITE new rating
//   table 2  movie_review  review : WRITE new review
//
// DAG (single-node auto-runnable; an RDMA two-node variant is a TODO):
//   Input(events.csv) → mr_parse(n) ─┐  partitions events BY KEY into
//   mr_seed (seed user_pwd)           ┴→ slots BASE..BASE+n
//   → mr_apply × n  (each reads its partition slot, writes partial tally to +100)
//   → Aggregate(→300) → mr_summary → Output
//
// The partition-by-key happens inside mr_parse (slot = BASE + key % n) rather
// than via a Shuffle node: our Shuffle routes whole upstream *slots* to
// downstream slots (keyed on upstream_id), not individual records, so folding the
// per-record keying into the parse stage is the single-node fit. The dedicated
// cross-node RDMA routing variant (key-owning node per range) is the TODO.
//
// Event record (one text line per event, produced by gen_events.py):
//   "<op>,<key>,<val>"   op ∈ {login,rate,review}
//   login: <val> is the password attempt (the generator emits the correct
//          password so login_ok == #login events — the deterministic gate).
//   rate/review: <val> is the new value to store.
// ─────────────────────────────────────────────────────────────────────────────

use alloc::vec::Vec;
use crate::api::ShmApi;

// Table ids (kept distinct so the (table,key) pair hashes into disjoint logical
// keys; collisions inside a bucket are tolerated — we filter on read).
const T_PWD: u32 = 0;
const T_RATING: u32 = 1;
const T_REVIEW: u32 = 2;

const BASE_SLOT: u32 = 10;       // mr_parse partitions events into BASE_SLOT..BASE_SLOT+n
const PARTIAL_OFFSET: u32 = 100; // mr_apply(in_slot) writes its tally to in_slot+100

// FNV-1a over (table,key) → bucket-friendly state key.
fn state_hash(table: u32, key: u32) -> u32 {
    let mut h: u32 = 2166136261;
    for b in table.to_le_bytes().iter().chain(key.to_le_bytes().iter()) {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

/// Read the newest value stored under (table,key), or None. read_shared_chain
/// returns every node in the bucket (newest first), possibly from other keys that
/// collided, so we filter on the (table,key) prefix embedded in each record.
fn state_read(table: u32, key: u32) -> Option<Vec<u8>> {
    let kh = state_hash(table, key);
    for (_w, rec) in ShmApi::read_shared_chain(kh) {
        if rec.len() >= 8 {
            let t = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]);
            let k = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
            if t == table && k == key {
                return Some(rec[8..].to_vec());
            }
        }
    }
    None
}

/// Append a value for (table,key). Newest insert shadows older ones on read.
fn state_write(table: u32, key: u32, val: &[u8]) {
    let kh = state_hash(table, key);
    let mut buf = Vec::with_capacity(8 + val.len());
    buf.extend_from_slice(&table.to_le_bytes());
    buf.extend_from_slice(&key.to_le_bytes());
    buf.extend_from_slice(val);
    ShmApi::insert_shared_data(kh, 0, &buf);
}

/// Seed the `user_pwd` table with `n` deterministic passwords "pw<key>" so that
/// login events (which carry the matching password) verify. The rating/review
/// tables need no pre-seed — those events are writes. arg = number of users.
#[no_mangle]
pub extern "C" fn mr_seed(n_users: u32) {
    for key in 0..n_users {
        let pw = alloc::format!("pw{}", key);
        state_write(T_PWD, key, pw.as_bytes());
    }
}

/// Parse the event stream from the Input I/O slot and partition each event BY KEY
/// into one of `n_partitions` slots (slot = BASE_SLOT + key % n). arg = n_partitions
/// (kept small so it doubles safely as a slot candidate in any partitioner scan).
#[no_mangle]
pub extern "C" fn mr_parse(n_partitions: u32) {
    let n = n_partitions.max(1);
    let records = ShmApi::read_all_inputs();
    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("");
        for line in s.split('\n') {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            // key is the 2nd CSV field; route by key so a logical key always
            // lands on the same partition (state-access locality).
            let key: u32 = line.split(',').nth(1)
                .and_then(|k| k.trim().parse().ok()).unwrap_or(0);
            ShmApi::append_stream_data(BASE_SLOT + key % n, line.as_bytes());
        }
    }
}

/// Apply the per-event op against the keyed SHM state for this partition.
/// Reads its partition slot `in_slot` (set by the Shuffle), writes a partial
/// tally to `in_slot + PARTIAL_OFFSET`. arg = in_slot.
#[no_mangle]
pub extern "C" fn mr_apply(in_slot: u32) {
    let records = ShmApi::read_all_stream_records(in_slot);
    let mut events: u64 = 0;
    let mut login_ok: u64 = 0;
    let mut rate_writes: u64 = 0;
    let mut review_writes: u64 = 0;

    for (_origin, rec) in &records {
        let s = match core::str::from_utf8(rec) { Ok(s) => s, Err(_) => continue };
        let mut it = s.splitn(3, ',');
        let op = match it.next() { Some(o) => o, None => continue };
        let key: u32 = match it.next().and_then(|k| k.trim().parse().ok()) {
            Some(k) => k, None => continue,
        };
        let val = it.next().unwrap_or("");
        events += 1;
        match op {
            "login" => {
                if let Some(stored) = state_read(T_PWD, key) {
                    if stored == val.as_bytes() { login_ok += 1; }
                }
            }
            "rate" => {
                state_write(T_RATING, key, val.as_bytes());
                rate_writes += 1;
            }
            "review" => {
                state_write(T_REVIEW, key, val.as_bytes());
                review_writes += 1;
            }
            _ => {}
        }
    }

    let tally = alloc::format!(
        "events={},login_ok={},rate={},review={}",
        events, login_ok, rate_writes, review_writes);
    ShmApi::append_stream_data(in_slot + PARTIAL_OFFSET, tally.as_bytes());
}

/// Sum the per-partition tallies from `agg_slot`, write the run summary to the
/// Output node. The four totals are the deterministic correctness gate.
#[no_mangle]
pub extern "C" fn mr_summary(agg_slot: u32) {
    let (mut events, mut login_ok, mut rate, mut review): (u64, u64, u64, u64) = (0, 0, 0, 0);
    for (_origin, rec) in &ShmApi::read_all_stream_records(agg_slot) {
        let s = core::str::from_utf8(rec).unwrap_or("");
        for field in s.split(',') {
            let mut kv = field.splitn(2, '=');
            let k = kv.next().unwrap_or("");
            let v: u64 = kv.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            match k {
                "events" => events += v,
                "login_ok" => login_ok += v,
                "rate" => rate += v,
                "review" => review += v,
                _ => {}
            }
        }
    }
    ShmApi::write_output_str("=== media_review_results ===");
    ShmApi::write_output_str(&alloc::format!("total_events={}", events));
    ShmApi::write_output_str(&alloc::format!("login_ok={}", login_ok));
    ShmApi::write_output_str(&alloc::format!("rate_writes={}", rate));
    ShmApi::write_output_str(&alloc::format!("review_writes={}", review));
}
