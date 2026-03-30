// Dedicated I/O area: a separate, non-overlapping slot pool for host↔guest
// data exchange.
//
// Stream slots (0..STREAM_SLOT_COUNT) are purely for inter-worker pipelines.
// I/O slots (0..IO_SLOT_COUNT) are reserved for host-injected input and
// guest-emitted output that the host `Inputer` / `Outputer` manages.
//
// The page-chain mechanics are identical to the stream area; this module just
// indexes `sb.io_heads` / `sb.io_tails` instead of `sb.writer_heads` /
// `sb.writer_tails`, using the shared helpers from `stream_area`.
//
// Public API:
//   append_io_data(slot, data)         — write a record to an I/O slot
//   read_latest_io_data(slot)           — read the most recent record
//   read_all_io_records(slot)           — read all records in order

use core::sync::atomic::Ordering;
use alloc::vec::Vec;
use common::IO_SLOT_COUNT;
use super::ShmApi;
use super::stream_area::{chain_append_prefixed, chain_read_latest, chain_read_all};

impl ShmApi {
    /// Append a length-prefixed record to I/O slot `io_slot`.
    ///
    /// Used by guests to emit output destined for the host `Outputer`.
    /// Pass `OUTPUT_IO_SLOT` (or any slot the DAG `Output` node targets)
    /// as `io_slot`.
    pub fn append_io_data(io_slot: u32, payload: &[u8]) {
        debug_assert!(
            (io_slot as usize) < IO_SLOT_COUNT,
            "I/O slot {} out of range (IO_SLOT_COUNT={})",
            io_slot, IO_SLOT_COUNT,
        );
        let sb = Self::superblock();
        chain_append_prefixed(
            &sb.io_heads[io_slot as usize],
            &sb.io_tails[io_slot as usize],
            io_slot,
            payload,
        );
    }

    /// Return the most recent record from I/O slot `io_slot`.
    ///
    /// Returns `None` when the host has not written any data into this slot.
    pub fn read_latest_io_data(io_slot: u32) -> Option<(u32, Vec<u8>)> {
        let head = Self::superblock().io_heads[io_slot as usize].load(Ordering::Acquire);
        chain_read_latest(head)
    }

    /// Return every record from I/O slot `io_slot` in order.
    ///
    /// The host `Inputer` writes one record per non-empty line of the loaded
    /// file; this returns them all so the guest can iterate over every line.
    /// Returns an empty `Vec` when no input has been written.
    pub fn read_all_io_records(io_slot: u32) -> Vec<(u32, Vec<u8>)> {
        let head = Self::superblock().io_heads[io_slot as usize].load(Ordering::Acquire);
        chain_read_all(head)
    }

    /// Return the next unconsumed record from I/O slot `io_slot`, advancing a
    /// per-slot cursor stored as a named SHM atomic (`"io_cursor_{slot}"`).
    ///
    /// Returns `None` when no new record has arrived since the last call —
    /// allowing pipeline stages to return early and idle cleanly.
    pub fn read_next_io_record(io_slot: u32) -> Option<(u32, Vec<u8>)> {
        use core::sync::atomic::Ordering;
        let name = alloc::format!("io_cursor_{}", io_slot);
        let atomic = Self::get_named_atomic(&name);
        let idx = atomic.load(Ordering::Acquire) as usize;
        let records = Self::read_all_io_records(io_slot);
        if idx >= records.len() { return None; }
        atomic.fetch_add(1, Ordering::Release);
        records.into_iter().nth(idx)
    }
}
