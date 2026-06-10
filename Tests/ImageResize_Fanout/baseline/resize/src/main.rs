// Baseline resizer — one task of the Roadrunner wasmedge/native fanout baseline.
//
// Reads ONE image from STDIN (our internal binary format, identical to
// Tests/ImageResize_Fanout/gen_images.py and Executor guest img_decode):
//     [w: u16 LE][h: u16 LE][ch: u8][pixels: w*h*ch]
// and downscales it 0.5x nearest-neighbour — byte-for-byte the same compute as
// the guest `img_resize` in Executor/guest/src/workloads/img_pipeline.rs.
//
// STDIN (not a file arg) on purpose: WasmEdge 0.11.2's WASI directory preopen
// (`--dir`) is broken on this host's kernel, but stdin needs no preopen. The
// driver pipes the image into each task's stdin — each task still gets its own
// independent copy (no shared zero-copy), which is exactly the baseline.
//
// The point: in this baseline each task loads the image INDEPENDENTLY from disk
// (no shared zero-copy delivery), so fanning out to N tasks pays the per-task
// read+decode N times. Our system instead splices one shared page chain to all
// N workers. Comparing the two fanout curves isolates the delivery cost.
//
// The resized buffer is computed and dropped (no output write) to mirror the
// guest `img_resize`, which also discards — we measure delivery+resize, not I/O.

use std::io::Read;

fn main() {
    let mut data = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut data) {
        eprintln!("read stdin: {e}"); std::process::exit(1);
    }
    if data.len() < 5 { eprintln!("short image ({} bytes)", data.len()); std::process::exit(1); }

    let w = u16::from_le_bytes([data[0], data[1]]) as usize;
    let h = u16::from_le_bytes([data[2], data[3]]) as usize;
    let ch = data[4] as usize;
    let pixels = &data[5..];
    if pixels.len() < w * h * ch { eprintln!("truncated pixels"); std::process::exit(1); }

    let (ow, oh) = (w / 2, h / 2);
    if ow == 0 || oh == 0 { return; }
    let mut buf = vec![0u8; ow * oh * ch];
    for r in 0..oh {
        for c in 0..ow {
            let src = ((r * 2) * w + (c * 2)) * ch;   // nearest-neighbour pick
            let dst = (r * ow + c) * ch;
            buf[dst..dst + ch].copy_from_slice(&pixels[src..src + ch]);
        }
    }
    // Prevent the resize from being optimised away (matches the guest's volatile read).
    std::hint::black_box(&buf);
}
