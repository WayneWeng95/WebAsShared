//! Node scoring for workload placement decisions.
//!
//! Scores each node based on SCX stats, memory utilization, and job state.
//! Lower score = less loaded = better candidate for receiving work.

use crate::scx_cluster::ScxClusterView;
use node_agent_common as common;
use std::collections::HashMap;

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

/// Return per-host sandbox colocation limits derived from CPU core headroom.
///
/// Formula: `max(1, floor(cpu_cores × (1 − cpu_busy / 100)))`.
/// Uses the more precise SCX `cpu_busy` when available, falls back to the
/// /proc-based `cpu_usage_pct`. Returns 1 when core count is unknown.
pub fn host_limits(view: &ScxClusterView) -> HashMap<u32, usize> {
    view.snapshots
        .iter()
        .map(|(&id, status)| {
            let cores = status.cpu_cores as f64;
            if cores == 0.0 {
                return (id, 1usize);
            }
            let busy_pct = status
                .scx
                .as_ref()
                .map(|s| s.cpu_busy)
                .unwrap_or(status.cpu_usage_pct as f64);
            let free = (1.0 - busy_pct / 100.0).clamp(0.0, 1.0);
            let limit = (cores * free).floor() as usize;
            (id, limit.max(1))
        })
        .collect()
}

