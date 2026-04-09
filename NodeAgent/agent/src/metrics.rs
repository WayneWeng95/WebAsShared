//! System and SHM metrics collection.
//!
//! Reads CPU usage from /proc/stat, RSS from /proc/self/status,
//! and SHM bump allocator offset from the Superblock.

use anyhow::Result;
use scheduler::ScxNodeSnapshot;
use serde::{Deserialize, Serialize};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeMetrics {
    pub node_id: u32,
    pub timestamp_ms: u64,
    pub cpu_usage_pct: f32,
    pub rss_bytes: u64,
    pub shm_bump_offset: u32,
    pub executor_running: bool,
    pub current_job_id: Option<String>,
    pub job_elapsed_ms: Option<u64>,
    /// SCX sched_ext scheduler stats (None if SCX is not running or disabled).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scx: Option<ScxNodeSnapshot>,
}

/// State for computing CPU usage deltas between samples.
pub struct MetricsCollector {
    node_id: u32,
    prev_cpu_total: u64,
    prev_cpu_idle: u64,
    scx_client: Option<scheduler::ScxStatsClient>,
}

impl MetricsCollector {
    pub fn new(node_id: u32) -> Self {
        Self {
            node_id,
            prev_cpu_total: 0,
            prev_cpu_idle: 0,
            scx_client: None,
        }
    }

    /// Create a collector with SCX stats collection enabled.
    pub fn with_scx(node_id: u32, socket_path: Option<&str>) -> Self {
        Self {
            node_id,
            prev_cpu_total: 0,
            prev_cpu_idle: 0,
            scx_client: Some(scheduler::ScxStatsClient::new(socket_path)),
        }
    }

    /// Sample current system metrics.
    pub fn sample(
        &mut self,
        shm_path: Option<&str>,
        executor_running: bool,
        current_job_id: Option<&str>,
        job_elapsed_ms: Option<u64>,
    ) -> NodeMetrics {
        let cpu_usage_pct = self.sample_cpu().unwrap_or(0.0);
        let rss_bytes = sample_rss().unwrap_or(0);
        let shm_bump_offset = shm_path
            .and_then(|p| read_shm_bump_offset(p).ok())
            .unwrap_or(0);

        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let scx = self.scx_client.as_ref().and_then(|c| c.fetch());

        NodeMetrics {
            node_id: self.node_id,
            timestamp_ms,
            cpu_usage_pct,
            rss_bytes,
            shm_bump_offset,
            executor_running,
            current_job_id: current_job_id.map(|s| s.to_string()),
            job_elapsed_ms,
            scx,
        }
    }

    /// Read /proc/stat and compute CPU usage since last sample.
    fn sample_cpu(&mut self) -> Result<f32> {
        let content = fs::read_to_string("/proc/stat")?;
        let first_line = content.lines().next().unwrap_or("");
        // "cpu  user nice system idle iowait irq softirq steal guest guest_nice"
        let fields: Vec<u64> = first_line
            .split_whitespace()
            .skip(1) // skip "cpu"
            .filter_map(|s| s.parse().ok())
            .collect();

        if fields.len() < 4 {
            return Ok(0.0);
        }

        let idle = fields[3];
        let total: u64 = fields.iter().sum();

        let delta_total = total.saturating_sub(self.prev_cpu_total);
        let delta_idle = idle.saturating_sub(self.prev_cpu_idle);

        self.prev_cpu_total = total;
        self.prev_cpu_idle = idle;

        if delta_total == 0 {
            return Ok(0.0);
        }

        Ok(100.0 * (1.0 - delta_idle as f32 / delta_total as f32))
    }
}

/// Read VmRSS from /proc/self/status.
fn sample_rss() -> Result<u64> {
    let content = fs::read_to_string("/proc/self/status")?;
    for line in content.lines() {
        if line.starts_with("VmRSS:") {
            // "VmRSS:    12345 kB"
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let kb: u64 = parts[1].parse().unwrap_or(0);
                return Ok(kb * 1024);
            }
        }
    }
    Ok(0)
}

/// Read the bump_allocator field from the SHM Superblock.
///
/// The Superblock layout (from common/src/lib.rs):
///   offset 0: magic (u32)
///   offset 4: bump_allocator (AtomicU32)
///
/// We read the raw bytes at offset 4 as a little-endian u32.
fn read_shm_bump_offset(shm_path: &str) -> Result<u32> {
    let data = fs::read(shm_path)?;
    if data.len() < 8 {
        return Ok(0);
    }
    // bump_allocator is at offset 4 (after the 4-byte magic field).
    let bytes: [u8; 4] = [data[4], data[5], data[6], data[7]];
    Ok(u32::from_le_bytes(bytes))
}

/// Append a metrics record to a JSON-lines log file.
pub fn append_metrics_log(path: &str, metrics: &NodeMetrics) -> Result<()> {
    use std::io::Write;
    let json = serde_json::to_string(metrics)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", json)?;
    Ok(())
}
