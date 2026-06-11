use anyhow::{anyhow, Result};
use super::types::{StreamPipelineParams, PyPipelineParams, StreamOutputParams, RemoteSlotKind};
use super::stage_fanout;
use super::workers::{WasmLoopWorker, PyLoopWorker};
use crate::runtime::remote::{execute_remote_recv, execute_remote_send};
use crate::runtime::mem_operation::reclaimer;
use crate::runtime::input_output::persistence::PersistenceWriter;
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

// ─── Per-slot read watermark (per-round race guard) ───────────────────────────
//
// In the pipelined wave schedule, stage S (producing round R) and stage S+1
// (consuming round R-1) run *concurrently* within a tick, and adjacent stages
// share a fixed stream slot.  A consumer that reads "all records since its
// cursor" would race ahead into the records the next round's producer is
// concurrently appending to the same slot — scrambling the per-round boundary
// (only the cumulative total survived).
//
// The host is the only party that runs sequentially *between* ticks, so it
// publishes a per-slot watermark: before each tick's scatter it stores
// `stream_hi_{slot}` = (committed record count of `slot` as of the tick start)
// + 1.  Consumer stages read only up to that watermark, so they see exactly the
// records their producer committed in previous ticks and never the round being
// produced concurrently.  The `+1` is a sentinel: a raw value of 0 means
// "unset" (non-pipeline callers), which the guest treats as unbounded — so the
// same workload code is correct under both `WasmGrouping` (serial) and
// `StreamPipeline` (pipelined).

/// Find-or-create the named SHM atomic in the registry and return its arena
/// index.  Mirrors the spinlock protocol of `host_resolve_atomic` (worker.rs)
/// so a worker process resolving the same name lands on the same index.
fn register_or_get_atomic(splice_addr: usize, name: &str) -> usize {
    use std::sync::atomic::Ordering;
    let mut key = [0u8; 52];
    let src = name.as_bytes();
    let n = src.len().min(52);
    key[..n].copy_from_slice(&src[..n]);

    let sb       = unsafe { &*(splice_addr as *const Superblock) };
    let reg_base = (splice_addr + REGISTRY_OFFSET as usize) as *mut RegistryEntry;

    while sb.registry_lock
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        std::hint::spin_loop();
    }
    let count = sb.next_atomic_idx.load(Ordering::Relaxed);
    let mut idx = u32::MAX;
    for i in 0..count {
        let e = unsafe { &*reg_base.add(i as usize) };
        if e.name == key { idx = e.index; break; }
    }
    if idx == u32::MAX {
        idx = count;
        unsafe {
            core::ptr::write(reg_base.add(count as usize), RegistryEntry {
                name: key,
                index: idx,
                payload_offset: common::AtomicShmOffset::new(0),
                payload_len: std::sync::atomic::AtomicU32::new(0),
            });
        }
        sb.next_atomic_idx.store(count + 1, Ordering::Relaxed);
    }
    sb.registry_lock.store(0, Ordering::Release);
    idx as usize
}

/// Store `val` into the atomic-arena slot at `idx`.
fn store_atomic(splice_addr: usize, idx: usize, val: u64) {
    use std::sync::atomic::{AtomicU64, Ordering};
    let ptr = (splice_addr + atomic_shm_offset(idx) as usize) as *mut AtomicU64;
    unsafe { (*ptr).store(val, Ordering::Release) };
}

/// Load the atomic-arena slot at `idx`.
fn load_atomic(splice_addr: usize, idx: usize) -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    let ptr = (splice_addr + atomic_shm_offset(idx) as usize) as *const AtomicU64;
    unsafe { (*ptr).load(Ordering::Acquire) }
}

/// Target input records per worker per tick — the autoscale set-point.  A stage
/// handling more than this per worker scales up (one more worker), less scales
/// down, so steady-state width ≈ ceil(load / SCALE_TARGET).
const SCALE_TARGET: f64 = 8.0;

