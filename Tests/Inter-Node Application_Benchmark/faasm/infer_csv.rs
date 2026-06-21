// infer_csv.rs — Faasm MNIST-inference Faaslet that PARSES THE RAW CSV IN-WASM,
// in the timed window — the fair wasm-vs-wasm counterpart to the WasMem guest
// `infer_predict` (which also parses CSV in-wasm). Replaces the host-side numpy
// pre-parse: the driver ships the raw CSV text shard; this module does the parse.
// Same integer forward pass + alloc-free stack-array parse as the optimized guest.
//
//   stdin  = [C:u32 le][F:u32 le] then C*F i64 le (model, row-major) then the raw
//            CSV text shard (label,f0..f(F-1) per line; header/blank lines skipped)
//   stdout = [correct:i64 le][total:i64 le][predsum:i64 le]
use std::io::{self, Read, Write};

const MAXF: usize = 64; // reusable feature buffer (data F is 16)

fn rd_u32(s: &mut impl Read) -> u32 {
    let mut b = [0u8; 4];
    s.read_exact(&mut b).expect("short header");
    u32::from_le_bytes(b)
}

fn main() {
    let mut stdin = io::stdin();
    let c = rd_u32(&mut stdin) as usize;
    let f = rd_u32(&mut stdin) as usize;

    let mut mb = vec![0u8; c * f * 8];
    stdin.read_exact(&mut mb).expect("short model");
    let mut w = vec![0i64; c * f];
    for (i, slot) in w.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        b.copy_from_slice(&mb[i * 8..i * 8 + 8]);
        *slot = i64::from_le_bytes(b);
    }

    let mut csv = Vec::new();
    stdin.read_to_end(&mut csv).expect("short csv");
    let text = String::from_utf8_lossy(&csv);

    let (mut correct, mut total, mut predsum) = (0i64, 0i64, 0i64);
    let mut feats = [0i64; MAXF];   // reused per record — alloc-free hot loop
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("label") { continue; }
        let mut it = line.split(',');
        let label: i64 = match it.next().and_then(|t| t.parse().ok()) { Some(l) => l, None => continue };
        let mut nf = 0usize;
        let mut bad = false;
        for v in it {
            if nf >= MAXF { bad = true; break; }
            feats[nf] = v.parse().unwrap_or(0);
            nf += 1;
        }
        if bad || nf != f { continue; }
        let mut best = 0usize;
        let mut best_val = i64::MIN;
        for cls in 0..c {
            let mut score: i64 = 0;
            for j in 0..f { score += w[cls * f + j] * feats[j]; }
            if score > best_val { best_val = score; best = cls; }
        }
        total += 1;
        predsum += best as i64;
        if best as i64 == label { correct += 1; }
    }

    let mut out = Vec::with_capacity(24);
    for v in [correct, total, predsum] { out.extend_from_slice(&v.to_le_bytes()); }
    io::stdout().write_all(&out).ok();
}
