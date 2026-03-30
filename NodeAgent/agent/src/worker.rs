//! Worker daemon: connects to coordinator, receives jobs, launches executor.

use crate::config::AgentConfig;
use crate::executor::ExecutorHandle;
use crate::metrics::{self, MetricsCollector};
use crate::protocol::*;
use anyhow::{Context, Result};
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

    let mut stream = connect_with_retry(&coord_addr, 10, Duration::from_secs(2))
        .with_context(|| format!("connect to coordinator at {}", coord_addr))?;

    println!("[worker {}] connected", config.node_id);

    // Send Ready message.
    send_message(
        &mut stream,
        &make_message(MessageKind::Ready, &ReadyPayload { node_id: config.node_id })?,
    )?;

    let mut collector = MetricsCollector::new(config.node_id);
    let mut current_executor: Option<ExecutorHandle> = None;
    let mut current_shm_path: Option<String> = None;

    // Main loop: receive messages from coordinator.
    // Use non-blocking reads with a poll interval so we can also monitor the executor.
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;

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

                        // Launch executor.
                        let handle = ExecutorHandle::spawn(
                            Path::new(&config.paths.executor_bin),
                            Path::new(&config.paths.executor_work_dir),
                            &payload.dag_json,
                            &payload.job_id,
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
                        }
                    }

                    MessageKind::Ping => {
                        send_message(&mut stream, &make_signal(MessageKind::Pong))?;
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
                let err_str = format!("{}", e);
                if !err_str.contains("timed out")
                    && !err_str.contains("WouldBlock")
                    && !err_str.contains("Resource temporarily unavailable")
                {
                    // Real error — coordinator disconnected?
                    eprintln!(
                        "[worker {}] connection error: {}",
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
                    if result.success {
                        println!(
                            "[worker {}] job {} completed in {}ms",
                            config.node_id, job_id, result.duration_ms
                        );
                        send_message(
                            &mut stream,
                            &make_message(
                                MessageKind::JobCompleted,
                                &JobCompletedPayload {
                                    job_id,
                                    duration_ms: result.duration_ms,
                                    stdout_tail: result.stdout_tail,
                                },
                            )?,
                        )?;
                    } else {
                        eprintln!(
                            "[worker {}] job {} failed (exit={:?})",
                            config.node_id, job_id, result.exit_code
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
                }
                None => {
                    // Still running — send periodic metrics.
                    // (Throttled by the 500ms read timeout above.)
                    let m = collector.sample(
                        current_shm_path.as_deref(),
                        true,
                        Some(&exec.job_id),
                        Some(exec.elapsed_ms()),
                    );
                    let _ = metrics::append_metrics_log(&config.metrics.log_path, &m);

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

/// Extract shm_path from a DAG JSON string (best-effort).
fn extract_shm_path(dag_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(dag_json).ok()?;
    v.get("shm_path")?.as_str().map(|s| s.to_string())
}
