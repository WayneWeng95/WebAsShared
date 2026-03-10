use anyhow::Result;
use std::env;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::shm::format_shared_memory;

// Used for testing the shared memory read and write functionality

/// Entry point for the manager process.
/// Initializes the shared memory file, spawns 4 reader and 4 writer worker subprocesses,
/// waits for all to finish, then chains `func_a` and `func_b` invocations sequentially.
pub fn run_manager() -> Result<()> {
    println!("[Manager] Formatting Initial 16MB Dynamic Shared Memory...");
    
    let shm_path_buf = Path::new("/dev/shm").join("my_serverless_shm");
    let shm_path = shm_path_buf.to_str().unwrap().to_string();

    // Initialize the shared memory
    format_shared_memory(&shm_path)?;

    let myself = env::current_exe()?;

    // Start the reader tasks
    let mut reader_handles = Vec::new(); 
    for i in 0..4 {
        reader_handles.push(Command::new(&myself).arg("reader").arg(&shm_path).arg(i.to_string()).spawn()?);
    }
    thread::sleep(Duration::from_millis(300));

    // Start the writer tasks
    let mut writer_handles = Vec::new();
    for i in 0..4 {
        writer_handles.push(Command::new(&myself).arg("writer").arg(&shm_path).arg(i.to_string()).spawn()?);
    }

    // Wait for all tasks to complete
    for mut handle in writer_handles { handle.wait()?; }
    for mut handle in reader_handles { handle.wait()?; }

    Command::new(&myself).arg("func_a").arg(&shm_path).arg("1").spawn()?;

    thread::sleep(Duration::from_millis(500));
    
    Command::new(&myself).arg("func_b").arg(&shm_path).arg("2").spawn()?;

    thread::sleep(Duration::from_millis(500));

    // ─────────────────────────────────────────────────────────────────────
    // Routing tests
    //
    // Each test gets its own freshly-formatted shared memory region so the
    // page chains from the basic read/write phase above do not interfere.
    // The subprocess handles the full scenario: it populates stream slots via
    // WASM guest helpers, applies the host-side routing API, then reads back
    // through WASM to verify the data arrived in the expected downstream slot.
    // ─────────────────────────────────────────────────────────────────────
    println!("\n[Manager] ── Starting routing tests ──");

    // Test 1: HostStream 1→1 bridge
    // Producer 0 writes to slot 0; host wires slot 1's head/tail to slot 0's
    // page chain (zero copy, two atomic stores); WASM reader confirms slot 1
    // returns producer 0's last record.
    let stream_shm = Path::new("/dev/shm").join("shm_stream_bridge");
    let stream_shm_str = stream_shm.to_str().unwrap().to_string();
    format_shared_memory(&stream_shm_str)?;
    Command::new(&myself)
        .arg("stream_bridge_test").arg(&stream_shm_str).arg("0")
        .spawn()?.wait()?;

    // Test 2: AggregateConnection N→1 merge
    // Producers 0, 1, 2 each write 3 records to slots 0–2; host splices all
    // three chains onto slot 3; WASM reader on slot 3 returns producer 2's
    // final record (last chain merged), confirming the aggregate path.
    let agg_shm = Path::new("/dev/shm").join("shm_aggregate_test");
    let agg_shm_str = agg_shm.to_str().unwrap().to_string();
    format_shared_memory(&agg_shm_str)?;
    Command::new(&myself)
        .arg("aggregate_test").arg(&agg_shm_str).arg("0")
        .spawn()?.wait()?;

    // Test 3: ShuffleConnection — ModuloPartition (upstream_id % num_downstream)
    // Equivalent to the former |up| up % 2 closure; now expressed as a named
    // policy struct.  Upstream 0 → slot 2, upstream 1 → slot 3.
    let shuffle_shm = Path::new("/dev/shm").join("shm_shuffle_modulo");
    let shuffle_shm_str = shuffle_shm.to_str().unwrap().to_string();
    format_shared_memory(&shuffle_shm_str)?;
    Command::new(&myself)
        .arg("shuffle_test").arg(&shuffle_shm_str).arg("0")
        .spawn()?.wait()?;

    // Test 4: ShuffleConnection — RoundRobinPartition
    // Upstreams supplied in reverse order [1, 0] so call-order assignment
    // differs from ID-value assignment: slot 2 → producer 1, slot 3 → producer 0.
    let rr_shm = Path::new("/dev/shm").join("shm_shuffle_roundrobin");
    let rr_shm_str = rr_shm.to_str().unwrap().to_string();
    format_shared_memory(&rr_shm_str)?;
    Command::new(&myself)
        .arg("shuffle_roundrobin_test").arg(&rr_shm_str).arg("0")
        .spawn()?.wait()?;

    // Test 5: ShuffleConnection — FixedMapPartition
    // Explicit swapped table {0→slot1, 1→slot0}: upstream 0 → downstream slot 3,
    // upstream 1 → downstream slot 2, the inverse of what Modulo produces.
    let fm_shm = Path::new("/dev/shm").join("shm_shuffle_fixedmap");
    let fm_shm_str = fm_shm.to_str().unwrap().to_string();
    format_shared_memory(&fm_shm_str)?;
    Command::new(&myself)
        .arg("shuffle_fixedmap_test").arg(&fm_shm_str).arg("0")
        .spawn()?.wait()?;

    // Heavy shuffle tests — same topology variants but with 150 records per
    // producer (~10 KB / 2–3 pages per upstream slot) to exercise multi-page
    // chain splicing and cross-slot record counting.

    // Heavy Test 1: ModuloPartition — straight 2→2, 150 records each
    let heavy_modulo_shm = Path::new("/dev/shm").join("shm_shuffle_heavy_modulo");
    let heavy_modulo_shm_str = heavy_modulo_shm.to_str().unwrap().to_string();
    format_shared_memory(&heavy_modulo_shm_str)?;
    Command::new(&myself)
        .arg("shuffle_heavy_test").arg(&heavy_modulo_shm_str).arg("0")
        .spawn()?.wait()?;

    // Heavy Test 2: RoundRobinPartition — reversed upstreams [1,0], 150 records each
    let heavy_rr_shm = Path::new("/dev/shm").join("shm_shuffle_heavy_rr");
    let heavy_rr_shm_str = heavy_rr_shm.to_str().unwrap().to_string();
    format_shared_memory(&heavy_rr_shm_str)?;
    Command::new(&myself)
        .arg("shuffle_roundrobin_heavy_test").arg(&heavy_rr_shm_str).arg("0")
        .spawn()?.wait()?;

    // Heavy Test 3: FixedMapPartition fan-in {0→0,1→0} — both upstreams to slot 2,
    // slot 3 deliberately empty; 300 merged records verified in downstream slot 2
    let heavy_fm_shm = Path::new("/dev/shm").join("shm_shuffle_heavy_fixedmap");
    let heavy_fm_shm_str = heavy_fm_shm.to_str().unwrap().to_string();
    format_shared_memory(&heavy_fm_shm_str)?;
    Command::new(&myself)
        .arg("shuffle_fixedmap_heavy_test").arg(&heavy_fm_shm_str).arg("0")
        .spawn()?.wait()?;

    println!("[Manager] ── Routing tests completed ──\n");
    println!("[Manager] Completed.");
    Ok(())
}