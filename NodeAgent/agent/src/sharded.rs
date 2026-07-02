//! Sharded (job-level divide-and-merge) execution — Goal 1, single-node.
//!
//! A `ShardedJob` runs a workload whose input is too large for one executor's
//! ~3.48 GiB SHM window by splitting it into `N` shards, running each shard in
//! its OWN SHM region with its OWN executor (concurrently), then merging the
//! smaller partial outputs. Split and merge are NATIVE `host` subprocesses (no
//! WASM, no 4 GiB cap); only the per-shard compute runs in WASM.
//!
//! This path is LOCAL to the coordinator node — it spawns everything here and
//! touches no worker sockets, so it needs none of the concurrent-job control
//! plane (that's Goal 2). See `Dev_docs/parallel_jobs.md`.

use crate::config::AgentConfig;
use crate::coordinator::{CoordinatorState, WorkerConn};
use crate::executor::ExecutorHandle;
use crate::protocol::*;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::net::TcpStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A divide-and-merge job spec, submitted as the DAG JSON (detected by the
/// presence of the `per_shard_dag` key).
#[derive(Debug, Deserialize)]
pub struct ShardedJob {
    /// Path to the large input file (on the coordinator node).
    pub input: String,
    /// Number of shards `N`. If omitted (0), it is derived from the input size:
    /// `N = ceil(filesize / max_shard_bytes)` (see `max_shard_bytes`).
    #[serde(default)]
    pub shards: usize,
    /// Target max bytes per shard, used only when `shards` is omitted, so a large
    /// file is auto-split into enough shards that each fits under one executor's
    /// SHM window. Default `DEFAULT_MAX_SHARD_BYTES` (3 GiB).
    #[serde(default)]
    pub max_shard_bytes: Option<u64>,
    /// SHM path prefix; shard `i` uses `{shm_path_prefix}_s{i}`.
    pub shm_path_prefix: String,
    /// Template per-shard DAG (a normal single-node Dag). The tokens
    /// `{{SHARD_INPUT}}` and `{{SHARD_OUTPUT}}` are substituted per shard, and
    /// the top-level `shm_path` is overridden to the shard's unique region.
    pub per_shard_dag: serde_json::Value,
    /// How to fold the partials into the final output.
    pub merge: MergeSpec,
    /// Final output path.
    pub output: String,
    /// Working directory for shard/partial files (default: `/tmp/sharded_{id}`).
    #[serde(default)]
    pub work_dir: Option<String>,
    /// Spread shards across the cluster instead of running them all locally.
    /// Shards are processed in waves of one-per-node (coordinator + connected
    /// workers); with more shards than nodes the extra shards run in later waves.
    /// Default `false` (all shards local).
    #[serde(default)]
    pub distribute: bool,
    /// LOCAL path only: the max number of shard executors running at once, so a
    /// file with more shards than fit in RAM is processed in windows of this many.
    /// Default = `shards` (all at once). Ignored when `distribute` is set (there
    /// the wave size is the node count).
    #[serde(default)]
    pub max_concurrent: Option<usize>,
    /// Keep the intermediate shard/partial files and SHM regions after the job
    /// (for debugging). Default `false` — they are removed once the merge is done.
    #[serde(default)]
    pub keep_intermediates: bool,
}

#[derive(Debug, Deserialize)]
pub struct MergeSpec {
    /// Built-in reducer name (`wordcount`, `counters`, `concat`).
    #[serde(default)]
    pub reducer: Option<String>,
    /// Workload-supplied native merge source (P3 — not yet implemented).
    #[serde(default)]
    pub source: Option<String>,
}

/// Default target bytes-per-shard for size-based auto-split: 3 GiB, comfortably
/// under one executor's ~3.48 GiB SHM window (leaving room for working state).
const DEFAULT_MAX_SHARD_BYTES: u64 = 3 * 1024 * 1024 * 1024;

/// Resolve the shard count when it wasn't given explicitly: derive it from the
/// input file size and the target bytes-per-shard, so a file too large for one
/// SHM window is split into just enough shards. No-op when `shards` is already set.
fn resolve_shards(spec: &mut ShardedJob) -> Result<()> {
    if spec.shards > 0 {
        return Ok(());
    }
    let size = std::fs::metadata(&spec.input)
        .with_context(|| format!("stat input for auto-split: {}", spec.input))?
        .len();
    let target = spec.max_shard_bytes.unwrap_or(DEFAULT_MAX_SHARD_BYTES);
    if target == 0 {
        bail!("sharded job: max_shard_bytes must be > 0");
    }
    let n = (size.div_ceil(target)).max(1) as usize;
    spec.shards = n;
    println!(
        "[coordinator]   auto-split: {} byte input / {} per shard → {} shard(s)",
        size, target, n
    );
    Ok(())
}

/// True if the submitted JSON is a `ShardedJob` (vs a SymbolicDag / ClusterDag).
pub fn is_sharded_job(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.get("per_shard_dag").cloned())
        .is_some()
}

