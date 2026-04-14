// Background persistence writer for SHM data.
//
// Usage:
//   let writer = PersistenceWriter::new();
//   writer.snapshot(splice_addr, &opts);   // non-blocking: copies SHM to heap, queues I/O
//   writer.join();                          // wait for all background writes to finish
//
// The calling thread takes a heap snapshot of the requested SHM regions (atomics,
// stream page-chains, shared-state payloads) and sends it through an mpsc channel.
// A single background thread processes the queue and writes the files, so main
// execution is never blocked waiting for disk I/O.
//
// File layout (relative to output_dir):
//   atomics.txt          — one "name=value" line per named atomic variable
//   stream_{id}.txt      — one "[  N] <record>" line per length-prefixed record
//   shared_{name}.bin    — raw committed payload bytes for each registry entry

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;

use common::*;

// -----------------------------------------------------------------------------
// Public options
// -----------------------------------------------------------------------------

/// Selects which categories of SHM data the writer will persist.
#[derive(Debug, Clone)]
pub struct PersistenceOptions {
    /// Root directory for all output files (created if absent).
    pub output_dir: String,
    /// Persist all named atomic variables visible in the Registry.
    pub atomics: bool,
    /// Stream slot IDs whose length-prefixed page-chain records should be saved.
    pub stream_slots: Vec<usize>,
    /// Persist all Manager-committed shared-state payloads visible in the Registry.
    pub shared_state: bool,
}

// -----------------------------------------------------------------------------
// Heap-only snapshot types (Send: no raw pointers, only Vec / String)
// -----------------------------------------------------------------------------

struct AtomicEntry  { name: String, value: u64 }
struct StreamEntry  { slot_id: usize, records: Vec<(u32, Vec<u8>)> }
struct SharedEntry  { name: String, payload: Vec<u8> }

struct Snapshot {
    output_dir: PathBuf,
    atomics: Vec<AtomicEntry>,
    streams: Vec<StreamEntry>,
    shared:  Vec<SharedEntry>,
}

// -----------------------------------------------------------------------------
// Lightweight watch: a single stream slot or shared-state entry
// -----------------------------------------------------------------------------

/// The item captured by a single `watch_stream` / `watch_shared` call.
/// Carries only heap-allocated data — no raw pointers, fully `Send`.
enum WatchItem {
    Stream { slot_id: usize, records: Vec<(u32, Vec<u8>)>, output: PathBuf },
    Shared { name: String,   payload: Vec<u8>,             output: PathBuf },
}

// -----------------------------------------------------------------------------
// Background writer
// -----------------------------------------------------------------------------

enum PersistMsg { Write(Snapshot), Watch(WatchItem), Stop }

pub struct PersistenceWriter {
    tx:     mpsc::Sender<PersistMsg>,
    handle: Option<thread::JoinHandle<()>>,
}

