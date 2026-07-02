//! Coordinator daemon: accepts workers, distributes jobs, aggregates results.

use crate::config::AgentConfig;
use crate::cluster_dag::ClusterDag;
use crate::executor::ExecutorHandle;
use crate::file_staging;
use crate::metrics;
use crate::protocol::*;
use anyhow::{bail, Context, Result};
use connect::rdma::context::RdmaContext;
use connect::rdma::exchange::QpInfo;
use connect::rdma::memory_region::MemoryRegion;
use connect::rdma::queue_pair::QueuePair;
use scheduler::ScxClusterView;
use partitioner::{PlacementHints, SymbolicDag};
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// State for a connected worker.
pub(crate) struct WorkerConn {
    pub(crate) node_id: u32,
    pub(crate) stream: TcpStream,
    pub(crate) running_job: Option<String>,
    last_heartbeat: Instant,
}

/// Shared coordinator state.
pub(crate) struct CoordinatorState {
    pub(crate) workers: HashMap<u32, WorkerConn>,
    current_job_id: Option<String>,
    /// Aggregated SCX stats from all nodes.
    scx_view: ScxClusterView,
    /// Nodes currently reserved by a concurrent placed job (Goal 2). A reserved
    /// node's socket is owned exclusively by that job's thread; the main-loop
    /// metrics drain skips it so it can't steal the job's messages. Includes the
    /// coordinator's own id when it runs a placed job locally.
    pub(crate) reserved_nodes: HashSet<u32>,
}

