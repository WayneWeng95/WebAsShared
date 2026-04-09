//! Lightweight client for the SCX sched_ext stats UNIX domain socket.
//!
//! Protocol: send a JSON request line, receive a JSON response line.
//! Socket default: `/var/run/scx/root/stats`

use node_agent_common as common;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Snapshot of SCX scheduler stats relevant for cross-node scheduling decisions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScxNodeSnapshot {
    /// Overall CPU busy percentage (100.0 = all CPUs fully busy).
    pub cpu_busy: f64,
    /// Weighted system load (sum of weight * duty_cycle).
    pub load: f64,
    /// Number of task migrations from load balancing.
    pub nr_migrations: u64,
    /// Current scheduling time slice in microseconds.
    pub slice_us: u64,
    /// Time spent in userspace scheduler (seconds).
    pub time_used: f64,
    /// Per-NUMA-node statistics.
    #[serde(default)]
    pub numa_nodes: BTreeMap<usize, ScxNumaStats>,
}

/// Per-NUMA-node scheduling statistics from SCX.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScxNumaStats {
    /// Weighted load on this NUMA node.
    pub load: f64,
    /// Load imbalance from average across NUMA nodes.
    pub imbal: f64,
}

/// Client that fetches stats from a local SCX scheduler via UNIX socket.
pub struct ScxStatsClient {
    socket_path: String,
}

/// Raw SCX stats response envelope.
#[derive(Deserialize)]
struct StatsResponse {
    errno: i32,
    #[serde(default)]
    args: Option<StatsResponseArgs>,
}

#[derive(Deserialize)]
struct StatsResponseArgs {
    resp: serde_json::Value,
}

impl ScxStatsClient {
    pub fn new(socket_path: Option<&str>) -> Self {
        Self {
            socket_path: socket_path
                .unwrap_or(common::DEFAULT_SCX_SOCKET)
                .to_string(),
        }
    }

    /// Fetch a snapshot of SCX stats. Returns `None` if the scheduler is not
    /// running or the socket is unavailable.
    pub fn fetch(&self) -> Option<ScxNodeSnapshot> {
        let stream = UnixStream::connect(&self.socket_path).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_millis(common::SCX_READ_TIMEOUT_MS)))
            .ok()?;
        stream
            .set_write_timeout(Some(Duration::from_millis(common::SCX_CONNECT_TIMEOUT_MS)))
            .ok()?;

        self.fetch_from_stream(stream)
    }

    fn fetch_from_stream(&self, mut stream: UnixStream) -> Option<ScxNodeSnapshot> {
        // Send stats request.
        let req = r#"{"req":"stats","args":{}}"#;
        writeln!(stream, "{}", req).ok()?;
        stream.flush().ok()?;

        // Read single-line JSON response.
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;

        let resp: StatsResponse = serde_json::from_str(&line).ok()?;
        if resp.errno != 0 {
            return None;
        }

        let data = resp.args?.resp;
        Some(parse_scx_stats(&data))
    }
}

/// Extract the fields we care about from raw SCX stats JSON.
/// This is intentionally lenient — missing fields get defaults.
fn parse_scx_stats(data: &serde_json::Value) -> ScxNodeSnapshot {
    let cpu_busy = data.get("cpu_busy").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let load = data.get("load").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let nr_migrations = data.get("nr_migrations").and_then(|v| v.as_u64()).unwrap_or(0);
    let slice_us = data.get("slice_us").and_then(|v| v.as_u64()).unwrap_or(0);
    let time_used = data.get("time_used").and_then(|v| v.as_f64()).unwrap_or(0.0);

    let mut numa_nodes = BTreeMap::new();
    if let Some(nodes_obj) = data.get("nodes").and_then(|v| v.as_object()) {
        for (key, node_val) in nodes_obj {
            if let Ok(idx) = key.parse::<usize>() {
                let node_load = node_val.get("load").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let imbal = node_val.get("imbal").and_then(|v| v.as_f64()).unwrap_or(0.0);
                numa_nodes.insert(idx, ScxNumaStats { load: node_load, imbal });
            }
        }
    }

    ScxNodeSnapshot {
        cpu_busy,
        load,
        nr_migrations,
        slice_us,
        time_used,
        numa_nodes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_scx_stats_full() {
        let data: serde_json::Value = serde_json::from_str(r#"{
            "cpu_busy": 45.6,
            "load": 123.4,
            "nr_migrations": 100,
            "slice_us": 20000,
            "time_used": 0.05,
            "nodes": {
                "0": { "load": 60.0, "imbal": 2.5 },
                "1": { "load": 63.4, "imbal": -2.5 }
            }
        }"#).unwrap();

        let snap = parse_scx_stats(&data);
        assert!((snap.cpu_busy - 45.6).abs() < 0.01);
        assert_eq!(snap.nr_migrations, 100);
        assert_eq!(snap.numa_nodes.len(), 2);
        assert!((snap.numa_nodes[&0].imbal - 2.5).abs() < 0.01);
    }

    #[test]
    fn test_parse_scx_stats_empty() {
        let data = serde_json::Value::Object(serde_json::Map::new());
        let snap = parse_scx_stats(&data);
        assert_eq!(snap.cpu_busy, 0.0);
        assert_eq!(snap.nr_migrations, 0);
        assert!(snap.numa_nodes.is_empty());
    }
}
