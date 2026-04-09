// Host-side logger: appends structured text entries into the SHM LOG_ARENA.
//
// The LOG_ARENA is a 16 MB region inside the shared memory superblock whose
// write cursor (`log_offset`) lives at superblock byte-offset +12.  Entries
// written here by the host appear alongside WASM guest log entries and are
// read back by the existing reader in `worker.rs` (`func_b` case):
//
//   let log_bytes = slice::from_raw_parts(log_data_ptr, log_offset as usize);
//   println!("{}", String::from_utf8_lossy(log_bytes));
//
// Entry format (plain UTF-8 text, one per line):
//   "[LEVEL][tag] message\n"
//
// Thread safety: the cursor is advanced with `fetch_add(Ordering::AcqRel)` so
// multiple threads can log concurrently.  Each thread receives an exclusive
// byte-range reservation before writing, so entries never overlap.  The byte
// writes themselves are not individually atomic but are isolated per entry, so
// interleaving is impossible at the entry boundary level.
//
// When the arena is full, entries are silently dropped (no panic, no error) —
// the same approach the guest bump allocator uses when its pages run out.
//
// Usage:
//   let logger = HostLogger::new(splice_addr);
//   logger.info("Loader",  "mmap'd 42 MB input file");
//   logger.warn("Slicer",  "only 3 split points for 8 workers (fewer lines)");
//   logger.error("Worker", "WASM instance returned unexpected status");

use std::sync::atomic::Ordering;

use common::{LOG_ARENA_OFFSET, LOG_ARENA_SIZE, ShmOffset, Superblock};

// -----------------------------------------------------------------------------
// Log level
// -----------------------------------------------------------------------------

/// Severity of a log entry.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info  => "INFO ",
            Level::Warn  => "WARN ",
            Level::Error => "ERROR",
        }
    }
}

// -----------------------------------------------------------------------------
// HostLogger
// -----------------------------------------------------------------------------

/// Appends structured log entries into the SHM LOG_ARENA.
///
/// Constructed from the `splice_addr` — the base address of the SHM superblock
/// as mapped into the host process (identical to the value used by `HostStream`,
/// `ShmIo`, and `BucketOrganizer`).
///
/// `HostLogger` is `Copy`: pass it freely to closures and worker threads.
#[derive(Clone, Copy)]
pub struct HostLogger {
    /// Base address of the SHM superblock in this process's VMA.
    splice_addr: usize,
    /// Minimum level that is written; entries below this threshold are dropped.
    min_level: Level,
}

// SAFETY: all mutations go through the AtomicU32 write cursor; the arena bytes
// are written into exclusive, per-entry-reserved ranges.
unsafe impl Send for HostLogger {}
unsafe impl Sync for HostLogger {}

impl HostLogger {
    /// Create a logger that writes at `Info` level and above.
    pub fn new(splice_addr: usize) -> Self {
        Self { splice_addr, min_level: Level::Info }
    }

    /// Create a logger with an explicit minimum level filter.
    pub fn with_level(splice_addr: usize, min_level: Level) -> Self {
        Self { splice_addr, min_level }
    }

    // ── level-specific helpers ────────────────────────────────────────────────

    pub fn debug(&self, tag: &str, msg: &str) { self.log(Level::Debug, tag, msg); }
    pub fn info (&self, tag: &str, msg: &str) { self.log(Level::Info,  tag, msg); }
    pub fn warn (&self, tag: &str, msg: &str) { self.log(Level::Warn,  tag, msg); }
    pub fn error(&self, tag: &str, msg: &str) { self.log(Level::Error, tag, msg); }

    // ── core write ────────────────────────────────────────────────────────────

    /// Format and append one log entry into the SHM LOG_ARENA.
    ///
    /// Also mirrors the entry to stdout so it is visible in the terminal
    /// alongside the existing `println!` output from other host modules.
    ///
    /// Silently drops the entry if the arena is full or the level is below
    /// the configured minimum.
    pub fn log(&self, level: Level, tag: &str, msg: &str) {
        if level < self.min_level {
            return;
        }

        // Format: "[LEVEL][tag] message\n"
        let entry = format!("[{}][{}] {}\n", level.as_str(), tag, msg);
        let bytes = entry.as_bytes();
        let entry_len = bytes.len() as ShmOffset;

        // Mirror to stdout for live terminal visibility.
        print!("[Host]{}", entry);

        // ── SHM write ────────────────────────────────────────────────────────

        // Access log_offset through the Superblock struct — same pattern as
        // ShmIo::superblock() in routing/shuffle.rs, avoids hardcoded offsets.
        let superblock = unsafe { &*(self.splice_addr as *const Superblock) };
        let log_cursor = &superblock.log_offset;

        let max = LOG_ARENA_SIZE;

        // Reserve space atomically; multiple threads get non-overlapping slots.
        let old_offset = log_cursor.fetch_add(entry_len, Ordering::AcqRel);

        // Bounds-check: if even the start exceeds the arena, nothing to write.
        if old_offset >= max {
            // Undo the reservation so the cursor doesn't drift into oblivion.
            log_cursor.fetch_sub(entry_len, Ordering::AcqRel);
            return;
        }

        // Clamp write length if this entry would straddle the arena boundary.
        let available = (max - old_offset) as usize;
        let write_len = bytes.len().min(available);

        let log_base = unsafe {
            (self.splice_addr as *mut u8).add(LOG_ARENA_OFFSET as usize)
        };

        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                log_base.add(old_offset as usize),
                write_len,
            );
        }

        // If we clamped the write, fix the cursor to reflect the actual bytes
        // written so the reader sees a clean, complete arena.
        if write_len < bytes.len() {
            log_cursor.store(old_offset + write_len as ShmOffset, Ordering::Release);
        }
    }
}
