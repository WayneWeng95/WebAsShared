// ─────────────────────────────────────────────────────────────────────────────
// Matrix multiply  —  SUMMA-style 2D block decomposition (r×c process grid)
//
// C = A·B, both N×N row-major f64. Tile into an r×c block grid (A → r block-rows
// of N/r rows; B → c block-cols of N/c cols). Worker (i,j) computes the block
// C_ij = A_i*·B_*j. On a single node every panel lives in the shared-memory page
// chain, so the SUMMA "broadcast" of each panel to the workers sharing its row /
// column is **zero-copy** (the headline; the cross-node version streams the
// k-panels over RDMA). The only copy is the one-shot panel redistribution in
// `mat_tile` — A row-panels are a contiguous byte range, B col-panels are gathered
// (the realistic SUMMA B redistribution) — and that moves bytes through SHM with
// **zero serialization**, vs the baselines' pickle/Redis round-trips.
//
// Slot layout:
//   I/O slot 0          : matrix A (single binary record, written by Input)
//   I/O slot 2          : matrix B (single binary record, written by Input)
//   stream slots 10..10+r : A block-row panels  (br × N, row-major)   [mat_tile]
//   stream slots 40..40+c : B block-col panels  (N × bc, row-major)   [mat_tile]
//   stream slots 100..    : per-worker C blocks  (linear i*c+j)       [mat_block]
//   stream slot  600      : aggregated C blocks (host Aggregate)
//   I/O slot 1 (OUTPUT)   : checksum lines                            [mat_assemble]
//
// DAG stages:
//   Input A → slot 0, Input B → slot 2   (binary)
//   mat_tile(arg)                        → A/B panels into the stream slots above
//   mat_block × (r·c)                    → C_ij block → slot C_BASE + i*c+j
//   Aggregate                            → merge the r·c C blocks → slot 600
//   mat_assemble(arg=600)                → fold a global checksum → OUTPUT
//   Output                               → flush checksum lines to file
//
// The `Func` DAG node passes a single u32, so every parameter is packed into
// `arg` (the same convention word_count uses for base|count):
//   N : bits 0..12   (≤ 8191)
//   c : bits 13..16  (≤ 15, block-cols)
//   r : bits 17..20  (≤ 15, block-rows)
//   j : bits 21..25  (≤ 31, this worker's block-col)         [mat_block only]
//   i : bits 26..30  (≤ 31, this worker's block-row)         [mat_block only]
// mat_assemble's arg is just the aggregated stream slot.
//
// C-block record format (mat_block → mat_assemble):
//   [i:u32 le][j:u32 le][br:u32 le][bc:u32 le] then br*bc f64 (le) row-major.
//
// Correctness gate: A,B have small integer entries, so every C entry is an exact
// integer < 2^53 — the f64 checksum (sum of all entries) is therefore exact and
// order-independent, matching `gen_matrix.py`'s numpy int64 sum bit-for-bit.
// ─────────────────────────────────────────────────────────────────────────────

use alloc::vec::Vec;
use crate::api::ShmApi;

/// I/O slot holding matrix B (A uses INPUT_IO_SLOT 0; OUTPUT_IO_SLOT is 1).
const B_INPUT_SLOT: u32 = 2;
/// First stream slot for A block-row panels (panel i → A_BASE + i).
const A_BASE: u32 = 10;
/// First stream slot for B block-col panels (panel j → B_BASE + j).
const B_BASE: u32 = 40;
/// First stream slot for per-worker C blocks (worker (i,j) → C_BASE + i*c + j).
const C_BASE: u32 = 100;

/// Concatenate every record in I/O `slot` into one contiguous byte buffer.
/// A `binary: true` Input loads a file as a single record, so this is normally a
/// one-element read; the concat keeps it robust to chunked loads.
fn read_io_bytes(slot: u32) -> Vec<u8> {
    let records = ShmApi::read_all_inputs_from(slot);
    if records.len() == 1 {
        return records.into_iter().next().map(|(_, b)| b).unwrap_or_default();
    }
    let mut out = Vec::new();
    for (_origin, rec) in records {
        out.extend_from_slice(&rec);
    }
    out
}

/// Read the single binary record from stream `slot` as one byte buffer.
fn read_stream_bytes(slot: u32) -> Vec<u8> {
    let records = ShmApi::read_all_stream_records(slot);
    if records.len() == 1 {
        return records.into_iter().next().map(|(_, b)| b).unwrap_or_default();
    }
    let mut out = Vec::new();
    for (_origin, rec) in records {
        out.extend_from_slice(&rec);
    }
    out
}

