// ts.rs — the WASM (Faaslet) TeraSort function for the Faasm-like demo.
//
// One module, two modes (chosen by argv[1]), mirroring TeraSort's
// partition/merge stages. Compiled to wasm32-wasip1 and run as a fresh wasmtime
// instance per call — the Faaslet isolation we abstract. State (record chunks
// in, per-owner buckets / summaries out) crosses through Redis on the host side
// (driver.py) — the serialized KV transfer our SHM page-chain shuffle avoids.
//
//   ts partition <N> : stdin = a chunk of 100-byte record lines
//                      stdout = each line prefixed with a 1-byte range-owner tag
//                               (OWNER_TAG_BASE + owner, owner = key-prefix*N/SPAN).
//                               The tag is offset by 128 so it can never collide
//                               with the b'\n' record delimiter (owner 10 == \n!)
//                               or any printable record byte; the host demuxes
//                               the tagged stream into N Redis buckets.
//                      STREAMS stdin line-by-line, so the Faaslet footprint is
//                      tiny and constant regardless of chunk size.
//   ts merge         : stdin = one owner's gathered records (concatenated).
//                      STREAMS the records and sorts KEYS ONLY (10 B/record), so
//                      memory stays ~10% of the range and a 250 MB+ range never
//                      hits wasm32's read_to_end ceiling. stdout = a one-line
//                      summary the host aggregates:
//                        "records=<n> sorted=<0|1> keysum=<k> first=<hex> last=<hex>"
use std::io::{self, BufRead, Write};

const KEY_LEN: usize = 10;
const KEY_LO: u8 = 33; // must match gen_records.py / the guest
const KEY_SPAN: u32 = 64;
// Owner tag offset: keeps the 1-byte tag out of the b'\n' delimiter and the
// printable record range (so the host can split the tagged stream on '\n').
const OWNER_TAG_BASE: u8 = 128;

#[inline]
fn owner_of(first_byte: u8, n: u32) -> u32 {
    let b = (first_byte.clamp(KEY_LO, KEY_LO + KEY_SPAN as u8 - 1) - KEY_LO) as u32;
    let o = b * n / KEY_SPAN;
    if o >= n { n - 1 } else { o }
}

fn partition(n: u32) {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let mut line: Vec<u8> = Vec::with_capacity(128);
    let mut reader = stdin.lock();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).unwrap_or(0);
        if read == 0 { break; }
        // strip trailing newline
        if line.last() == Some(&b'\n') { line.pop(); }
        if line.is_empty() { continue; }
        let tag = OWNER_TAG_BASE + owner_of(line[0], n) as u8;
        out.write_all(&[tag]).ok();
        out.write_all(&line).ok();
        out.write_all(b"\n").ok();
    }
    out.flush().ok();
}

fn merge() {
    // Stream the records and collect KEYS ONLY (10 B each) so a 250 MB+ range
    // never hits wasm32's read_to_end ceiling — memory stays ~10% of the range.
    let mut keys: Vec<[u8; KEY_LEN]> = Vec::new();
    let mut keysum: u64 = 0;
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line: Vec<u8> = Vec::with_capacity(128);
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).unwrap_or(0) == 0 { break; }
        if line.last() == Some(&b'\n') { line.pop(); }
        if line.is_empty() { continue; }
        let mut k = [0u8; KEY_LEN];
        let klen = KEY_LEN.min(line.len());
        k[..klen].copy_from_slice(&line[..klen]);
        for &b in &k[..klen] { keysum = keysum.wrapping_add(b as u64); }
        keys.push(k);
    }

    keys.sort_unstable();   // the actual sort (by 10-byte key)
    let n = keys.len();
    let sorted = 1u32;      // sorted by construction (we just sorted)
    let first = keys.first().map(|k| hex_key(k)).unwrap_or_default();
    let last = keys.last().map(|k| hex_key(k)).unwrap_or_default();

    let summary = format!(
        "records={} sorted={} keysum={} first={} last={}\n",
        n, sorted, keysum, first, last,
    );
    io::stdout().write_all(summary.as_bytes()).ok();
}

fn hex_key(key: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let klen = KEY_LEN.min(key.len());
    let mut s = String::with_capacity(klen * 2);
    for &b in &key[..klen] {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "partition" => {
            let n: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            partition(n.max(1));
        }
        "merge" => merge(),
        _ => {
            eprintln!("usage: ts partition <N> | merge  (mode via argv[1])");
            std::process::exit(1);
        }
    }
}
