use anyhow::{anyhow, Result};
use super::types::{StreamPipelineParams, PyPipelineParams, RemoteSlotKind};
use super::workers::{WasmLoopWorker, PyLoopWorker};
use crate::runtime::remote::{execute_remote_recv, execute_remote_send};
use crate::runtime::mem_operation::reclaimer;
use common::{atomic_shm_offset, REGISTRY_OFFSET, RegistryEntry, Superblock};

// ─── Guest cursor reset helper ────────────────────────────────────────────────

/// Reset the WASM guest's `read_next_*_record` cursor for `slot` to 0 in the
/// SHM ATOMIC_ARENA.
///
/// The WASM guest stores per-slot cursors as named SHM atomics:
///   - stream slot `id`  →  `"stream_cursor_{id}"`
///   - I/O slot `id`     →  `"io_cursor_{id}"`
///
/// After an `rdma_recv` that replaces the page chain, the cursor must be reset
/// so that `read_next_stream_record` / `read_next_io_record` start from the
/// beginning of the fresh chain.
///
/// Silently succeeds if the guest has not yet registered the cursor atomic
/// (implying it was never advanced, so the effective value is already 0).
fn reset_guest_slot_cursor(splice_addr: usize, slot: usize, slot_kind: RemoteSlotKind) {
    use std::sync::atomic::{AtomicU64, Ordering};

    let cursor_name = match slot_kind {
        RemoteSlotKind::Stream => format!("stream_cursor_{}", slot),
        RemoteSlotKind::Io     => format!("io_cursor_{}", slot),
    };

    let mut name_key = [0u8; 52];
    let src = cursor_name.as_bytes();
    name_key[..src.len().min(52)].copy_from_slice(&src[..src.len().min(52)]);

    let sb    = unsafe { &*(splice_addr as *const Superblock) };
    let base  = (splice_addr + REGISTRY_OFFSET as usize) as *const RegistryEntry;
    let count = sb.next_atomic_idx.load(std::sync::atomic::Ordering::Acquire) as usize;
    for i in 0..count {
        let entry = unsafe { &*base.add(i) };
        if entry.name == name_key {
            let ptr = (splice_addr + atomic_shm_offset(entry.index as usize) as usize)
                as *mut AtomicU64;
            unsafe { (*ptr).store(0, Ordering::Release) };
            return;
        }
    }
    // Atomic not yet registered — cursor is implicitly 0, nothing to reset.
}

// ─── StreamPipeline executor ──────────────────────────────────────────────────

