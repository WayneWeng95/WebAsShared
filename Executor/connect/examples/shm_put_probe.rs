// shm_put_probe — isolate WHY the engine page-chain PUT is ~2x slower than a flat
// contiguous memcpy, by sweeping the three suspects independently:
//
//   1. chunk size   — the page-chain writes in PAGE_DATA_SIZE (4084 B) spans;
//                     a flat copy writes the whole payload in one span.
//   2. header gaps  — a 12-byte page header sits between every data span, so the
//                     write destination is non-contiguous.
//   3. atomic fence — the engine does a Release `cursor.store` per page; that
//                     fence blocks streaming/non-temporal stores.
//
// Each strategy writes the SAME 128 MiB payload into a mapped region and reports
// median write bandwidth (GiB/s).  The "flat" row is the upper bound (what the
// modelled bench.py shm rows do).  Comparing rows tells us which factor costs
// what, and whether bigger pages / relaxed ordering recover the gap.
//
//   cargo run --release --example shm_put_probe
//   cargo run --release --example shm_put_probe -- --size 134217728 --iters 30

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use anyhow::{bail, Result};

const GAP: usize = 12; // page header bytes between data spans (next_offset u64 + cursor u32)

#[derive(Clone, Copy, PartialEq)]
enum Fence {
    None,    // plain (non-atomic) store of the cursor — or skip entirely
    Relaxed, // atomic store, Relaxed (one fence at the very end)
    Release, // atomic store, Release per span (what the engine does today)
}

/// Write `payload` into `dst` in `chunk`-sized spans.  If `gap` > 0 a header gap
/// precedes each span (non-contiguous, like the page chain) and, per `fence`, a
/// per-span cursor atomic is stored at the gap.  Returns nothing; timed by caller.
unsafe fn write(dst: *mut u8, payload: &[u8], chunk: usize, gap: usize, fence: Fence) {
    let mut off = 0usize; // running offset into dst
    let mut src = payload;
    while !src.is_empty() {
        let hdr = off;
        off += gap; // skip the header region (if any)
        let n = chunk.min(src.len());
        std::ptr::copy_nonoverlapping(src.as_ptr(), dst.add(off), n);
        match fence {
            Fence::None => {
                if gap > 0 {
                    *(dst.add(hdr) as *mut u32) = n as u32; // plain store, no fence
                }
            }
            Fence::Relaxed => {
                (*(dst.add(hdr) as *const AtomicU32)).store(n as u32, Ordering::Relaxed);
            }
            Fence::Release => {
                (*(dst.add(hdr) as *const AtomicU32)).store(n as u32, Ordering::Release);
            }
        }
        off += n;
        src = &src[n..];
    }
    if fence == Fence::Relaxed {
        std::sync::atomic::fence(Ordering::Release); // single end fence for the whole write
    }
}

fn region_bytes(payload: usize, chunk: usize, gap: usize) -> usize {
    let spans = payload.div_ceil(chunk.max(1));
    payload + spans * gap + 4096 // + slack for the trailing atomic
}

fn pct(mut xs: Vec<f64>, p: f64) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let k = (((p / 100.0) * (xs.len() as f64 - 1.0)).round() as usize).min(xs.len() - 1);
    xs[k]
}

/// Time one strategy: median GiB/s over `iters` writes of `payload`.
unsafe fn bench(dst: *mut u8, payload: &[u8], chunk: usize, gap: usize, fence: Fence, iters: usize, warmup: usize) -> f64 {
    for _ in 0..warmup {
        write(dst, payload, chunk, gap, fence);
    }
    let mut us = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = Instant::now();
        write(dst, payload, chunk, gap, fence);
        us.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    let p50 = pct(us, 50.0).max(1e-6);
    payload.len() as f64 / (p50 * 1e-6) / (1024f64.powi(3))
}

fn fmt_chunk(n: usize) -> String {
    if n < 1024 { format!("{n}B") } else if n < 1024 * 1024 { format!("{}KiB", n / 1024) } else { format!("{}MiB", n / (1024 * 1024)) }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let size = flag(&args, "--size").and_then(|s| s.parse().ok()).unwrap_or(128 * 1024 * 1024);
    let iters = flag(&args, "--iters").and_then(|s| s.parse().ok()).unwrap_or(20);
    let warmup = flag(&args, "--warmup").and_then(|s| s.parse().ok()).unwrap_or(5);

    // Page-data sizes to sweep.  4084 = the real engine page (PAGE_DATA_SIZE).
    let chunks = [4084usize, 16 * 1024, 64 * 1024, 256 * 1024,
                  1024 * 1024, 4 * 1024 * 1024, 16 * 1024 * 1024, 64 * 1024 * 1024];

    // Map a region big enough for the worst case (smallest chunk = most gaps).
    let region = region_bytes(size, *chunks.iter().min().unwrap(), GAP).max(size) + (16 << 20);
    let base = unsafe {
        libc::mmap(std::ptr::null_mut(), region, libc::PROT_READ | libc::PROT_WRITE,
                   libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
    };
    if base == libc::MAP_FAILED {
        bail!("mmap failed: {}", std::io::Error::last_os_error());
    }
    let dst = base as *mut u8;
    let payload = vec![0xCDu8; size];

    println!("shm_put_probe — write {} per op, median GiB/s ({} iters, {} warmup)",
             fmt_chunk(size), iters, warmup);
    println!("GAP={GAP}B header between spans.  'flat' = one contiguous memcpy (model upper bound).\n");

    // Upper bound: a single flat contiguous memcpy (no chunking, no gap, no fence).
    let flat = unsafe { bench(dst, &payload, size, 0, Fence::None, iters, warmup) };
    println!("  flat (1 contiguous memcpy)                         {flat:>8.3} GiB/s   [baseline]\n");

    println!("  {:<10}{:>14}{:>14}{:>16}{:>16}", "page", "chunk-nogap", "chunk+gap", "chunk+gap+RELAXED", "chunk+gap+RELEASE");
    println!("  {:<10}{:>14}{:>14}{:>16}{:>16}", "", "(no fence)", "(no fence)", "store", "store (engine)");
    for &c in &chunks {
        let nogap   = unsafe { bench(dst, &payload, c, 0,   Fence::None,    iters, warmup) };
        let gap     = unsafe { bench(dst, &payload, c, GAP, Fence::None,    iters, warmup) };
        let relaxed = unsafe { bench(dst, &payload, c, GAP, Fence::Relaxed, iters, warmup) };
        let release = unsafe { bench(dst, &payload, c, GAP, Fence::Release, iters, warmup) };
        let tag = if c == 4084 { " (engine)" } else { "" };
        println!("  {:<10}{:>14.3}{:>14.3}{:>16.3}{:>16.3}{}", fmt_chunk(c), nogap, gap, relaxed, release, tag);
    }
    println!("\nRead across a row: chunk-size effect.  Compare columns: gap and fence effects.");
    println!("4084 B + gap + RELEASE ≈ the current engine PUT; 'flat' ≈ the modelled mmap PUT.");

    unsafe { libc::munmap(base, region); }
    Ok(())
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}
