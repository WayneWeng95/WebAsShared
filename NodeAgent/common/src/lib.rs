//! Tunable constants for the NodeAgent subsystem.
//!
//! All compile-time defaults for the agent, scheduler, and SCX client live here.
//! TOML config fields override these at runtime where applicable.
//!
//! Layout follows the Executor's common/src/lib.rs convention: grouped
//! `pub const` declarations with ASCII section separators.

// ─── Network ────────────────────────────────────────────────────────────────

/// Default TCP port for the NodeAgent control plane.
pub const DEFAULT_AGENT_PORT: u16 = 9500;

/// Maximum message size over the control plane (bytes).
pub const MAX_MSG_SIZE: u32 = 64 * 1024 * 1024; // 64 MiB

/// Worker → coordinator connection retry count.
pub const WORKER_CONNECT_RETRIES: u32 = 10;

/// Delay between worker connection retries (seconds).
pub const WORKER_CONNECT_RETRY_INTERVAL_S: u64 = 2;

/// Socket read timeout for the worker's polling loop (ms).
pub const WORKER_POLL_TIMEOUT_MS: u64 = 500;

/// Client socket read timeout (seconds).
pub const CLIENT_READ_TIMEOUT_S: u64 = 300;

// ─── Executor ───────────────────────────────────────────────────────────────

/// Default path to the Executor binary (from project root, single-node mode).
pub const DEFAULT_EXECUTOR_BIN: &str = "Executor/target/release/host";

/// Default path to the Executor binary (from NodeAgent dir, multi-node mode).
pub const DEFAULT_EXECUTOR_BIN_RELATIVE: &str = "../Executor/target/release/host";

/// Default working directory for the Executor process (multi-node).
pub const DEFAULT_EXECUTOR_WORK_DIR: &str = "../Executor/host";

// ─── Metrics & Monitoring ───────────────────────────────────────────────────

/// Default metrics sampling interval (ms).
pub const DEFAULT_METRICS_INTERVAL_MS: u64 = 2000;

/// Default metrics log file path.
pub const DEFAULT_METRICS_LOG: &str = "/tmp/node_agent_metrics.jsonl";

/// Default interval for printing node status to the console (seconds).
pub const DEFAULT_STATUS_PRINT_INTERVAL_S: u64 = 5;

/// Main loop sleep granularity (ms) — controls polling resolution.
pub const POLL_SLEEP_MS: u64 = 200;

/// Maximum lines of stdout/stderr tail captured from executor (multi-node).
pub const EXECUTOR_OUTPUT_TAIL_LINES: usize = 100;

// ─── Timeouts ───────────────────────────────────────────────────────────────

/// Maximum wall-clock time for a single job (seconds).
pub const DEFAULT_JOB_TIMEOUT_S: u64 = 300;

/// Health-check ping interval for idle workers (seconds).
pub const DEFAULT_HEALTH_CHECK_S: u64 = 5;

// ─── SCX sched_ext ──────────────────────────────────────────────────────────

/// Whether SCX stats collection is enabled by default.
pub const DEFAULT_SCX_ENABLED: bool = true;

/// Default UNIX socket path for the SCX stats server.
pub const DEFAULT_SCX_SOCKET: &str = "/var/run/scx/root/stats";

/// Timeout for connecting to the SCX stats socket (ms).
pub const SCX_CONNECT_TIMEOUT_MS: u64 = 500;

/// Timeout for reading from the SCX stats socket (ms).
pub const SCX_READ_TIMEOUT_MS: u64 = 1000;

// ─── Scheduler Advisor ──────────────────────────────────────────────────────

/// Weight: CPU busy score (from SCX).
pub const ADVISOR_W_CPU_BUSY: f64 = 0.30;

/// Weight: memory utilization score (from /proc RSS).
pub const ADVISOR_W_MEMORY: f64 = 0.20;

/// Weight: NUMA imbalance score (from SCX).
pub const ADVISOR_W_NUMA_IMBAL: f64 = 0.15;

/// Weight: task migration rate score (from SCX).
pub const ADVISOR_W_MIGRATIONS: f64 = 0.10;

/// Weight: job-running penalty (node already has an active executor).
pub const ADVISOR_W_JOB_RUNNING: f64 = 0.25;
