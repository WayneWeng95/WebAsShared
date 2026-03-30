mod cluster_dag;
mod config;
mod coordinator;
mod executor;
mod metrics;
mod protocol;
mod worker;

use anyhow::{bail, Context, Result};
use config::{AgentConfig, Role};
use metrics::MetricsCollector;
use std::path::Path;
use std::time::Instant;

const DEFAULT_EXECUTOR_BIN: &str = "Executor/target/release/host";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        return Ok(());
    }

    match args[1].as_str() {
        "run" => cmd_run(&args[2..]),
        "start" => cmd_start(&args[2..]),
        "submit" => cmd_submit(&args[2..]),
        "status" => cmd_status(&args[2..]),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        other => {
            eprintln!("unknown command: {}", other);
            print_usage();
            bail!("unknown command: {}", other);
        }
    }
}

// ── Single-node run ─────────────────────────────────────────────────────────

/// Run a DAG locally in single-node mode.  No coordinator, no TCP — just
/// spawn the Executor as a subprocess, collect metrics, and report results.
fn cmd_run(args: &[String]) -> Result<()> {
    let dag_path = parse_dag_flag(args)?;
    let executor_bin = parse_executor_flag(args)
        .unwrap_or_else(|_| DEFAULT_EXECUTOR_BIN.to_string());
    let metrics_log = parse_string_flag(args, "--metrics-log")
        .unwrap_or_else(|_| "/tmp/node_agent_metrics.jsonl".to_string());

    let dag_json = std::fs::read_to_string(&dag_path)
        .with_context(|| format!("read DAG file: {}", dag_path))?;

    // Extract shm_path for metrics.
    let shm_path: Option<String> = serde_json::from_str::<serde_json::Value>(&dag_json)
        .ok()
        .and_then(|v| v.get("shm_path")?.as_str().map(|s| s.to_string()));

    let executor_path = Path::new(&executor_bin);
    if !executor_path.exists() {
        bail!(
            "executor binary not found: {}\n  \
             Build it first: cd Executor && cargo +nightly build --release",
            executor_bin
        );
    }

    // CWD for the executor = current directory (project root).
    let work_dir = std::env::current_dir()?;

    println!("NodeAgent — single-node mode");
    println!("  DAG:      {}", dag_path);
    println!("  Executor: {}", executor_bin);
    println!("  CWD:      {}", work_dir.display());
    println!();

    let job_id = format!("local_{}", std::process::id());
    let start = Instant::now();

    let mut handle = executor::ExecutorHandle::spawn(
        executor_path,
        &work_dir,
        &dag_json,
        &job_id,
        true, // live output to terminal
    )?;

    println!("[run] executor started (pid={})", handle.pid());

    // Poll executor and collect metrics until it finishes.
    let mut collector = MetricsCollector::new(0);
    let metrics_interval = std::time::Duration::from_secs(2);
    let mut last_metrics = Instant::now();

    loop {
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Sample metrics periodically.
        if last_metrics.elapsed() >= metrics_interval {
            let m = collector.sample(
                shm_path.as_deref(),
                true,
                Some(&job_id),
                Some(handle.elapsed_ms()),
            );
            let _ = metrics::append_metrics_log(&metrics_log, &m);
            last_metrics = Instant::now();
        }

        // Check if executor finished.
        if let Some(result) = handle.try_wait()? {
            let elapsed = start.elapsed();
            println!();
            if result.success {
                println!("[run] completed in {:.2}s", elapsed.as_secs_f64());
            } else {
                eprintln!(
                    "[run] FAILED (exit={:?}) in {:.2}s",
                    result.exit_code,
                    elapsed.as_secs_f64()
                );
            }

            if !result.success {
                bail!("executor exited with code {:?}", result.exit_code);
            }
            return Ok(());
        }
    }
}

// ── Multi-node commands ─────────────────────────────────────────────────────

/// Start the agent daemon (coordinator or worker, based on config).
fn cmd_start(args: &[String]) -> Result<()> {
    let config_path = parse_config_flag(args)?;
    let config = AgentConfig::load(Path::new(&config_path))?;

    println!("NodeAgent v0.1.0");
    println!("  node_id: {}", config.node_id);
    println!("  role:    {:?}", config.role);
    println!("  cluster: {} nodes", config.total_nodes());

    match config.role {
        Role::Coordinator => coordinator::run_coordinator(&config),
        Role::Worker => worker::run_worker(&config),
    }
}

