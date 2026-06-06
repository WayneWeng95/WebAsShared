# `Vec<(u32, Vec<u8>)>`

`Vec<(u32, Vec<u8>)>` is the fully-decoded contents of one slot — the entire chain of records that producers wrote into that slot, parsed out of the raw `[len][origin][payload]…` byte stream into a list you can iterate.

## The outer `Vec` = the list of records in the slot

One element per record; a slot is append-only, so each write (by any producer, or each input line) becomes one entry, and reading the slot walks its page chain to rebuild this list (`read_chain_records`).

## Each `(u32, Vec<u8>)` = one record

- **`u32` — the `origin`**: the slot id of whoever produced the record (stamped at write time, e.g. `slot_loader.rs:332`); it's a provenance tag that matters most at aggregation slots (200, 300) where records from different producers merge, and is often ignored for single-source slots (`for (_origin, rec)`).
- **`Vec<u8>` — the `payload`**: the actual record bytes, untyped — a UTF-8 text line (corpus line, CSV row), a serialized number, or a binary blob (PPM image) — which the slot layer never interprets; the guest function does.

## What it represents in the data flow

Semantically `Vec<(u32, Vec<u8>)>` is "the inbox of a DAG node" — the materialized input a worker reads before processing, or the materialized output the next stage consumes. For example:

- Word Count map reads its shard slot → `Vec<(u32, Vec<u8>)>` where each payload is a line of text → it tokenizes each.
- An Aggregate slot read → `Vec<(u32, Vec<u8>)>` where the `origin` values differ, telling you which mapper each partial result came from.
- Final output (`SlotFlusher::collect`) → `Vec<(u32, Vec<u8>)>` whose payloads get written one-per-line to the result file.

In one line: it's a slot's worth of `(who-produced-it, raw-bytes)` records — the decoded, in-memory form of the byte pipe, ready for a worker to process or for the host to flush to disk.