/// Desired active width for a dynamic stage given its smoothed per-tick load,
/// clamped to `[floor, max]`.  Used with a ±1-step-per-tick move (hysteresis) so
/// the width ramps rather than flapping on noisy load.
fn desired_width(load_ema: f64, floor: usize, max: usize) -> usize {
    let want = (load_ema / SCALE_TARGET).ceil() as usize;
    want.clamp(floor.max(1), max)
}

/// Committed record count of stream `slot` (cheap, count-only chain walk).
fn slot_record_count(splice_addr: usize, slot: usize) -> usize {
    let sb = unsafe { &*(splice_addr as *const Superblock) };
    crate::runtime::input_output::persistence::count_stream_records(splice_addr, sb, slot)
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

    // Per-stage fan-out width (parallel workers).
    //   static_width = `width` (floor / fixed parallelism; default 1).
    //   spawn_w      = workers to pre-spawn = max(static_width, max_width).
    //   is_dyn       = `max_width > static_width` → the stage AUTOSCALES its
    //                  active width in [static_width, max_width] per tick from
    //                  observed input load (Phase 4); else it stays fixed.
    // A stage with neither field → width 1, the byte-for-byte original path, so
    // existing DAGs are unaffected.
    let static_width: Vec<usize> = params.stages.iter()
        .map(|s| s.width.unwrap_or(1).max(1))
        .collect();
    let spawn_w: Vec<usize> = params.stages.iter().enumerate()
        .map(|(s, stage)| stage.max_width.unwrap_or(0).max(static_width[s]))
        .collect();
    let is_dyn: Vec<bool> = params.stages.iter().enumerate()
        .map(|(s, stage)| stage.max_width.unwrap_or(0) > static_width[s])
        .collect();
    let max_spawn = spawn_w.iter().copied().max().unwrap_or(1);
    if max_spawn > 1 && !stage_fanout::fits_band(depth, max_spawn) {
        return Err(anyhow!(
            "[{}] StreamPipeline width too large: depth {} × width {} exceeds the \
             reserved sub-slot band (max width {}, depth ≤ 8)",
            node_id, depth, max_spawn, stage_fanout::MAX_STAGE_WIDTH
        ));
    }

    // Spawn `spawn_w` persistent workers per stage (wasmtime init paid once each).
    // Dynamic stages pre-spawn up to `max_width` and gate how many run per tick.
    let mut workers: Vec<Vec<WasmLoopWorker>> = params.stages.iter().enumerate()
        .map(|(s, stage)| (0..spawn_w[s])
            .map(|_| WasmLoopWorker::spawn(&stage.func, shm_path, wasm_path, node_id))
            .collect::<Result<Vec<_>>>())
        .collect::<Result<Vec<_>>>()?;

    // Current active width per stage (mutated each tick for dynamic stages).
    let mut active_w: Vec<usize> = static_width.clone();

    let slot_chain: Vec<String> = params.stages.iter().enumerate()
        .map(|(s, stage)| {
            let w = if is_dyn[s] { format!("×≤{}", spawn_w[s]) }
                    else if static_width[s] > 1 { format!("×{}", static_width[s]) }
                    else { String::new() };
            stage.arg1.map_or_else(|| format!("{}(r){}", stage.arg0, w),
                                   |a| format!("{}→{}{}", stage.arg0, a, w))
        })
        .collect();
    let total_workers: usize = spawn_w.iter().sum();
    println!(
        "  StreamPipeline: {} rounds, {} stages | {} ({} persistent workers)",
        rounds, depth, slot_chain.join(" › "), total_workers
    );

    // ── Phase-3 load signals + Phase-4 autoscale state ────────────────────────
    // `stage_load`/`stage_ticks`: accumulated input records + active ticks per
    // stage (a load profile printed at the end).  `load_ema`: smoothed
    // records/tick driving the autoscale policy.  `prev_count`: last seen
    // committed count of each stage's input slot (for the per-tick delta).
    let mut stage_load  = vec![0usize; depth];
    let mut stage_ticks = vec![0usize; depth];
    let mut width_peak  = static_width.clone();
    let mut load_ema    = vec![0.0f64; depth];
    let mut prev_count  = vec![0usize; depth];

    // ── Per-round race guard: per-slot read watermarks ────────────────────────
    // Each consumer stage reads its input slot (`arg0`) bounded by a watermark
    // the host refreshes every tick.  Register the watermark + per-slot cursor
    // atomics once and reset them to 0 so this run starts clean (correct for
    // multi-run / Reset mode, where the same fixed slots are reused).
    let mut hi_idx: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    let mut cursor_idx: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for stage in &params.stages {
        let slot = stage.arg0;
        let cur = register_or_get_atomic(splice_addr, &format!("pipe_cursor_{}", slot));
        store_atomic(splice_addr, cur, 0);
        cursor_idx.insert(slot, cur);
        let hi = register_or_get_atomic(splice_addr, &format!("stream_hi_{}", slot));
        store_atomic(splice_addr, hi, 0);
        hi_idx.insert(slot, hi);
    }

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
                    let ch = m.recv_channel_stream(rdma.peer);
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

        // ── Load signal + autoscale (BEFORE scatter) ─────────────────────────
        //    Per active stage: measure this tick's input load (new committed
        //    records on arg0 since last tick), smooth it, and — for a dynamic
        //    stage — move its active width one step toward the load-driven
        //    target (hysteresis).  Then publish the width-1 read watermark or
        //    scatter the widened input, using the just-chosen active width.
        for &(s_idx, _) in &active {
            let slot = params.stages[s_idx].arg0;
            let cnt  = slot_record_count(splice_addr, slot as usize);
            let load = cnt.saturating_sub(prev_count[s_idx]);
            prev_count[s_idx] = cnt;
            stage_load[s_idx]  += load;
            stage_ticks[s_idx] += 1;

            if is_dyn[s_idx] {
                load_ema[s_idx] = if stage_ticks[s_idx] == 1 { load as f64 }
                                  else { 0.5 * load_ema[s_idx] + 0.5 * load as f64 };
                let want = desired_width(load_ema[s_idx], static_width[s_idx], spawn_w[s_idx]);
                // ±1 step per tick toward the target.
                active_w[s_idx] = if want > active_w[s_idx] { active_w[s_idx] + 1 }
                                  else if want < active_w[s_idx] { active_w[s_idx] - 1 }
                                  else { active_w[s_idx] };
                width_peak[s_idx] = width_peak[s_idx].max(active_w[s_idx]);
            }

            if active_w[s_idx] == 1 {
                // Width-1 fast path: publish the watermark; the worker reads arg0.
                if let Some(&hi) = hi_idx.get(&slot) {
                    store_atomic(splice_addr, hi, cnt as u64 + 1); // +1 sentinel: 0 = unset
                }
            } else {
                // Host plays the consumer: snapshot round R's input window and
                // scatter it round-robin into the stage's private sub-slots.
                scatter_widened_input(splice_addr, slot, active_w[s_idx], s_idx, &cursor_idx)?;
            }
        }

        // Scatter: write all commands first — workers wake and run concurrently.
        // Width-1: send the stage's single worker on (arg0 → a1).  Width-N: send
        // each active worker on its (sub_in → sub_out) private slot pair.
        for &(s_idx, a1) in &active {
            if active_w[s_idx] == 1 {
                workers[s_idx][0].send(params.stages[s_idx].arg0, a1)
                    .map_err(|e| anyhow!("[{}] stage {} tick {} send: {}", node_id, s_idx, tick, e))?;
            } else {
                for k in 0..active_w[s_idx] {
                    let si = stage_fanout::sub_in_slot(s_idx, k) as u32;
                    let so = stage_fanout::sub_out_slot(s_idx, k) as u32;
                    workers[s_idx][k].send(si, so)
                        .map_err(|e| anyhow!("[{}] stage {} worker {} tick {} send: {}", node_id, s_idx, k, tick, e))?;
                }
            }
        }
        // Gather: collect responses; for widened stages, merge the active
        // workers' output sub-slots back into the stage's real output slot (a1)
        // in round-robin record order, then free the sub-slots.
        for &(s_idx, a1) in &active {
            if active_w[s_idx] == 1 {
                workers[s_idx][0].recv()
                    .map_err(|e| anyhow!("[{}] stage {} tick {}: {}", node_id, s_idx, tick, e))?;
            } else {
                for k in 0..active_w[s_idx] {
                    workers[s_idx][k].recv()
                        .map_err(|e| anyhow!("[{}] stage {} worker {} tick {}: {}", node_id, s_idx, k, tick, e))?;
                }
                gather_widened_output(splice_addr, s_idx, active_w[s_idx], a1)?;
            }
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
                    let ch        = m.send_channel_stream(rdma.peer);
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
                    let ch = m.send_channel_stream(rdma.peer);
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
                // The recv slot is freed and refilled with exactly the next round
                // each tick, so a watermark-based consumer (pipe_read_window) must
                // also restart its window from 0 — otherwise its `pipe_cursor`
                // keeps climbing while the slot count resets, and every round
                // after the first reads an empty window.  (No-op for read_next
                // stages, which use the cursor reset just above.)
                if let Some(&cur) = cursor_idx.get(&(rdma.slot as u32)) {
                    store_atomic(splice_addr, cur, 0);
                }

                let slot   = rdma.slot;
                let sk     = rdma.slot_kind;
                let proto  = rdma.protocol;
                let ch        = m.recv_channel_stream(rdma.peer);
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

    // ── Load profile (Phase 3) + autoscale summary (Phase 4) ──────────────────
    // Per-stage input records / active ticks, and — for dynamic stages — the
    // peak active width the load drove them to.  Identifies the bottleneck stage
    // and shows the autoscaler responding to load.
    println!("  [{}] StreamPipeline load profile:", node_id);
    for s in 0..depth {
        let avg = if stage_ticks[s] > 0 { stage_load[s] as f64 / stage_ticks[s] as f64 } else { 0.0 };
        let wdesc = if is_dyn[s] {
            format!("dyn {}→peak {} (max {})", static_width[s], width_peak[s], spawn_w[s])
        } else {
            format!("width {}", static_width[s])
        };
        println!("    stage {} {:<16} {}: {} records / {} ticks (avg {:.1}/tick)",
                 s, params.stages[s].func, wdesc, stage_load[s], stage_ticks[s], avg);
    }

    // Close stdin on all workers (EOF → they exit) and wait for them.
    for stage_workers in workers {
        for w in stage_workers {
            w.finish()?;
        }
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

// ─── Widened-stage scatter / gather (stage fan-out, width > 1) ────────────────

/// Snapshot this tick's input window for a widened stage from `arg0` and scatter
/// it round-robin into the stage's private input sub-slots, then prime each
/// sub-slot's read cursors + watermark so the worker reads its whole shard
/// (works for both `read_next_*` and `pipe_read_window` workloads).  Advances
/// the host's own `arg0` consume cursor by the window size.
fn scatter_widened_input(
    splice_addr: usize,
    arg0: u32,
    width: usize,
    s_idx: usize,
    cursor_idx: &std::collections::HashMap<u32, usize>,
) -> Result<()> {
    // Race-safe window: only records committed before this tick's producers run.
    let cnt = slot_record_count(splice_addr, arg0 as usize);
    let cur = cursor_idx.get(&arg0).map(|&i| load_atomic(splice_addr, i) as usize).unwrap_or(0);
    let all = stage_fanout::read_slot_records(splice_addr, arg0 as usize);
    let window = all.into_iter().skip(cur).take(cnt.saturating_sub(cur));

    // Reset the private sub-slots (fresh chain each tick).
    for k in 0..width {
        stage_fanout::free_sub_slot(splice_addr, stage_fanout::sub_in_slot(s_idx, k));
        stage_fanout::free_sub_slot(splice_addr, stage_fanout::sub_out_slot(s_idx, k));
    }
    // Round-robin scatter: record j → worker j % width.
    for (j, (origin, payload)) in window.enumerate() {
        let si = stage_fanout::sub_in_slot(s_idx, j % width);
        stage_fanout::append_stream_record(splice_addr, si, origin, &payload)?;
    }
    // Advance the host's consume cursor past the window.
    if let Some(&i) = cursor_idx.get(&arg0) { store_atomic(splice_addr, i, cnt as u64); }

    // Prime each sub-input slot's cursors + watermark so the worker reads it all.
    for k in 0..width {
        let si = stage_fanout::sub_in_slot(s_idx, k);
        let cnt_k = slot_record_count(splice_addr, si);
        for name in [format!("stream_cursor_{}", si), format!("pipe_cursor_{}", si)] {
            let idx = register_or_get_atomic(splice_addr, &name);
            store_atomic(splice_addr, idx, 0);
        }
        let hi = register_or_get_atomic(splice_addr, &format!("stream_hi_{}", si));
        store_atomic(splice_addr, hi, cnt_k as u64 + 1); // +1 sentinel: 0 = unset
    }
    Ok(())
}

/// Merge a widened stage's output sub-slots back into its real output slot `a1`
/// in round-robin record order (worker 0 rec 0, worker 1 rec 0, …), preserving
/// each record's `origin`, then free the sub-slots.
fn gather_widened_output(splice_addr: usize, s_idx: usize, width: usize, a1: u32) -> Result<()> {
    let outs: Vec<Vec<(u32, Vec<u8>)>> = (0..width)
        .map(|k| stage_fanout::read_slot_records(splice_addr, stage_fanout::sub_out_slot(s_idx, k)))
        .collect();
    let max_len = outs.iter().map(|v| v.len()).max().unwrap_or(0);
    for pos in 0..max_len {
        for out in &outs {
            if let Some((origin, payload)) = out.get(pos) {
                stage_fanout::append_stream_record(splice_addr, a1 as usize, *origin, payload)?;
            }
        }
    }
    for k in 0..width {
        stage_fanout::free_sub_slot(splice_addr, stage_fanout::sub_in_slot(s_idx, k));
        stage_fanout::free_sub_slot(splice_addr, stage_fanout::sub_out_slot(s_idx, k));
    }
    Ok(())
}

// ─── StreamOutput executor (per-round streaming sink) ─────────────────────────

/// Execute a `StreamOutput` node — a per-round output sink (one file per round).
///
/// For each round `R` in `0..rounds`:
///   1. If `rdma_recv` is configured (coordinator-side per-round RETURN), receive
///      round R from the peer over the dedicated streaming RDMA lane
///      (`recv_channel_stream` / conn-4) into a freshly-freed `slot`.  This is the
///      receive half of the worker's embedded `rdma_send`; the two ride opposite
///      directions of the streaming lane so a source pipeline on the same host can
///      send concurrently on conn-3 without a control-channel collision.
///   2. Capture the slot's record(s) and queue a binary-safe write to
///      `paths[R % paths.len()]` via the background `PersistenceWriter` (records are
///      copied to heap synchronously, so the slot can be freed immediately after).
///   3. Free `slot` so round R+1's receive starts from an empty page chain.
///
/// Runs on its own OS thread (see the scheduler in `mod.rs`) so it can receive
/// while a source `StreamPipeline` streams input out on the main thread.
pub(super) fn execute_stream_output(
    params:      &StreamOutputParams,
    node_id:     &str,
    splice_addr: usize,
    mesh:        Option<&std::sync::Arc<connect::MeshNode>>,
) -> Result<()> {
    let rounds = params.rounds as usize;
    if params.paths.is_empty() {
        return Err(anyhow!("[{}] StreamOutput has no output paths", node_id));
    }
    let is_io = matches!(params.slot_kind, RemoteSlotKind::Io);
    println!(
        "  StreamOutput: {} rounds → {} path(s), slot {} ({:?}), binary={}, recv={}",
        rounds, params.paths.len(), params.slot, params.slot_kind, params.binary,
        params.rdma_recv.is_some()
    );

    let writer = PersistenceWriter::new();

    for round in 0..rounds {
        // ── Per-round receive (coordinator-side output return) ────────────────
        if let Some(rdma) = &params.rdma_recv {
            let m = mesh.ok_or_else(|| anyhow!(
                "[{}] StreamOutput.rdma_recv requires dag.rdma to be configured", node_id
            ))?;
            let ch = m.recv_channel_stream(rdma.peer);
            execute_remote_recv(splice_addr, rdma.slot, rdma.slot_kind, &ch, rdma.protocol, m)
                .map_err(|e| anyhow!("[{}] StreamOutput recv round {}: {}", node_id, round, e))?;
            println!("    [{}] StreamOutput round {} received into slot {}", node_id, round, rdma.slot);
        }

        // ── Write this round to its own file ──────────────────────────────────
        let path = &params.paths[round % params.paths.len()];
        if params.binary {
            writer.watch_slot_binary(splice_addr, params.slot, is_io, path);
        } else {
            // Text line-dump (Stream slots only); mirrors `watch_stream`.
            writer.watch_stream(splice_addr, params.slot, path);
        }
        println!("    [{}] StreamOutput round {} → \"{}\"", node_id, round, path);

        // ── Free the slot so the next round's receive starts clean ────────────
        if params.rdma_recv.is_some() {
            match params.slot_kind {
                RemoteSlotKind::Stream => reclaimer::free_stream_slot(splice_addr, params.slot),
                RemoteSlotKind::Io     => reclaimer::free_io_slot(splice_addr, params.slot),
            }
            reset_guest_slot_cursor(splice_addr, params.slot, params.slot_kind);
        }
    }

    // Drop joins the writer — all queued per-round files are flushed here.
    drop(writer);
    println!("  StreamOutput done ({} rounds written)", rounds);
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

    // ── Per-round race guard: per-slot read watermarks (see execute_stream_pipeline).
    // PyPipeline stages read their input slot `arg`; refresh `stream_hi_{arg}`
    // each tick to the pre-scatter committed count so a Python consumer cannot
    // race ahead into the next round's concurrently-produced records.
    let mut hi_idx: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    let mut cursor_idx: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for stage in &params.stages {
        let slot = stage.arg;
        let cur = register_or_get_atomic(splice_addr, &format!("pipe_cursor_{}", slot));
        store_atomic(splice_addr, cur, 0);
        cursor_idx.insert(slot, cur);
        let hi = register_or_get_atomic(splice_addr, &format!("stream_hi_{}", slot));
        store_atomic(splice_addr, hi, 0);
        hi_idx.insert(slot, hi);
    }

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
                    let ch = m.recv_channel_stream(rdma.peer);
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

        // Publish read watermarks BEFORE scatter (pre-tick committed counts).
        for &(s_idx, _) in &active {
            let slot = params.stages[s_idx].arg;
            if let Some(&hi) = hi_idx.get(&slot) {
                let cnt = slot_record_count(splice_addr, slot as usize);
                store_atomic(splice_addr, hi, cnt as u64 + 1); // +1 sentinel: 0 = unset
            }
        }

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
                    let ch        = m.send_channel_stream(rdma.peer);
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
                    let ch = m.send_channel_stream(rdma.peer);
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
                // Watermark consumer restart (see execute_stream_pipeline): the
                // recv slot is refilled with exactly the next round each tick.
                if let Some(&cur) = cursor_idx.get(&(rdma.slot as u32)) {
                    store_atomic(splice_addr, cur, 0);
                }

                let slot   = rdma.slot;
                let sk     = rdma.slot_kind;
                let proto  = rdma.protocol;
                let ch        = m.recv_channel_stream(rdma.peer);
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