impl PersistenceWriter {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<PersistMsg>();
        let handle = thread::spawn(move || {
            while let Ok(msg) = rx.recv() {
                match msg {
                    PersistMsg::Write(s)  => write_snapshot(s),
                    PersistMsg::Watch(w)  => write_watch(w),
                    PersistMsg::Stop      => break,
                }
            }
        });
        Self { tx, handle: Some(handle) }
    }

    /// Copy the requested SHM regions into heap memory and queue the result
    /// for background file I/O.  Returns immediately without blocking.
    pub fn snapshot(&self, splice_addr: usize, opts: &PersistenceOptions) {
        let s = take_snapshot(splice_addr, opts);
        let _ = self.tx.send(PersistMsg::Write(s));
    }

    /// Copy only the records of a single stream `slot_id` and write them to
    /// `output` (an exact file path) in the background.
    pub fn watch_stream(&self, splice_addr: usize, slot_id: usize, output: impl Into<PathBuf>) {
        let sb      = unsafe { &*(splice_addr as *const Superblock) };
        let records = read_stream_records(splice_addr, sb, slot_id);
        let _ = self.tx.send(PersistMsg::Watch(WatchItem::Stream {
            slot_id,
            records,
            output: output.into(),
        }));
    }

    /// Copy only the committed payload of a single named shared-state entry and
    /// write it to `output` (an exact file path) in the background.
    /// Logs a warning if `name` is not found in the registry or has no payload.
    pub fn watch_shared(&self, splice_addr: usize, name: &str, output: impl Into<PathBuf>) {
        let sb       = unsafe { &*(splice_addr as *const Superblock) };
        let count    = sb.next_atomic_idx.load(Ordering::Acquire) as usize;
        let reg_base = splice_addr + REGISTRY_OFFSET as usize;

        for i in 0..count {
            let entry      = unsafe { &*((reg_base + i * 64) as *const RegistryEntry) };
            let name_len   = entry.name.iter().position(|&b| b == 0).unwrap_or(52);
            let entry_name = String::from_utf8_lossy(&entry.name[..name_len]);
            if entry_name != name { continue; }

            let payload_offset = entry.payload_offset.load(Ordering::Acquire);
            if payload_offset == 0 {
                eprintln!("[PersistenceWriter] watch_shared: '{}' has no committed payload yet", name);
                return;
            }
            let total_len = entry.payload_len.load(Ordering::Acquire) as usize;
            let payload   = read_shared_payload(splice_addr, payload_offset, total_len);
            let _ = self.tx.send(PersistMsg::Watch(WatchItem::Shared {
                name:   name.to_string(),
                payload,
                output: output.into(),
            }));
            return;
        }
        eprintln!("[PersistenceWriter] watch_shared: '{}' not found in registry", name);
    }

    /// Signal the background thread to stop and block until it finishes all
    /// queued writes.
    pub fn join(&mut self) {
        let _ = self.tx.send(PersistMsg::Stop);
        if let Some(h) = self.handle.take() { let _ = h.join(); }
    }
}

impl Drop for PersistenceWriter {
    fn drop(&mut self) { self.join(); }
}

// -----------------------------------------------------------------------------
// Snapshot: copy SHM data to heap (runs in the calling thread)
// -----------------------------------------------------------------------------

fn take_snapshot(splice_addr: usize, opts: &PersistenceOptions) -> Snapshot {
    let sb    = unsafe { &*(splice_addr as *const Superblock) };
    let count = sb.next_atomic_idx.load(Ordering::Acquire) as usize;

    // ── Atomics ──────────────────────────────────────────────────────────────
    let mut atomics = Vec::new();
    if opts.atomics {
        let reg_base  = splice_addr + REGISTRY_OFFSET as usize;
        let atom_base = splice_addr + ATOMIC_ARENA_OFFSET as usize;
        for i in 0..count {
            let entry = unsafe { &*((reg_base + i * std::mem::size_of::<RegistryEntry>()) as *const RegistryEntry) };
            let name_len = entry.name.iter().position(|&b| b == 0).unwrap_or(52);
            let name = String::from_utf8_lossy(&entry.name[..name_len]).into_owned();
            // AtomicU64 values are packed consecutively in the atomic arena.
            let value = unsafe {
                let ptr = (atom_base as *const std::sync::atomic::AtomicU64).add(i);
                (*ptr).load(Ordering::Acquire)
            };
            atomics.push(AtomicEntry { name, value });
        }
    }

    // ── Stream records ────────────────────────────────────────────────────────
    let streams: Vec<StreamEntry> = opts.stream_slots.iter().map(|&slot| {
        StreamEntry { slot_id: slot, records: read_stream_records(splice_addr, sb, slot) }
    }).collect();

    // ── Shared state ──────────────────────────────────────────────────────────
    let mut shared = Vec::new();
    if opts.shared_state {
        let reg_base = splice_addr + REGISTRY_OFFSET as usize;
        for i in 0..count {
            let entry = unsafe { &*((reg_base + i * std::mem::size_of::<RegistryEntry>()) as *const RegistryEntry) };
            let payload_offset = entry.payload_offset.load(Ordering::Acquire);
            if payload_offset == 0 { continue; }
            let total_len = entry.payload_len.load(Ordering::Acquire) as usize;
            let name_len = entry.name.iter().position(|&b| b == 0).unwrap_or(52);
            let name = String::from_utf8_lossy(&entry.name[..name_len]).into_owned();
            let payload = read_shared_payload(splice_addr, payload_offset, total_len);
            shared.push(SharedEntry { name, payload });
        }
    }

    Snapshot {
        output_dir: PathBuf::from(&opts.output_dir),
        atomics,
        streams,
        shared,
    }
}

