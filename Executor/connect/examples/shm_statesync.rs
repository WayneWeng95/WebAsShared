// shm_statesync — intra-node StateSync micro-benchmark driven by the REAL engine
// page-chain, not the modelled /dev/shm mmap row in bench.py.
//
// This is the native Rust harness the StateSync-local README calls for:
//
//   > A follow-up can swap in the real path: ... add a native Rust harness
//   > calling the host SHM primitives.
//
// It formats a shared-memory file exactly like `host::shm::format_shared_memory`,
// maps it, and moves one unit of state producer->consumer through a stream slot
// using the engine's own length-prefixed page-chain protocol (the same byte
// layout the host's `SlotLoader::write_bytes` writes and
// `persistence::read_chain_records` reads — both mirrored here over the
// authoritative `common::{Superblock, Page}` types so it stays in lockstep).
//
// Two rows are emitted, the real-engine counterparts of bench.py's modelled
// shm-copy / shm-zerocopy:
//   shm-copy-engine     GET materializes the payload out of the page chain
//                       (one memcpy per page span) — the SerDe-free copy path.
//   shm-zerocopy-engine GET is the engine's real zero-copy DELIVERY: a routing
//                       Bridge splice (host::routing::chain_splicer::chain_onto)
//                       that hands the producer's whole page chain to the
//                       consumer's slot by moving head/tail pointers — O(1), no
//                       walk, no copy.  This is the fair counterpart to the
//                       modelled row's O(1) `memoryview` handoff: both deliver
//                       state in constant time, but this one actually transfers
//                       ownership of the data to the next stage.
//
// Same size schedule, same put/get + fan-out timing loop, and the same CSV
// schema as StateSync-local/bench.py, so the rows drop straight into
// results.csv and plot.py.
//
//   cargo run --release --example shm_statesync -- \
//       --sizes 16384 1048576 16777216 134217728 --iters 30 --warmup 5 \
//       --readers 1 --csv engine_local.csv
//
// PUT measures the data write into a pre-allocated, reused page chain by default
// (fair to the modelled rows, which reuse a fixed mmap region — no per-op
// allocation).  Pass --include-alloc to fold page allocation into the PUT timer.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{bail, Result};
use common::{PageId, ShmOffset, Superblock, BUMP_ALLOCATOR_START};

// Backing file large enough to hold the biggest single payload's page chain
// plus the fixed arenas below the bump start.  Only one payload lives at a time
// (the slot is reset between iterations — steady-state overwrite, matching the
// fixed-region model in bench.py), so 512 MiB comfortably covers a 128 MiB row.
const SHM_BYTES: usize = 512 * 1024 * 1024;
const SRC_SLOT: usize = 10; // producer's output slot
const DST_SLOT_BASE: usize = 11; // consumer slot(s); fan-out uses 11, 12, … 11+readers-1

/// Delivery mechanism measured by the GET timer.
#[derive(Clone, Copy)]
enum Mode {
    /// Consumer materializes the payload out of the page chain (one memcpy per span).
    Copy,
    /// Engine zero-copy routing: Bridge-splice the chain onto the consumer slot
    /// (O(1) head/tail pointer move).  Fan-out = Broadcast (link src to N slots).
    ZeroCopyBridge,
}

// ── SHM bring-up ────────────────────────────────────────────────────────────

// Page header byte offsets — mirror common::Page (asserted in common/src/lib.rs):
//   next_offset u64 @ 0,  cursor u32 @ 8,  data @ 12.
const PG_NEXT: usize = 0;
const PG_CURSOR: usize = 8;
const PG_DATA: usize = 12;

struct Shm {
    base: usize,
    path: String,
    /// Bytes per page (header included).  4096 = the real engine page; larger
    /// values are a HARNESS-ONLY experiment (the engine is untouched) to show how
    /// page size affects PUT write bandwidth.
    page_bytes: usize,
    /// Payload bytes per page = page_bytes - PG_DATA.
    page_data: usize,
}