/// Execute a `StreamPipeline` node.
///
/// One persistent `wasm-loop` subprocess per stage.  Stages run in an
/// overlapping wave schedule: stage S is active on tick T when
/// `S ≤ T < rounds + S`.  Within a tick all active stages run concurrently
/// via scatter/gather (commands sent first, responses collected after), so
/// each stage's output for round R is ready before stage S+1 consumes it in
/// round R.
///
/// ```text
///   tick 0:  [stage·0·r0]
///   tick 1:  [stage·0·r1]  [stage·1·r0]
///   tick 2:  [stage·0·r2]  [stage·1·r1]  [stage·2·r0]
///   ...
/// ```
///
/// Total ticks = `rounds + depth − 1`.
///
/// ## RDMA pipelining (background threads)
///
/// When both `rdma_recv` and `rdma_send` are configured, RDMA transfers are
/// overlapped with stage processing using two background thread strategies:
///
/// - **Pre-fetch recv**: at the *end* of tick R (after stages finish), the
///   current recv slot is freed/reset and a background thread is spawned to
///   receive round R+1.  The next tick starts by joining that thread, so
///   recv(R+1) overlaps with stage(R).
///
/// - **Double-buffer send** (only when `rdma_send.free_after = true`): the
///   last stage alternates between `rdma.slot` (even rounds) and
///   `rdma.slot+1` (odd rounds).  After stages complete for round R, a
///   background thread sends from the current buffer slot and frees it.
///   The next round's stages write to the *other* slot concurrently, so
///   send(R) overlaps with stage(R+1).  The previous send thread is joined
///   just before spawning the next one to maintain TCP stream ordering.
pub(super) fn execute_stream_pipeline(
    params:      &StreamPipelineParams,
    node_id:     &str,
    shm_path:    &str,
    wasm_path:   &str,
    splice_addr: usize,
    mesh:        Option<&std::sync::Arc<connect::MeshNode>>,
) -> Result<()> {
    let rounds = params.rounds as usize;
    let depth  = params.stages.len();
    if depth == 0 {
        return Err(anyhow!("[{}] StreamPipeline has no stages", node_id));
    }

    // Spawn one persistent worker per stage (wasmtime init paid once).
    let mut workers: Vec<WasmLoopWorker> = params.stages.iter()
        .map(|s| WasmLoopWorker::spawn(&s.func, shm_path, wasm_path, node_id))
        .collect::<Result<Vec<_>>>()?;

    let slot_chain: Vec<String> = params.stages.iter()
        .map(|s| s.arg1.map_or_else(|| format!("{}(r)", s.arg0), |a| format!("{}→{}", s.arg0, a)))
        .collect();
    println!(
        "  StreamPipeline: {} rounds, {} stages | {} ({} persistent workers)",
        rounds, depth, slot_chain.join(" › "), depth
    );

    let ts = || {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_millis();
        ms
    };

    // Background thread handles for overlapping RDMA with stage processing.
    //   pending_send: in-flight send for the previous round.  Joined before
    //                 the next send is spawned (TCP stream serialization).
    //   pending_recv: pre-fetched recv for the current round.  Joined at the
    //                 start of that tick before stages run.
    let mut pending_send: Option<std::thread::JoinHandle<Result<()>>> = None;
    let mut pending_recv: Option<std::thread::JoinHandle<Result<()>>> = None;

    let total_ticks = rounds + depth - 1;
    for tick in 0..total_ticks {
        println!("    [{}] tick {} start  (+{}ms)", node_id, tick, ts());

        // ── RDMA recv ────────────────────────────────────────────────────────
        // tick 0: first recv is synchronous (nothing pre-fetched yet).
        // tick r>0: join the background recv thread spawned at the end of
        //           tick r-1 (pre-fetched while stage r-1 was running).
        if tick < rounds {
            if let (Some(rdma), Some(m)) = (&params.rdma_recv, mesh) {
                if tick == 0 {
                    println!("    [{}] tick {} waiting for rdma_recv round 0 ...", node_id, tick);
                    let ch = m.recv_channel(rdma.peer);
                    execute_remote_recv(splice_addr, rdma.slot, rdma.slot_kind, &ch, rdma.protocol, m)
                        .map_err(|e| anyhow!("[{}] rdma_recv tick 0: {}", node_id, e))?;
                    println!("    [{}] rdma_recv round 0 into slot {} done  (+{}ms)", node_id, rdma.slot, ts());
                } else {
                    // Join the background recv thread spawned at end of previous tick.
                    if let Some(h) = pending_recv.take() {
                        println!("    [{}] tick {} joining pre-fetched rdma_recv round {} ...", node_id, tick, tick);
                        h.join()
                            .map_err(|_| anyhow!("[{}] recv thread panicked at tick {}", node_id, tick))??;
                        println!("    [{}] rdma_recv round {} into slot {} done  (+{}ms)", node_id, tick, rdma.slot, ts());
                    }
                }
            }
        }

        // ── Double-buffer slot for this tick ──────────────────────────────────
        // When rdma_send.free_after is true, the last stage alternates between
        // rdma.slot (even rounds) and rdma.slot+1 (odd rounds) so a background
        // send of round R can overlap with stage(R+1) without slot conflicts.
        let buf_slot: Option<usize> = if tick >= depth - 1 {
            params.rdma_send.as_ref().and_then(|rdma| {
                if rdma.free_after {
                    let round = tick - (depth - 1);
                    Some(rdma.slot + (round % 2))
                } else {
                    None
                }
            })
        } else {
            None
        };

        // ── Collect active stages ─────────────────────────────────────────────
        let active: Vec<(usize, u32)> = params.stages.iter().enumerate()
            .filter_map(|(s_idx, stage)| {
                let tick_round = tick as isize - s_idx as isize;
                if tick_round >= 0 && (tick_round as usize) < rounds {
                    // When double-buffering, override the LAST stage's arg1 to
                    // the current buffer slot so its output lands in the right
                    // slot for the pending background send.
                    let a1 = if let Some(bs) = buf_slot {
                        if s_idx == depth - 1 { bs as u32 }
                        else { stage.arg1.unwrap_or(tick_round as u32) }
                    } else {
                        stage.arg1.unwrap_or(tick_round as u32)
                    };
                    Some((s_idx, a1))
                } else {
                    None
                }
            })
            .collect();

        let stage_desc: Vec<String> = active.iter()
            .map(|&(s, _)| format!("{}(r{})", params.stages[s].func, tick as isize - s as isize))
            .collect();
        println!("    [{}] tick {} stages: [{}]", node_id, tick, stage_desc.join(", "));

        // Scatter: write all commands first — workers wake and run concurrently.
        for &(s_idx, a1) in &active {
            workers[s_idx].send(params.stages[s_idx].arg0, a1)
                .map_err(|e| anyhow!("[{}] stage {} tick {} send: {}", node_id, s_idx, tick, e))?;
        }
        // Gather: collect responses in order (workers may already be done).
        for &(s_idx, _) in &active {
            workers[s_idx].recv()
                .map_err(|e| anyhow!("[{}] stage {} tick {}: {}", node_id, s_idx, tick, e))?;
        }

        // ── RDMA send ─────────────────────────────────────────────────────────
        if tick >= depth - 1 {
            if let (Some(rdma), Some(m)) = (&params.rdma_send, mesh) {
                let round = tick - (depth - 1);
                if rdma.free_after {
                    // Background send with double-buffering.
                    // Join the previous send thread first (TCP channel serialization
                    // — only one outstanding send per connection at a time).
                    if let Some(h) = pending_send.take() {
                        h.join()
                            .map_err(|_| anyhow!("[{}] send thread panicked", node_id))??;
                    }

                    let slot   = buf_slot.unwrap(); // set above when free_after
                    let sk     = rdma.slot_kind;
                    let proto  = rdma.protocol;
                    let ch        = m.send_channel(rdma.peer);
                    let mesh_arc  = m.clone();
                    let node_s    = node_id.to_string();
                    let tid       = tick;

                    println!("    [{}] tick {} spawning background rdma_send round {} from slot {} ...",
                             node_id, tick, round, slot);
                    pending_send = Some(std::thread::spawn(move || {
                        execute_remote_send(splice_addr, slot, sk, &ch, proto, &mesh_arc)
                            .map_err(|e| anyhow!("[{}] rdma_send tick {}: {}", node_s, tid, e))?;
                        // Free the buffer slot so the next use of this slot
                        // (two rounds later) starts with an empty chain.
                        match sk {
                            RemoteSlotKind::Stream => reclaimer::free_stream_slot(splice_addr, slot),
                            RemoteSlotKind::Io     => reclaimer::free_io_slot(splice_addr, slot),
                        }
                        Ok(())
                    }));
                } else {
                    // Synchronous send (free_after=false — slot accumulates
                    // across rounds; background send would race with stages).
                    println!("    [{}] tick {} sending rdma_send round {} ...", node_id, tick, round);
                    let ch = m.send_channel(rdma.peer);
                    execute_remote_send(splice_addr, rdma.slot, rdma.slot_kind, &ch, rdma.protocol, m)
                        .map_err(|e| anyhow!("[{}] rdma_send tick {}: {}", node_id, tick, e))?;
                    println!("    [{}] rdma_send round {} from slot {} done  (+{}ms)",
                             node_id, round, rdma.slot, ts());
                }
            }
        }

        // ── Pre-fetch recv for the next round ─────────────────────────────────
        // The current slot's data has just been consumed by this tick's stages.
        // Free and reset it now so the background thread can write round+1 data
        // into it while this node begins processing other work (or the next tick
        // blocks only on the join, not on the full network transfer).
        let next_round = tick + 1;
        if next_round < rounds {
            if let (Some(rdma), Some(m)) = (&params.rdma_recv, mesh) {
                match rdma.slot_kind {
                    RemoteSlotKind::Stream => reclaimer::free_stream_slot(splice_addr, rdma.slot),
                    RemoteSlotKind::Io     => reclaimer::free_io_slot(splice_addr, rdma.slot),
                }
                reset_guest_slot_cursor(splice_addr, rdma.slot, rdma.slot_kind);

                let slot   = rdma.slot;
                let sk     = rdma.slot_kind;
                let proto  = rdma.protocol;
                let ch        = m.recv_channel(rdma.peer);
                let mesh_arc  = m.clone();
                let node_s    = node_id.to_string();
                let nr        = next_round;

                println!("    [{}] tick {} pre-fetching rdma_recv round {} in background ...",
                         node_id, tick, next_round);
                pending_recv = Some(std::thread::spawn(move || {
                    execute_remote_recv(splice_addr, slot, sk, &ch, proto, &mesh_arc)
                        .map_err(|e| anyhow!("[{}] rdma_recv prefetch round {}: {}", node_s, nr, e))
                }));
            }
        }

        println!("    [{}] tick {} done  (+{}ms)", node_id, tick, ts());
    }

    // Join any remaining background send thread.
    if let Some(h) = pending_send.take() {
        h.join()
            .map_err(|_| anyhow!("[{}] final send thread panicked", node_id))??;
        println!("    [{}] final rdma_send joined  (+{}ms)", node_id, ts());
    }

    // Close stdin on all workers (EOF → they exit) and wait for them.
    for w in workers {
        w.finish()?;
    }

    // Dump the accumulated per-round summaries from the last stage's output slot.
    if let Some(summary_slot) = params.stages.last().and_then(|s| s.arg1) {
        let exe = std::env::current_exe()
            .map_err(|e| anyhow!("cannot find current exe: {}", e))?;
        let status = std::process::Command::new(exe)
            .args(["wasm-call", shm_path, wasm_path, "dump_stream_records", "fatptr",
                   &summary_slot.to_string()])
            .status()
            .map_err(|e| anyhow!("[{}] dump_stream_records spawn: {}", node_id, e))?;
        if !status.success() {
            return Err(anyhow!("[{}] dump_stream_records failed", node_id));
        }
    }

    println!("  StreamPipeline done");
    Ok(())
}