impl CoordinatorState {
    /// Load-aware placement order for the `live` nodes: least-loaded first, using
    /// the SCX scheduler score (same signal the partitioner uses). Nodes without a
    /// score yet (no metrics) are appended in id order so they're still usable.
    pub(crate) fn placement_order(&self, live: &[u32]) -> Vec<u32> {
        let mut order: Vec<u32> = scheduler::score_nodes(&self.scx_view)
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| live.contains(id))
            .collect();
        for &n in live {
            if !order.contains(&n) {
                order.push(n);
            }
        }
        order
    }

    /// Reserve a free node for a placed job. Prefers `prefer` (if given and free);
    /// otherwise the first free node in `order` (which the caller sorts least-
    /// loaded first). Returns the reserved node id, or `None` if all are busy.
    pub(crate) fn reserve_node(&mut self, prefer: Option<u32>, order: &[u32]) -> Option<u32> {
        let pick = prefer
            .filter(|n| order.contains(n) && !self.reserved_nodes.contains(n))
            .or_else(|| order.iter().copied().find(|n| !self.reserved_nodes.contains(n)))?;
        self.reserved_nodes.insert(pick);
        Some(pick)
    }

    pub(crate) fn release_node(&mut self, node: u32) {
        self.reserved_nodes.remove(&node);
    }
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
        scx_view: ScxClusterView::new(),
        reserved_nodes: HashSet::new(),
    }));

    // Cloneable handle to config so per-job threads (concurrent placed jobs) can
    // own their own copy without borrowing the loop's reference.
    let config_arc = Arc::new(config.clone());

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

    let status_interval = Duration::from_secs(config.metrics.status_print_interval_s);
    let mut last_status_print = Instant::now();
    let mut self_metrics = metrics::MetricsCollector::new(config.node_id);

    // Main event loop: accept client connections for submit/status.
    loop {
        // Drain any pending metrics from worker streams (doubles as heartbeat).
        {
            let mut s = state.lock().unwrap();
            let reserved = s.reserved_nodes.clone();
            let mut pending: Vec<metrics::NodeMetrics> = Vec::new();
            for worker in s.workers.values_mut() {
                // Skip nodes a concurrent placed job owns — its thread reads that
                // socket, and draining here would steal the job's JobCompleted.
                if reserved.contains(&worker.node_id) {
                    continue;
                }
                worker.stream.set_read_timeout(Some(Duration::from_millis(1))).ok();
                while let Ok(msg) = recv_message(&mut worker.stream) {
                    worker.last_heartbeat = Instant::now();
                    if msg.kind == MessageKind::Metrics {
                        if let Ok(m) = serde_json::from_value::<metrics::NodeMetrics>(msg.payload) {
                            pending.push(m);
                        }
                    }
                }
            }
            for m in pending {
                s.scx_view.update(
                    m.node_id,
                    m.scx.clone(),
                    m.cpu_usage_pct,
                    m.rss_bytes,
                    m.executor_running,
                    m.current_job_id.clone(),
                    m.timestamp_ms,
                    m.cpu_cores,
                );
            }
        }

        // Periodic cluster status printout.
        if last_status_print.elapsed() >= status_interval {
            let s = state.lock().unwrap();
            let self_sample = self_metrics.sample(None, false, s.current_job_id.as_deref(), None, None);
            let heartbeat_timeout = Duration::from_secs(config.timeouts.health_check_s * 3);
            print_cluster_status(&s, &self_sample, expected_workers, heartbeat_timeout);
            drop(s);
            last_status_print = Instant::now();
        }

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
                                    last_heartbeat: Instant::now(),
                                });
                            }
                            MessageKind::SubmitJob if crate::placed::is_submit_placed(&msg) => {
                                // Placed independent job (Goal 2): run it on its own
                                // thread so the accept loop keeps serving and multiple
                                // placed jobs execute concurrently on different nodes.
                                let cfg = Arc::clone(&config_arc);
                                let st = Arc::clone(&state);
                                thread::spawn(move || {
                                    let mut stream = stream;
                                    if let Err(e) =
                                        crate::placed::handle_placed_submit(&cfg, &st, msg, &mut stream)
                                    {
                                        eprintln!("[coordinator] placed submit error: {:#}", e);
                                    }
                                });
                            }
                            MessageKind::SubmitJob => {
                                // Partitioned / sharded jobs stay on the accept thread
                                // (serialized). Never let a bad/failed submit tear down
                                // the loop — log and keep serving.
                                if let Err(e) = handle_submit(&config, &state, msg, &mut stream) {
                                    eprintln!("[coordinator] submit handling error: {:#}", e);
                                }
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

    // Divide-and-merge (sharded) jobs run entirely on this node: split → N local
    // executors → merge. They share the SubmitJob message/ACK/JobResult wire, but
    // none of the cluster partitioning / RDMA mesh path below.
    if crate::sharded::is_sharded_job(&payload.cluster_dag_json) {
        return crate::sharded::handle_sharded_submit(
            config,
            state,
            &payload.cluster_dag_json,
            client_stream,
        );
    }

    // Snapshot capacity + live membership before acquiring any further locks.
    // `live_ids` is the set of physical nodes participating in this job: the
    // coordinator (node 0) plus every connected worker — an ARBITRARY subset, not
    // a contiguous prefix. The partitioner works in a dense logical space
    // 0..live_ids.len(); we map logical id `i` → physical node `live_ids[i]` when
    // building the RDMA mesh and delivering per-node DAGs.
    let (placement_hints, live_ids): (PlacementHints, Vec<u32>) = {
        let s = state.lock().unwrap();
        let connected: std::collections::HashSet<u32> = s.workers.keys().copied().collect();
        let live_ids = live_node_ids(&connected, config.node_id);
        let hints = PlacementHints {
            capacity: scheduler::cluster_capacity(&s.scx_view),
            host_limit: scheduler::host_limits(&s.scx_view),
            cores: scheduler::host_cores(&s.scx_view),
            random: false,
        };
        (hints, live_ids)
    };

    // Resolve the submitted DAG against the live cluster: a raw SymbolicDag is
    // partitioned here (where live-node scaling / SCX placement / converge run),
    // a pre-partitioned ClusterDag passes through. Then apply the mode transform
    // (Func→WasmVoid for Rust, →PyFunc for Python) — this must happen after
    // partitioning, since the partitioner always emits the native WasmVoid kind.
    let (resolved_json, partitioned) =
        resolve_dag(&payload.cluster_dag_json, &placement_hints, &live_ids)?;
    if payload.python_mode {
        println!("[coordinator] Python mode — transforming partitioned DAG for PyFunc execution");
    }
    let transformed_json = crate::dag_transform::transform_cluster_dag(
        &resolved_json,
        payload.python_mode,
        payload.python_script.as_deref(),
        payload.python_wasm.as_deref(),
    )
    .context("transform partitioned ClusterDag for execution mode")?;
    let mut cluster_dag = ClusterDag::from_json(&transformed_json)?;

    // When we partitioned a SymbolicDag, its node ids are a dense LOGICAL space
    // (0..N). Map them onto the actual live PHYSICAL nodes: logical `i` →
    // `live_ids[i]`. A pre-partitioned ClusterDag already carries explicit
    // physical ids (dense 0.. matching the cluster IP roster) and is left as-is.
    let logical_to_physical: Option<&[u32]> = if partitioned {
        // Remap shared-input source ids (also logical) to physical.
        for si in &mut cluster_dag.shared_inputs {
            if let Some(&phys) = live_ids.get(si.source_node as usize) {
                si.source_node = phys;
            }
        }
        Some(live_ids.as_slice())
    } else {
        None
    };

    let job_id = format!("job_{}", chrono_simple_id());

    // Which input files each PHYSICAL worker actually loads — so we stage a file
    // only to the nodes that read it, instead of broadcasting every slice to every
    // node. Map the (logical) node ids from `node_dags` onto physical ids the same
    // way the mesh does (`logical i → live_ids[i]`, identity for a pre-partitioned DAG).
    let needed_by_physical: std::collections::HashMap<u32, std::collections::HashSet<String>> = {
        let mut m: std::collections::HashMap<u32, std::collections::HashSet<String>> =
            std::collections::HashMap::new();
        for (logical, paths) in cluster_dag.input_paths_by_node() {
            let physical = match logical_to_physical {
                Some(map) => map.get(logical as usize).copied().unwrap_or(logical),
                None => logical,
            };
            m.entry(physical).or_default().extend(paths);
        }
        m
    };

    // Stage shared input files to workers before splitting and assigning jobs.
    // Try RDMA first; fall back to TCP on error (e.g. no RDMA hardware).
    if !cluster_dag.shared_inputs.is_empty() {
        let mut s = state.lock().unwrap();
        let rdma_result = stage_shared_inputs_rdma(
            &mut s.workers,
            config.node_id,
            &cluster_dag.shared_inputs,
            &needed_by_physical,
            &job_id,
        );
        drop(s);
        if let Err(e) = rdma_result {
            println!("[coordinator] RDMA input staging unavailable ({:#}), falling back to TCP", e);
            let mut s = state.lock().unwrap();
            stage_shared_inputs_tcp(
                &mut s.workers,
                config.node_id,
                &cluster_dag.shared_inputs,
                &needed_by_physical,
                &job_id,
            )?;
            drop(s);
        }
    }

    // Build the IP list the split sees, indexed by LOGICAL node id. For the
    // partitioned path that's the live nodes' IPs in `live_ids` order (so the
    // RDMA mesh entry `i` points at physical node `live_ids[i]`); otherwise the
    // raw cluster roster.
    let split_ips: Vec<String> = match logical_to_physical {
        Some(map) => map
            .iter()
            .map(|&phys| {
                config.cluster.ips.get(phys as usize).cloned().ok_or_else(|| {
                    anyhow::anyhow!("live node {} has no configured IP in cluster roster", phys)
                })
            })
            .collect::<Result<_>>()?,
        None => config.cluster.ips.clone(),
    };

    let per_node_dags_logical = cluster_dag.split(&split_ips)?;

    // Rekey logical → physical so AssignJob delivery, the local-executor lookup,
    // and result collection all address the real node ids. (Identity map when
    // the DAG was already physical.)
    let per_node_dags: HashMap<u32, String> = match logical_to_physical {
        Some(map) => per_node_dags_logical
            .into_iter()
            .map(|(logical, dag)| {
                let phys = *map.get(logical as usize).unwrap_or(&logical);
                (phys, dag)
            })
            .collect(),
        None => per_node_dags_logical,
    };

    // Workers that actually receive a per-node DAG this run (excluding self).
    // Result collection must wait for EXACTLY these — not every connected node —
    // or a job partitioned across fewer nodes than are online (declared
    // total_nodes < live nodes) would block forever on nodes that got no job.
    let assigned_workers: std::collections::HashSet<u32> = per_node_dags
        .keys()
        .copied()
        .filter(|id| *id != config.node_id)
        .collect();

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
        let mut handle = ExecutorHandle::spawn(
            Path::new(&config.paths.executor_bin),
            Path::new(&config.paths.executor_work_dir),
            my_dag,
            &job_id,
            false, // capture output in multi-node mode
        )?;

        // Poll until the local executor completes, sampling metrics each tick so
        // RSS/SHM reflect THIS node's executor during the run (a blocking wait()
        // would freeze the coordinator's own metrics for the whole job).
        let exec_pid = handle.pid();
        let shm_path = crate::worker::extract_shm_path(my_dag);
        let mut collector = metrics::MetricsCollector::new(config.node_id);
        let interval = Duration::from_millis(config.metrics.interval_ms);
        let mut last_sample = Instant::now() - interval; // sample immediately
        let result = loop {
            if let Some(r) = handle.try_wait()? {
                break r;
            }
            if last_sample.elapsed() >= interval {
                let m = collector.sample(
                    shm_path.as_deref(), true, Some(&job_id),
                    Some(handle.elapsed_ms()), Some(exec_pid),
                );
                let _ = metrics::append_metrics_log(&config.metrics.log_path, &m);
                {
                    let mut s = state.lock().unwrap();
                    s.scx_view.update(config.node_id, m.scx.clone(), m.cpu_usage_pct,
                        m.rss_bytes, true, Some(job_id.clone()), m.timestamp_ms, m.cpu_cores);
                }
                last_sample = Instant::now();
            }
            thread::sleep(Duration::from_millis(50));
        };
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
        collect_worker_results(config, state, &job_id, client_stream, job_start, local_ms, &assigned_workers)?;
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
    assigned_workers: &std::collections::HashSet<u32>,
) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(config.timeouts.job_timeout_s);
    let mut completed_workers = Vec::new();
    let mut all_success = true;
    let mut summary_lines = Vec::new();
    let mut worker_durations: Vec<(u32, u64)> = Vec::new();

    // Wait for exactly the workers that received a per-node DAG this run (and are
    // still connected) — NOT every connected node — so a job partitioned across
    // fewer nodes than are online doesn't block on idle nodes.
    let expected_workers: Vec<u32> = {
        let s = state.lock().unwrap();
        s.workers.keys()
            .filter(|id| **id != config.node_id && assigned_workers.contains(id))
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
                    Ok(msg) => {
                        worker.last_heartbeat = Instant::now();
                        match msg.kind {
                        MessageKind::JobCompleted => {
                            let p: JobCompletedPayload =
                                serde_json::from_value(msg.payload)?;
                            println!(
                                "[coordinator] worker {} completed job in {}ms",
                                worker_id, p.duration_ms
                            );
                            for f in &p.result_files {
                                if let Some(dir) = std::path::Path::new(&f.rel_path).parent() {
                                    std::fs::create_dir_all(dir).ok();
                                }
                                match std::fs::write(&f.rel_path, &f.data) {
                                    Ok(_) => println!("[coordinator] result: {}", f.rel_path),
                                    Err(e) => eprintln!("[coordinator] failed to write result '{}': {}", f.rel_path, e),
                                }
                            }
                            worker_durations.push((*worker_id, p.duration_ms));
                            summary_lines.push(format!(
                                "worker {}: completed in {}ms",
                                worker_id, p.duration_ms
                            ));
                            completed_workers.push(*worker_id);
                            worker.running_job = None;
                            // Clear stale snapshot so scheduler sees idle state.
                            if let Some(snap) = s.scx_view.snapshots.get_mut(worker_id) {
                                snap.executor_running = false;
                                snap.current_job_id = None;
                            }
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
                            // Clear stale snapshot so scheduler sees idle state.
                            if let Some(snap) = s.scx_view.snapshots.get_mut(worker_id) {
                                snap.executor_running = false;
                                snap.current_job_id = None;
                            }
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
                            // Log metrics and update cluster view.
                            if let Ok(m) = serde_json::from_value::<metrics::NodeMetrics>(msg.payload) {
                                s.scx_view.update(
                                    m.node_id,
                                    m.scx.clone(),
                                    m.cpu_usage_pct,
                                    m.rss_bytes,
                                    m.executor_running,
                                    m.current_job_id.clone(),
                                    m.timestamp_ms,
                                    m.cpu_cores,
                                );
                                let _ = metrics::append_metrics_log(&config.metrics.log_path, &m);
                            }
                        }
                        _ => {}
                    }},
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

    println!("[coordinator] ── Job Summary ──────────────────────────");
    println!("[coordinator]   node 0 (local):  {}ms", local_ms);
    for (wid, wms) in &worker_durations {
        println!("[coordinator]   node {} (worker): {}ms", wid, wms);
    }
    println!("[coordinator]   total wall time: {}ms", total_wall_ms);
    println!("[coordinator] ─────────────────────────────────────────");

    // Build summary for client.
    let mut client_summary = Vec::new();
    client_summary.push(format!("node 0 (local): {}ms", local_ms));
    for (wid, wms) in &worker_durations {
        client_summary.push(format!("node {} (worker): {}ms", wid, wms));
    }
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

    let scx_cluster = if s.scx_view.node_count() > 0 {
        Some(s.scx_view.clone())
    } else {
        None
    };

    send_message(
        client_stream,
        &make_message(
            MessageKind::StatusResponse,
            &StatusResponsePayload {
                workers,
                current_job: s.current_job_id.clone(),
                scx_cluster,
            },
        )?,
    )?;
    Ok(())
}

