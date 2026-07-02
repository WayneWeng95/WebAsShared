//! Placed independent jobs (Goal 2, Phase 1).
//!
//! The coordinator acts as a placer: a whole single-node job is assigned to one
//! node, its input is shipped there, it runs, and its output is shipped back.
//! Concurrent placed jobs land on DIFFERENT nodes, so each job exclusively owns
//! its node's socket for its duration — no two jobs ever share a socket, which is
//! what lets them run truly concurrently without any reader-thread demux.
//!
//! Submission: a JSON object with a top-level `dag` key (a normal single-node
//! Executor DAG). Detected in `handle_submit` and dispatched here on its own
//! thread. `distribute`-style multi-node partitioned jobs are unaffected.

use crate::config::AgentConfig;
use crate::coordinator::CoordinatorState;
use crate::executor::ExecutorHandle;
use crate::protocol::*;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::net::TcpStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize)]
pub struct PlacedJob {
    /// A single-node Executor DAG (its own `shm_path`, `nodes`, Input/Output).
    pub dag: serde_json::Value,
    /// Input files to ship to the target node before running. Omit if the node
    /// already has the data (e.g. placement on the coordinator itself).
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Optional explicit placement; otherwise the coordinator picks a free node.
    #[serde(default)]
    pub target_node: Option<u32>,
}

/// True if the JSON is a PlacedJob (top-level `dag` key). Distinct from a sharded
/// job (`per_shard_dag`), ClusterDag (`node_dags`), or SymbolicDag (`nodes`).
pub fn is_placed_job(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .map(|v| v.get("dag").is_some() && v.get("per_shard_dag").is_none())
        .unwrap_or(false)
}

/// True if a SubmitJob message carries a PlacedJob.
pub fn is_submit_placed(msg: &Message) -> bool {
    msg.payload
        .get("cluster_dag_json")
        .and_then(|v| v.as_str())
        .map(is_placed_job)
        .unwrap_or(false)
}

/// Handle a placed-job submission (runs on its own thread). ACKs immediately,
/// then reports the result. Never propagates an error to the caller.
pub fn handle_placed_submit(
    config: &AgentConfig,
    state: &Arc<Mutex<CoordinatorState>>,
    msg: Message,
    client_stream: &mut TcpStream,
) -> Result<()> {
    let payload: SubmitJobPayload =
        serde_json::from_value(msg.payload).context("parse SubmitJob payload")?;
    let spec: PlacedJob =
        serde_json::from_str(&payload.cluster_dag_json).context("parse PlacedJob spec")?;
    let job_id = format!("placed_{}", simple_id());

    send_message(
        client_stream,
        &make_message(MessageKind::SubmitAck, &serde_json::json!({ "job_id": &job_id }))?,
    )?;

    let (success, summary) = match run_placed_job(config, state, &spec, &job_id) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("[coordinator] placed job {} failed: {:#}", job_id, e);
            (false, format!("placed job failed: {:#}", e))
        }
    };

    let _ = send_message(
        client_stream,
        &make_message(
            MessageKind::JobResult,
            &JobResultPayload { job_id, success, summary },
        )?,
    );
    Ok(())
}

