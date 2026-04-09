//! Node scoring for workload placement decisions.
//!
//! Scores each node based on its SCX stats: lower score = less loaded = better
//! candidate for receiving work.

use crate::scx_cluster::ScxClusterView;

/// Weight factors for scoring components.
const W_CPU_BUSY: f64 = 0.50;
const W_NUMA_IMBAL: f64 = 0.30;
const W_MIGRATIONS: f64 = 0.20;

/// Score all nodes in the cluster view.
/// Returns a list of (node_id, score) sorted by score ascending (best first).
/// Lower score = less loaded = better candidate.
pub fn score_nodes(view: &ScxClusterView) -> Vec<(u32, f64)> {
    if view.snapshots.is_empty() {
        return Vec::new();
    }

    // Find max migration count for normalization.
    let max_migrations = view
        .snapshots
        .values()
        .map(|s| s.nr_migrations)
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    let mut scores: Vec<(u32, f64)> = view
        .snapshots
        .iter()
        .map(|(&node_id, snap)| {
            // CPU busy: already 0-100 range, normalize to 0-1.
            let cpu_score = snap.cpu_busy / 100.0;

            // NUMA imbalance: average absolute imbalance across NUMA nodes.
            let numa_imbal_score = if snap.numa_nodes.is_empty() {
                0.0
            } else {
                let total_imbal: f64 = snap.numa_nodes.values().map(|n| n.imbal.abs()).sum();
                let avg_imbal = total_imbal / snap.numa_nodes.len() as f64;
                // Normalize: imbalance is relative to load, cap at 1.0.
                (avg_imbal / snap.load.max(1.0)).min(1.0)
            };

            // Migration rate: normalized against the busiest node.
            let migration_score = snap.nr_migrations as f64 / max_migrations;

            let score =
                W_CPU_BUSY * cpu_score + W_NUMA_IMBAL * numa_imbal_score + W_MIGRATIONS * migration_score;

            (node_id, score)
        })
        .collect();

    scores.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    scores
}

/// Select the best N node IDs for placement (least loaded first).
pub fn best_nodes(view: &ScxClusterView, n: usize) -> Vec<u32> {
    score_nodes(view)
        .into_iter()
        .take(n)
        .map(|(id, _)| id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scx_client::{ScxNodeSnapshot, ScxNumaStats};
    use std::collections::BTreeMap;

    #[test]
    fn test_score_nodes_prefers_less_loaded() {
        let mut view = ScxClusterView::new();

        // Node 0: heavily loaded
        let mut numa0 = BTreeMap::new();
        numa0.insert(0, ScxNumaStats { load: 80.0, imbal: 10.0 });
        view.update(
            0,
            ScxNodeSnapshot {
                cpu_busy: 90.0,
                load: 80.0,
                nr_migrations: 500,
                numa_nodes: numa0,
                ..Default::default()
            },
            1000,
        );

        // Node 1: lightly loaded
        let mut numa1 = BTreeMap::new();
        numa1.insert(0, ScxNumaStats { load: 20.0, imbal: 1.0 });
        view.update(
            1,
            ScxNodeSnapshot {
                cpu_busy: 20.0,
                load: 20.0,
                nr_migrations: 50,
                numa_nodes: numa1,
                ..Default::default()
            },
            1000,
        );

        let scores = score_nodes(&view);
        assert_eq!(scores.len(), 2);
        // Node 1 should be first (lower score)
        assert_eq!(scores[0].0, 1);
        assert_eq!(scores[1].0, 0);
    }

    #[test]
    fn test_best_nodes() {
        let mut view = ScxClusterView::new();
        view.update(0, ScxNodeSnapshot { cpu_busy: 90.0, ..Default::default() }, 1000);
        view.update(1, ScxNodeSnapshot { cpu_busy: 10.0, ..Default::default() }, 1000);
        view.update(2, ScxNodeSnapshot { cpu_busy: 50.0, ..Default::default() }, 1000);

        let best = best_nodes(&view, 2);
        assert_eq!(best.len(), 2);
        assert_eq!(best[0], 1); // least loaded
        assert_eq!(best[1], 2); // second least
    }

    #[test]
    fn test_empty_view() {
        let view = ScxClusterView::new();
        assert!(score_nodes(&view).is_empty());
        assert!(best_nodes(&view, 5).is_empty());
    }
}
