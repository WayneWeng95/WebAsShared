// infer_predict.rs — the WASM (Faaslet) inference function for the Faasm-like
// MNIST inference demo. One fresh wasmtime instance per shard (an SFI-isolated
// Faaslet); the host driver moves the model + shard + result through Redis
// (serialized state — the transfer our zero-copy SHM page-chain avoids). SAME
// integer forward pass as the WebAsShared guest → true WASM-vs-WASM, identical
// prediction checksum.
//
//   stdin  = [n:u32 le][f:u32 le] then C*f i64 le (model, C=10) then
//            n samples, each [label:i32 le] then f × i32 le (features)
//   stdout = [correct:i64 le][total:i64 le][predsum:i64 le]
use std::io::{self, Read, Write};

const C: usize = 10;

fn rd_u32(s: &mut impl Read) -> u32 {
    let mut b = [0u8; 4];
    s.read_exact(&mut b).expect("short header");
    u32::from_le_bytes(b)
}

fn main() {
    let mut stdin = io::stdin();
    let n = rd_u32(&mut stdin) as usize;
    let f = rd_u32(&mut stdin) as usize;
    let mut rest = Vec::new();
    stdin.read_to_end(&mut rest).expect("short body");
    let mut off = 0usize;

    let mut w = vec![0i64; C * f];
    for slot in w.iter_mut() {
        let mut b = [0u8; 8];
        b.copy_from_slice(&rest[off..off + 8]);
        *slot = i64::from_le_bytes(b);
        off += 8;
    }

    let (mut correct, mut total, mut predsum) = (0i64, 0i64, 0i64);
    for _ in 0..n {
        let mut lb = [0u8; 4];
        lb.copy_from_slice(&rest[off..off + 4]);
        let label = i32::from_le_bytes(lb) as i64;
        off += 4;
        let mut best = 0usize;
        let mut best_val = i64::MIN;
        for cls in 0..C {
            let mut score: i64 = 0;
            for j in 0..f {
                let mut b = [0u8; 4];
                b.copy_from_slice(&rest[off + j * 4..off + j * 4 + 4]);
                score += w[cls * f + j] * (i32::from_le_bytes(b) as i64);
            }
            if score > best_val { best_val = score; best = cls; }
        }
        off += f * 4;
        total += 1;
        predsum += best as i64;
        if best as i64 == label { correct += 1; }
    }

    let mut out = Vec::with_capacity(24);
    for v in [correct, total, predsum] { out.extend_from_slice(&v.to_le_bytes()); }
    io::stdout().write_all(&out).ok();
}