impl Shm {
    /// Create + format + map the SHM file, mirroring host::shm::format_shared_memory.
    fn create(path: &str, page_bytes: usize) -> Result<Self> {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(SHM_BYTES as u64)?;

        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SHM_BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            bail!("mmap failed: {}", std::io::Error::last_os_error());
        }
        let shm = Shm {
            base: base as usize,
            path: path.to_string(),
            page_bytes,
            page_data: page_bytes - PG_DATA,
        };

        // Format the superblock: magic + bump start + capacity.  Every other
        // field (slot head/tail arrays, free list) is zero from the fresh map.
        let sb = shm.sb();
        sb.magic = 0xDEAD_BEEF;
        sb.bump_allocator.store(BUMP_ALLOCATOR_START, Ordering::Release);
        sb.global_capacity.store(SHM_BYTES as ShmOffset, Ordering::Release);
        Ok(shm)
    }

    #[allow(clippy::mut_from_ref)]
    fn sb(&self) -> &mut Superblock {
        unsafe { &mut *(self.base as *mut Superblock) }
    }

    // ── Raw page-header access (page-size agnostic) ──────────────────────────
    fn next_at(&self, id: PageId) -> &AtomicU64 {
        unsafe { &*((self.base + id as usize + PG_NEXT) as *const AtomicU64) }
    }
    fn cursor_at(&self, id: PageId) -> &AtomicU32 {
        unsafe { &*((self.base + id as usize + PG_CURSOR) as *const AtomicU32) }
    }
    fn data_ptr(&self, id: PageId) -> *mut u8 {
        (self.base + id as usize + PG_DATA) as *mut u8
    }

    /// Bump-allocate one zeroed page; returns its PageId (byte offset).
    fn alloc_page(&self) -> Result<PageId> {
        let sb = self.sb();
        let id = sb.bump_allocator.fetch_add(self.page_bytes as ShmOffset, Ordering::AcqRel) as PageId;
        if id as usize + self.page_bytes > SHM_BYTES {
            bail!("SHM exhausted at {id:#x} (raise SHM_BYTES or lower --page-bytes)");
        }
        self.next_at(id).store(0, Ordering::Relaxed);
        self.cursor_at(id).store(0, Ordering::Relaxed);
        Ok(id)
    }

    /// Reset the src slot, `readers` dst slots, and the bump allocator to the
    /// formatted state, so the next PUT reuses the same offsets (steady-state
    /// single-key overwrite).  Resetting dst slots clears any prior splice.
    fn reset(&self, readers: usize) {
        let sb = self.sb();
        sb.writer_heads[SRC_SLOT].store(0, Ordering::Release);
        sb.writer_tails[SRC_SLOT].store(0, Ordering::Release);
        for r in 0..readers {
            sb.writer_heads[DST_SLOT_BASE + r].store(0, Ordering::Release);
            sb.writer_tails[DST_SLOT_BASE + r].store(0, Ordering::Release);
        }
        sb.bump_allocator.store(BUMP_ALLOCATOR_START, Ordering::Release);
    }

    // ── PUT: append [len u32][origin u32][payload] to the slot's page chain ──
    // Byte-for-byte the host's SlotLoader::append_record + write_bytes.
    fn put(&self, slot: usize, payload: &[u8]) -> Result<()> {
        self.write_bytes(slot, &(payload.len() as u32).to_le_bytes())?;
        self.write_bytes(slot, &(slot as u32).to_le_bytes())?; // origin = slot
        self.write_bytes(slot, payload)
    }

    fn write_bytes(&self, slot: usize, mut data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let sb = self.sb();
        let mut tail: PageId = sb.writer_tails[slot].load(Ordering::Acquire);
        if tail == 0 {
            tail = self.alloc_page()?;
            sb.writer_heads[slot].store(tail, Ordering::Release);
            sb.writer_tails[slot].store(tail, Ordering::Release);
        }
        while !data.is_empty() {
            let cursor = self.cursor_at(tail).load(Ordering::Relaxed) as usize;
            let space = self.page_data.saturating_sub(cursor);
            if space == 0 {
                let next = self.alloc_page()?;
                self.next_at(tail).store(next, Ordering::Release);
                sb.writer_tails[slot].store(next, Ordering::Release);
                tail = next;
                continue;
            }
            let n = space.min(data.len());
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data.as_ptr(),
                    self.data_ptr(tail).add(cursor),
                    n,
                );
            }
            self.cursor_at(tail).store((cursor + n) as ShmOffset, Ordering::Release);
            data = &data[n..];
        }
        Ok(())
    }

    // ── Pre-allocation path (PUT excluding allocation) ──────────────────────
    // The modelled bench.py shm rows PUT into a fixed, reused mmap region — no
    // allocation per op.  To compare PUT fairly we pre-allocate the page chain
    // ONCE per size and rewrite into it each iteration, so the PUT timer measures
    // only the data write (memcpy + cursor stores), not the page allocation.

    /// Allocate + link a chain big enough for `bytes`, and point `slot` at it.
    fn alloc_chain(&self, slot: usize, bytes: usize) -> Result<()> {
        let pages = bytes.max(1).div_ceil(self.page_data);
        let sb = self.sb();
        let mut head: PageId = 0;
        let mut prev: PageId = 0;
        for _ in 0..pages {
            let id = self.alloc_page()?;
            if prev == 0 {
                head = id;
            } else {
                self.next_at(prev).store(id, Ordering::Release);
            }
            prev = id;
        }
        sb.writer_heads[slot].store(head, Ordering::Release);
        sb.writer_tails[slot].store(head, Ordering::Release);
        Ok(())
    }

    /// Zero every page's cursor in `slot`'s pre-allocated chain (untimed reset
    /// between iterations) — keeps the next_offset links intact.
    fn reset_cursors(&self, slot: usize) {
        let mut id = self.sb().writer_heads[slot].load(Ordering::Acquire);
        while id != 0 {
            self.cursor_at(id).store(0, Ordering::Release);
            id = self.next_at(id).load(Ordering::Acquire);
        }
    }

    /// Clear the `readers` dst slots so the next splice re-links cleanly.
    fn reset_dst(&self, readers: usize) {
        let sb = self.sb();
        for r in 0..readers {
            sb.writer_heads[DST_SLOT_BASE + r].store(0, Ordering::Release);
            sb.writer_tails[DST_SLOT_BASE + r].store(0, Ordering::Release);
        }
    }

    /// Write [len][origin][payload] into the pre-allocated chain at `slot`
    /// WITHOUT allocating — walks the existing pages via next_offset, advancing
    /// cursors.  This is the PUT-minus-allocation cost.
    fn write_record_prealloc(&self, slot: usize, payload: &[u8]) {
        let sb = self.sb();
        let mut id = sb.writer_heads[slot].load(Ordering::Acquire);
        let mut cur = 0usize;
        let mut hdr = [0u8; 8];
        hdr[0..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        hdr[4..8].copy_from_slice(&(slot as u32).to_le_bytes());
        for part in [&hdr[..], payload] {
            let mut data = part;
            while !data.is_empty() {
                let space = self.page_data - cur;
                if space == 0 {
                    id = self.next_at(id).load(Ordering::Acquire);
                    cur = 0;
                    continue;
                }
                let n = space.min(data.len());
                unsafe {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), self.data_ptr(id).add(cur), n);
                }
                cur += n;
                self.cursor_at(id).store(cur as ShmOffset, Ordering::Release);
                data = &data[n..];
            }
        }
        sb.writer_tails[slot].store(id, Ordering::Release); // tail = last written page
    }

    // ── GET (copy): materialize the payload out of the chain (one memcpy per
    // page span).  Mirrors persistence::read_chain_records.  Returns bytes. ──
    fn get_copy(&self, slot: usize) -> usize {
        let head = self.sb().writer_heads[slot].load(Ordering::Acquire);
        let mut r = ChainReader::new(self, head);
        let mut hdr = [0u8; 8];
        if !r.read(&mut hdr) {
            return 0;
        }
        let len = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; len];
        if !r.read(&mut payload) {
            return 0;
        }
        // Defeat dead-store elimination: the return value is just `len`, so without
        // this the optimizer would drop the whole copy (the materialized bytes are
        // never read).  black_box on the data pointer forces the fill to happen.
        std::hint::black_box(payload.as_ptr());
        payload.len()
    }

    // ── GET (zero-copy delivery): Bridge-splice src's chain onto dst.  O(1) —
    // mirrors host::routing::chain_splicer::ChainSplicer::chain_onto exactly:
    // move head/tail pointers (one next_offset store), never touching payload.
    // Returns the delivered byte count (read from the record header, 1 page). ──
    fn chain_onto(&self, dst: usize, src: usize) {
        let sb = self.sb();
        let src_head = sb.writer_heads[src].load(Ordering::Acquire);
        if src_head == 0 {
            return;
        }
        let src_tail = sb.writer_tails[src].load(Ordering::Acquire);
        std::hint::black_box(src_tail); // keep the loads/splice from being elided
        let dst_tail = sb.writer_tails[dst].load(Ordering::Acquire);
        if dst_tail == 0 {
            sb.writer_heads[dst].store(src_head, Ordering::Release);
        } else {
            self.next_at(dst_tail).store(src_head, Ordering::Release);
        }
        sb.writer_tails[dst].store(src_tail, Ordering::Release);
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, SHM_BYTES);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

