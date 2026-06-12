// wc.rs — the WASM (Faaslet) word-count function for the Faasm-like demo.
//
// One module, two modes (chosen by argv[1]), mirroring the Faasm port's
// `wc_mapper` / `wc_reducer` (../wordcount.cpp): same tokenization (split on
// non-alpha, lowercase) and same wire format ("word\x1fcount\n") so the
// serialized state is byte-comparable. Compiled to wasm32-wasip1 and run as a
// fresh wasmtime instance per call — the Faaslet isolation we're abstracting.
//
//   wc map     : stdin = a corpus chunk        -> stdout = serialized partial counts
//   wc reduce  : stdin = concatenated partials -> stdout = merged counts
//
// stdin is processed in a STREAM (64 KB blocks), carrying word/line state across
// block boundaries — a single read_to_end into wasm32 linear memory fails past
// ~128 MB, and streaming also keeps the Faaslet footprint tiny (only the count
// map ever resides in memory). State (chunks in, partial out) crosses through
// Redis on the host side — the serialized KV transfer our SHM page-chain avoids.
use std::collections::BTreeMap;
use std::io::{self, Read, Write};

const SEP: u8 = 0x1f; // unit separator between word and count

// Count alpha-runs (lowercased) across a byte block; `word` carries the
// in-progress token between blocks.
fn count_block(data: &[u8], word: &mut String, counts: &mut BTreeMap<String, u64>) {
    for &b in data {
        if b.is_ascii_alphabetic() {
            word.push((b as char).to_ascii_lowercase());
        } else if !word.is_empty() {
            *counts.entry(std::mem::take(word)).or_insert(0) += 1;
        }
    }
}

// Merge "word\x1fcount\n" records across a byte block; `line` carries an
// incomplete trailing record between blocks.
fn merge_block(data: &[u8], line: &mut Vec<u8>, counts: &mut BTreeMap<String, u64>) {
    for &b in data {
        if b == b'\n' {
            absorb_line(line, counts);
            line.clear();
        } else {
            line.push(b);
        }
    }
}

fn absorb_line(line: &[u8], counts: &mut BTreeMap<String, u64>) {
    if line.is_empty() {
        return;
    }
    if let Some(pos) = line.iter().position(|&b| b == SEP) {
        let w = String::from_utf8_lossy(&line[..pos]).into_owned();
        let c: u64 = std::str::from_utf8(&line[pos + 1..])
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        *counts.entry(w).or_insert(0) += c;
    }
}

fn serialize(counts: &BTreeMap<String, u64>) -> Vec<u8> {
    let mut out = Vec::new();
    for (w, c) in counts {
        out.extend_from_slice(w.as_bytes());
        out.push(SEP);
        out.extend_from_slice(c.to_string().as_bytes());
        out.push(b'\n');
    }
    out
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let is_map = match mode.as_str() {
        "map" => true,
        "reduce" => false,
        _ => {
            eprintln!("usage: wc map|reduce  (mode via argv[1])");
            std::process::exit(1);
        }
    };

    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut word = String::new(); // map: carried token
    let mut line: Vec<u8> = Vec::new(); // reduce: carried record
    let mut stdin = io::stdin();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                eprintln!("stdin read error: {}", e);
                std::process::exit(1);
            }
        };
        if is_map {
            count_block(&buf[..n], &mut word, &mut counts);
        } else {
            merge_block(&buf[..n], &mut line, &mut counts);
        }
    }
    // flush trailing state
    if is_map {
        if !word.is_empty() {
            *counts.entry(word).or_insert(0) += 1;
        }
    } else {
        absorb_line(&line, &mut counts);
    }

    io::stdout().write_all(&serialize(&counts)).ok();
}
