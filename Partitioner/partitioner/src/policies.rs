//! Named placement policies for SymbolicDag auto-node distribution.
//!
//! A `PlacementPolicy` is embedded in a DAG JSON file as either a string
//! shorthand or a parameterised object:
//!
//! ```json
//! "placement_policy": "balanced"
//! "placement_policy": "pack"
//! "placement_policy": "spread"
//! "placement_policy": { "type": "weighted", "weights": [0.7, 0.3] }
//! "placement_policy": { "type": "balanced", "per_host_limit": 4 }
//! ```
//!
//! # Policy semantics
//!
//! | Policy      | Behaviour                                                      |
//! |-------------|----------------------------------------------------------------|
//! | `balanced`  | Equal capacity weight per host; nodes spread proportionally.  |
//! | `pack`      | Concentrate all auto-nodes on the single most-capable host.  |
//! | `spread`    | At most one auto-node per host (maximises distribution).      |
//! | `random`    | Each auto-node is assigned to a uniformly random host.        |
//! | `weighted`  | User-supplied per-host capacity weights (index = host id).   |
//!
//! All policies accept an optional `per_host_limit` field that caps the number
//! of auto-placed sandboxes on any single host.
//!
//! # Priority
//!
//! Live capacity hints from the coordinator always override the embedded
//! policy.  The policy applies only when no live hints are available.

use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::placer::PlacementHints;

// ── Public types ──────────────────────────────────────────────────────────────

/// Placement policy embedded in a `SymbolicDag` JSON file.
///
/// Accepts either a plain string (`"balanced"`, `"pack"`, `"spread"`) or
/// a full config object for parameterised policies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PlacementPolicy {
    /// Simple string shorthand: `"balanced"`, `"pack"`, or `"spread"`.
    Named(NamedPolicy),
    /// Parameterised config object: `{ "type": "...", ... }`.
    Config(PolicyConfig),
}

/// The string-shorthand policies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedPolicy {
    /// Equal weight per host, distributes nodes proportionally to host count.
    Balanced,
    /// Concentrate all auto-nodes on the single most-capable host.
    Pack,
    /// At most one auto-node per host (round-robin / maximise spread).
    Spread,
    /// Assign each auto-node to a uniformly random host (ignores dep-affinity).
    Random,
}

/// Full policy config for parameterised placement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    #[serde(rename = "type")]
    pub kind: PolicyKind,

    /// Per-host capacity weights (host 0 first).  Used only by `weighted`.
    /// Missing trailing hosts get weight 0.  Normalised internally.
    #[serde(default)]
    pub weights: Vec<f64>,

    /// Optional hard cap on auto-placed sandboxes per host.
    /// Applies uniformly across all hosts.  Absent means no cap.
    #[serde(default)]
    pub per_host_limit: Option<usize>,
}

/// Policy kind tag for the config object variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyKind {
    /// Equal capacity, proportional distribution (same as `"balanced"`).
    Balanced,
    /// Single-host packing (same as `"pack"`).
    Pack,
    /// One node per host (same as `"spread"`).
    Spread,
    /// Proportional to explicit per-host weights.
    Weighted,
}

// ── Resolution ────────────────────────────────────────────────────────────────

/// Default hints used when a DAG specifies no `placement_policy` and no raw
/// `hints`, and the coordinator provides no live capacity data.
///
/// **Current default: `pack`** — colocate all auto-placed nodes on the single
/// most-capable host to minimise cross-machine edges and RDMA traffic.
/// Change this function to switch the cluster-wide default policy.
pub fn default_hints(total_nodes: usize) -> PlacementHints {
    named_hints(&NamedPolicy::Pack, total_nodes, None)
}

/// Convert a `PlacementPolicy` into concrete `PlacementHints` the placer can use.
///
/// `total_nodes` comes from `SymbolicDag::total_nodes`.
pub fn resolve_policy(policy: &PlacementPolicy, total_nodes: usize) -> PlacementHints {
    match policy {
        PlacementPolicy::Named(name) => named_hints(name, total_nodes, None),
        PlacementPolicy::Config(cfg) => config_hints(cfg, total_nodes),
    }
}