// ── Page-chain reader (mirrors host PageReader) ───────────────────────────────

struct ChainReader<'a> {
    shm: &'a Shm,
    page_id: PageId,
    in_page: usize, // bytes already consumed from the current page
}

impl<'a> ChainReader<'a> {
    fn new(shm: &'a Shm, head: PageId) -> Self {
        ChainReader { shm, page_id: head, in_page: 0 }
    }

    /// Copy the next `buf.len()` bytes out of the chain (header / copy path).
    fn read(&mut self, buf: &mut [u8]) -> bool {
        let mut filled = 0;
        while filled < buf.len() {
            if self.page_id == 0 {
                return false;
            }
            let written = self.shm.cursor_at(self.page_id).load(Ordering::Acquire) as usize;
            let avail = written - self.in_page;
            if avail == 0 {
                self.page_id = self.shm.next_at(self.page_id).load(Ordering::Acquire);
                self.in_page = 0;
                continue;
            }
            let take = avail.min(buf.len() - filled);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.shm.data_ptr(self.page_id).add(self.in_page),
                    buf.as_mut_ptr().add(filled),
                    take,
                );
            }
            self.in_page += take;
            filled += take;
        }
        true
    }
}

// ── Measurement (mirrors bench.py's measure/stats) ────────────────────────────

