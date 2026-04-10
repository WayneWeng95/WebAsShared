//! Aggregated cluster-wide view of node health: SCX stats, memory, and job state.
//!
//! The coordinator maintains this view, updating it as workers report metrics.

use crate::scx_client::ScxNodeSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Combined status for a single node: SCX scheduler stats + system metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeStatus {
    /// SCX sched_ext stats (None if SCX is not running on this node).
    pub scx: Option<ScxNodeSnapshot>,
    /// CPU usage percentage (0–100).
    pub cpu_usage_pct: f32,
    /// Resident memory usage in bytes.
    pub rss_bytes: u64,
    /// Whether an executor is currently running on this node.
    pub executor_running: bool,
    /// Job ID currently running (if any).
    pub current_job_id: Option<String>,
}

/// Cluster-wide view of all node statuses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScxClusterView {
    /// Latest status per node.
    pub snapshots: HashMap<u32, NodeStatus>,
    /// Timestamp (ms since epoch) of last update per node.
    pub updated_at: HashMap<u32, u64>,
}

impl ScxClusterView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the full status for a given node.
    pub fn update(
        &mut self,
        node_id: u32,
        scx: Option<ScxNodeSnapshot>,
        cpu_usage_pct: f32,
        rss_bytes: u64,
        executor_running: bool,
        current_job_id: Option<String>,
        timestamp_ms: u64,
    ) {
        self.snapshots.insert(
            node_id,
            NodeStatus {
                scx,
                cpu_usage_pct,
                rss_bytes,
                executor_running,
                current_job_id,
            },
        );
        self.updated_at.insert(node_id, timestamp_ms);
    }

    /// Check if we have data for a node.
    pub fn has_node(&self, node_id: u32) -> bool {
        self.snapshots.contains_key(&node_id)
    }

    /// Get the number of nodes with data.
    pub fn node_count(&self) -> usize {
        self.snapshots.len()
    }

    /// Check if a node's data is stale (older than given threshold in ms).
    pub fn is_stale(&self, node_id: u32, now_ms: u64, max_age_ms: u64) -> bool {
        match self.updated_at.get(&node_id) {
            Some(&ts) => now_ms.saturating_sub(ts) > max_age_ms,
            None => true,
        }
    }

    /// Count how many nodes are currently running a job.
    pub fn busy_node_count(&self) -> usize {
        self.snapshots.values().filter(|s| s.executor_running).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cluster_view_update() {
        let mut view = ScxClusterView::new();
        assert_eq!(view.node_count(), 0);

        let snap = ScxNodeSnapshot {
            cpu_busy: 50.0,
            load: 100.0,
            ..Default::default()
        };

        view.update(1, Some(snap), 0.0, 1024 * 1024 * 512, false, None, 1000);
        assert!(view.has_node(1));
        assert!(!view.has_node(2));
        assert_eq!(view.node_count(), 1);
        assert_eq!(view.busy_node_count(), 0);
    }

    #[test]
    fn test_staleness() {
        let mut view = ScxClusterView::new();
        view.update(0, None, 0.0, 0, false, None, 1000);

        assert!(!view.is_stale(0, 2000, 5000)); // 1s old, threshold 5s
        assert!(view.is_stale(0, 7000, 5000));  // 6s old, threshold 5s
        assert!(view.is_stale(99, 2000, 5000)); // unknown node
    }

    #[test]
    fn test_busy_count() {
        let mut view = ScxClusterView::new();
        view.update(0, None, 0.0, 0, true, Some("job_1".into()), 1000);
        view.update(1, None, 0.0, 0, false, None, 1000);
        view.update(2, None, 0.0, 0, true, Some("job_2".into()), 1000);
        assert_eq!(view.busy_node_count(), 2);
    }
}
