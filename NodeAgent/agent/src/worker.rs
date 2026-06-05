//! Worker daemon: connects to coordinator, receives jobs, launches executor.

use crate::config::AgentConfig;
use node_agent_common as common;
use crate::executor::ExecutorHandle;
use crate::metrics::{self, MetricsCollector};
use crate::protocol::*;
use anyhow::{Context, Result};
use connect::rdma::context::RdmaContext;
use connect::rdma::exchange::QpInfo;
use connect::rdma::memory_region::MemoryRegion;
use connect::rdma::queue_pair::QueuePair;
use std::net::TcpStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// Run the worker daemon.  Connects to the coordinator and processes jobs.
pub fn run_worker(config: &AgentConfig) -> Result<()> {
    let coord_ip = config.coordinator_ip();
    let coord_addr = format!("{}:{}", coord_ip, config.cluster.agent_port);

    println!(
        "[worker {}] connecting to coordinator at {}",
        config.node_id, coord_addr
    );

    let mut stream = connect_with_retry(
        &coord_addr,
        common::WORKER_CONNECT_RETRIES,
        Duration::from_secs(common::WORKER_CONNECT_RETRY_INTERVAL_S),
    ).with_context(|| format!("connect to coordinator at {}", coord_addr))?;

    println!("[worker {}] connected", config.node_id);

    // Send Ready message.
    send_message(
        &mut stream,
        &make_message(MessageKind::Ready, &ReadyPayload { node_id: config.node_id })?,
    )?;

    let mut collector = if config.scx.enabled {
        MetricsCollector::with_scx(config.node_id, Some(&config.scx.socket_path))
    } else {
        MetricsCollector::new(config.node_id)
    };
    let mut current_executor: Option<ExecutorHandle> = None;
    let mut current_shm_path: Option<String> = None;
    let mut current_dag_json: Option<String> = None;
    let mut current_staged_files: Vec<String> = Vec::new();

    let status_interval = Duration::from_secs(config.metrics.status_print_interval_s);
    let mut last_status_print = Instant::now();

    // Main loop: receive messages from coordinator.
    // Use non-blocking reads with a poll interval so we can also monitor the executor.
    stream.set_read_timeout(Some(Duration::from_millis(common::WORKER_POLL_TIMEOUT_MS)))?;

    loop {
        // Try to receive a message (with timeout).
        match recv_message(&mut stream) {
            Ok(msg) => {
                match msg.kind {
                    MessageKind::AssignJob => {
                        let payload: AssignJobPayload =
                            serde_json::from_value(msg.payload)
                                .context("parse AssignJob payload")?;

                        println!(
                            "[worker {}] received job: {}",
                            config.node_id, payload.job_id
                        );

                        // Extract shm_path from the DAG JSON for metrics.
                        current_shm_path = extract_shm_path(&payload.dag_json);
                        current_dag_json = Some(payload.dag_json.clone());

                        // Launch executor.
                        let handle = ExecutorHandle::spawn(
                            Path::new(&config.paths.executor_bin),
                            Path::new(&config.paths.executor_work_dir),
                            &payload.dag_json,
                            &payload.job_id,
                            false, // capture output, don't print live
                        )?;

                        let pid = handle.pid();
                        println!(
                            "[worker {}] executor spawned (pid={})",
                            config.node_id, pid
                        );

                        // Notify coordinator.
                        send_message(
                            &mut stream,
                            &make_message(
                                MessageKind::JobStarted,
                                &JobStartedPayload {
                                    job_id: payload.job_id.clone(),
                                    executor_pid: pid,
                                },
                            )?,
                        )?;

                        current_executor = Some(handle);
                    }

                    MessageKind::AbortJob => {
                        if let Some(ref mut exec) = current_executor {
                            println!(
                                "[worker {}] aborting job {}",
                                config.node_id, exec.job_id
                            );
                            let _ = exec.kill();
                            current_executor = None;
                            current_shm_path = None;
                            current_dag_json = None;
                            remove_staged_files(&mut current_staged_files, config.node_id);
                        }
                    }

                    MessageKind::Ping => {
                        send_message(&mut stream, &make_signal(MessageKind::Pong))?;
                    }

                    MessageKind::StageFiles => {
                        let p: StageFilesPayload =
                            serde_json::from_value(msg.payload)
                                .context("parse StageFiles payload")?;
                        // Time staging separately so it can be excluded from
                        // compute when comparing per-node timings (Point 3).
                        let stage_start = Instant::now();
                        let mut staged_bytes: usize = 0;
                        for f in &p.files {
                            if let Some(dir) = std::path::Path::new(&f.rel_path).parent() {
                                std::fs::create_dir_all(dir).ok();
                            }
                            std::fs::write(&f.rel_path, &f.data)
                                .with_context(|| format!("write staged file: {}", f.rel_path))?;
                            staged_bytes += f.data.len();
                            current_staged_files.push(f.rel_path.clone());
                        }
                        println!(
                            "[worker {}] staged {} file(s), {:.1} MiB in {:.1} ms (excluded from compute)",
                            config.node_id, p.files.len(),
                            staged_bytes as f64 / (1024.0 * 1024.0),
                            stage_start.elapsed().as_secs_f64() * 1e3,
                        );
                        send_message(
                            &mut stream,
                            &make_message(
                                MessageKind::StageFilesAck,
                                &serde_json::json!({ "job_id": p.job_id }),
                            )?,
                        )?;
                    }

                    MessageKind::InputShareOffer => {
                        let offer: InputShareOfferPayload =
                            serde_json::from_value(msg.payload)
                                .context("parse InputShareOffer payload")?;
                        let paths = recv_rdma_input_share(&mut stream, &offer, config.node_id)
                            .context("RDMA input share")?;
                        current_staged_files.extend(paths);
                    }

                    _ => {
                        eprintln!(
                            "[worker {}] unexpected message: {:?}",
                            config.node_id, msg.kind
                        );
                    }
                }
            }
            Err(e) => {
                // Timeout is expected — check if the executor has finished.
                let err_str = format!("{:#}", e);
                if !err_str.contains("timed out")
                    && !err_str.contains("WouldBlock")
                    && !err_str.contains("Resource temporarily unavailable")
                {
                    // Real error — coordinator disconnected?
                    eprintln!(
                        "[worker {}] connection error: {:#}",
                        config.node_id, e
                    );
                    break;
                }
            }
        }

        // Poll the executor if running.
        if let Some(ref mut exec) = current_executor {
            match exec.try_wait()? {
                Some(result) => {
                    let job_id = exec.job_id.clone();
                    let dag_json = current_dag_json.take().unwrap_or_default();
                    if result.success {
                        println!(
                            "[worker {}] job {} completed in {}ms",
                            config.node_id, job_id, result.duration_ms
                        );
                        let result_files = collect_result_files(&dag_json, config.node_id);
                        send_message(
                            &mut stream,
                            &make_message(
                                MessageKind::JobCompleted,
                                &JobCompletedPayload {
                                    job_id,
                                    duration_ms: result.duration_ms,
                                    stdout_tail: result.stdout_tail,
                                    result_files,
                                },
                            )?,
                        )?;
                    } else {
                        eprintln!(
                            "[worker {}] job {} failed (exit={:?}):\n{}",
                            config.node_id, job_id, result.exit_code, result.stderr_tail
                        );
                        send_message(
                            &mut stream,
                            &make_message(
                                MessageKind::JobFailed,
                                &JobFailedPayload {
                                    job_id,
                                    exit_code: result.exit_code,
                                    stderr_tail: result.stderr_tail,
                                },
                            )?,
                        )?;
                    }
                    current_executor = None;
                    current_shm_path = None;
                    // current_dag_json already consumed by take() above
                    remove_staged_files(&mut current_staged_files, config.node_id);

                    // Send idle metrics so the coordinator's snapshot is up-to-date.
                    let idle = collector.sample(None, false, None, None);
                    let _ = send_message(
                        &mut stream,
                        &make_message(MessageKind::Metrics, &idle)?,
                    );
                }
                None => {
                    // Still running — send periodic metrics.
                    // (Throttled by the poll timeout above.)
                    let m = collector.sample(
                        current_shm_path.as_deref(),
                        true,
                        Some(&exec.job_id),
                        Some(exec.elapsed_ms()),
                    );
                    let _ = metrics::append_metrics_log(&config.metrics.log_path, &m);

                    // Print status to console periodically.
                    if last_status_print.elapsed() >= status_interval {
                        print_worker_status(config.node_id, &m);
                        last_status_print = Instant::now();
                    }

                    // Send metrics to coordinator at a lower rate.
                    static mut LAST_METRICS: Option<Instant> = None;
                    let should_send = unsafe {
                        match LAST_METRICS {
                            Some(t) => t.elapsed().as_millis() as u64 >= config.metrics.interval_ms,
                            None => true,
                        }
                    };
                    if should_send {
                        let _ = send_message(
                            &mut stream,
                            &make_message(MessageKind::Metrics, &m)?,
                        );
                        unsafe { LAST_METRICS = Some(Instant::now()); }
                    }
                }
            }
        } else {
            // Idle — still send periodic metrics so the coordinator has fresh data.
            if last_status_print.elapsed() >= status_interval {
                let m = collector.sample(None, false, None, None);
                print_worker_status(config.node_id, &m);
                let _ = send_message(
                    &mut stream,
                    &make_message(MessageKind::Metrics, &m)?,
                );
                last_status_print = Instant::now();
            }
        }
    }

    Ok(())
}