struct Stat {
    p50: f64,
    p99: f64,
    mean: f64,
}

fn stat(xs: &[f64]) -> Stat {
    let mean = xs.iter().sum::<f64>() / xs.len() as f64;
    Stat { p50: pct(xs, 50.0), p99: pct(xs, 99.0), mean }
}

fn pct(samples: &[f64], p: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut s = samples.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let k = (((p / 100.0) * (s.len() as f64 - 1.0)).round() as usize).min(s.len() - 1);
    s[k]
}

struct Row {
    name: String,
    size: usize,
    readers: usize,
    iters: usize,
    put: Stat,
    get: Stat,
    put_gibps: f64,
    get_gibps: f64,
}

/// Run one (approach, size) cell: warmup, then `iters` timed put + fan-out gets.
///
/// PUT appends the payload to the producer slot (real write into SHM — the data
/// enters memory once, fair to every approach).  GET is the consumer-side
/// delivery: a copy-out (`Mode::Copy`) or an O(1) Bridge splice
/// (`Mode::ZeroCopyBridge`).  Fan-out replays the delivery to `readers` slots
/// (Broadcast for the splice; N copies for the copy path).
fn measure(shm: &Shm, mode: Mode, size: usize, iters: usize, warmup: usize, readers: usize, prealloc: bool) -> Result<(Stat, Stat)> {
    let payload = vec![0xCDu8; size];

    // One delivery to all `readers` consumers; returns bytes delivered.
    let deliver = |s: &Shm| -> usize {
        let mut acc = 0usize;
        match mode {
            Mode::Copy => {
                for _ in 0..readers {
                    acc = acc.wrapping_add(s.get_copy(SRC_SLOT));
                }
            }
            Mode::ZeroCopyBridge => {
                for r in 0..readers {
                    s.chain_onto(DST_SLOT_BASE + r, SRC_SLOT); // O(1) pointer move
                    acc = acc.wrapping_add(size);
                }
            }
        }
        acc
    };

    // prealloc: allocate the chain ONCE and reuse it (PUT excludes allocation —
    // fair to the modelled rows, which reuse a fixed mmap region).  Otherwise
    // each PUT bump-allocates fresh pages (allocation included).
    if prealloc {
        shm.reset(readers);
        shm.alloc_chain(SRC_SLOT, 8 + size)?; // allocate the reused chain once
    }

    // One PUT (reset is always untimed; only the write/alloc is timed below).
    // Returns the timed microseconds.
    let timed_put = |s: &Shm| -> Result<f64> {
        if prealloc {
            s.reset_cursors(SRC_SLOT);                       // untimed
            s.reset_dst(readers);                            // untimed
            let t0 = Instant::now();
            s.write_record_prealloc(SRC_SLOT, &payload);     // write only (no alloc)
            Ok(t0.elapsed().as_secs_f64() * 1e6)
        } else {
            s.reset(readers);                                // untimed
            let t0 = Instant::now();
            s.put(SRC_SLOT, &payload)?;                      // alloc + write
            Ok(t0.elapsed().as_secs_f64() * 1e6)
        }
    };

    for _ in 0..warmup {
        timed_put(shm)?;
        std::hint::black_box(deliver(shm));
    }

    let mut put_us = Vec::with_capacity(iters);
    let mut get_us = Vec::with_capacity(iters);
    for _ in 0..iters {
        put_us.push(timed_put(shm)?);

        let t0 = Instant::now();
        std::hint::black_box(deliver(shm));
        get_us.push(t0.elapsed().as_secs_f64() * 1e6 / readers as f64);
    }
    Ok((stat(&put_us), stat(&get_us)))
}