// -----------------------------------------------------------------------------
// SHM reading helpers
// -----------------------------------------------------------------------------

/// Walks the length-prefixed page chain for stream `slot` and returns every record.
pub(crate) fn read_stream_records(base: usize, sb: &Superblock, slot: usize) -> Vec<(u32, Vec<u8>)> {
    read_chain_records(base, sb.writer_heads[slot].load(Ordering::Acquire) as ShmOffset)
}

/// Walks the length-prefixed page chain for I/O `slot` and returns every record.
pub(crate) fn read_io_records(base: usize, sb: &Superblock, slot: usize) -> Vec<(u32, Vec<u8>)> {
    read_chain_records(base, sb.io_heads[slot].load(Ordering::Acquire) as ShmOffset)
}

/// Core page-chain walker: given a `head` offset, returns every length-prefixed record as (origin, payload).
fn read_chain_records(base: usize, head: ShmOffset) -> Vec<(u32, Vec<u8>)> {
    if head == 0 { return Vec::new(); }

    let mut reader = PageReader::new(base, head);
    let mut records = Vec::new();
    loop {
        let mut len_buf = [0u8; 4];
        if !reader.read(&mut len_buf) { break; }
        let record_len = u32::from_le_bytes(len_buf) as usize;
        let mut origin_buf = [0u8; 4];
        if !reader.read(&mut origin_buf) { break; }
        let origin = u32::from_le_bytes(origin_buf);
        let mut payload = vec![0u8; record_len];
        if !reader.read(&mut payload) { break; }
        records.push((origin, payload));
    }
    records
}

/// Reads a committed shared-state payload from its page chain.
/// Page layout mirrors the guest `write_shared_state`: head page has a
/// `ChainNodeHeader` (20 bytes) followed by data; continuation pages have a
/// 4-byte next pointer followed by data.
fn read_shared_payload(base: usize, payload_offset: ShmOffset, total_len: usize) -> Vec<u8> {
    let mut result = vec![0u8; total_len];
    let mut current: ShmOffset = payload_offset;
    let mut bytes_read = 0;
    let mut is_head = true;

    while bytes_read < total_len && current != 0 {
        let ptr = (base + current as usize) as *const u8;
        let (header_size, next): (usize, ShmOffset) = if is_head {
            let hdr = unsafe { &*(ptr as *const ChainNodeHeader) };
            (std::mem::size_of::<ChainNodeHeader>(), hdr.next_payload_page as ShmOffset)
        } else {
            let next = unsafe { *(ptr as *const PageId) };
            (std::mem::size_of::<PageId>(), next as ShmOffset)
        };
        let capacity = PAGE_SIZE as usize - header_size;
        let n = capacity.min(total_len - bytes_read);
        unsafe {
            std::ptr::copy_nonoverlapping(
                ptr.add(header_size),
                result.as_mut_ptr().add(bytes_read),
                n,
            );
        }
        bytes_read += n;
        current    = next;
        is_head    = false;
    }
    result
}

/// Sequential page-chain reader for length-prefixed stream records.
struct PageReader {
    base:          usize,
    page_offset:   ShmOffset,
    cursor_in_page: ShmOffset,
}

impl PageReader {
    fn new(base: usize, head: ShmOffset) -> Self {
        Self { base, page_offset: head, cursor_in_page: 0 }
    }