/// Print cluster node states to the console.
fn print_cluster_status(state: &CoordinatorState, self_metrics: &metrics::NodeMetrics, expected_workers: usize, heartbeat_timeout: Duration) {
    // Coordinator's own status line (same format as workers).
    let self_rss_mb = self_metrics.rss_bytes as f64 / (1024.0 * 1024.0);
    let self_job_str = self_metrics.current_job_id.as_deref().unwrap_or("idle");
    let mut self_line = format!(
        "[coordinator] cpu={:.1}%, rss={:.0} MiB, job={}, workers={}/{}",
        self_metrics.cpu_usage_pct,
        self_rss_mb,
        self_job_str,
        state.workers.len(),
        expected_workers,
    );
    if let Some(ref scx) = self_metrics.scx {
        self_line.push_str(&format!(
            ", scx(cpu_busy={:.1}%, load={:.1}, migrations={})",
            scx.cpu_busy, scx.load, scx.nr_migrations,
        ));
    }
    println!("{}", self_line);

    // Each worker in the same key=value style.
    for w in state.workers.values() {
        let stale = w.last_heartbeat.elapsed() > heartbeat_timeout;
        let job_str = w.running_job.as_deref().unwrap_or("idle");
        let mut line = format!("[worker {}] job={}", w.node_id, job_str);

        if let Some(status) = state.scx_view.snapshots.get(&w.node_id) {
            let rss_mb = status.rss_bytes as f64 / (1024.0 * 1024.0);
            line.push_str(&format!(", cpu={:.1}%, rss={:.0} MiB", status.cpu_usage_pct, rss_mb));
            if let Some(ref scx) = status.scx {
                line.push_str(&format!(
                    ", scx(cpu_busy={:.1}%, load={:.1}, migrations={})",
                    scx.cpu_busy, scx.load, scx.nr_migrations,
                ));
            }
        }
        if stale {
            line.push_str(&format!(", STALE (no heartbeat for {:.0}s)", w.last_heartbeat.elapsed().as_secs_f64()));
        }
        println!("{}", line);
    }

    if state.scx_view.node_count() > 0 {
        let scores = scheduler::score_nodes(&state.scx_view);
        if !scores.is_empty() {
            let best: Vec<u32> = scores.iter().map(|(id, _)| *id).collect();
            println!("[coordinator] placement order: {:?} (best first)", best);
        }
    }
}