// ── CLI / args (mirrors bench.py flags) ───────────────────────────────────────

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sizes = multi_usize(&args, "--sizes")
        .unwrap_or_else(|| vec![16 * 1024, 1024 * 1024, 16 * 1024 * 1024, 128 * 1024 * 1024]);
    let iters = single(&args, "--iters").unwrap_or(30);
    let warmup = single(&args, "--warmup").unwrap_or(5);
    let readers = single(&args, "--readers").unwrap_or(1).max(1);
    let csv = flag(&args, "--csv");
    // PUT measures the data write into a pre-allocated, reused page chain by
    // default — fair to the modelled rows, which reuse a fixed mmap region (no
    // per-op allocation).  Pass --include-alloc to fold page allocation back in.
    let prealloc = !args.iter().any(|a| a == "--include-alloc");
    // --page-bytes N: HARNESS-ONLY experiment to show how page size affects PUT
    // bandwidth.  Default 4096 = the real engine page (the engine is untouched).
    let page_bytes = single(&args, "--page-bytes").unwrap_or(4096);
    if page_bytes < 4096 || page_bytes % 8 != 0 {
        bail!("--page-bytes must be >= 4096 and a multiple of 8 (got {page_bytes})");
    }
    let big_page = page_bytes != 4096;
    // Tag row names with the page size so a sweep accumulates into one CSV without
    // collisions (e.g. shm-zerocopy-engine-64KiB).
    let suffix = if big_page { format!("-{}", fmt_size(page_bytes)) } else { String::new() };

    let pid = std::process::id();
    let path = format!("/dev/shm/statesync_engine_{pid}");
    let shm = Shm::create(&path, page_bytes)?;

    println!("StateSync micro-benchmark — {}",
             if big_page { "page-chain (HARNESS page-size experiment — engine untouched)" }
             else { "REAL ENGINE page-chain (shm.rs format + slot page chains)" });
    println!("  readers={readers}  iters={iters}  warmup={warmup}  src_slot={SRC_SLOT}  page={} ({} data)  shm={}MiB",
             fmt_size(page_bytes), fmt_size(page_bytes - PG_DATA), SHM_BYTES / (1024 * 1024));
    println!("  zero-copy row = Bridge splice (chain_onto, O(1) pointer move); copy row = materialize payload");
    println!("  PUT = {}", if prealloc {
        "data write only (chain pre-allocated + reused — allocation excluded)"
    } else {
        "allocation + data write (fresh page chain each op)"
    });
    println!(
        "{:<26}{:>8}  {:>10}{:>10}{:>10}{:>10}  {:>10}{:>10}",
        "approach", "size", "put p50", "put p99", "get p50", "get p99", "put GiB/s", "get GiB/s"
    );

    let mut rows: Vec<Row> = Vec::new();
    for (base_name, mode) in [("shm-copy-engine", Mode::Copy), ("shm-zerocopy-engine", Mode::ZeroCopyBridge)] {
        let name = format!("{base_name}{suffix}");
        let name = name.as_str();
        for &size in &sizes {
            let (put, get) = measure(&shm, mode, size, iters, warmup, readers, prealloc)?;
            let put_gibps = size as f64 / (put.p50.max(1e-6) * 1e-6) / (1024f64.powi(3));
            let get_gibps = size as f64 / (get.p50.max(1e-6) * 1e-6) / (1024f64.powi(3));
            println!(
                "{:<26}{:>8}  {:>10.2}{:>10.2}{:>10.2}{:>10.2}  {:>10.3}{:>10.3}",
                name, fmt_size(size), put.p50, put.p99, get.p50, get.p99, put_gibps, get_gibps
            );
            rows.push(Row {
                name: name.to_string(), size, readers, iters,
                put, get, put_gibps, get_gibps,
            });
        }
        println!();
    }

    if let Some(path) = csv {
        write_csv(&path, &rows)?;
        println!("[csv] wrote {} rows -> {path}", rows.len());
    }
    Ok(())
}

