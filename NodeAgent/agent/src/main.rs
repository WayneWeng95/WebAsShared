mod cluster_dag;
mod config;
mod coordinator;
mod executor;
mod metrics;
mod protocol;
mod worker;

use anyhow::{bail, Context, Result};
use config::{AgentConfig, Role};
use std::path::Path;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        return Ok(());
    }

    match args[1].as_str() {
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

/// Parse --config flag from args.
fn parse_config_flag(args: &[String]) -> Result<String> {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--config" || arg == "-c" {
            if let Some(val) = args.get(i + 1) {
                return Ok(val.clone());
            }
        }
    }
    // Default config path.
    Ok("agent.toml".to_string())
}

/// Parse --dag flag from args.
fn parse_dag_flag(args: &[String]) -> Result<String> {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--dag" || arg == "-d" {
            if let Some(val) = args.get(i + 1) {
                return Ok(val.clone());
            }
        }
    }
    bail!("--dag <path> is required for submit command");
}

fn print_usage() {
    eprintln!("NodeAgent v0.1.0 — Distributed DAG execution coordinator");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  node-agent start  [--config agent.toml]              Start the agent daemon");
    eprintln!("  node-agent submit [--config agent.toml] --dag <file> Submit a ClusterDag job");
    eprintln!("  node-agent status [--config agent.toml]              Query cluster status");
    eprintln!();
    eprintln!("The agent runs as a coordinator (node 0) or worker based on the config file.");
    eprintln!("Workers connect to the coordinator and wait for job assignments.");
    eprintln!("Use 'submit' to send a ClusterDag JSON to the coordinator for distributed execution.");
}