/// Run a sharded job end-to-end: split → N shard executors → merge. The compute
/// stage runs either all locally, or one shard per node across the cluster when
/// `distribute` is set. Split and merge are always native and local (on the
/// coordinator). Sends `SubmitAck` immediately, then `JobResult` on completion.
pub fn handle_sharded_submit(
    config: &AgentConfig,
    state: &Arc<Mutex<CoordinatorState>>,
    spec_json: &str,
    client_stream: &mut TcpStream,
) -> Result<()> {
    let mut spec: ShardedJob =
        serde_json::from_str(spec_json).context("parse ShardedJob spec")?;
    // Derive the shard count from the input size when it wasn't given explicitly.
    if let Err(e) = resolve_shards(&mut spec) {
        // Pre-ACK failure (e.g. missing input): report and bail before the client
        // waits on a job that can't start.
        return Err(e);
    }
    let job_id = format!("sharded_{}", simple_id());

    // ACK as soon as the spec parses. From here on, any failure is reported to the
    // client as a failed JobResult rather than propagated — a bad submit must not
    // take down the coordinator's accept loop.
    send_message(
        client_stream,
        &make_message(MessageKind::SubmitAck, &serde_json::json!({ "job_id": &job_id }))?,
    )?;

    let (success, summary) = match run_sharded_job(config, state, &spec, &job_id) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("[coordinator] sharded job {} failed: {:#}", job_id, e);
            (false, format!("sharded job failed: {:#}", e))
        }
    };

    println!("[coordinator] ── Sharded Job Summary ─────────────────────");
    for line in summary.lines() {
        println!("[coordinator]   {}", line);
    }
    println!("[coordinator] ─────────────────────────────────────────");

    let _ = send_message(
        client_stream,
        &make_message(
            MessageKind::JobResult,
            &JobResultPayload { job_id, success, summary },
        )?,
    );
    Ok(())
}

