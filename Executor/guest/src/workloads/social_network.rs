// ─────────────────────────────────────────────────────────────────────────────
// SocialNetwork — RTSFaaS stateful event-stream workload, re-implemented on our
// stream-processing model. (Workload #6 companion to media_review.rs; spec frozen
// under Tests/Streaming Application_Benchmark/baseline/RTSFaaS/SocialNetwork/.)
//
// Same scoping as MediaReview: we re-implement the *application semantics* (event
// mix + keyed state access), NOT the RTSFaaS transactional stack. Four event
// DAGs from SocialNetwork.java, over three keyed tables held as keyed SHM state:
//
//   table 0  user_pwd      userLogin   : READ password, compare
//   table 1  user_profile  userProfile : READ profile
//   table 2  tweet         getTimeLine : READ tweet (×2 keys per event)
//                          postTweet    : WRITE tweet (×2 keys per event)
//
// The shipped Env/SocialNetwork.env config drives postTweet only
// (eventRatio=0;0;0;100); gen_events.py can mix the other three in.
//
// DAG: Input(events.csv) → sn_parse(n) ─┐ partitions events BY KEY into
//      sn_seed (seed pwd+profile+tweet)   ┴→ slots BASE..BASE+n
//      → sn_apply × n (partial tally → +100) → Aggregate(→300) → sn_summary → Output
//
// As in media_review.rs the partition-by-key is folded into the parse stage
// (slot = BASE + key % n); our Shuffle routes whole slots, not records. The
// cross-node RDMA routing variant is the TODO.
//
// Event record (one text line per event, from gen_events.py):
//   "<op>,<key>[,<val>]"   op ∈ {login,profile,timeline,post}
//   login    : <val> password attempt (generator emits the correct password).
//   profile  : read user_profile[key].
//   timeline : read tweet[key] and tweet[key+1] (the 2-key getTimeLine).
//   post     : write tweet[key] and tweet[key+1] = <val> (the 2-key postTweet).
// ─────────────────────────────────────────────────────────────────────────────

use alloc::vec::Vec;
use crate::api::ShmApi;

const T_PWD: u32 = 0;
const T_PROFILE: u32 = 1;
const T_TWEET: u32 = 2;

const BASE_SLOT: u32 = 10;
const PARTIAL_OFFSET: u32 = 100;

fn state_hash(table: u32, key: u32) -> u32 {
    let mut h: u32 = 2166136261;
    for b in table.to_le_bytes().iter().chain(key.to_le_bytes().iter()) {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

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

fn state_write(table: u32, key: u32, val: &[u8]) {
    let kh = state_hash(table, key);
    let mut buf = Vec::with_capacity(8 + val.len());
    buf.extend_from_slice(&table.to_le_bytes());
    buf.extend_from_slice(&key.to_le_bytes());
    buf.extend_from_slice(val);
    ShmApi::insert_shared_data(kh, 0, &buf);
}

/// Seed user_pwd ("pw<key>"), user_profile ("profile<key>") and the tweet table
/// ("tweet<key>") with `n` deterministic entries each, so login/profile/timeline
/// reads hit. arg = number of users (the tweet table is sized n+1 so the 2-key
/// `timeline`/`post` access of key+1 always lands in range).
#[no_mangle]
pub extern "C" fn sn_seed(n_users: u32) {
    for key in 0..n_users {
        state_write(T_PWD, key, alloc::format!("pw{}", key).as_bytes());
        state_write(T_PROFILE, key, alloc::format!("profile{}", key).as_bytes());
        state_write(T_TWEET, key, alloc::format!("tweet{}", key).as_bytes());
    }
    state_write(T_TWEET, n_users, alloc::format!("tweet{}", n_users).as_bytes());
}

/// Parse the event stream from the Input I/O slot and partition each event BY KEY
/// into one of `n_partitions` slots (slot = BASE_SLOT + key % n). arg = n_partitions.
#[no_mangle]
pub extern "C" fn sn_parse(n_partitions: u32) {
    let n = n_partitions.max(1);
    let records = ShmApi::read_all_inputs();
    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("");
        for line in s.split('\n') {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let key: u32 = line.split(',').nth(1)
                .and_then(|k| k.trim().parse().ok()).unwrap_or(0);
            ShmApi::append_stream_data(BASE_SLOT + key % n, line.as_bytes());
        }
    }
}

/// Apply the per-event op against keyed SHM state for this partition. Reads
/// `in_slot`, writes a partial tally to `in_slot + PARTIAL_OFFSET`. arg = in_slot.
#[no_mangle]
pub extern "C" fn sn_apply(in_slot: u32) {
    let records = ShmApi::read_all_stream_records(in_slot);
    let mut events: u64 = 0;
    let mut login_ok: u64 = 0;
    let mut profile_reads: u64 = 0;
    let mut timeline_reads: u64 = 0;
    let mut tweet_writes: u64 = 0;

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
            "profile" => {
                if state_read(T_PROFILE, key).is_some() { profile_reads += 1; }
            }
            "timeline" => {
                // getTimeLine touches two tweet keys (key, key+1).
                if state_read(T_TWEET, key).is_some() { timeline_reads += 1; }
                if state_read(T_TWEET, key + 1).is_some() { timeline_reads += 1; }
            }
            "post" => {
                // postTweet writes two tweet keys (key, key+1).
                state_write(T_TWEET, key, val.as_bytes());
                state_write(T_TWEET, key + 1, val.as_bytes());
                tweet_writes += 2;
            }
            _ => {}
        }
    }

    let tally = alloc::format!(
        "events={},login_ok={},profile={},timeline={},tweet={}",
        events, login_ok, profile_reads, timeline_reads, tweet_writes);
    ShmApi::append_stream_data(in_slot + PARTIAL_OFFSET, tally.as_bytes());
}

/// Sum per-partition tallies from `agg_slot` and write the run summary to Output.
#[no_mangle]
pub extern "C" fn sn_summary(agg_slot: u32) {
    let (mut events, mut login_ok, mut profile, mut timeline, mut tweet):
        (u64, u64, u64, u64, u64) = (0, 0, 0, 0, 0);
    for (_origin, rec) in &ShmApi::read_all_stream_records(agg_slot) {
        let s = core::str::from_utf8(rec).unwrap_or("");
        for field in s.split(',') {
            let mut kv = field.splitn(2, '=');
            let k = kv.next().unwrap_or("");
            let v: u64 = kv.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            match k {
                "events" => events += v,
                "login_ok" => login_ok += v,
                "profile" => profile += v,
                "timeline" => timeline += v,
                "tweet" => tweet += v,
                _ => {}
            }
        }
    }
    ShmApi::write_output_str("=== social_network_results ===");
    ShmApi::write_output_str(&alloc::format!("total_events={}", events));
    ShmApi::write_output_str(&alloc::format!("login_ok={}", login_ok));
    ShmApi::write_output_str(&alloc::format!("profile_reads={}", profile));
    ShmApi::write_output_str(&alloc::format!("timeline_reads={}", timeline));
    ShmApi::write_output_str(&alloc::format!("tweet_writes={}", tweet));
}