/// Submit a ClusterDag job to the coordinator.
fn cmd_submit(args: &[String]) -> Result<()> {
    let config_path = parse_config_flag(args)?;
    let dag_path = parse_dag_flag(args)?;
    let config = AgentConfig::load(Path::new(&config_path))?;

    let cluster_dag_json = std::fs::read_to_string(&dag_path)
        .with_context(|| format!("read ClusterDag file: {}", dag_path))?;

    // Validate the ClusterDag before sending.
    let _cluster_dag = cluster_dag::ClusterDag::from_json(&cluster_dag_json)?;

    let coord_addr = format!("{}:{}", config.coordinator_ip(), config.cluster.agent_port);
    println!("Submitting job to coordinator at {}...", coord_addr);

    let mut stream = std::net::TcpStream::connect(&coord_addr)
        .with_context(|| format!("connect to coordinator at {}", coord_addr))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(300)))?;

    // Send SubmitJob.
    protocol::send_message(
        &mut stream,
        &protocol::make_message(
            protocol::MessageKind::SubmitJob,
            &protocol::SubmitJobPayload { cluster_dag_json },
        )?,
    )?;

    // Wait for ACK.
    let ack = protocol::recv_message(&mut stream)?;
    if ack.kind == protocol::MessageKind::SubmitAck {
        let job_id = ack.payload.get("job_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        println!("Job submitted: {}", job_id);
    }

    // Wait for final result.
    println!("Waiting for job completion...");
    let result = protocol::recv_message(&mut stream)?;
    if result.kind == protocol::MessageKind::JobResult {
        let payload: protocol::JobResultPayload =
            serde_json::from_value(result.payload)?;
        println!("\n=== Job Result ===");
        println!("Job ID:  {}", payload.job_id);
        println!("Success: {}", payload.success);
        if !payload.summary.is_empty() {
            println!("Summary:\n{}", payload.summary);
        }
    }

    Ok(())
}

/// Query the coordinator for cluster status.
fn cmd_status(args: &[String]) -> Result<()> {
    let config_path = parse_config_flag(args)?;
    let config = AgentConfig::load(Path::new(&config_path))?;

    let coord_addr = format!("{}:{}", config.coordinator_ip(), config.cluster.agent_port);

    let mut stream = std::net::TcpStream::connect(&coord_addr)
        .with_context(|| format!("connect to coordinator at {}", coord_addr))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;

    protocol::send_message(
        &mut stream,
        &protocol::make_signal(protocol::MessageKind::StatusQuery),
    )?;

    let resp = protocol::recv_message(&mut stream)?;
    if resp.kind == protocol::MessageKind::StatusResponse {
        let payload: protocol::StatusResponsePayload =
            serde_json::from_value(resp.payload)?;

        println!("=== Cluster Status ===");
        println!("Current job: {}", payload.current_job.as_deref().unwrap_or("none"));
        println!("Workers:");
        for w in &payload.workers {
            println!(
                "  node {}: connected={}, job={}",
                w.node_id,
                w.connected,
                w.running_job.as_deref().unwrap_or("none")
            );
        }
    }

    Ok(())
}

// ── Arg parsing helpers ─────────────────────────────────────────────────────

fn parse_config_flag(args: &[String]) -> Result<String> {
    parse_string_flag(args, "--config")
        .or_else(|_| parse_string_flag(args, "-c"))
        .or_else(|_| Ok("agent.toml".to_string()))
}

fn parse_dag_flag(args: &[String]) -> Result<String> {
    parse_string_flag(args, "--dag")
        .or_else(|_| parse_string_flag(args, "-d"))
        .or_else(|_| {
            // Accept a positional arg (first arg that doesn't start with --)
            for arg in args {
                if !arg.starts_with("--") && !arg.starts_with("-") {
                    return Ok(arg.clone());
                }
            }
            bail!("--dag <path> is required")
        })
}

fn parse_executor_flag(args: &[String]) -> Result<String> {
    parse_string_flag(args, "--executor")
}

fn parse_string_flag(args: &[String], flag: &str) -> Result<String> {
    for (i, arg) in args.iter().enumerate() {
        if arg == flag {
            if let Some(val) = args.get(i + 1) {
                return Ok(val.clone());
            }
        }
    }
    bail!("{} not specified", flag)
}

fn print_usage() {
    eprintln!("NodeAgent v0.1.0 — Distributed DAG execution agent");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  node-agent run    <dag.json> [--executor <path>]     Run a DAG locally (single-node)");
    eprintln!("  node-agent start  [--config agent.toml]              Start the agent daemon (multi-node)");
    eprintln!("  node-agent submit [--config agent.toml] --dag <file> Submit a ClusterDag job");
    eprintln!("  node-agent status [--config agent.toml]              Query cluster status");
    eprintln!();
    eprintln!("Single-node mode:");
    eprintln!("  Run from the project root (WebAsShared/).  The agent spawns the Executor");
    eprintln!("  as a subprocess, collects metrics, and reports results.");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  node-agent run DAGs/workload_dag/finra_demo.json");
    eprintln!("  node-agent run DAGs/workload_dag/py_word_count_demo.json");
    eprintln!("  node-agent run DAGs/demo_dag/img_pipeline_demo.json");
}
