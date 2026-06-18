// sgd_grad.rs — the WASM (Faaslet) gradient function for the Faasm-like
// synchronous-SGD demo.
//
// Faasm runs functions as WASM Faaslets over a distributed KV; the real stack
// can't stand up here (images gone — see ../README.md), so this abstracts the
// mechanism: one gradient computation per fresh wasmtime instance (an SFI-isolated
// Faaslet, AOT-compiled like Faasm's cached machine code), with the host driver
// moving the data shard / model / gradient through Redis (serialized state — the
// transfer our zero-copy SHM page-chain avoids).
//
// SAME integer kernel as the WebAsShared guest (Executor/.../ml_training.rs
// sgd_grad): pred = Σ w·x, err = pred − target, gsum += err·x, all i64 — so
// ours-vs-Faasm is a true WASM-vs-WASM head-to-head differing only in the data
// path. The host aggregates the per-Faaslet gradient SUMS and takes the central
// integer step (identical to sgd_core), so the weight-checksum gate matches.
//
//   stdin  = [n:u32 le][f:u32 le] then C*f i64 le (model, C=10, row-major)
//            then n samples, each [label:i32 le] then f × i32 le (features)
//   stdout = C*f i64 le (gradient SUM over this shard, row-major)
use std::io::{self, Read, Write};

const C: usize = 10; // N_CLASSES (matches the guest)
const TARGET: i64 = 1024;

fn rd_u32(s: &mut impl Read) -> u32 {
    let mut b = [0u8; 4];
    s.read_exact(&mut b).expect("short header");
    u32::from_le_bytes(b)
}

fn main() {
    // Read the whole frame up front (one Faaslet invocation).
    let mut stdin = io::stdin();
    let n = rd_u32(&mut stdin) as usize;
    let f = rd_u32(&mut stdin) as usize;

    let mut rest = Vec::new();
    stdin.read_to_end(&mut rest).expect("short body");
    let mut off = 0usize;

    // model: C*f i64
    let mut w = vec![0i64; C * f];
    for slot in w.iter_mut() {
        let mut b = [0u8; 8];
        b.copy_from_slice(&rest[off..off + 8]);
        *slot = i64::from_le_bytes(b);
        off += 8;
    }

    let mut g = vec![0i64; C * f];
    for _ in 0..n {
        let mut lb = [0u8; 4];
        lb.copy_from_slice(&rest[off..off + 4]);
        let label = i32::from_le_bytes(lb) as i64;
        off += 4;
        let mut x = vec![0i64; f];
        for xi in x.iter_mut() {
            let mut b = [0u8; 4];
            b.copy_from_slice(&rest[off..off + 4]);
            *xi = i32::from_le_bytes(b) as i64;
            off += 4;
        }
        for cls in 0..C {
            let mut pred: i64 = 0;
            for j in 0..f {
                pred += w[cls * f + j] * x[j];
            }
            let target = if cls as i64 == label { TARGET } else { 0 };
            let err = pred - target;
            for j in 0..f {
                g[cls * f + j] += err * x[j];
            }
        }
    }

    let mut out = Vec::with_capacity(g.len() * 8);
    for v in &g {
        out.extend_from_slice(&v.to_le_bytes());
    }
    io::stdout().write_all(&out).ok();
}
