//! Coordinator daemon: accepts workers, distributes jobs, aggregates results.

use crate::config::AgentConfig;
use crate::cluster_dag::ClusterDag;
use crate::executor::ExecutorHandle;
use crate::metrics;
use crate::protocol::*;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// State for a connected worker.
struct WorkerConn {
    node_id: u32,
    stream: TcpStream,
    running_job: Option<String>,
}

/// Shared coordinator state.
struct CoordinatorState {
    workers: HashMap<u32, WorkerConn>,
    current_job_id: Option<String>,
}

/// Run the coordinator daemon.
pub fn run_coordinator(config: &AgentConfig) -> Result<()> {
    let bind_addr = format!("0.0.0.0:{}", config.cluster.agent_port);
    let listener = TcpListener::bind(&bind_addr)
        .with_context(|| format!("bind to {}", bind_addr))?;

    println!(
        "[coordinator] listening on {} (cluster: {} nodes)",
        bind_addr,
        config.total_nodes()
    );

    let state = Arc::new(Mutex::new(CoordinatorState {
        workers: HashMap::new(),
        current_job_id: None,
    }));

    // Accept worker connections in a background thread.
    let accept_state = Arc::clone(&state);
    let expected_workers = config.total_nodes() - 1; // exclude self
    let accept_listener = listener.try_clone()?;
    let _accept_handle = thread::spawn(move || {
        accept_workers(accept_listener, accept_state, expected_workers);
    });

    // Also accept submit/status client connections on the same listener.
    // We use a non-blocking approach: the main thread handles client commands
    // while workers connect in the background.

    // Set a timeout on the listener so we can also check for keyboard input.
    listener.set_nonblocking(true)?;

    println!("[coordinator] waiting for workers...");
    println!("[coordinator] use `node-agent submit` to submit jobs");
    println!("[coordinator] press Ctrl+C to stop");

    // Main event loop: accept client connections for submit/status.
    loop {
        match listener.accept() {
            Ok((mut stream, addr)) => {
                // Could be a worker or a client command.
                // Ensure accepted socket is blocking (listener is non-blocking).
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                match recv_message(&mut stream) {
                    Ok(msg) => {
                        match msg.kind {
                            MessageKind::Ready => {
                                // Worker connecting — hand off to state.
                                let payload: ReadyPayload =
                                    serde_json::from_value(msg.payload)?;
                                println!(
                                    "[coordinator] worker {} connected from {}",
                                    payload.node_id, addr
                                );
                                let mut s = state.lock().unwrap();
                                s.workers.insert(payload.node_id, WorkerConn {
                                    node_id: payload.node_id,
                                    stream,
                                    running_job: None,
                                });
                            }
                            MessageKind::SubmitJob => {
                                handle_submit(&config, &state, msg, &mut stream)?;
                            }
                            MessageKind::StatusQuery => {
                                handle_status(&state, &mut stream)?;
                            }
                            _ => {
                                eprintln!(
                                    "[coordinator] unexpected message from {}: {:?}",
                                    addr, msg.kind
                                );
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[coordinator] failed to read from {}: {}", addr, e);
                    }
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // No pending connections — sleep briefly.
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("[coordinator] accept error: {}", e);
            }
        }
    }
}

/// Accept worker connections (runs in a background thread).
fn accept_workers(
    listener: TcpListener,
    state: Arc<Mutex<CoordinatorState>>,
    _expected: usize,
) {
    // This thread is now superseded by the main loop which handles all accepts.
    // Kept as a placeholder for future dedicated worker handling.
    let _ = (listener, state);
}

/// Handle a SubmitJob request from a client.
fn handle_submit(
    config: &AgentConfig,
    state: &Arc<Mutex<CoordinatorState>>,
    msg: Message,
    client_stream: &mut TcpStream,
) -> Result<()> {
    let payload: SubmitJobPayload =
        serde_json::from_value(msg.payload).context("parse SubmitJob payload")?;

    let cluster_dag = ClusterDag::from_json(&payload.cluster_dag_json)?;
    let per_node_dags = cluster_dag.split(&config.cluster.ips)?;

    let job_id = format!("job_{}", chrono_simple_id());

    println!("[coordinator] submitting job {} ({} nodes)", job_id, per_node_dags.len());

    // Send ACK to client immediately.
    send_message(
        client_stream,
        &make_message(MessageKind::SubmitAck, &serde_json::json!({ "job_id": &job_id }))?,
    )?;

    let mut s = state.lock().unwrap();
    s.current_job_id = Some(job_id.clone());

    // Phase 1: Send AssignJob to all workers first (before launching local executor).
    // This is critical for RDMA timing: all nodes must start roughly together.
    for (node_id, dag_json) in &per_node_dags {
        if *node_id == config.node_id {
            continue; // self — handle below
        }

        if let Some(worker) = s.workers.get_mut(node_id) {
            println!("[coordinator] assigning job to worker {}", node_id);
            if let Err(e) = send_message(
                &mut worker.stream,
                &make_message(
                    MessageKind::AssignJob,
                    &AssignJobPayload {
                        job_id: job_id.clone(),
                        dag_json: dag_json.clone(),
                    },
                )?,
            ) {
                eprintln!("[coordinator] failed to send to worker {}: {}", node_id, e);
            } else {
                worker.running_job = Some(job_id.clone());
            }
        } else {
            eprintln!("[coordinator] worker {} not connected!", node_id);
        }
    }

    drop(s); // Release lock before spawning executor.

    // Phase 2: Launch local executor (for this node's DAG).
    let job_start = Instant::now();
    if let Some(my_dag) = per_node_dags.get(&config.node_id) {
        println!("[coordinator] launching local executor");
        let handle = ExecutorHandle::spawn(
            Path::new(&config.paths.executor_bin),
            Path::new(&config.paths.executor_work_dir),
            my_dag,
            &job_id,
            false, // capture output in multi-node mode
        )?;

        // Wait for local executor to complete.
        let result = handle.wait()?;
        let local_ms = result.duration_ms;
        if result.success {
            println!(
                "[coordinator] local executor completed in {}ms",
                local_ms
            );
        } else {
            eprintln!(
                "[coordinator] local executor failed (exit={:?}): {}",
                result.exit_code, result.stderr_tail
            );
        }

        // Collect results from workers.
        collect_worker_results(config, state, &job_id, client_stream, job_start, local_ms)?;
    }

    Ok(())
}

/// Collect JobCompleted/JobFailed from all workers after job submission.
fn collect_worker_results(
    config: &AgentConfig,
    state: &Arc<Mutex<CoordinatorState>>,
    job_id: &str,
    client_stream: &mut TcpStream,
    job_start: Instant,
    local_ms: u64,
) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(config.timeouts.job_timeout_s);
    let mut completed_workers = Vec::new();
    let mut all_success = true;
    let mut summary_lines = Vec::new();
    let mut worker_durations: Vec<(u32, u64)> = Vec::new();

    let expected_workers: Vec<u32> = {
        let s = state.lock().unwrap();
        s.workers.keys()
            .filter(|id| **id != config.node_id)
            .cloned()
            .collect()
    };

    while completed_workers.len() < expected_workers.len() {
        if start.elapsed() > timeout {
            eprintln!("[coordinator] timeout waiting for workers");
            all_success = false;
            summary_lines.push("TIMEOUT: not all workers completed".to_string());
            break;
        }

        let mut s = state.lock().unwrap();
        for worker_id in &expected_workers {
            if completed_workers.contains(worker_id) {
                continue;
            }
            if let Some(worker) = s.workers.get_mut(worker_id) {
                worker.stream.set_read_timeout(Some(Duration::from_millis(100)))?;
                match recv_message(&mut worker.stream) {
                    Ok(msg) => match msg.kind {
                        MessageKind::JobCompleted => {
                            let p: JobCompletedPayload =
                                serde_json::from_value(msg.payload)?;
                            println!(
                                "[coordinator] worker {} completed job in {}ms",
                                worker_id, p.duration_ms
                            );
                            worker_durations.push((*worker_id, p.duration_ms));
                            summary_lines.push(format!(
                                "worker {}: completed in {}ms",
                                worker_id, p.duration_ms
                            ));
                            completed_workers.push(*worker_id);
                            worker.running_job = None;
                        }
                        MessageKind::JobFailed => {
                            let p: JobFailedPayload =
                                serde_json::from_value(msg.payload)?;
                            eprintln!(
                                "[coordinator] worker {} failed (exit={:?}): {}",
                                worker_id, p.exit_code, p.stderr_tail
                            );
                            summary_lines.push(format!(
                                "worker {}: FAILED (exit={:?})",
                                worker_id, p.exit_code
                            ));
                            all_success = false;
                            completed_workers.push(*worker_id);
                            worker.running_job = None;
                        }
                        MessageKind::JobStarted => {
                            let p: JobStartedPayload =
                                serde_json::from_value(msg.payload)?;
                            println!(
                                "[coordinator] worker {} started (pid={})",
                                worker_id, p.executor_pid
                            );
                        }
                        MessageKind::Metrics => {
                            // Log metrics from worker.
                            if let Ok(m) = serde_json::from_value::<metrics::NodeMetrics>(msg.payload) {
                                let _ = metrics::append_metrics_log(&config.metrics.log_path, &m);
                            }
                        }
                        _ => {}
                    },
                    Err(_) => {
                        // Timeout — continue polling.
                    }
                }
            }
        }
        drop(s);
        thread::sleep(Duration::from_millis(50));
    }

    // Print timing summary.
    let total_wall_ms = job_start.elapsed().as_millis() as u64;
    let sum_all: u64 = local_ms + worker_durations.iter().map(|(_, ms)| ms).sum::<u64>();
    let overlap_ms = sum_all.saturating_sub(total_wall_ms);
    let slowest_worker = worker_durations.iter().map(|(_, ms)| *ms).max().unwrap_or(0);

    println!("[coordinator] ── Job Summary ──────────────────────────");
    println!("[coordinator]   node 0 (local):  {}ms", local_ms);
    for (wid, wms) in &worker_durations {
        println!("[coordinator]   node {} (worker): {}ms", wid, wms);
    }
    println!("[coordinator]   ────────────────────────────────────");
    println!("[coordinator]   overlap:         {}ms", overlap_ms);
    println!("[coordinator]   speedup:         {:.2}x  ({} / {}ms)",
        sum_all as f64 / total_wall_ms as f64, sum_all, total_wall_ms);
    println!("[coordinator]   bottleneck:      {}",
        if local_ms >= slowest_worker { "node 0 (local)" }
        else { "worker (waiting for data)" });
    println!("[coordinator]   total wall time: {}ms", total_wall_ms);
    println!("[coordinator] ─────────────────────────────────────────");

    // Build summary for client.
    let mut client_summary = Vec::new();
    client_summary.push(format!("node 0 (local): {}ms", local_ms));
    for (wid, wms) in &worker_durations {
        client_summary.push(format!("node {} (worker): {}ms", wid, wms));
    }
    client_summary.push(format!("overlap: {}ms, speedup: {:.2}x", overlap_ms, sum_all as f64 / total_wall_ms as f64));
    client_summary.push(format!("total wall time: {}ms", total_wall_ms));

    // Send final result to the client.
    let summary = client_summary.join("\n");
    let _ = send_message(
        client_stream,
        &make_message(
            MessageKind::JobResult,
            &JobResultPayload {
                job_id: job_id.to_string(),
                success: all_success,
                summary,
            },
        )?,
    );

    let mut s = state.lock().unwrap();
    s.current_job_id = None;

    Ok(())
}

/// Handle a StatusQuery request.
fn handle_status(
    state: &Arc<Mutex<CoordinatorState>>,
    client_stream: &mut TcpStream,
) -> Result<()> {
    let s = state.lock().unwrap();
    let workers: Vec<WorkerStatus> = s.workers.values().map(|w| WorkerStatus {
        node_id: w.node_id,
        connected: true,
        running_job: w.running_job.clone(),
    }).collect();

    send_message(
        client_stream,
        &make_message(
            MessageKind::StatusResponse,
            &StatusResponsePayload {
                workers,
                current_job: s.current_job_id.clone(),
            },
        )?,
    )?;
    Ok(())
}

/// Generate a simple timestamp-based ID (no external deps).
fn chrono_simple_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}", ts)
}
