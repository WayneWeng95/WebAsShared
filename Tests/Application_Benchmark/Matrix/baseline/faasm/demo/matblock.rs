// matblock.rs — the WASM (Faaslet) block-multiply function for the Faasm-like
// matrix-multiply demo.
//
// Faasm runs matrix-multiply as WASM functions (Faaslets) over a distributed KV;
// the real stack can't stand up here (images gone — see ../README.md), so this
// abstracts the mechanism: one block multiply C_ij = A_i*·B_*j per fresh wasmtime
// instance (an SFI-isolated Faaslet, AOT-compiled like Faasm's cached machine
// code), with the host driver moving the A/B panels and C blocks through Redis
// (serialized state — the transfer our zero-copy SHM page-chain avoids).
//
// SAME naive ikj kernel as the WebAsShared guest (Executor/.../matrix.rs), so
// ours-vs-Faasm is a true WASM-vs-WASM head-to-head differing only in the data
// path (zero-copy SHM panels vs Redis-serialized blocks).
//
//   stdin  = [br:u32 le][bc:u32 le][n:u32 le] then br*n f64 (A panel, row-major)
//            then n*bc f64 (B panel, row-major)
//   stdout = br*bc f64 (C block, row-major)
use std::io::{self, Read, Write};

fn rd_u32(s: &mut impl Read) -> u32 {
    let mut b = [0u8; 4];
    s.read_exact(&mut b).expect("short header");
    u32::from_le_bytes(b)
}

fn rd_f64s(s: &mut impl Read, count: usize) -> Vec<f64> {
    let mut bytes = vec![0u8; count * 8];
    s.read_exact(&mut bytes).expect("short matrix payload");
    bytes
        .chunks_exact(8)
        .map(|c| f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
        .collect()
}

fn main() {
    let mut stdin = io::stdin();
    let br = rd_u32(&mut stdin) as usize;
    let bc = rd_u32(&mut stdin) as usize;
    let n = rd_u32(&mut stdin) as usize;
    let a = rd_f64s(&mut stdin, br * n); // br × n
    let b = rd_f64s(&mut stdin, n * bc); // n × bc

    let mut cmat = vec![0.0f64; br * bc];
    for row in 0..br {
        let a_row = &a[row * n..row * n + n];
        let c_row = &mut cmat[row * bc..row * bc + bc];
        for k in 0..n {
            let aik = a_row[k];
            if aik == 0.0 {
                continue;
            }
            let b_row = &b[k * bc..k * bc + bc];
            for col in 0..bc {
                c_row[col] += aik * b_row[col];
            }
        }
    }

    let mut out = Vec::with_capacity(cmat.len() * 8);
    for v in &cmat {
        out.extend_from_slice(&v.to_le_bytes());
    }
    io::stdout().write_all(&out).ok();
}