// ── Policy implementations ────────────────────────────────────────────────────

fn config_hints(cfg: &PolicyConfig, total_nodes: usize) -> PlacementHints {
    match cfg.kind {
        PolicyKind::Balanced => named_hints(&NamedPolicy::Balanced, total_nodes, cfg.per_host_limit),
        PolicyKind::Pack     => named_hints(&NamedPolicy::Pack,     total_nodes, cfg.per_host_limit),
        PolicyKind::Spread   => named_hints(&NamedPolicy::Spread,   total_nodes, cfg.per_host_limit),
        PolicyKind::Weighted => weighted_hints(&cfg.weights, total_nodes, cfg.per_host_limit),
    }
}

fn named_hints(policy: &NamedPolicy, total_nodes: usize, per_host_limit: Option<usize>) -> PlacementHints {
    let host_limit = per_host_limit
        .map(|lim| (0..total_nodes as u32).map(|i| (i, lim)).collect())
        .unwrap_or_default();

    match policy {
        NamedPolicy::Balanced => {
            // Equal capacity share → proportional allocation.
            let w = 1.0 / total_nodes as f64;
            PlacementHints {
                capacity: (0..total_nodes as u32).map(|i| (i, w)).collect(),
                host_limit,
                random: false,
                ..Default::default()
            }
        }

        NamedPolicy::Pack => {
            // Give host 0 a capacity share just above the uniform threshold so
            // the placer's single-host packing path fires (best_cap > 1/N).
            let uniform = 1.0 / total_nodes as f64;
            let boost = (uniform + 0.05_f64).min(1.0);
            let rest = if total_nodes > 1 {
                (1.0 - boost) / (total_nodes - 1) as f64
            } else {
                0.0
            };
            PlacementHints {
                capacity: (0..total_nodes as u32)
                    .map(|i| (i, if i == 0 { boost } else { rest }))
                    .collect(),
                host_limit,
                random: false,
                ..Default::default()
            }
        }

        NamedPolicy::Spread => {
            // Empty capacity (uniform round-robin) + limit of 1 per host.
            // If caller already supplied per_host_limit, honour it; otherwise force 1.
            let spread_limit = per_host_limit.unwrap_or(1);
            PlacementHints {
                capacity: HashMap::new(),
                host_limit: (0..total_nodes as u32).map(|i| (i, spread_limit)).collect(),
                random: false,
                ..Default::default()
            }
        }

        NamedPolicy::Random => PlacementHints {
            capacity: HashMap::new(),
            host_limit: host_limit,
            random: true,
            ..Default::default()
        },
    }
}