/// Reserve a node, dispatch the job there, release the node, and summarise.
fn run_placed_job(
    config: &AgentConfig,
    state: &Arc<Mutex<CoordinatorState>>,
    spec: &PlacedJob,
    job_id: &str,
) -> Result<(bool, String)> {
    let start = Instant::now();

    // Live nodes = coordinator + connected workers (ascending).
    let live: Vec<u32> = {
        let s = state.lock().unwrap();
        let mut v: Vec<u32> = s.workers.keys().copied().collect();
        if !v.contains(&config.node_id) {
            v.push(config.node_id);
        }
        v.sort_unstable();
        v
    };

    // Reserve a node (the placement decision). Load-aware: pick the least-loaded
    // free node. If every node is busy, QUEUE — wait for one to free up rather
    // than rejecting — up to the job timeout.
    let queue_timeout = Duration::from_secs(config.timeouts.job_timeout_s);
    let wait_start = Instant::now();
    let mut announced_wait = false;
    let node = loop {
        {
            let mut s = state.lock().unwrap();
            let order = s.placement_order(&live, config.node_id);
            if let Some(n) = s.reserve_node(spec.target_node, &order) {
                break n;
            }
        }
        if wait_start.elapsed() > queue_timeout {
            return Ok((
                false,
                format!("queued but no node freed within {:?}", queue_timeout),
            ));
        }
        if !announced_wait {
            println!(
                "[coordinator] placed job {} queued — all {} node(s) busy, waiting…",
                job_id, live.len()
            );
            announced_wait = true;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    println!("[coordinator] placed job {} → node {}", job_id, node);

    // Dispatch; always release the node afterward.
    let outcome = dispatch_to_node(config, state, spec, job_id, node);
    state.lock().unwrap().release_node(node);
    let success = outcome?;

    let summary = format!(
        "node: {}\nstatus: {}\ntotal: {}ms",
        node,
        if success { "ok" } else { "failed" },
        start.elapsed().as_millis()
    );
    Ok((success, summary))
}

/// Run the job on `node`: locally if it's the coordinator, otherwise stage the
/// inputs to the worker, assign the DAG, and collect the output back.
fn dispatch_to_node(
    config: &AgentConfig,
    state: &Arc<Mutex<CoordinatorState>>,
    spec: &PlacedJob,
    job_id: &str,
    node: u32,
) -> Result<bool> {
    let host_bin = Path::new(&config.paths.executor_bin);
    // Apply the unified-kind transform so the DAG may use friendly `Func` nodes
    // (a no-op if it's already in native `WasmVoid` form). Rust mode for now.
    let dag_json = crate::dag_transform::transform_dag(
        &serde_json::to_string(&spec.dag).context("serialize placed DAG")?,
        false,
        None,
        None,
    )
    .context("transform placed DAG")?;
    let timeout = Duration::from_secs(config.timeouts.job_timeout_s);

    if node == config.node_id {
        // Local placement: inputs are already on this node's filesystem.
        let mut handle = ExecutorHandle::spawn(
            host_bin,
            Path::new(&config.paths.executor_work_dir),
            &dag_json,
            job_id,
            false,
        )
        .context("spawn local placed executor")?;
        let start = Instant::now();
        loop {
            if let Some(r) = handle.try_wait()? {
                if !r.success {
                    eprintln!(
                        "[coordinator] placed job {} (local) failed (exit={:?}): {}",
                        job_id, r.exit_code, r.stderr_tail
                    );
                }
                return Ok(r.success);
            }
            if start.elapsed() > timeout {
                let _ = handle.kill();
                bail!("placed job {} (local) timed out", job_id);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // Remote placement. Because the node is reserved, this thread exclusively owns
    // its socket: clone a read handle to receive completion without holding the
    // state lock across blocking reads (writes still go through the locked stream).
    let read_stream: TcpStream = {
        let mut s = state.lock().unwrap();
        let worker = s
            .workers
            .get_mut(&node)
            .ok_or_else(|| anyhow::anyhow!("reserved node {} not connected", node))?;
        for inp in &spec.inputs {
            crate::sharded::stage_file_to_worker(worker, inp, job_id)
                .with_context(|| format!("stage input {} to node {}", inp, node))?;
        }
        send_message(
            &mut worker.stream,
            &make_message(
                MessageKind::AssignJob,
                &AssignJobPayload { job_id: job_id.to_string(), dag_json },
            )?,
        )
        .with_context(|| format!("assign placed job to node {}", node))?;
        worker.running_job = Some(job_id.to_string());
        worker.stream.try_clone().context("clone worker read stream")?
    };
    println!("[coordinator] placed job {} staged + assigned to node {}", job_id, node);

    // Wait for JobCompleted / JobFailed on the owned read handle.
    let mut read_stream = read_stream;
    read_stream.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let start = Instant::now();
    let success = loop {
        match recv_message(&mut read_stream) {
            Ok(msg) => match msg.kind {
                MessageKind::JobCompleted => {
                    let p: JobCompletedPayload = serde_json::from_value(msg.payload)?;
                    for f in &p.result_files {
                        if let Some(dir) = Path::new(&f.rel_path).parent() {
                            std::fs::create_dir_all(dir).ok();
                        }
                        std::fs::write(&f.rel_path, &f.data)
                            .with_context(|| format!("write output from node {}: {}", node, f.rel_path))?;
                    }
                    println!(
                        "[coordinator] placed job {} done on node {} in {}ms",
                        job_id, node, p.duration_ms
                    );
                    break true;
                }
                MessageKind::JobFailed => {
                    let p: JobFailedPayload = serde_json::from_value(msg.payload)?;
                    eprintln!(
                        "[coordinator] placed job {} failed on node {} (exit={:?}): {}",
                        job_id, node, p.exit_code, p.stderr_tail
                    );
                    break false;
                }
                // JobStarted / Metrics / etc. — ignore.
                _ => {}
            },
            Err(_) => {
                if start.elapsed() > timeout {
                    bail!("placed job {} on node {} timed out", job_id, node);
                }
            }
        }
    };

    // Clear the worker's running flag.
    if let Some(worker) = state.lock().unwrap().workers.get_mut(&node) {
        worker.running_job = None;
    }
    Ok(success)
}

fn simple_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}", ts)
}