/// Same schema as StateSync-local/bench.py so rows merge into results.csv.
fn write_csv(path: &str, rows: &[Row]) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    writeln!(
        f,
        "approach,size_bytes,readers,iters,put_p50_us,put_p99_us,put_mean_us,\
         get_p50_us,get_p99_us,get_mean_us,put_gibps,get_gibps"
    )?;
    for r in rows {
        writeln!(
            f,
            "{},{},{},{},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{},{}",
            r.name, r.size, r.readers, r.iters,
            r.put.p50, r.put.p99, r.put.mean,
            r.get.p50, r.get.p99, r.get.mean,
            r.put_gibps, r.get_gibps
        )?;
    }
    Ok(())
}

// ── tiny arg helpers ──────────────────────────────────────────────────────────

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn single(args: &[String], name: &str) -> Option<usize> {
    flag(args, name).and_then(|s| s.parse().ok())
}

fn multi_usize(args: &[String], name: &str) -> Option<Vec<usize>> {
    let i = args.iter().position(|a| a == name)?;
    let mut out = Vec::new();
    for a in &args[i + 1..] {
        match a.parse::<usize>() {
            Ok(v) => out.push(v),
            Err(_) => break,
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn fmt_size(n: usize) -> String {
    if n < 1024 {
        format!("{n}B")
    } else if n < 1024 * 1024 {
        format!("{}KiB", n / 1024)
    } else if n < 1024 * 1024 * 1024 {
        format!("{}MiB", n / (1024 * 1024))
    } else {
        format!("{}GiB", n / (1024 * 1024 * 1024))
    }
}