/// Connect to a TCP address with retries.
fn connect_with_retry(addr: &str, max_retries: u32, interval: Duration) -> Result<TcpStream> {
    for attempt in 1..=max_retries {
        match TcpStream::connect(addr) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                if attempt < max_retries {
                    eprintln!(
                        "  connect attempt {}/{} failed: {} — retrying in {:?}",
                        attempt, max_retries, e, interval
                    );
                    thread::sleep(interval);
                } else {
                    return Err(e.into());
                }
            }
        }
    }
    unreachable!()
}

/// Print worker node status to the console.
fn print_worker_status(node_id: u32, m: &metrics::NodeMetrics) {
    let rss_mb = m.rss_bytes as f64 / (1024.0 * 1024.0);
    let job_str = m.current_job_id.as_deref().unwrap_or("idle");
    let elapsed_str = m.job_elapsed_ms
        .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
        .unwrap_or_else(|| "-".into());

    let mut line = format!(
        "[worker {}] cpu={:.1}%, rss={:.0} MiB, job={}, elapsed={}",
        node_id, m.cpu_usage_pct, rss_mb, job_str, elapsed_str,
    );

    if let Some(ref scx) = m.scx {
        line.push_str(&format!(
            ", scx(cpu_busy={:.1}%, load={:.1}, migrations={})",
            scx.cpu_busy, scx.load, scx.nr_migrations,
        ));
    }

    println!("{}", line);
}

