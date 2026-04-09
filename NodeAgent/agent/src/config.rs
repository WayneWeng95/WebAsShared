//! Agent configuration: parsed from `agent.toml`.

use node_agent_common as common;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct AgentConfig {
    /// This node's ID in the cluster (0-based).
    pub node_id: u32,
    /// Role: "coordinator" or "worker".
    pub role: Role,
    /// Cluster membership.
    pub cluster: ClusterConfig,
    /// Paths to the Executor binary and working directory.
    pub paths: PathsConfig,
    /// Metrics collection settings.
    #[serde(default)]
    pub metrics: MetricsConfig,
    /// Timeout settings.
    #[serde(default)]
    pub timeouts: TimeoutConfig,
    /// SCX sched_ext integration settings.
    #[serde(default)]
    pub scx: ScxConfig,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Coordinator,
    Worker,
}

#[derive(Debug, Deserialize)]
pub struct ClusterConfig {
    /// IP addresses of all nodes in the cluster, in node_id order.
    pub ips: Vec<String>,
    /// TCP port for NodeAgent control plane.
    #[serde(default = "default_agent_port")]
    pub agent_port: u16,
}

fn default_agent_port() -> u16 { common::DEFAULT_AGENT_PORT }

#[derive(Debug, Deserialize)]
pub struct PathsConfig {
    /// Path to the Executor binary (host).
    #[serde(default = "default_executor_bin")]
    pub executor_bin: String,
    /// Working directory for the Executor process.
    #[serde(default = "default_executor_work_dir")]
    pub executor_work_dir: String,
}

fn default_executor_bin() -> String { "../Executor/target/release/host".into() }
fn default_executor_work_dir() -> String { common::DEFAULT_EXECUTOR_WORK_DIR.into() }

#[derive(Debug, Deserialize)]
pub struct MetricsConfig {
    /// Metrics sampling interval in milliseconds.
    #[serde(default = "default_metrics_interval")]
    pub interval_ms: u64,
    /// Path to the metrics JSON-lines log file.
    #[serde(default = "default_metrics_log")]
    pub log_path: String,
    /// Interval for printing node status to the console (seconds).
    #[serde(default = "default_status_print_interval")]
    pub status_print_interval_s: u64,
}

fn default_metrics_interval() -> u64 { common::DEFAULT_METRICS_INTERVAL_MS }
fn default_metrics_log() -> String { common::DEFAULT_METRICS_LOG.into() }
fn default_status_print_interval() -> u64 { common::DEFAULT_STATUS_PRINT_INTERVAL_S }

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            interval_ms: default_metrics_interval(),
            log_path: default_metrics_log(),
            status_print_interval_s: default_status_print_interval(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TimeoutConfig {
    /// Maximum job execution time in seconds.
    #[serde(default = "default_job_timeout")]
    pub job_timeout_s: u64,
    /// Health check ping interval in seconds.
    #[serde(default = "default_health_check")]
    pub health_check_s: u64,
}

fn default_job_timeout() -> u64 { common::DEFAULT_JOB_TIMEOUT_S }
fn default_health_check() -> u64 { common::DEFAULT_HEALTH_CHECK_S }

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            job_timeout_s: default_job_timeout(),
            health_check_s: default_health_check(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ScxConfig {
    /// Whether SCX stats collection is enabled.
    #[serde(default = "default_scx_enabled")]
    pub enabled: bool,
    /// Path to the SCX stats UNIX socket.
    #[serde(default = "default_scx_socket")]
    pub socket_path: String,
}

fn default_scx_enabled() -> bool { common::DEFAULT_SCX_ENABLED }
fn default_scx_socket() -> String { common::DEFAULT_SCX_SOCKET.into() }

impl Default for ScxConfig {
    fn default() -> Self {
        Self {
            enabled: default_scx_enabled(),
            socket_path: default_scx_socket(),
        }
    }
}

impl AgentConfig {
    /// Load config from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("read config file: {}", path.display()))?;
        toml::from_str(&content)
            .with_context(|| format!("parse config file: {}", path.display()))
    }

    /// Get the coordinator's IP address.
    pub fn coordinator_ip(&self) -> &str {
        // Node 0 is always the coordinator.
        self.cluster.ips.first().map(|s| s.as_str()).unwrap_or("127.0.0.1")
    }

    /// Get the total number of nodes in the cluster.
    pub fn total_nodes(&self) -> usize {
        self.cluster.ips.len()
    }
}