// ─── PyPipeline executor ──────────────────────────────────────────────────────

/// Execute a `PyPipeline` node.
///
/// One persistent `runner.py --loop` process per stage.  Mirrors
/// `StreamPipeline`'s wave schedule but for Python workloads: stage S is
/// active on tick T when `S ≤ T < rounds + S`.  Within a tick all active
/// stages run concurrently via scatter/gather:
///
/// ```text
///   tick 0:  [stage·0·r0]
///   tick 1:  [stage·0·r1]  [stage·1·r0]
///   tick 2:  [stage·0·r2]  [stage·1·r1]  [stage·2·r0]
///   ...
/// ```
///
/// When a stage's `arg2` is `None`, the current round index is injected so
/// the worker can use it as an output slot selector or cursor hint.
///
/// Total ticks = `rounds + depth − 1`.
///
/// RDMA pipelining follows the same pre-fetch recv / double-buffer send
/// strategy as `StreamPipeline` — see that function's documentation.
pub(super) fn execute_py_pipeline(
    params:        &PyPipelineParams,
    node_id:       &str,
    shm_path:      &str,
    python_script: &str,
    python_wasm:   Option<&str>,
    splice_addr:   usize,
    mesh:          Option<&std::sync::Arc<connect::MeshNode>>,
) -> Result<()> {
    let rounds = params.rounds as usize;
    let depth  = params.stages.len();
    if depth == 0 {
        return Err(anyhow!("[{}] PyPipeline has no stages", node_id));
    }

    // Spawn one persistent Python worker per stage (startup paid once).
    let mut workers: Vec<PyLoopWorker> = (0..depth)
        .map(|_| PyLoopWorker::spawn(shm_path, python_script, python_wasm, node_id))
        .collect::<Result<Vec<_>>>()?;

    let slot_chain: Vec<String> = params.stages.iter()
        .map(|s| s.arg2.map_or_else(|| format!("{}(r)", s.arg), |a2| format!("{}→{}", s.arg, a2)))
        .collect();
    println!(
        "  PyPipeline: {} rounds, {} stages | {} ({} persistent workers)",
        rounds, depth, slot_chain.join(" › "), depth
    );

    let ts = || {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_millis()
    };

    let mut pending_send: Option<std::thread::JoinHandle<Result<()>>> = None;
    let mut pending_recv: Option<std::thread::JoinHandle<Result<()>>> = None;

    let total_ticks = rounds + depth - 1;
    for tick in 0..total_ticks {
        println!("    [{}] tick {} start  (+{}ms)", node_id, tick, ts());

        // ── RDMA recv ────────────────────────────────────────────────────────
        if tick < rounds {
            if let (Some(rdma), Some(m)) = (&params.rdma_recv, mesh) {
                if tick == 0 {
                    println!("    [{}] tick {} waiting for rdma_recv round 0 ...", node_id, tick);
                    let ch = m.recv_channel(rdma.peer);
                    execute_remote_recv(splice_addr, rdma.slot, rdma.slot_kind, &ch, rdma.protocol, m)
                        .map_err(|e| anyhow!("[{}] rdma_recv tick 0: {}", node_id, e))?;
                    println!("    [{}] rdma_recv round 0 into slot {} done  (+{}ms)", node_id, rdma.slot, ts());
                } else {
                    if let Some(h) = pending_recv.take() {
                        println!("    [{}] tick {} joining pre-fetched rdma_recv round {} ...", node_id, tick, tick);
                        h.join()
                            .map_err(|_| anyhow!("[{}] recv thread panicked at tick {}", node_id, tick))??;
                        println!("    [{}] rdma_recv round {} into slot {} done  (+{}ms)", node_id, tick, rdma.slot, ts());
                    }
                }
            }
        }

        // ── Double-buffer slot for this tick ──────────────────────────────────
        let buf_slot: Option<usize> = if tick >= depth - 1 {
            params.rdma_send.as_ref().and_then(|rdma| {
                if rdma.free_after {
                    let round = tick - (depth - 1);
                    Some(rdma.slot + (round % 2))
                } else {
                    None
                }
            })
        } else {
            None
        };

        // ── Collect active stages ─────────────────────────────────────────────
        let active: Vec<(usize, Option<u32>)> = params.stages.iter().enumerate()
            .filter_map(|(s_idx, stage)| {
                let tick_round = tick as isize - s_idx as isize;
                if tick_round >= 0 && (tick_round as usize) < rounds {
                    // Override the last stage's arg2 to the double-buffer slot.
                    let a2 = if let Some(bs) = buf_slot {
                        if s_idx == depth - 1 { Some(bs as u32) }
                        else { stage.arg2.or(Some(tick_round as u32)) }
                    } else {
                        stage.arg2.or(Some(tick_round as u32))
                    };
                    Some((s_idx, a2))
                } else {
                    None
                }
            })
            .collect();

        let stage_desc: Vec<String> = active.iter()
            .map(|&(s, _)| format!("{}(r{})", params.stages[s].func, tick as isize - s as isize))
            .collect();
        println!("    [{}] tick {} stages: [{}]", node_id, tick, stage_desc.join(", "));

        // Scatter: write all commands first — workers wake and run concurrently.
        for &(s_idx, a2) in &active {
            workers[s_idx].call_async(&params.stages[s_idx].func, params.stages[s_idx].arg, a2)
                .map_err(|e| anyhow!("[{}] stage {} tick {} send: {}", node_id, s_idx, tick, e))?;
        }
        // Gather: collect responses in order.
        for &(s_idx, _) in &active {
            workers[s_idx].recv()
                .map_err(|e| anyhow!("[{}] stage {} tick {}: {}", node_id, s_idx, tick, e))?;
        }

        // ── RDMA send ─────────────────────────────────────────────────────────
        if tick >= depth - 1 {
            if let (Some(rdma), Some(m)) = (&params.rdma_send, mesh) {
                let round = tick - (depth - 1);
                if rdma.free_after {
                    // Background send with double-buffering.
                    if let Some(h) = pending_send.take() {
                        h.join()
                            .map_err(|_| anyhow!("[{}] send thread panicked", node_id))??;
                    }

                    let slot   = buf_slot.unwrap();
                    let sk     = rdma.slot_kind;
                    let proto  = rdma.protocol;
                    let ch        = m.send_channel(rdma.peer);
                    let mesh_arc  = m.clone();
                    let node_s    = node_id.to_string();
                    let tid       = tick;

                    println!("    [{}] tick {} spawning background rdma_send round {} from slot {} ...",
                             node_id, tick, round, slot);
                    pending_send = Some(std::thread::spawn(move || {
                        execute_remote_send(splice_addr, slot, sk, &ch, proto, &mesh_arc)
                            .map_err(|e| anyhow!("[{}] rdma_send tick {}: {}", node_s, tid, e))?;
                        match sk {
                            RemoteSlotKind::Stream => reclaimer::free_stream_slot(splice_addr, slot),
                            RemoteSlotKind::Io     => reclaimer::free_io_slot(splice_addr, slot),
                        }
                        Ok(())
                    }));
                } else {
                    // Synchronous send (free_after=false).
                    println!("    [{}] tick {} sending rdma_send round {} ...", node_id, tick, round);
                    let ch = m.send_channel(rdma.peer);
                    execute_remote_send(splice_addr, rdma.slot, rdma.slot_kind, &ch, rdma.protocol, m)
                        .map_err(|e| anyhow!("[{}] rdma_send tick {}: {}", node_id, tick, e))?;
                    println!("    [{}] rdma_send round {} from slot {} done  (+{}ms)",
                             node_id, round, rdma.slot, ts());
                }
            }
        }

        // ── Pre-fetch recv for the next round ─────────────────────────────────
        let next_round = tick + 1;
        if next_round < rounds {
            if let (Some(rdma), Some(m)) = (&params.rdma_recv, mesh) {
                match rdma.slot_kind {
                    RemoteSlotKind::Stream => reclaimer::free_stream_slot(splice_addr, rdma.slot),
                    RemoteSlotKind::Io     => reclaimer::free_io_slot(splice_addr, rdma.slot),
                }
                reset_guest_slot_cursor(splice_addr, rdma.slot, rdma.slot_kind);

                let slot   = rdma.slot;
                let sk     = rdma.slot_kind;
                let proto  = rdma.protocol;
                let ch        = m.recv_channel(rdma.peer);
                let mesh_arc  = m.clone();
                let node_s    = node_id.to_string();
                let nr        = next_round;

                println!("    [{}] tick {} pre-fetching rdma_recv round {} in background ...",
                         node_id, tick, next_round);
                pending_recv = Some(std::thread::spawn(move || {
                    execute_remote_recv(splice_addr, slot, sk, &ch, proto, &mesh_arc)
                        .map_err(|e| anyhow!("[{}] rdma_recv prefetch round {}: {}", node_s, nr, e))
                }));
            }
        }

        println!("    [{}] tick {} done  (+{}ms)", node_id, tick, ts());
    }

    // Join any remaining background send thread.
    if let Some(h) = pending_send.take() {
        h.join()
            .map_err(|_| anyhow!("[{}] final send thread panicked", node_id))??;
        println!("    [{}] final rdma_send joined  (+{}ms)", node_id, ts());
    }

    // Close stdin on all workers (EOF → they exit) and wait.
    for w in workers {
        w.finish()?;
    }

    println!("  PyPipeline done");
    Ok(())
}