/// Execute a sharded job (split → compute → merge) and return `(success,
/// summary)`. All fallible work lives here so the caller can turn an error into a
/// failed result instead of crashing the coordinator.
fn run_sharded_job(
    config: &AgentConfig,
    state: &Arc<Mutex<CoordinatorState>>,
    spec: &ShardedJob,
    job_id: &str,
) -> Result<(bool, String)> {
    if spec.shards == 0 {
        bail!("shards must be >= 1");
    }
    // Exactly one of a built-in reducer or a workload-supplied native merge source.
    let merge_mode = match (
        spec.merge.reducer.as_deref().filter(|r| !r.is_empty()),
        spec.merge.source.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(r), None) => MergeMode::Reducer(r.to_string()),
        (None, Some(s)) => MergeMode::Source(s.to_string()),
        (Some(_), Some(_)) => bail!("set exactly one of merge.reducer / merge.source, not both"),
        (None, None) => bail!("merge.reducer or merge.source is required"),
    };
    let merge_desc = match &merge_mode {
        MergeMode::Reducer(r) => format!("reducer={}", r),
        MergeMode::Source(s) => format!("source={}", s),
    };

    let work_dir = spec
        .work_dir
        .clone()
        .unwrap_or_else(|| format!("/tmp/{}", job_id));
    std::fs::create_dir_all(&work_dir)
        .with_context(|| format!("create work dir: {}", work_dir))?;

    println!(
        "[coordinator] sharded job {}: input={} shards={} {} {}→ {}",
        job_id,
        spec.input,
        spec.shards,
        merge_desc,
        if spec.distribute { "(distributed) " } else { "" },
        spec.output
    );

    let job_start = Instant::now();
    let host_bin = Path::new(&config.paths.executor_bin);

    // ── Stage 1: native split ────────────────────────────────────────────────
    let shard_prefix = format!("{}/shard", work_dir);
    let split_start = Instant::now();
    run_native(
        host_bin,
        &["split", &spec.input, &spec.shards.to_string(), &shard_prefix],
    )
    .context("native split stage")?;
    let split_ms = split_start.elapsed().as_millis() as u64;

    // ── Stage 2: run the shards (all-local, or one-per-node distributed) ──────
    let compute_start = Instant::now();
    let partials: Vec<String> = (0..spec.shards)
        .map(|i| format!("{}/partial.{}", work_dir, i))
        .collect();
    let timeout = Duration::from_secs(config.timeouts.job_timeout_s);
    let (shard_ms, failures) = if spec.distribute {
        run_shards_distributed(config, state, spec, job_id, &shard_prefix, &partials, timeout)?
    } else {
        run_shards_local(config, spec, job_id, &shard_prefix, &partials, timeout)?
    };
    let compute_ms = compute_start.elapsed().as_millis() as u64;

    // ── Stage 3: native merge (only if every shard produced its partial) ──────
    let mut merge_ms = 0u64;
    let success = failures.is_empty();
    if success {
        let merge_start = Instant::now();
        match &merge_mode {
            MergeMode::Reducer(reducer) => {
                // Built-in reducer via `host merge <reducer> <output> <partial...>`.
                let mut merge_args: Vec<String> =
                    vec!["merge".into(), reducer.clone(), spec.output.clone()];
                merge_args.extend(partials.iter().cloned());
                let merge_ref: Vec<&str> = merge_args.iter().map(|s| s.as_str()).collect();
                run_native(host_bin, &merge_ref).context("native merge stage")?;
            }
            MergeMode::Source(source) => {
                run_source_merge(source, &work_dir, &spec.output, &partials)
                    .context("source merge stage")?;
            }
        }
        merge_ms = merge_start.elapsed().as_millis() as u64;
    }

    let total_ms = job_start.elapsed().as_millis() as u64;
    let mut summary = Vec::new();
    summary.push(format!("split:   {}ms", split_ms));
    summary.push(format!(
        "compute: {}ms ({} shards, slowest {}ms)",
        compute_ms,
        spec.shards,
        shard_ms.iter().copied().max().unwrap_or(0)
    ));
    if success {
        summary.push(format!("merge:   {}ms", merge_ms));
        summary.push(format!("output:  {}", spec.output));
    } else {
        summary.extend(failures.iter().cloned());
    }
    summary.push(format!("total:   {}ms", total_ms));

    // Reclaim the intermediate shard/partial files and SHM regions (the final
    // output is kept). Best-effort: on the distributed path only the coordinator's
    // own shard SHM is local here — worker-side staged files are already reaped by
    // the worker on job completion.
    if !spec.keep_intermediates {
        cleanup_intermediates(spec, &shard_prefix, &partials);
    }
    Ok((success, summary.join("\n")))
}

/// Remove the per-shard input files, partial outputs, and local SHM regions of a
/// finished sharded job. Best-effort — missing files (e.g. a shard's SHM on a
/// remote worker) are ignored.
fn cleanup_intermediates(spec: &ShardedJob, shard_prefix: &str, partials: &[String]) {
    for i in 0..spec.shards {
        let _ = std::fs::remove_file(format!("{}.{}", shard_prefix, i));
        let _ = std::fs::remove_file(format!("{}_s{}", spec.shm_path_prefix, i));
    }
    for p in partials {
        let _ = std::fs::remove_file(p);
    }
}