/// Return per-host capacity weights normalized so they sum to 1.0.
///
/// Capacity is `1.0 - load_score`, so 1.0 means fully idle and 0.0 means
/// maxed out. A small floor (0.05) ensures even a loaded host gets some weight
/// so the placer can still spill to it if necessary.
///
/// Returns an empty map when the cluster view has no data.
pub fn cluster_capacity(view: &ScxClusterView) -> HashMap<u32, f64> {
    let scores = score_nodes(view);
    if scores.is_empty() {
        return HashMap::new();
    }
    let raw: Vec<(u32, f64)> = scores
        .iter()
        .map(|&(id, score)| (id, (1.0 - score).max(0.05)))
        .collect();
    let total: f64 = raw.iter().map(|(_, c)| c).sum();
    raw.into_iter().map(|(id, c)| (id, c / total)).collect()
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
            8 * 1024 * 1024 * 1024,
            true,
            Some("job_123".into()),
            1000,
            0,
        );

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
            512 * 1024 * 1024,
            false,
            None,
            1000,
            0,
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
        view.update(0, None, 0.0, 1024 * 1024 * 100, true, Some("job_1".into()), 1000, 0);
        view.update(1, None, 0.0, 1024 * 1024 * 100, false, None, 1000, 0);

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
        view.update(0, None, 0.0, 16 * 1024 * 1024 * 1024, false, None, 1000, 0);
        view.update(1, None, 0.0, 1 * 1024 * 1024 * 1024, false, None, 1000, 0);

        let scores = score_nodes(&view);
        assert_eq!(scores[0].0, 1, "low-memory node preferred");
        assert_eq!(scores[1].0, 0, "high-memory node ranks lower");
    }

    #[test]
    fn test_best_nodes() {
        let mut view = ScxClusterView::new();
        view.update(0, Some(ScxNodeSnapshot { cpu_busy: 90.0, ..Default::default() }), 0.0, 4_000_000_000, true, Some("j".into()), 1000, 0);
        view.update(1, Some(ScxNodeSnapshot { cpu_busy: 10.0, ..Default::default() }), 0.0, 500_000_000, false, None, 1000, 0);
        view.update(2, Some(ScxNodeSnapshot { cpu_busy: 50.0, ..Default::default() }), 0.0, 2_000_000_000, false, None, 1000, 0);

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
        let mut view = ScxClusterView::new();
        view.update(0, None, 0.0, 8_000_000_000, true, Some("job".into()), 1000, 0);
        view.update(1, None, 0.0, 1_000_000_000, false, None, 1000, 0);

        let scores = score_nodes(&view);
        assert_eq!(scores[0].0, 1, "idle low-memory node preferred even without SCX");
    }

    // ── host_limits tests ──────────────────────────────────────────────────────

    #[test]
    fn test_host_limits_scx_cpu_busy() {
        // Uses SCX cpu_busy when available.
        // Node 0: 8 cores, 25 % busy → floor(8 × 0.75) = 6
        // Node 1: 4 cores, 80 % busy → floor(4 × 0.20) = 0 → clamped to 1
        let mut view = ScxClusterView::new();
        view.update(
            0,
            Some(ScxNodeSnapshot { cpu_busy: 25.0, ..Default::default() }),
            0.0, 0, false, None, 1000, 8,
        );
        view.update(
            1,
            Some(ScxNodeSnapshot { cpu_busy: 80.0, ..Default::default() }),
            0.0, 0, false, None, 1000, 4,
        );

        let limits = host_limits(&view);
        assert_eq!(limits[&0], 6, "8 cores @ 25% busy → 6 free");
        assert_eq!(limits[&1], 1, "4 cores @ 80% busy → 0 free, clamped to 1");
    }

    #[test]
    fn test_host_limits_falls_back_to_proc_cpu() {
        // No SCX → uses cpu_usage_pct.
        // 16 cores, 50 % → floor(16 × 0.5) = 8
        let mut view = ScxClusterView::new();
        view.update(0, None, 50.0, 0, false, None, 1000, 16);

        let limits = host_limits(&view);
        assert_eq!(limits[&0], 8, "16 cores @ 50% proc-cpu → 8 free");
    }

    #[test]
    fn test_host_limits_unknown_cores_returns_one() {
        // cpu_cores = 0 (e.g. worker hasn't reported yet) → conservative limit of 1.
        let mut view = ScxClusterView::new();
        view.update(
            0,
            Some(ScxNodeSnapshot { cpu_busy: 10.0, ..Default::default() }),
            0.0, 0, false, None, 1000, 0,
        );

        let limits = host_limits(&view);
        assert_eq!(limits[&0], 1, "unknown core count → safe limit of 1");
    }

    #[test]
    fn test_host_limits_fully_idle() {
        // 0 % busy — all cores available.
        let mut view = ScxClusterView::new();
        view.update(
            0,
            Some(ScxNodeSnapshot { cpu_busy: 0.0, ..Default::default() }),
            0.0, 0, false, None, 1000, 12,
        );

        let limits = host_limits(&view);
        assert_eq!(limits[&0], 12, "12 cores @ 0% busy → all 12 available");
    }

    #[test]
    fn test_host_limits_fully_saturated() {
        // 100 % busy → 0 free cores → clamped to 1 (never block all placement).
        let mut view = ScxClusterView::new();
        view.update(
            0,
            Some(ScxNodeSnapshot { cpu_busy: 100.0, ..Default::default() }),
            0.0, 0, false, None, 1000, 8,
        );

        let limits = host_limits(&view);
        assert_eq!(limits[&0], 1, "saturated host still gets limit=1");
    }

    #[test]
    fn test_host_limits_drives_placement() {
        // Integration: build PlacementHints from advisor functions and feed to the
        // partitioner's assign_nodes — verify that the idle host absorbs more work.
        //
        // Node 0: 8 cores, 10 % busy → limit 7
        // Node 1: 4 cores, 90 % busy → limit 0, clamped to 1
        let mut view = ScxClusterView::new();
        view.update(
            0,
            Some(ScxNodeSnapshot { cpu_busy: 10.0, ..Default::default() }),
            0.0, 500_000_000, false, None, 1000, 8,
        );
        view.update(
            1,
            Some(ScxNodeSnapshot { cpu_busy: 90.0, ..Default::default() }),
            0.0, 4_000_000_000, false, None, 1000, 4,
        );

        let hints = partitioner::PlacementHints {
            capacity: cluster_capacity(&view),
            host_limit: host_limits(&view),
            random: false,
        };

        // 5 auto-nodes; host 0 can take up to 7, host 1 up to 1.
        let mut nodes: Vec<partitioner::SymbolicNode> = (0..5)
            .map(|i| partitioner::SymbolicNode {
                id: format!("n{}", i),
                deps: vec![],
                node_id: None,
                output_slot: None,
                fanout: None,
                placement: None,
                barrier_group: None,
                kind: serde_json::json!({"Func": {"slot": i}}),
            })
            .collect();

        partitioner::placer::assign_nodes(&mut nodes, 2, &hints, None);

        let on_0 = nodes.iter().filter(|n| n.node_id == Some(0)).count();
        let on_1 = nodes.iter().filter(|n| n.node_id == Some(1)).count();

        assert_eq!(on_0 + on_1, 5, "all nodes must be assigned");
        assert!(on_1 <= 1, "busy host (limit=1) must not exceed its limit, got {}", on_1);
        assert!(on_0 >= 4, "idle host should absorb most nodes, got {}", on_0);
    }
}
