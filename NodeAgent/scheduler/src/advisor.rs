//! Node scoring for workload placement decisions.
//!
//! Scores each node based on SCX stats, memory utilization, and job state.
//! Lower score = less loaded = better candidate for receiving work.

use crate::scx_cluster::ScxClusterView;
use node_agent_common as common;

/// Score all nodes in the cluster view.
/// Returns a list of (node_id, score) sorted by score ascending (best first).
/// Lower score = less loaded = better candidate.
pub fn score_nodes(view: &ScxClusterView) -> Vec<(u32, f64)> {
    if view.snapshots.is_empty() {
        return Vec::new();
    }

    // Find max values across the cluster for normalization.
    let max_migrations = view
        .snapshots
        .values()
        .filter_map(|s| s.scx.as_ref())
        .map(|scx| scx.nr_migrations)
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    let max_rss = view
        .snapshots
        .values()
        .map(|s| s.rss_bytes)
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    let mut scores: Vec<(u32, f64)> = view
        .snapshots
        .iter()
        .map(|(&node_id, status)| {
            // Job running: binary penalty — a node already running a job is less desirable.
            let job_score = if status.executor_running { 1.0 } else { 0.0 };

            // Memory utilization: normalized against the most memory-heavy node.
            let memory_score = status.rss_bytes as f64 / max_rss;

            // SCX-derived scores (default to 0 if SCX is unavailable).
            let (cpu_score, numa_imbal_score, migration_score) =
                if let Some(ref scx) = status.scx {
                    let cpu = scx.cpu_busy / 100.0;

                    let numa_imbal = if scx.numa_nodes.is_empty() {
                        0.0
                    } else {
                        let total_imbal: f64 =
                            scx.numa_nodes.values().map(|n| n.imbal.abs()).sum();
                        let avg_imbal = total_imbal / scx.numa_nodes.len() as f64;
                        (avg_imbal / scx.load.max(1.0)).min(1.0)
                    };

                    let migration = scx.nr_migrations as f64 / max_migrations;

                    (cpu, numa_imbal, migration)
                } else {
                    (0.0, 0.0, 0.0)
                };

            let score = common::ADVISOR_W_CPU_BUSY * cpu_score
                + common::ADVISOR_W_MEMORY * memory_score
                + common::ADVISOR_W_NUMA_IMBAL * numa_imbal_score
                + common::ADVISOR_W_MIGRATIONS * migration_score
                + common::ADVISOR_W_JOB_RUNNING * job_score;

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

        // Node 0: heavily loaded, running a job, high memory
        let mut numa0 = BTreeMap::new();
        numa0.insert(0, ScxNumaStats { load: 80.0, imbal: 10.0 });
        view.update(
            0,
            Some(ScxNodeSnapshot {
                cpu_busy: 90.0,
                load: 80.0,
                nr_migrations: 500,
                numa_nodes: numa0,
                ..Default::default()
            }),
            0.0,
            8 * 1024 * 1024 * 1024, // 8 GiB
            true,
            Some("job_123".into()),
            1000,
        );

        // Node 1: lightly loaded, idle, low memory
        let mut numa1 = BTreeMap::new();
        numa1.insert(0, ScxNumaStats { load: 20.0, imbal: 1.0 });
        view.update(
            1,
            Some(ScxNodeSnapshot {
                cpu_busy: 20.0,
                load: 20.0,
                nr_migrations: 50,
                numa_nodes: numa1,
                ..Default::default()
            }),
            0.0,
            512 * 1024 * 1024, // 512 MiB
            false,
            None,
            1000,
        );

        let scores = score_nodes(&view);
        assert_eq!(scores.len(), 2);
        // Node 1 should be first (lower score)
        assert_eq!(scores[0].0, 1);
        assert_eq!(scores[1].0, 0);
    }

    #[test]
    fn test_job_running_penalty() {
        let mut view = ScxClusterView::new();

        // Both nodes have identical CPU/memory, but node 0 is running a job.
        view.update(0, None, 0.0, 1024 * 1024 * 100, true, Some("job_1".into()), 1000);
        view.update(1, None, 0.0, 1024 * 1024 * 100, false, None, 1000);

        let scores = score_nodes(&view);
        assert_eq!(scores[0].0, 1, "idle node should be preferred");
        assert_eq!(scores[1].0, 0, "busy node should rank lower");
        // The difference should be exactly ADVISOR_W_JOB_RUNNING.
        let diff = scores[1].1 - scores[0].1;
        assert!((diff - common::ADVISOR_W_JOB_RUNNING).abs() < 0.001);
    }

    #[test]
    fn test_memory_pressure() {
        let mut view = ScxClusterView::new();

        // Node 0: high memory usage
        view.update(0, None, 0.0, 16 * 1024 * 1024 * 1024, false, None, 1000);
        // Node 1: low memory usage
        view.update(1, None, 0.0, 1 * 1024 * 1024 * 1024, false, None, 1000);

        let scores = score_nodes(&view);
        assert_eq!(scores[0].0, 1, "low-memory node preferred");
        assert_eq!(scores[1].0, 0, "high-memory node ranks lower");
    }

    #[test]
    fn test_best_nodes() {
        let mut view = ScxClusterView::new();
        view.update(0, Some(ScxNodeSnapshot { cpu_busy: 90.0, ..Default::default() }), 0.0, 4_000_000_000, true, Some("j".into()), 1000);
        view.update(1, Some(ScxNodeSnapshot { cpu_busy: 10.0, ..Default::default() }), 0.0, 500_000_000, false, None, 1000);
        view.update(2, Some(ScxNodeSnapshot { cpu_busy: 50.0, ..Default::default() }), 0.0, 2_000_000_000, false, None, 1000);

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

    #[test]
    fn test_no_scx_still_scores() {
        // Even without SCX data, memory and job state should be scored.
        let mut view = ScxClusterView::new();
        view.update(0, None, 0.0, 8_000_000_000, true, Some("job".into()), 1000);
        view.update(1, None, 0.0, 1_000_000_000, false, None, 1000);

        let scores = score_nodes(&view);
        assert_eq!(scores[0].0, 1, "idle low-memory node preferred even without SCX");
    }
}
