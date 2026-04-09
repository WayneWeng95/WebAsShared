//! Aggregated SCX stats view across all cluster nodes.
//!
//! The coordinator maintains this view, updating it as workers report metrics.

use crate::scx_client::ScxNodeSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Cluster-wide view of SCX scheduler stats from all nodes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScxClusterView {
    /// Latest SCX snapshot per node.
    pub snapshots: HashMap<u32, ScxNodeSnapshot>,
    /// Timestamp (ms since epoch) of last update per node.
    pub updated_at: HashMap<u32, u64>,
}

impl ScxClusterView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the snapshot for a given node.
    pub fn update(&mut self, node_id: u32, snapshot: ScxNodeSnapshot, timestamp_ms: u64) {
        self.snapshots.insert(node_id, snapshot);
        self.updated_at.insert(node_id, timestamp_ms);
    }

    /// Check if we have data for a node.
    pub fn has_node(&self, node_id: u32) -> bool {
        self.snapshots.contains_key(&node_id)
    }

    /// Get the number of nodes with SCX data.
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

        view.update(1, snap, 1000);
        assert!(view.has_node(1));
        assert!(!view.has_node(2));
        assert_eq!(view.node_count(), 1);
    }

    #[test]
    fn test_staleness() {
        let mut view = ScxClusterView::new();
        view.update(0, ScxNodeSnapshot::default(), 1000);

        assert!(!view.is_stale(0, 2000, 5000)); // 1s old, threshold 5s
        assert!(view.is_stale(0, 7000, 5000));  // 6s old, threshold 5s
        assert!(view.is_stale(99, 2000, 5000)); // unknown node
    }
}
