// Host-side reader for the reserved OUTPUT_SLOT_ID stream.
//
// The guest calls `ShmApi::write_output` / `ShmApi::write_output_str` to
// append final result records into the OUTPUT_SLOT_ID stream slot.  After
// the producing DAG node completes, an `Output` node triggers `Outputer::save`
// which reads those records from SHM and writes them to the given file path.
//
// No background threads: the read and write happen synchronously in the DAG
// executor, so the output file is guaranteed to exist before the next node runs.
//
// Usage:
//   let outputer = Outputer::new(splice_addr);
//   let count = outputer.save(Path::new("/tmp/result.txt"))?;
//   println!("saved {} records", count);

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use common::{OUTPUT_SLOT_ID, Superblock};

use super::writer::read_stream_records;

pub struct Outputer {
    splice_addr: usize,
}

impl Outputer {
    pub fn new(splice_addr: usize) -> Self {
        Self { splice_addr }
    }

    /// Read all records from the output slot and write them to `path`.
    ///
    /// Each record occupies one line (terminated by `\n`).  Parent directories
    /// are created if absent.  Returns the number of records written, which is
    /// zero when no worker called `ShmApi::write_output` during this run.
    pub fn save(&self, path: &Path) -> Result<usize> {
        let records = self.collect();
        let count = records.len();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut f = fs::File::create(path)?;
        for rec in &records {
            f.write_all(rec)?;
            f.write_all(b"\n")?;
        }

        println!(
            "[Outputer] output slot {} ({} records) → {}",
            OUTPUT_SLOT_ID, count, path.display()
        );
        Ok(count)
    }

    /// Read all records from the output slot into memory without writing to disk.
    ///
    /// Useful when the caller wants to inspect the output before deciding where
    /// (or whether) to save it.
    pub fn collect(&self) -> Vec<Vec<u8>> {
        let sb = unsafe { &*(self.splice_addr as *const Superblock) };
        read_stream_records(self.splice_addr, sb, OUTPUT_SLOT_ID as usize)
    }
}