    /// Fill `dest` from the page chain; returns `false` if the chain ends early.
    fn read(&mut self, dest: &mut [u8]) -> bool {
        let mut written = 0usize;
        while written < dest.len() {
            if self.page_offset == 0 { return false; }
            let page = unsafe {
                &*((self.base + self.page_offset as usize) as *const Page)
            };
            let page_written = page.cursor.load(Ordering::Acquire);
            let available    = page_written.saturating_sub(self.cursor_in_page);
            if available == 0 {
                self.page_offset    = page.next_offset.load(Ordering::Acquire) as ShmOffset;
                self.cursor_in_page = 0;
                continue;
            }
            let n = (available as usize).min(dest.len() - written);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    page.data.as_ptr().add(self.cursor_in_page as usize),
                    dest.as_mut_ptr().add(written),
                    n,
                );
            }
            self.cursor_in_page += n as ShmOffset;
            written             += n;
        }
        true
    }
}

// -----------------------------------------------------------------------------
// File I/O (runs in the background thread)
// -----------------------------------------------------------------------------

fn write_watch(item: WatchItem) {
    match item {
        WatchItem::Stream { slot_id, records, output } => {
            if let Some(parent) = output.parent() { let _ = fs::create_dir_all(parent); }
            match fs::File::create(&output) {
                Ok(mut f) => {
                    for (i, (origin, rec)) in records.iter().enumerate() {
                        let _ = writeln!(f, "[{:4}][src={}] {}", i, origin, String::from_utf8_lossy(rec));
                    }
                    println!("[PersistenceWriter] watch stream {} ({} records) → {}",
                        slot_id, records.len(), output.display());
                }
                Err(e) => eprintln!("[PersistenceWriter] watch stream {} failed: {}", slot_id, e),
            }
        }
        WatchItem::Shared { name, payload, output } => {
            if let Some(parent) = output.parent() { let _ = fs::create_dir_all(parent); }
            match fs::write(&output, &payload) {
                Ok(_) => println!("[PersistenceWriter] watch shared '{}' ({} bytes) → {}",
                    name, payload.len(), output.display()),
                Err(e) => eprintln!("[PersistenceWriter] watch shared '{}' failed: {}", name, e),
            }
        }
    }
}

fn write_snapshot(s: Snapshot) {
    if let Err(e) = fs::create_dir_all(&s.output_dir) {
        eprintln!("[PersistenceWriter] Cannot create {}: {}", s.output_dir.display(), e);
        return;
    }

    // atomics.txt — one "name=value" per line
    if !s.atomics.is_empty() {
        let path = s.output_dir.join("atomics.txt");
        match fs::File::create(&path) {
            Ok(mut f) => {
                for a in &s.atomics { let _ = writeln!(f, "{}={}", a.name, a.value); }
                println!("[PersistenceWriter] atomics ({} entries) → {}",
                    s.atomics.len(), path.display());
            }
            Err(e) => eprintln!("[PersistenceWriter] atomics write failed: {}", e),
        }
    }

    // stream_{id}.txt — one "[N] record" per line
    for st in &s.streams {
        let path = s.output_dir.join(format!("stream_{}.txt", st.slot_id));
        match fs::File::create(&path) {
            Ok(mut f) => {
                for (i, (origin, rec)) in st.records.iter().enumerate() {
                    let _ = writeln!(f, "[{:4}][src={}] {}", i, origin, String::from_utf8_lossy(rec));
                }
                println!("[PersistenceWriter] stream {} ({} records) → {}",
                    st.slot_id, st.records.len(), path.display());
            }
            Err(e) => eprintln!("[PersistenceWriter] stream {} write failed: {}", st.slot_id, e),
        }
    }

    // shared_{name}.bin — raw payload bytes
    for ss in &s.shared {
        let safe_name: String = ss.name.chars()
            .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
            .collect();
        let path = s.output_dir.join(format!("shared_{}.bin", safe_name));
        match fs::write(&path, &ss.payload) {
            Ok(_) => println!("[PersistenceWriter] shared '{}' ({} bytes) → {}",
                ss.name, ss.payload.len(), path.display()),
            Err(e) => eprintln!("[PersistenceWriter] shared '{}' write failed: {}", ss.name, e),
        }
    }
}