/// Accept either a pre-partitioned ClusterDag or a raw SymbolicDag and return
/// the resolved ClusterDag-shaped JSON (still in unified-kind form — the caller
/// applies the execution-mode transform) plus whether it was partitioned here.
///
/// Detection is by key presence:
/// - `node_dags` present → already a ClusterDag, use as-is (returns `false`).
/// - `nodes` present     → SymbolicDag; partition it server-side using live
///                         capacity hints so the placer can colocate sandboxes
///                         on the least-loaded hosts (returns `true`).
///
/// `live_ids` are the physical node ids participating in this job, ascending.
/// The partitioner emits a dense logical space `0..effective`; the SCX hints
/// (keyed by physical id) are remapped into that logical space here, and the
/// caller maps logical ids back to `live_ids[i]` for delivery.
fn resolve_dag(
    dag_json: &str,
    capacity: &PlacementHints,
    live_ids: &[u32],
) -> Result<(String, bool)> {
    let probe: serde_json::Value =
        serde_json::from_str(dag_json).context("parse submitted DAG JSON")?;

    if probe.get("node_dags").is_some() {
        // Already partitioned — its node layout is fixed; we can't rescale it.
        return Ok((dag_json.to_string(), false));
    }

    if probe.get("nodes").is_some() {
        let live = live_ids.len();
        let mut symbolic = SymbolicDag::from_json(dag_json)?;
        // Resolve how many nodes to partition across against the live cluster:
        //   - total_nodes omitted     → use ALL available nodes.
        //   - total_nodes <= live     → use the requested count.
        //   - total_nodes >  live     → clamp to what's available and warn.
        let effective = match symbolic.total_nodes {
            None => {
                println!("[coordinator] total_nodes not set — using all {} available node(s) {:?}", live, live_ids);
                live.max(1)
            }
            Some(d) if d > live => {
                println!("[coordinator] requested total_nodes={} but only {} node(s) online {:?} — using {}",
                         d, live, live_ids, live);
                live.max(1)
            }
            Some(d) => {
                println!("[coordinator] partitioning across {} node(s) ({} online {:?})", d, live, live_ids);
                d.max(1)
            }
        };
        symbolic.total_nodes = Some(effective);
        // Remap the SCX-derived hints (keyed by PHYSICAL node id) into the dense
        // LOGICAL space 0..effective the placer operates in: logical id `i`
        // corresponds to physical node `live_ids[i]`. Nodes beyond `effective`
        // (when total_nodes < live) are dropped, so the placer never picks a host
        // outside the partition.
        let clamped = PlacementHints {
            capacity: live_ids.iter().take(effective).enumerate()
                .filter_map(|(logical, phys)| capacity.capacity.get(phys).map(|&c| (logical as u32, c)))
                .collect(),
            host_limit: live_ids.iter().take(effective).enumerate()
                .filter_map(|(logical, phys)| capacity.host_limit.get(phys).map(|&l| (logical as u32, l)))
                .collect(),
            cores: live_ids.iter().take(effective).enumerate()
                .filter_map(|(logical, phys)| capacity.cores.get(phys).map(|&c| (logical as u32, c)))
                .collect(),
            random: capacity.random,
        };
        // Keep the hints if we have EITHER capacity weights (placement) or core
        // counts (fanout cap) — core data is present even when SCX scheduling is not.
        let hints = if clamped.capacity.is_empty() && clamped.cores.is_empty() {
            None
        } else {
            Some(&clamped)
        };
        let cluster_value = partitioner::partition(&symbolic, hints)
            .context("partition SymbolicDag")?;
        let cluster_json = serde_json::to_string(&cluster_value)?;
        return Ok((cluster_json, true));
    }

    bail!("submitted JSON is neither a ClusterDag (has node_dags) nor a SymbolicDag (has nodes)")
}