// ── RDMA constants (mirror the Executor mesh values) ─────────────────────────
const RDMA_PORT: u8 = 2;
const GID_IDX:   u8 = 2;

fn rand_psn() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0x4321)
        & 0x00FF_FFFF
}

/// Handle an RDMA input share offer from the coordinator.
///
/// 1. Creates a local RDMA context, QP, and receive-buffer MR.
/// 2. Sends `InputShareAccept` with the local QP info.
/// 3. Waits for `InputShareGo`.
/// 4. Posts an RDMA READ to pull the file data from coordinator's MR.
/// 5. Writes each file to its destination path on disk.
/// 6. Sends `InputShareDone`.
fn remove_staged_files(paths: &mut Vec<String>, node_id: u32) {
    for path in paths.drain(..) {
        if let Err(e) = std::fs::remove_file(&path) {
            eprintln!("[worker {}] failed to remove staged file '{}': {}", node_id, path, e);
        } else {
            println!("[worker {}] removed staged file '{}'", node_id, path);
        }
    }
}

fn recv_rdma_input_share(
    stream: &mut TcpStream,
    offer: &InputShareOfferPayload,
    node_id: u32,
) -> anyhow::Result<Vec<String>> {
    let total = offer.total_len as usize;
    if total > u32::MAX as usize {
        anyhow::bail!("InputShareOffer total_len {} exceeds RDMA READ limit", total);
    }

    let ctx = RdmaContext::new(None, 64)?;
    let gid = ctx.query_gid(RDMA_PORT, GID_IDX as i32)?;
    let port_attr = ctx.query_port(RDMA_PORT)?;

    // Allocate the receive buffer and register it.
    let recv_mr = MemoryRegion::alloc_and_register(&ctx, total)?;

    // Create QP and connect to coordinator's QP.
    let qp = QueuePair::create(&ctx)?;
    let psn = rand_psn();
    qp.to_init(RDMA_PORT)?;

    let mut coord_gid = [0u8; 16];
    coord_gid.copy_from_slice(&offer.gid);
    let coord_info = QpInfo {
        qpn: offer.qpn, psn: offer.psn, gid: coord_gid, lid: offer.lid,
        rkey: 0, addr: 0, len: 0,
    };
    qp.to_rtr(&coord_info, RDMA_PORT, GID_IDX)?;
    qp.to_rts(psn)?;

    // Send InputShareAccept with our QP info + receive-buffer MR info so the
    // coordinator can RDMA-WRITE the file data directly into our buffer.
    let accept = InputShareAcceptPayload {
        job_id: offer.job_id.clone(),
        worker_id: node_id,
        qpn: qp.qpn(),
        psn,
        gid: gid.raw.to_vec(),
        lid: port_attr.lid,
        rkey: recv_mr.rkey(),
        addr: recv_mr.addr(),
    };
    send_message(stream, &make_message(MessageKind::InputShareAccept, &accept)?)?;

    // Wait for InputShareGo — coordinator has completed the RDMA WRITE,
    // so our receive buffer now contains the file data.
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    loop {
        match recv_message(stream) {
            Ok(msg) if msg.kind == MessageKind::InputShareGo => break,
            Ok(_) => {}
            Err(e) => return Err(e.context("waiting for InputShareGo")),
        }
    }

    // Write individual files to disk and collect the paths for later cleanup.
    let buf = recv_mr.as_bytes();
    let mut written_paths: Vec<String> = Vec::new();
    for entry in &offer.files {
        let start = entry.offset as usize;
        let end   = start + entry.len as usize;
        if let Some(dir) = Path::new(&entry.path).parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(&entry.path, &buf[start..end])
            .with_context(|| format!("write RDMA-received file: {}", entry.path))?;
        println!("[worker {}] received via RDMA: '{}'", node_id, entry.path);
        written_paths.push(entry.path.clone());
    }

    // Restore poll timeout and notify coordinator.
    stream.set_read_timeout(Some(Duration::from_millis(common::WORKER_POLL_TIMEOUT_MS)))?;
    send_message(
        stream,
        &make_message(MessageKind::InputShareDone, &serde_json::json!({ "job_id": offer.job_id }))?,
    )?;

    println!("[worker {}] RDMA input share done ({} bytes)", node_id, total);
    Ok(written_paths)
}