/// Build the per-shard DAG for shard `i` (input = `{shard_prefix}.{i}`, output =
/// `partials[i]`, unique SHM region `{prefix}_s{i}`).
fn shard_dag(
    spec: &ShardedJob,
    shard_prefix: &str,
    partials: &[String],
    i: usize,
) -> Result<String> {
    let shard_input = format!("{}.{}", shard_prefix, i);
    let shm_path = format!("{}_s{}", spec.shm_path_prefix, i);
    build_shard_dag(&spec.per_shard_dag, &shard_input, &partials[i], &shm_path)
        .with_context(|| format!("build per-shard DAG for shard {}", i))
}

/// Run all shards as local executors on this node, at most `max_concurrent` at a
/// time (a sliding window). This is the memory bound: a file with more shards
/// than fit in RAM is processed in windows instead of all-at-once. Returns
/// per-shard durations and any failure messages.
fn run_shards_local(
    config: &AgentConfig,
    spec: &ShardedJob,
    job_id: &str,
    shard_prefix: &str,
    partials: &[String],
    timeout: Duration,
) -> Result<(Vec<u64>, Vec<String>)> {
    let host_bin = Path::new(&config.paths.executor_bin);
    let window = spec.max_concurrent.unwrap_or(spec.shards).clamp(1, spec.shards);
    if window < spec.shards {
        println!(
            "[coordinator]   running {} shards, up to {} at a time",
            spec.shards, window
        );
    }

    let mut shard_ms = vec![0u64; spec.shards];
    let mut failures = Vec::new();
    let mut next = 0usize; // next shard index to launch
    let mut running: Vec<(usize, ExecutorHandle)> = Vec::new();
    let start = Instant::now();

    loop {
        // Fill the window with pending shards.
        while running.len() < window && next < spec.shards {
            let i = next;
            next += 1;
            let dag_json = shard_dag(spec, shard_prefix, partials, i)?;
            // Distinct job_id per shard so temp DAG files / metrics don't collide.
            let shard_job_id = format!("{}_s{}", job_id, i);
            let handle = ExecutorHandle::spawn(
                host_bin,
                Path::new(&config.paths.executor_work_dir),
                &dag_json,
                &shard_job_id,
                false,
            )
            .with_context(|| format!("spawn executor for shard {}", i))?;
            println!("[coordinator]   shard {} → local executor pid {}", i, handle.pid());
            running.push((i, handle));
        }
        // Nothing running and nothing pending → done.
        if running.is_empty() {
            break;
        }

        std::thread::sleep(Duration::from_millis(50));

        // Reap finished executors, freeing window slots for the next shards.
        let mut still = Vec::with_capacity(running.len());
        for (i, mut handle) in running.drain(..) {
            match handle.try_wait()? {
                Some(result) => {
                    shard_ms[i] = result.duration_ms;
                    if result.success {
                        println!("[coordinator]   shard {} done in {}ms", i, result.duration_ms);
                    } else {
                        let msg = format!(
                            "shard {} FAILED (exit={:?}): {}",
                            i, result.exit_code, result.stderr_tail
                        );
                        eprintln!("[coordinator]   {}", msg);
                        failures.push(msg);
                    }
                }
                None => still.push((i, handle)),
            }
        }
        running = still;

        if start.elapsed() > timeout {
            for (i, mut handle) in running.drain(..) {
                let _ = handle.kill();
                failures.push(format!("shard {} TIMED OUT", i));
            }
            break;
        }
    }
    Ok((shard_ms, failures))
}