/// Reinterpret a little-endian f64 byte buffer as `Vec<f64>`.
fn bytes_to_f64(bytes: &[u8]) -> Vec<f64> {
    bytes
        .chunks_exact(8)
        .map(|c| f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
        .collect()
}

/// Unpack the packed `(r, c, N)` triple shared by `mat_tile` and `mat_block`.
/// Layout: n[0..12] (13b, N<8192) | c[13..16] (4b) | r[17..20] (4b); mat_block
/// also packs j[21..25] (5b) | i[26..30] (5b). r,c are 4 bits (was 3) so grids up
/// to 15×15 fit — needed for the 8×8 grid (64 blocks). Keep in sync with
/// gen_matrix_ap_dag.py.
fn unpack_rcn(arg: u32) -> (usize, usize, usize) {
    let n = (arg & 0x1FFF) as usize;
    let c = ((arg >> 13) & 0xF) as usize;
    let r = ((arg >> 17) & 0xF) as usize;
    (r, c, n)
}

/// Tile A into r contiguous block-rows and B into c gathered block-cols, writing
/// each panel as one binary record to its stream slot.
///   arg = (r<<16) | (c<<13) | N
#[no_mangle]
pub extern "C" fn mat_tile(arg: u32) {
    let (r, c, n) = unpack_rcn(arg);
    if r == 0 || c == 0 || n == 0 {
        return;
    }
    let br = n / r; // rows per A block-row
    let bc = n / c; // cols per B block-col
    let f = core::mem::size_of::<f64>(); // 8

    let a = read_io_bytes(common::INPUT_IO_SLOT);
    let b = read_io_bytes(B_INPUT_SLOT);

    // A block-row i = rows [i*br, (i+1)*br) — a contiguous byte range of A.
    for i in 0..r {
        let start = i * br * n * f;
        let end = (i + 1) * br * n * f;
        if end <= a.len() {
            ShmApi::append_stream_data(A_BASE + i as u32, &a[start..end]);
        }
    }

    // B block-col j = cols [j*bc, (j+1)*bc) across all N rows — strided, so gather
    // into a contiguous (N × bc) row-major panel.
    let mut panel = alloc::vec![0u8; n * bc * f];
    for j in 0..c {
        for k in 0..n {
            let src = (k * n + j * bc) * f;
            let dst = k * bc * f;
            let len = bc * f;
            if src + len <= b.len() {
                panel[dst..dst + len].copy_from_slice(&b[src..src + len]);
            }
        }
        ShmApi::append_stream_data(B_BASE + j as u32, &panel);
    }
}

/// Compute one block C_ij = A_i*·B_*j and emit it to slot `C_BASE + i*c + j`.
///   arg = (i<<24) | (j<<19) | (r<<16) | (c<<13) | N
#[no_mangle]
pub extern "C" fn mat_block(arg: u32) {
    let (r, c, n) = unpack_rcn(arg);
    let j = ((arg >> 21) & 0x1F) as usize;
    let i = ((arg >> 26) & 0x1F) as usize;
    if r == 0 || c == 0 || n == 0 {
        return;
    }
    let br = n / r;
    let bc = n / c;

    let a = bytes_to_f64(&read_stream_bytes(A_BASE + i as u32)); // br × N
    let b = bytes_to_f64(&read_stream_bytes(B_BASE + j as u32)); // N × bc
    if a.len() < br * n || b.len() < n * bc {
        return;
    }

    // C_ij = A_panel (br×N) · B_panel (N×bc), ikj loop order (cache-friendly).
    let mut cmat = alloc::vec![0.0f64; br * bc];
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

    // Serialize the block: [i,j,br,bc] header + row-major f64 payload.
    let mut out = Vec::with_capacity(16 + cmat.len() * 8);
    out.extend_from_slice(&(i as u32).to_le_bytes());
    out.extend_from_slice(&(j as u32).to_le_bytes());
    out.extend_from_slice(&(br as u32).to_le_bytes());
    out.extend_from_slice(&(bc as u32).to_le_bytes());
    for v in &cmat {
        out.extend_from_slice(&v.to_le_bytes());
    }
    ShmApi::append_stream_data(C_BASE + (i * c + j) as u32, &out);
}

/// Fold a global checksum over every aggregated C block and write it out.
///   arg = aggregated stream slot (e.g. 600)
#[no_mangle]
pub extern "C" fn mat_assemble(agg_slot: u32) {
    let records = ShmApi::read_all_stream_records(agg_slot);
    let mut checksum = 0.0f64; // exact: every entry is an integer < 2^53
    let mut elements: u64 = 0;
    let mut blocks: u64 = 0;

    for (_origin, rec) in &records {
        if rec.len() < 16 {
            continue;
        }
        let br = u32::from_le_bytes([rec[8], rec[9], rec[10], rec[11]]) as usize;
        let bc = u32::from_le_bytes([rec[12], rec[13], rec[14], rec[15]]) as usize;
        let payload = &rec[16..];
        let want = br * bc * 8;
        if payload.len() < want {
            continue;
        }
        for v in payload[..want]
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
        {
            checksum += v;
        }
        elements += (br * bc) as u64;
        blocks += 1;
    }

    ShmApi::write_output_str("=== matrix_multiply ===");
    ShmApi::write_output_str(&alloc::format!("blocks={}", blocks));
    ShmApi::write_output_str(&alloc::format!("elements={}", elements));
    ShmApi::write_output_str(&alloc::format!("checksum={}", checksum as i64));
}
