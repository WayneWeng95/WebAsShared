use core::sync::atomic::Ordering;
use alloc::format;
use alloc::string::String;
use crate::api::ShmApi;

/// Barrier test: 3 workers write to private slots, synchronize, then read
/// each other's data and write a combined result to a summary slot.
///
/// arg encodes: low byte = worker_id (0..2), the rest is unused.
///
/// Slot layout:
///   300 + worker_id  → each worker's private output   (written before barrier)
///   310 + worker_id  → each worker's combined summary  (written after barrier)
///
/// Barrier usage:
///   barrier_id = 0,  party_count = 3
///
/// The verification node reads slots 310..312 and checks that every worker
/// observed all three peers' data.
#[no_mangle]
pub extern "C" fn barrier_worker(worker_id: u32) {
    let party_count = 3u32;
    let my_slot = 300 + worker_id;

    // ── Phase 1: produce into private slot ──────────────────────────────────
    let payload = format!("data_from_worker_{}", worker_id);
    ShmApi::append_stream_data(my_slot, payload.as_bytes());

    // Also bump a named atomic so the verification node can confirm all 3 ran.
    let counter = ShmApi::get_named_atomic("barrier_test_arrivals");
    counter.fetch_add(1, Ordering::SeqCst);

    // ── Barrier: wait for all 3 workers ─────────────────────────────────────
    ShmApi::barrier_wait(0, party_count);

    // ── Phase 2: read all peers' slots and combine ──────────────────────────
    let mut combined = String::new();
    for peer in 0..party_count {
        let peer_slot = 300 + peer;
        let records = ShmApi::read_all_stream_records(peer_slot);
        for (_origin, data) in &records {
            if !combined.is_empty() { combined.push('|'); }
            combined.push_str(core::str::from_utf8(data).unwrap_or("?"));
        }
    }

    // Write combined result to summary slot.
    let summary_slot = 310 + worker_id;
    let summary = format!("worker_{}_saw:[{}]", worker_id, combined);
    ShmApi::append_stream_data(summary_slot, summary.as_bytes());
}

/// Verify barrier test results: reads slots 310..312, checks each worker
/// observed all three peers.  Writes PASS/FAIL to the output I/O slot.
#[no_mangle]
pub extern "C" fn barrier_verify(_arg: u32) {
    let mut pass = true;
    let mut report = String::new();

    // Check that all 3 workers arrived (named atomic).
    let counter = ShmApi::get_named_atomic("barrier_test_arrivals");
    let arrivals = counter.load(Ordering::SeqCst);
    report.push_str(&format!("arrivals={}\n", arrivals));
    if arrivals != 3 { pass = false; }

    // Check each summary slot.
    for w in 0u32..3 {
        let slot = 310 + w;
        let records = ShmApi::read_all_stream_records(slot);
        if records.is_empty() {
            report.push_str(&format!("slot {}: EMPTY\n", slot));
            pass = false;
            continue;
        }
        let (_origin, data) = &records[0];
        let text = core::str::from_utf8(data).unwrap_or("(invalid utf8)");
        report.push_str(&format!("slot {}: {}\n", slot, text));

        // Each worker should have seen all 3 peers' data.
        for peer in 0..3u32 {
            let needle = format!("data_from_worker_{}", peer);
            if !text.contains(needle.as_str()) {
                report.push_str(&format!("  FAIL: missing '{}'\n", needle));
                pass = false;
            }
        }
    }

    let verdict = if pass { "PASS" } else { "FAIL" };
    let full_report = format!("barrier_test: {}\n{}", verdict, report);
    ShmApi::append_io_data(1, full_report.as_bytes());
}

/// Multi-barrier test: 4 workers use two barriers (id 1 and id 2) to
/// coordinate a two-phase computation.
///
/// Phase 1: each worker writes a partial sum to slot 320+id.
/// Barrier 1: synchronize.
/// Phase 2: each worker reads all partial sums, computes a global sum,
///           writes it to a named atomic.
/// Barrier 2: synchronize.
/// Phase 3: worker 0 verifies all workers computed the same global sum.
#[no_mangle]
pub extern "C" fn multi_barrier_worker(worker_id: u32) {
    let party_count = 4u32;
    let my_slot = 320 + worker_id;

    // Phase 1: write partial sum (worker_id + 1) * 10.
    let partial = (worker_id + 1) * 10;
    let payload = format!("{}", partial);
    ShmApi::append_stream_data(my_slot, payload.as_bytes());

    // Barrier 1: all partials written.
    ShmApi::barrier_wait(1, party_count);

    // Phase 2: read all partial sums and compute global.
    let mut global_sum: u32 = 0;
    for peer in 0..party_count {
        let peer_slot = 320 + peer;
        let records = ShmApi::read_all_stream_records(peer_slot);
        for (_origin, data) in &records {
            let s = core::str::from_utf8(data).unwrap_or("0");
            global_sum += s.parse::<u32>().unwrap_or(0);
        }
    }

    // Each worker stores its computed global sum.
    let name = format!("multi_barrier_sum_{}", worker_id);
    let atomic = ShmApi::get_named_atomic(&name);
    atomic.store(global_sum as u64, Ordering::SeqCst);

    // Barrier 2: all sums computed.
    ShmApi::barrier_wait(2, party_count);

    // Phase 3: worker 0 checks all agree.
    if worker_id == 0 {
        let expected: u32 = (1 + 2 + 3 + 4) * 10; // = 100
        let mut all_match = true;
        let mut report = String::new();
        for w in 0..party_count {
            let name = format!("multi_barrier_sum_{}", w);
            let atomic = ShmApi::get_named_atomic(&name);
            let val = atomic.load(Ordering::SeqCst) as u32;
            report.push_str(&format!("worker_{}_sum={}\n", w, val));
            if val != expected { all_match = false; }
        }
        let verdict = if all_match { "PASS" } else { "FAIL" };
        let full = format!("multi_barrier_test: {} (expected={})\n{}", verdict, expected, report);
        ShmApi::append_io_data(2, full.as_bytes());
    }
}

/// Verify multi-barrier test: reads I/O slot 2 (written by worker 0).
#[no_mangle]
pub extern "C" fn multi_barrier_verify(_arg: u32) {
    let records = ShmApi::read_all_io_records(2);
    if records.is_empty() {
        ShmApi::append_io_data(3, b"multi_barrier_verify: FAIL (no result in slot 2)");
        return;
    }
    // Forward the result to I/O slot 3 for the Output node.
    for (_origin, data) in &records {
        ShmApi::append_io_data(3, &data);
    }
}