/// The physical node ids participating in a job, ascending: the coordinator
/// (`coordinator_id`, always present) plus every currently-connected worker.
///
/// Workers may be an ARBITRARY subset — e.g. node 1 offline while node 2 is up
/// yields `[0, 2]`. The partitioner still emits a dense logical space
/// `0..len`; the coordinator maps logical id `i` to physical node `result[i]`
/// when building the RDMA mesh and delivering per-node DAGs, so a gap in the
/// physical ids no longer wastes the nodes above it.
///
/// (The coordinator is assumed to be node 0, the smallest id — so `result[0]`
/// is always the coordinator, which runs logical node 0's DAG locally.)
fn live_node_ids(connected: &std::collections::HashSet<u32>, coordinator_id: u32) -> Vec<u32> {
    let mut ids: Vec<u32> = connected.iter().copied().collect();
    if !ids.contains(&coordinator_id) {
        ids.push(coordinator_id);
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

// ── RDMA port / GID constants (same as the Executor mesh) ────────────────────
const RDMA_PORT: u8 = 2;
const GID_IDX:   u8 = 2;

/// Generate a random-ish packet sequence number (same logic as mesh::rand_psn).
fn rand_psn() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0x4321)
        & 0x00FF_FFFF
}

/// Stage shared input files to remote workers via RDMA READ.
///
/// Coordinator registers all input files in one flat MR, creates one RC QP per
/// worker, exchanges QP metadata over the existing TCP control channel, then
/// signals workers to RDMA-READ the data.  Returns `Err` if RDMA is
/// unavailable (caller falls back to TCP StageFiles).
fn stage_shared_inputs_rdma(
    workers: &mut HashMap<u32, WorkerConn>,
    own_node_id: u32,
    shared_inputs: &[crate::cluster_dag::SharedInput],
    needed: &HashMap<u32, std::collections::HashSet<String>>,
    job_id: &str,
) -> Result<()> {
    let all_remote: Vec<u32> = workers
        .keys()
        .filter(|&&id| id != own_node_id)
        .cloned()
        .collect();
    if all_remote.is_empty() {
        return Ok(());
    }

    // Read all files that are owned by this node into a flat source buffer.
    struct FileEntry { path: String, offset: usize, len: usize }
    let mut entries: Vec<FileEntry> = Vec::new();
    let mut flat: Vec<u8> = Vec::new();

    for input in shared_inputs {
        if input.source_node != own_node_id {
            continue;
        }
        let data = std::fs::read(&input.path)
            .with_context(|| format!("read shared input: {}", input.path))?;
        let offset = flat.len();
        let len = data.len();
        flat.extend_from_slice(&data);
        entries.push(FileEntry { path: input.path.clone(), offset, len });
    }
    if entries.is_empty() || flat.is_empty() {
        return Ok(());
    }
    if flat.len() > u32::MAX as usize {
        bail!("shared input total size {} exceeds 4 GiB RDMA READ limit", flat.len());
    }

    // Per-worker plan: only the files THIS worker loads, laid out compactly in its
    // own receive buffer (local_off), each RDMA-WRITten from the file's position in
    // the source buffer (global_off). Workers that load nothing get no offer at all.
    struct WFile { path: String, global_off: usize, local_off: usize, len: usize }
    let empty = std::collections::HashSet::new();
    let mut per_worker: HashMap<u32, Vec<WFile>> = HashMap::new();
    for &id in &all_remote {
        let want = needed.get(&id).unwrap_or(&empty);
        let mut local = 0usize;
        let mut v = Vec::new();
        for e in &entries {
            if want.contains(&e.path) {
                v.push(WFile { path: e.path.clone(), global_off: e.offset, local_off: local, len: e.len });
                local += e.len;
            }
        }
        if !v.is_empty() {
            per_worker.insert(id, v);
        }
    }
    let remote_ids: Vec<u32> = per_worker.keys().copied().collect();
    if remote_ids.is_empty() {
        println!("[coordinator] no worker needs a shared input — skipping staging");
        return Ok(());
    }
    let skipped = all_remote.len() - remote_ids.len();
    if skipped > 0 {
        println!("[coordinator] staging: {} worker(s) load no shared input — not sending to them", skipped);
    }

    // Pin the buffer on the heap (Box<[u8]> address is stable).
    let mut buf: Box<[u8]> = flat.into_boxed_slice();

    // RDMA context + MR registration (errors here trigger TCP fallback).
    let ctx = RdmaContext::new(None, 64)?;
    let mr = MemoryRegion::register_external(&ctx, buf.as_mut_ptr(), buf.len())?;

    let gid = ctx.query_gid(RDMA_PORT, GID_IDX as i32)?;
    let port_attr = ctx.query_port(RDMA_PORT)?;

    // Create one QP per worker and transition to INIT.
    let mut qps: HashMap<u32, (QueuePair, u32)> = HashMap::new();
    for &id in &remote_ids {
        let qp = QueuePair::create(&ctx)?;
        let psn = rand_psn();
        qp.to_init(RDMA_PORT)?;
        qps.insert(id, (qp, psn));
    }

    // Send a per-worker InputShareOffer: only that worker's files, with offsets
    // into ITS OWN compact receive buffer (total_len = just those files' bytes).
    for &id in &remote_ids {
        let (qp, psn) = &qps[&id];
        let wf = &per_worker[&id];
        let file_entries: Vec<InputFileEntry> = wf
            .iter()
            .map(|f| InputFileEntry {
                path: f.path.clone(),
                offset: f.local_off as u64,
                len: f.len as u64,
            })
            .collect();
        let worker_total: u64 = wf.iter().map(|f| f.len as u64).sum();
        let offer = InputShareOfferPayload {
            job_id: job_id.to_string(),
            qpn: qp.qpn(),
            psn: *psn,
            gid: gid.raw.to_vec(),
            lid: port_attr.lid,
            rkey: mr.rkey(),
            addr: mr.addr(),
            total_len: worker_total,
            files: file_entries,
        };
        if let Some(w) = workers.get_mut(&id) {
            send_message(&mut w.stream, &make_message(MessageKind::InputShareOffer, &offer)?)
                .with_context(|| format!("send InputShareOffer to worker {}", id))?;
        }
    }

    println!("[coordinator] waiting for RDMA InputShareAccept from {} workers", remote_ids.len());

    // Collect InputShareAccept from all workers.
    let timeout = Duration::from_secs(30);
    let start = Instant::now();
    let mut accepts: HashMap<u32, InputShareAcceptPayload> = HashMap::new();

    while accepts.len() < remote_ids.len() {
        if start.elapsed() > timeout {
            bail!("timeout waiting for InputShareAccept from {:?}",
                  remote_ids.iter().filter(|id| !accepts.contains_key(id)).collect::<Vec<_>>());
        }
        for &id in &remote_ids {
            if accepts.contains_key(&id) { continue; }
            if let Some(w) = workers.get_mut(&id) {
                w.stream.set_read_timeout(Some(Duration::from_millis(100))).ok();
                match recv_message(&mut w.stream) {
                    Ok(msg) if msg.kind == MessageKind::InputShareAccept => {
                        let accept: InputShareAcceptPayload =
                            serde_json::from_value(msg.payload).context("parse InputShareAccept")?;
                        println!("[coordinator] worker {} accepted RDMA share", id);
                        accepts.insert(id, accept);
                    }
                    Ok(_) | Err(_) => {}
                }
            }
        }
    }

    // Transition each coordinator QP to RTR → RTS.
    for &id in &remote_ids {
        let accept = &accepts[&id];
        let mut peer_gid = [0u8; 16];
        peer_gid.copy_from_slice(&accept.gid);
        let peer_info = QpInfo { qpn: accept.qpn, psn: accept.psn, gid: peer_gid, lid: accept.lid, rkey: 0, addr: 0, len: 0 };
        let (qp, psn) = qps.get_mut(&id).unwrap();
        qp.to_rtr(&peer_info, RDMA_PORT, GID_IDX)?;
        qp.to_rts(*psn)?;
    }

    // A single RDMA WRITE work request is capped at the IB max message size
    // (2 GiB — ibv_sge.length is u32 and the spec limits one message to 2^31).
    // Posting one >2 GiB WRITE for a large shared input (e.g. a 3–4 GB corpus)
    // is silently dropped by the HCA (completion still reports SUCCESS), so the
    // workers' buffers stay empty and the reduce sees only node 0's local slice.
    // Split the transfer into ≤1 GiB chunks: post chunk c to every worker, poll
    // all completions, then advance the offset (RC QPs preserve concurrency
    // across workers; staging is pre-job and untimed, so the serialization per
    // chunk is free).
    // Each worker gets only ITS files: RDMA-WRITE each file from its position in the
    // source buffer (global_off) to the file's slot in that worker's compact receive
    // buffer (local_off), chunked to ≤1 GiB per WR (the 2 GiB WR cap).
    const RDMA_WRITE_CHUNK: usize = 1 << 30; // 1 GiB, well under the 2 GiB WR cap
    let mut total_sent = 0usize;
    for &id in &remote_ids {
        let accept = &accepts[&id];
        let (qp, _) = &qps[&id];
        for f in &per_worker[&id] {
            let mut o = 0usize;
            while o < f.len {
                let len = std::cmp::min(RDMA_WRITE_CHUNK, f.len - o);
                qp.post_rdma_write_raw(
                    mr.addr() + (f.global_off + o) as u64, mr.lkey(), len as u32,
                    accept.addr + (f.local_off + o) as u64, accept.rkey,
                )?;
                qp.poll_one_blocking()
                    .with_context(|| format!("RDMA WRITE '{}' to worker {} failed", f.path, id))?;
                o += len;
            }
            total_sent += f.len;
        }
        let nf = per_worker[&id].len();
        let nb: usize = per_worker[&id].iter().map(|f| f.len).sum();
        println!("[coordinator] RDMA WRITE to worker {} complete ({} file(s), {} bytes)", id, nf, nb);
    }
    println!("[coordinator] staged {} bytes total (vs {} if broadcast to all)",
             total_sent, buf.len() * remote_ids.len());

    // Signal all workers that their buffers are ready.
    for &id in &remote_ids {
        if let Some(w) = workers.get_mut(&id) {
            send_message(
                &mut w.stream,
                &make_message(MessageKind::InputShareGo, &serde_json::json!({ "job_id": job_id }))?,
            ).with_context(|| format!("send InputShareGo to worker {}", id))?;
        }
    }

    println!("[coordinator] waiting for RDMA InputShareDone from {} workers", remote_ids.len());

    // Wait for InputShareDone from all workers.
    let start = Instant::now();
    let mut done: HashSet<u32> = HashSet::new();

    while done.len() < remote_ids.len() {
        if start.elapsed() > timeout {
            bail!("timeout waiting for InputShareDone from {:?}",
                  remote_ids.iter().filter(|id| !done.contains(id)).collect::<Vec<_>>());
        }
        for &id in &remote_ids {
            if done.contains(&id) { continue; }
            if let Some(w) = workers.get_mut(&id) {
                w.stream.set_read_timeout(Some(Duration::from_millis(100))).ok();
                match recv_message(&mut w.stream) {
                    Ok(msg) if msg.kind == MessageKind::InputShareDone => {
                        println!("[coordinator] worker {} RDMA input share complete", id);
                        done.insert(id);
                    }
                    Ok(_) | Err(_) => {}
                }
            }
        }
    }

    println!("[coordinator] RDMA input staging complete ({} source files, per-worker subsets)",
             entries.len());

    // buf, mr, qps all dropped here (MR deregistered after all READs are done).
    Ok(())
}