/// Extract shm_path from a DAG JSON string (best-effort).
fn extract_shm_path(dag_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(dag_json).ok()?;
    v.get("shm_path")?.as_str().map(|s| s.to_string())
}

/// Parse Output node paths from the per-node DAG JSON.
fn extract_output_paths(dag_json: &str) -> Vec<String> {
    let v: serde_json::Value = match serde_json::from_str(dag_json) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let nodes = match v.get("nodes").and_then(|n| n.as_array()) {
        Some(n) => n,
        None => return vec![],
    };
    let mut paths = Vec::new();
    for node in nodes {
        let output = match node.get("kind").and_then(|k| k.get("Output")) {
            Some(o) => o,
            None => continue,
        };
        if let Some(p) = output.get("path").and_then(|v| v.as_str()) {
            paths.push(p.to_string());
        }
        if let Some(arr) = output.get("paths").and_then(|v| v.as_array()) {
            for p in arr {
                if let Some(s) = p.as_str() {
                    paths.push(s.to_string());
                }
            }
        }
    }
    paths
}

/// Read the output files written by the executor and return them as StagedFiles.
fn collect_result_files(dag_json: &str, node_id: u32) -> Vec<crate::protocol::StagedFile> {
    let paths = extract_output_paths(dag_json);
    let mut files = Vec::new();
    for path in paths {
        match std::fs::read(&path) {
            Ok(data) => {
                println!("[worker {}] returning result '{}'", node_id, path);
                files.push(crate::protocol::StagedFile { rel_path: path, data });
            }
            Err(e) => {
                eprintln!("[worker {}] could not read output '{}': {}", node_id, path, e);
            }
        }
    }
    files
}
