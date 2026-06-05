// Host-side reader for reserved output stream slots.
//
// The guest calls `ShmApi::write_output` / `ShmApi::write_output_str` to
// append final result records into `OUTPUT_SLOT_ID`, or calls
// `ShmApi::write_output_to(slot, data)` to target any other slot.  After
// the producing DAG node completes, an `Output` node triggers `SlotFlusher::save`
// (or `save_slot` for a non-default slot) which reads those records from SHM
// and writes them to the given file path, one record per line.
//
// No background threads: the read and write happen synchronously so the output
// file is guaranteed to exist before the next node runs.
//
// Usage:
//   let outputer = SlotFlusher::new(splice_addr);
//   // Default slot (OUTPUT_SLOT_ID):
//   let n = outputer.save(Path::new("/tmp/result.txt"))?;
//   // Explicit slot:
//   let n = outputer.save_slot(Path::new("/tmp/result.txt"), 42)?;

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use common::{OUTPUT_IO_SLOT, Superblock};

use super::persistence::read_io_records;

pub struct SlotFlusher {
    splice_addr: usize,
}

impl SlotFlusher {
    pub fn new(splice_addr: usize) -> Self {
        Self { splice_addr }
    }

    /// Read all records from `OUTPUT_SLOT_ID` and write them to `path`.
    ///
    /// Convenience wrapper around `save_slot(path, OUTPUT_IO_SLOT)`.
    pub fn save(&self, path: &Path) -> Result<usize> {
        self.save_slot(path, OUTPUT_IO_SLOT)
    }

    /// Read all records from `slot` and write them to `path`.
    ///
    /// Each record occupies one line (terminated by `\n`).  Parent directories
    /// are created if absent.  Returns the number of records written, which is
    /// zero when no worker wrote to this slot during the run.
    pub fn save_slot(&self, path: &Path, slot: u32) -> Result<usize> {
        let records = self.collect_slot(slot);
        let count = records.len();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut f = fs::File::create(path)?;
        for (_origin, rec) in &records {
            f.write_all(rec)?;
            f.write_all(b"\n")?;
        }

        println!(
            "[SlotFlusher] slot {} ({} records) → {}",
            slot, count, path.display()
        );
        Ok(count)
    }

    /// Write record *i* of `slot` to `paths[i % paths.len()]` — one file per
    /// record — instead of concatenating every record into a single file.
    ///
    /// Use when a single run produces N records that should land in N separate
    /// files (e.g. a Pipeline emitting one processed image per round).  Records
    /// are written verbatim (no trailing `\n`, unlike `save_slot`, since these
    /// are typically whole binary payloads).  Returns the number of records
    /// written.  Paths that receive no record are left untouched.
    pub fn save_slot_split(&self, paths: &[std::path::PathBuf], slot: u32) -> Result<usize> {
        let records = self.collect_slot(slot);
        if paths.is_empty() {
            return Ok(0);
        }
        for (i, (_origin, rec)) in records.iter().enumerate() {
            let path = &paths[i % paths.len()];
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::File::create(path)?.write_all(rec)?;
            println!(
                "[SlotFlusher] slot {} record {} ({} bytes) → {}",
                slot, i, rec.len(), path.display()
            );
        }
        Ok(records.len())
    }

    /// Collect all records from the default `OUTPUT_IO_SLOT` into memory without writing to disk.
    pub fn collect(&self) -> Vec<(u32, Vec<u8>)> {
        self.collect_slot(OUTPUT_IO_SLOT)
    }

    /// Collect all records from I/O `slot` into memory without writing to disk.
    pub fn collect_slot(&self, slot: u32) -> Vec<(u32, Vec<u8>)> {
        let sb = unsafe { &*(self.splice_addr as *const Superblock) };
        read_io_records(self.splice_addr, sb, slot as usize)
    }
}