/// Proportional allocation using caller-supplied per-host weights.
fn weighted_hints(
    weights: &[f64],
    total_nodes: usize,
    per_host_limit: Option<usize>,
) -> PlacementHints {
    let sum: f64 = weights.iter().copied().sum::<f64>().max(f64::EPSILON);
    let capacity = (0..total_nodes as u32)
        .map(|i| {
            let w = weights.get(i as usize).copied().unwrap_or(0.0) / sum;
            (i, w)
        })
        .collect();
    let host_limit = per_host_limit
        .map(|lim| (0..total_nodes as u32).map(|i| (i, lim)).collect())
        .unwrap_or_default();
    PlacementHints { capacity, host_limit, random: false, ..Default::default() }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placer::assign_nodes;
    use crate::symbolic_dag::SymbolicNode;
    use serde_json::json;

    fn make_auto(id: &str) -> SymbolicNode {
        SymbolicNode {
            id: id.to_string(), deps: vec![], node_id: None, output_slot: None,
            fanout: None, placement: None, barrier_group: None, split: None, out_weight: None,
            kind: json!({"Func": {"slot": 0}}),
        }
    }

    fn place(nodes: &mut Vec<SymbolicNode>, policy: &PlacementPolicy, n: usize) {
        let hints = resolve_policy(policy, n);
        assign_nodes(nodes, n, &hints, None);
    }

    #[test]
    fn balanced_spreads_across_both_hosts() {
        let mut nodes: Vec<_> = (0..4).map(|i| make_auto(&format!("n{}", i))).collect();
        place(&mut nodes, &PlacementPolicy::Named(NamedPolicy::Balanced), 2);
        let on0 = nodes.iter().filter(|n| n.node_id == Some(0)).count();
        let on1 = nodes.iter().filter(|n| n.node_id == Some(1)).count();
        assert_eq!(on0 + on1, 4);
        assert!(on0 >= 1 && on1 >= 1, "balanced must use both hosts: {}/{}", on0, on1);
    }

    #[test]
    fn pack_places_all_on_one_host() {
        let mut nodes: Vec<_> = (0..3).map(|i| make_auto(&format!("n{}", i))).collect();
        place(&mut nodes, &PlacementPolicy::Named(NamedPolicy::Pack), 2);
        let on0 = nodes.iter().filter(|n| n.node_id == Some(0)).count();
        assert_eq!(on0, 3, "pack should put all 3 on host 0, got {}", on0);
    }

    #[test]
    fn spread_limits_one_per_host() {
        let mut nodes: Vec<_> = (0..2).map(|i| make_auto(&format!("n{}", i))).collect();
        place(&mut nodes, &PlacementPolicy::Named(NamedPolicy::Spread), 2);
        let on0 = nodes.iter().filter(|n| n.node_id == Some(0)).count();
        let on1 = nodes.iter().filter(|n| n.node_id == Some(1)).count();
        assert!(on0 <= 1 && on1 <= 1, "spread must not exceed 1 per host: {}/{}", on0, on1);
    }

    #[test]
    fn weighted_honours_capacity_ratio() {
        let mut nodes: Vec<_> = (0..10).map(|i| make_auto(&format!("n{}", i))).collect();
        let policy = PlacementPolicy::Config(PolicyConfig {
            kind: PolicyKind::Weighted,
            weights: vec![0.8, 0.2],
            per_host_limit: None,
        });
        place(&mut nodes, &policy, 2);
        let on0 = nodes.iter().filter(|n| n.node_id == Some(0)).count();
        let on1 = nodes.iter().filter(|n| n.node_id == Some(1)).count();
        // 80/20 split → host 0 should get significantly more
        assert!(on0 > on1, "weighted 80/20: expected host 0 majority, got {}/{}", on0, on1);
    }

    #[test]
    fn random_uses_all_hosts() {
        // With 20 auto-nodes and 4 hosts, every host should appear at least once.
        let mut nodes: Vec<_> = (0..20).map(|i| make_auto(&format!("n{}", i))).collect();
        place(&mut nodes, &PlacementPolicy::Named(NamedPolicy::Random), 4);
        for host in 0..4u32 {
            let count = nodes.iter().filter(|n| n.node_id == Some(host)).count();
            assert!(count >= 1, "random: host {} got no nodes (very unlikely)", host);
        }
    }

    #[test]
    fn random_ignores_dep_affinity() {
        // A chain a→b→c assigned randomly should still get valid host ids.
        let mut nodes = vec![make_auto("a"), make_auto("b"), make_auto("c")];
        nodes[1].deps = vec!["a".into()];
        nodes[2].deps = vec!["b".into()];
        place(&mut nodes, &PlacementPolicy::Named(NamedPolicy::Random), 3);
        for n in &nodes {
            assert!(n.node_id.unwrap() < 3, "node_id out of range");
        }
    }

    #[test]
    fn policy_roundtrips_json() {
        let cases = [
            r#""balanced""#,
            r#""pack""#,
            r#""spread""#,
            r#""random""#,
            r#"{"type":"weighted","weights":[0.7,0.3]}"#,
            r#"{"type":"balanced","per_host_limit":4}"#,
        ];
        for s in &cases {
            let p: PlacementPolicy = serde_json::from_str(s)
                .unwrap_or_else(|e| panic!("failed to parse {}: {}", s, e));
            let _ = serde_json::to_string(&p).expect("serialize");
        }
    }
}
