mod cluster_dag;
mod config;
mod coordinator;
mod dag_transform;
mod executor;
mod metrics;
mod protocol;
mod worker;

use node_agent_common as common;

use anyhow::{bail, Context, Result};
use config::{AgentConfig, Role};
use metrics::MetricsCollector;
use std::path::Path;
use std::time::Instant;

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
        .unwrap_or_else(|_| common::DEFAULT_EXECUTOR_BIN.to_string());
    let metrics_log = parse_string_flag(args, "--metrics-log")
        .unwrap_or_else(|_| common::DEFAULT_METRICS_LOG.to_string());
    let status_interval_s: u64 = parse_string_flag(args, "--status-interval")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(common::DEFAULT_STATUS_PRINT_INTERVAL_S);
    let python_mode = has_flag(args, "--python");
    let aot_mode = has_flag(args, "--aot");
    let python_script = parse_string_flag(args, "--python-script").ok();
    let mut python_wasm = parse_string_flag(args, "--python-wasm").ok();

    // AOT: pre-compile python.wasm → .cwasm if needed.
    if python_mode && aot_mode {
        let wasm_path = python_wasm
            .as_deref()
            .unwrap_or(dag_transform::DEFAULT_PYTHON_WASM);
        let cwasm_path = aot_compile(wasm_path)?;
        python_wasm = Some(cwasm_path);
    }

    let raw_dag_json = std::fs::read_to_string(&dag_path)
        .with_context(|| format!("read DAG file: {}", dag_path))?;

    // Transform unified Func nodes into WasmVoid or PyFunc.
    let dag_json = dag_transform::transform_dag(
        &raw_dag_json,
        python_mode,
        python_script.as_deref(),
        python_wasm.as_deref(),
    )?;

    if python_mode {
        println!("Mode: Python{}", if aot_mode { " (AOT)" } else { "" });
    }

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
    let metrics_interval = std::time::Duration::from_millis(common::DEFAULT_METRICS_INTERVAL_MS);
    let mut last_metrics = Instant::now();
    let status_interval = std::time::Duration::from_secs(status_interval_s);
    let mut last_status = Instant::now();

    loop {
        std::thread::sleep(std::time::Duration::from_millis(common::POLL_SLEEP_MS));

        // Sample metrics periodically.
        if last_metrics.elapsed() >= metrics_interval {
            let m = collector.sample(
                shm_path.as_deref(),
                true,
                Some(&job_id),
                Some(handle.elapsed_ms()),
            );
            let _ = metrics::append_metrics_log(&metrics_log, &m);

            // Print node status to console.
            if last_status.elapsed() >= status_interval {
                print_node_status(&m);
                last_status = Instant::now();
            }

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
    let python_mode = has_flag(args, "--python");
    let aot_mode = has_flag(args, "--aot");
    let python_script = parse_string_flag(args, "--python-script").ok();
    let mut python_wasm = parse_string_flag(args, "--python-wasm").ok();
    let config = AgentConfig::load(Path::new(&config_path))?;

    // AOT: pre-compile python.wasm → .cwasm if needed.
    if python_mode && aot_mode {
        let wasm_path = python_wasm
            .as_deref()
            .unwrap_or(dag_transform::DEFAULT_PYTHON_WASM);
        let cwasm_path = aot_compile(wasm_path)?;
        python_wasm = Some(cwasm_path);
    }

    let raw_json = std::fs::read_to_string(&dag_path)
        .with_context(|| format!("read ClusterDag file: {}", dag_path))?;

    if python_mode {
        println!("Mode: Python (distributed){}", if aot_mode { " (AOT)" } else { "" });
    }

    // Always transform: converts unified Func nodes to native kinds (WasmVoid or PyFunc).
    let cluster_dag_json = dag_transform::transform_cluster_dag(
        &raw_json,
        python_mode,
        python_script.as_deref(),
        python_wasm.as_deref(),
    )?;

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

        // Display node scheduling data if available.
        if let Some(ref scx_view) = payload.scx_cluster {
            println!("\n=== Node Scheduling Stats ===");
            let scores = scheduler::score_nodes(scx_view);
            for (node_id, score) in &scores {
                if let Some(status) = scx_view.snapshots.get(node_id) {
                    let job_str = status.current_job_id.as_deref().unwrap_or("idle");
                    let rss_mb = status.rss_bytes as f64 / (1024.0 * 1024.0);
                    print!(
                        "  node {}: rss={:.0} MiB, job={}, score={:.3}",
                        node_id, rss_mb, job_str, score
                    );
                    if let Some(ref scx) = status.scx {
                        print!(
                            ", cpu_busy={:.1}%, load={:.1}, migrations={}",
                            scx.cpu_busy, scx.load, scx.nr_migrations
                        );
                        for (numa_id, numa) in &scx.numa_nodes {
                            print!(
                                "\n    NUMA {}: load={:.1}, imbal={:.1}",
                                numa_id, numa.load, numa.imbal
                            );
                        }
                    }
                    println!();
                }
            }
            if !scores.is_empty() {
                println!("  Best placement order: {:?}", scheduler::advisor::best_nodes(scx_view, scores.len()));
            }
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

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

// ── AOT compilation ────────────────────────────────────────────────────────

/// Pre-compile a .wasm file to .cwasm (Wasmtime AOT) if not already cached.
/// Returns the path to the .cwasm file.
fn aot_compile(wasm_path: &str) -> Result<String> {
    let cwasm_path = wasm_path
        .strip_suffix(".wasm")
        .map(|base| format!("{}.cwasm", base))
        .unwrap_or_else(|| format!("{}.cwasm", wasm_path));

    if Path::new(&cwasm_path).exists() {
        println!("[aot] using cached {}", cwasm_path);
        return Ok(cwasm_path);
    }

    if !Path::new(wasm_path).exists() {
        bail!("python WASM not found: {}", wasm_path);
    }

    // Find wasmtime binary.
    let wasmtime = std::env::var("WASMTIME").unwrap_or_else(|_| {
        let home_candidate = std::env::var("HOME")
            .map(|h| format!("{}/.wasmtime/bin/wasmtime", h))
            .unwrap_or_default();
        if !home_candidate.is_empty() && Path::new(&home_candidate).exists() {
            home_candidate
        } else {
            "wasmtime".to_string()
        }
    });

    println!("[aot] compiling {} → {} ...", wasm_path, cwasm_path);
    let start = std::time::Instant::now();

    let status = std::process::Command::new(&wasmtime)
        .arg("compile")
        .arg(wasm_path)
        .arg("-o")
        .arg(&cwasm_path)
        .status()
        .with_context(|| format!("run `{} compile`", wasmtime))?;

    if !status.success() {
        bail!("`wasmtime compile` failed (exit={})", status);
    }

    println!("[aot] compiled in {:.1}s", start.elapsed().as_secs_f64());
    Ok(cwasm_path)
}

// ── Status printing ─────────────────────────────────────────────────────────

/// Print a single-node status line to the console.
fn print_node_status(m: &metrics::NodeMetrics) {
    let rss_mb = m.rss_bytes as f64 / (1024.0 * 1024.0);
    let job_str = m.current_job_id.as_deref().unwrap_or("idle");
    let elapsed_str = m.job_elapsed_ms
        .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
        .unwrap_or_else(|| "-".into());

    let mut line = format!(
        "[status] node={}, cpu={:.1}%, rss={:.0} MiB, shm_bump={}, job={}, elapsed={}",
        m.node_id, m.cpu_usage_pct, rss_mb, m.shm_bump_offset, job_str, elapsed_str,
    );

    if let Some(ref scx) = m.scx {
        line.push_str(&format!(
            ", scx(cpu_busy={:.1}%, load={:.1}, migrations={})",
            scx.cpu_busy, scx.load, scx.nr_migrations,
        ));
    }

    println!("{}", line);
}

fn print_usage() {
    eprintln!("NodeAgent v0.1.0 — Distributed DAG execution agent");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  node-agent run    <dag.json> [--python] [--executor <path>]");
    eprintln!("  node-agent start  [--config agent.toml]");
    eprintln!("  node-agent submit [--config agent.toml] --dag <file> [--python] [--aot]");
    eprintln!("  node-agent status [--config agent.toml]");
    eprintln!();
    eprintln!("Run flags:");
    eprintln!("  --python                   Execute with Python guest (default: Rust/WASM)");
    eprintln!("  --aot                      AOT-compile python.wasm to .cwasm (skips JIT at runtime)");
    eprintln!("  --python-script <path>     Python runner script (default: Executor/py_guest/python/runner.py)");
    eprintln!("  --python-wasm <path>       Python WASM runtime (default: /opt/myapp/python-3.12.0.wasm)");
    eprintln!("  --executor <path>          Executor binary (default: {})", common::DEFAULT_EXECUTOR_BIN);
    eprintln!("  --metrics-log <path>       Metrics log file (default: {})", common::DEFAULT_METRICS_LOG);
    eprintln!("  --status-interval <secs>   Status print interval (default: {}s)", common::DEFAULT_STATUS_PRINT_INTERVAL_S);
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  node-agent run DAGs/workload_dag/finra_demo.json");
    eprintln!("  node-agent run DAGs/workload_dag/finra_demo.json --python");
    eprintln!("  node-agent run DAGs/workload_dag/finra_demo.json --python --aot");
    eprintln!("  node-agent run DAGs/workload_dag/word_count_demo.json");
    eprintln!("  node-agent run DAGs/demo_dag/img_pipeline_demo.json");
    eprintln!("  node-agent submit --config agent.toml --dag cluster_dags/finra.json");
    eprintln!("  node-agent submit --config agent.toml --dag cluster_dags/finra.json --python");
    eprintln!("  node-agent submit --config agent.toml --dag cluster_dags/finra.json --python --aot");
}