/// Send staged files to all workers (except self) and wait for StageFilesAck.
/// TCP fallback for shared-input staging that, like the RDMA path, sends each
/// worker ONLY the files its node-DAG loads (per `needed`). Workers that load no
/// shared input get no StageFiles message at all.
fn stage_shared_inputs_tcp(
    workers: &mut HashMap<u32, WorkerConn>,
    own_node_id: u32,
    shared_inputs: &[crate::cluster_dag::SharedInput],
    needed: &HashMap<u32, std::collections::HashSet<String>>,
    job_id: &str,
) -> Result<()> {
    // Read each coordinator-owned file once.
    let mut data_by_path: HashMap<String, Vec<u8>> = HashMap::new();
    for input in shared_inputs {
        if input.source_node != own_node_id { continue; }
        let data = std::fs::read(&input.path)
            .with_context(|| format!("read shared input: {}", input.path))?;
        data_by_path.insert(input.path.clone(), data);
    }
    if data_by_path.is_empty() { return Ok(()); }

    let remote_ids: Vec<u32> = workers.keys().filter(|&&id| id != own_node_id).cloned().collect();
    let empty = std::collections::HashSet::new();
    let mut pending: std::collections::HashSet<u32> = std::collections::HashSet::new();

    for &id in &remote_ids {
        let want = needed.get(&id).unwrap_or(&empty);
        let files: Vec<StagedFile> = data_by_path
            .iter()
            .filter(|(pth, _)| want.contains(*pth))
            .map(|(pth, d)| StagedFile { rel_path: pth.clone(), data: d.clone() })
            .collect();
        if files.is_empty() { continue; }
        let payload = StageFilesPayload { job_id: job_id.to_string(), files };
        let msg = make_message(MessageKind::StageFiles, &payload)?;
        if let Some(w) = workers.get_mut(&id) {
            send_message(&mut w.stream, &msg)
                .with_context(|| format!("send StageFiles to worker {}", id))?;
            pending.insert(id);
        }
    }
    if pending.is_empty() { return Ok(()); }

    let timeout = Duration::from_secs(30);
    let start = Instant::now();
    let wait_ids: Vec<u32> = pending.iter().copied().collect();
    while !pending.is_empty() {
        if start.elapsed() > timeout {
            bail!("timeout waiting for StageFilesAck from {:?}", pending);
        }
        for &id in &wait_ids {
            if !pending.contains(&id) { continue; }
            if let Some(w) = workers.get_mut(&id) {
                w.stream.set_read_timeout(Some(Duration::from_millis(100))).ok();
                match recv_message(&mut w.stream) {
                    Ok(msg) if msg.kind == MessageKind::StageFilesAck => {
                        println!("[coordinator] worker {} staged files (TCP)", id);
                        pending.remove(&id);
                    }
                    Ok(_) | Err(_) => {}
                }
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn stage_shared_inputs(
    workers: &mut HashMap<u32, WorkerConn>,
    own_node_id: u32,
    payload: &StageFilesPayload,
) -> Result<()> {
    let remote_ids: Vec<u32> = workers
        .keys()
        .filter(|&&id| id != own_node_id)
        .cloned()
        .collect();

    if remote_ids.is_empty() {
        return Ok(());
    }

    let msg = make_message(MessageKind::StageFiles, payload)?;
    for &id in &remote_ids {
        if let Some(w) = workers.get_mut(&id) {
            send_message(&mut w.stream, &msg)
                .with_context(|| format!("send StageFiles to worker {}", id))?;
        }
    }

    // Wait for StageFilesAck from every remote worker.
    let timeout = Duration::from_secs(30);
    let start = Instant::now();
    let mut pending: std::collections::HashSet<u32> = remote_ids.iter().cloned().collect();

    while !pending.is_empty() {
        if start.elapsed() > timeout {
            bail!("timeout waiting for StageFilesAck from {:?}", pending);
        }
        for &id in &remote_ids {
            if !pending.contains(&id) {
                continue;
            }
            if let Some(w) = workers.get_mut(&id) {
                w.stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .ok();
                match recv_message(&mut w.stream) {
                    Ok(msg) if msg.kind == MessageKind::StageFilesAck => {
                        println!("[coordinator] worker {} staged files", id);
                        pending.remove(&id);
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
        }
    }

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

#[cfg(test)]
mod tests {
    use super::live_node_ids;
    use std::collections::HashSet;

    fn ids(slice: &[u32]) -> HashSet<u32> {
        slice.iter().copied().collect()
    }

    #[test]
    fn coordinator_always_present_even_with_no_workers() {
        assert_eq!(live_node_ids(&ids(&[]), 0), vec![0]);
    }

    #[test]
    fn workers_need_not_be_contiguous() {
        // node 1 offline, node 2 up → node 2 is still used (logical 1 → phys 2).
        assert_eq!(live_node_ids(&ids(&[2]), 0), vec![0, 2]);
        assert_eq!(live_node_ids(&ids(&[3, 5]), 0), vec![0, 3, 5]);
    }

    #[test]
    fn sorted_deduped_and_coordinator_not_double_counted() {
        // connected set already containing the coordinator id must not duplicate.
        assert_eq!(live_node_ids(&ids(&[0, 2, 1]), 0), vec![0, 1, 2]);
    }
}