/// Distribute the shards across the cluster in waves of one-per-node: the
/// coordinator runs one shard locally and each connected worker runs one (as an
/// independent single-node job — no RDMA mesh), then the next wave picks up the
/// remaining shards. Returns per-shard durations and any failure messages.
fn run_shards_distributed(
    config: &AgentConfig,
    state: &Arc<Mutex<CoordinatorState>>,
    spec: &ShardedJob,
    job_id: &str,
    shard_prefix: &str,
    partials: &[String],
    timeout: Duration,
) -> Result<(Vec<u64>, Vec<String>)> {
    // Live nodes: coordinator + connected workers, ascending. nodes[0] is the
    // coordinator (smallest id), so it takes the first shard of each wave locally.
    let nodes: Vec<u32> = {
        let s = state.lock().unwrap();
        let mut ids: Vec<u32> = s.workers.keys().copied().collect();
        if !ids.contains(&config.node_id) {
            ids.push(config.node_id);
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let n = nodes.len();
    let waves = spec.shards.div_ceil(n);
    println!(
        "[coordinator]   distributing {} shard(s) across {} node(s) {:?} in {} wave(s)",
        spec.shards, n, nodes, waves
    );

    let mut shard_ms = vec![0u64; spec.shards];
    let mut failures: Vec<String> = Vec::new();
    for w in 0..waves {
        let lo = w * n;
        let hi = ((w + 1) * n).min(spec.shards);
        // Assign shard `lo+m` to nodes[m] for this wave.
        let assignments: Vec<(usize, u32)> = (lo..hi).map(|i| (i, nodes[i - lo])).collect();
        run_wave(
            config, state, spec, job_id, shard_prefix, partials, timeout, &assignments,
            &mut shard_ms, &mut failures,
        )?;
        // Fail fast: if a shard failed there's no point running later waves (the
        // merge won't happen anyway).
        if !failures.is_empty() {
            break;
        }
    }
    Ok((shard_ms, failures))
}

/// Run one wave of the distributed path: for each `(shard, node)` assignment,
/// the coordinator runs its own shard locally and each worker is staged-to and
/// assigned its shard; then wait for all of them. Appends per-shard durations and
/// failures to the shared accumulators.
#[allow(clippy::too_many_arguments)]
fn run_wave(
    config: &AgentConfig,
    state: &Arc<Mutex<CoordinatorState>>,
    spec: &ShardedJob,
    job_id: &str,
    shard_prefix: &str,
    partials: &[String],
    timeout: Duration,
    assignments: &[(usize, u32)],
    shard_ms: &mut [u64],
    failures: &mut Vec<String>,
) -> Result<()> {
    let host_bin = Path::new(&config.paths.executor_bin);
    let mut local_handle: Option<(usize, ExecutorHandle)> = None;

    // Phase A: stage + assign remote shards, spawn the coordinator's shard. Held
    // under the state lock — the main accept loop is blocked during a submit, so
    // nothing else reads the worker sockets concurrently.
    {
        let mut s = state.lock().unwrap();
        for &(i, node) in assignments {
            let dag_json = shard_dag(spec, shard_prefix, partials, i)?;
            let shard_job_id = format!("{}_s{}", job_id, i);
            if node == config.node_id {
                let handle = ExecutorHandle::spawn(
                    host_bin,
                    Path::new(&config.paths.executor_work_dir),
                    &dag_json,
                    &shard_job_id,
                    false,
                )
                .with_context(|| format!("spawn local executor for shard {}", i))?;
                println!("[coordinator]   shard {} → local executor pid {}", i, handle.pid());
                local_handle = Some((i, handle));
            } else {
                let shard_input = format!("{}.{}", shard_prefix, i);
                let worker = s
                    .workers
                    .get_mut(&node)
                    .ok_or_else(|| anyhow::anyhow!("assigned node {} not connected", node))?;
                stage_file_to_worker(worker, &shard_input, &shard_job_id)
                    .with_context(|| format!("stage shard {} to node {}", i, node))?;
                send_message(
                    &mut worker.stream,
                    &make_message(
                        MessageKind::AssignJob,
                        &AssignJobPayload { job_id: shard_job_id.clone(), dag_json },
                    )?,
                )
                .with_context(|| format!("assign shard {} to node {}", i, node))?;
                worker.running_job = Some(shard_job_id);
                println!("[coordinator]   shard {} → node {} (staged + assigned)", i, node);
            }
        }
    }

    // Phase B: wait for this wave's local + remote shards.
    let remote_shards: Vec<(usize, u32)> = assignments
        .iter()
        .copied()
        .filter(|(_, n)| *n != config.node_id)
        .collect();
    let mut remote_done: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut local_done = local_handle.is_none();
    let start = Instant::now();
    loop {
        if let Some((i, handle)) = local_handle.as_mut() {
            if !local_done {
                if let Some(result) = handle.try_wait()? {
                    local_done = true;
                    shard_ms[*i] = result.duration_ms;
                    if result.success {
                        println!("[coordinator]   shard {} (local) done in {}ms", i, result.duration_ms);
                    } else {
                        failures.push(format!(
                            "shard {} (local) FAILED (exit={:?}): {}",
                            i, result.exit_code, result.stderr_tail
                        ));
                    }
                }
            }
        }
        {
            let mut s = state.lock().unwrap();
            for (i, node) in &remote_shards {
                if remote_done.contains(i) {
                    continue;
                }
                if let Some(worker) = s.workers.get_mut(node) {
                    worker.stream.set_read_timeout(Some(Duration::from_millis(100))).ok();
                    match recv_message(&mut worker.stream) {
                        Ok(msg) => match msg.kind {
                            MessageKind::JobCompleted => {
                                let p: JobCompletedPayload =
                                    serde_json::from_value(msg.payload)?;
                                // Write the partial file(s) the worker produced back
                                // to disk on the coordinator so the merge can read them.
                                for f in &p.result_files {
                                    if let Some(dir) = Path::new(&f.rel_path).parent() {
                                        std::fs::create_dir_all(dir).ok();
                                    }
                                    std::fs::write(&f.rel_path, &f.data).with_context(|| {
                                        format!("write partial from node {}: {}", node, f.rel_path)
                                    })?;
                                }
                                shard_ms[*i] = p.duration_ms;
                                remote_done.insert(*i);
                                worker.running_job = None;
                                println!(
                                    "[coordinator]   shard {} (node {}) done in {}ms",
                                    i, node, p.duration_ms
                                );
                            }
                            MessageKind::JobFailed => {
                                let p: JobFailedPayload = serde_json::from_value(msg.payload)?;
                                failures.push(format!(
                                    "shard {} (node {}) FAILED (exit={:?}): {}",
                                    i, node, p.exit_code, p.stderr_tail
                                ));
                                remote_done.insert(*i);
                                worker.running_job = None;
                            }
                            // JobStarted / Metrics / etc. — consume and ignore.
                            _ => {}
                        },
                        Err(_) => {} // read timeout — keep polling.
                    }
                }
            }
        }
        if local_done && remote_done.len() == remote_shards.len() {
            break;
        }
        if start.elapsed() > timeout {
            for (i, node) in &remote_shards {
                if !remote_done.contains(i) {
                    failures.push(format!("shard {} (node {}) TIMED OUT", i, node));
                }
            }
            if !local_done {
                if let Some((i, handle)) = local_handle.as_mut() {
                    let _ = handle.kill();
                    failures.push(format!("shard {} (local) TIMED OUT", i));
                }
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// Send one file to a worker via TCP `StageFiles` and wait for its ack. Non-ack
/// messages (e.g. periodic Metrics) are consumed and ignored.
pub(crate) fn stage_file_to_worker(worker: &mut WorkerConn, path: &str, job_id: &str) -> Result<()> {
    let data = std::fs::read(path).with_context(|| format!("read shard file: {}", path))?;
    let payload = StageFilesPayload {
        job_id: job_id.to_string(),
        files: vec![StagedFile { rel_path: path.to_string(), data }],
    };
    send_message(&mut worker.stream, &make_message(MessageKind::StageFiles, &payload)?)?;
    let start = Instant::now();
    loop {
        worker.stream.set_read_timeout(Some(Duration::from_millis(200))).ok();
        match recv_message(&mut worker.stream) {
            Ok(msg) if msg.kind == MessageKind::StageFilesAck => return Ok(()),
            Ok(_) => {}
            Err(_) => {}
        }
        if start.elapsed() > Duration::from_secs(60) {
            bail!("timeout waiting for StageFilesAck from node {}", worker.node_id);
        }
    }
}

/// Build a per-shard Dag JSON from the template by substituting the input/output
/// tokens and forcing a unique per-shard `shm_path`.
fn build_shard_dag(
    template: &serde_json::Value,
    shard_input: &str,
    shard_output: &str,
    shm_path: &str,
) -> Result<String> {
    let templ_str = serde_json::to_string(template).context("serialize per-shard DAG template")?;
    let substituted = templ_str
        .replace("{{SHARD_INPUT}}", shard_input)
        .replace("{{SHARD_OUTPUT}}", shard_output);
    // Apply the same unified-kind transform the normal submit path uses, so the
    // template can be authored with friendly `Func`/`Pipeline`/`Grouping` nodes
    // (the Executor only understands the native `WasmVoid`/… kinds). Rust mode
    // only for now — Python sharded jobs are a later concern.
    let transformed = crate::dag_transform::transform_dag(&substituted, false, None, None)
        .context("transform per-shard DAG")?;
    let mut v: serde_json::Value =
        serde_json::from_str(&transformed).context("re-parse transformed per-shard DAG")?;
    // Force a unique SHM region per shard regardless of what the template said —
    // this is the isolation that lets the N executors coexist.
    v["shm_path"] = serde_json::json!(shm_path);
    serde_json::to_string(&v).context("serialize per-shard DAG")
}

/// How to fold the shard partials into the final output.
enum MergeMode {
    /// A built-in reducer name (possibly with a `:arg`, e.g. `topk:100`).
    Reducer(String),
    /// Path to a workload-supplied native merge source (a single self-contained
    /// Rust file).
    Source(String),
}

/// Compile and run a workload-supplied native merge source.
///
/// The source is a single self-contained Rust file whose `main` receives, via
/// argv: `[1]` = the final output path, `[2..]` = the partial file paths. It is
/// compiled with `rustc -O` and run as an ordinary process — native, so it has
/// no WASM / 4 GiB ceiling, which is the whole reason the merge lives outside the
/// executor. This covers workloads whose combine step isn't a simple associative
/// fold over `key <sep> count` tallies (the built-in reducers).
fn run_source_merge(source: &str, work_dir: &str, output: &str, partials: &[String]) -> Result<()> {
    if !Path::new(source).exists() {
        bail!("merge source not found: {}", source);
    }
    let bin = format!("{}/merge_bin", work_dir);
    let status = std::process::Command::new("rustc")
        .args(["-O", source, "-o", &bin])
        .status()
        .with_context(|| format!("compile merge source: rustc -O {} -o {}", source, bin))?;
    if !status.success() {
        bail!("compiling merge source '{}' failed (exit={:?})", source, status.code());
    }
    let mut args: Vec<String> = vec![output.to_string()];
    args.extend(partials.iter().cloned());
    let status = std::process::Command::new(&bin)
        .args(&args)
        .status()
        .with_context(|| format!("run compiled merge binary: {}", bin))?;
    if !status.success() {
        bail!("merge binary '{}' failed (exit={:?})", bin, status.code());
    }
    Ok(())
}

/// Run a native `host` subcommand to completion, inheriting stdout/stderr.
fn run_native(host_bin: &Path, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(host_bin)
        .args(args)
        .status()
        .with_context(|| format!("spawn `{} {}`", host_bin.display(), args.join(" ")))?;
    if !status.success() {
        bail!("`host {}` failed (exit={:?})", args.join(" "), status.code());
    }
    Ok(())
}

fn simple_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}", ts)
}
