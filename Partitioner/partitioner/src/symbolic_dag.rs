use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::placer::PlacementHints;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolicDag {
    pub shm_path_prefix: String,
    #[serde(default)]
    pub wasm_path: Option<String>,
    #[serde(default)]
    pub python_script: Option<String>,
    #[serde(default)]
    pub python_wasm: Option<String>,
    #[serde(default)]
    pub log_level: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub runs: Option<u32>,
    #[serde(default = "default_true")]
    pub transfer: bool,
    pub total_nodes: usize,
    /// Optional hard cap on sandboxes per host.  `None` (default) means the
    /// placer uses the auto-derived limit from CPU cores and busy %.
    /// Set to `Some(n)` to override and cap at n regardless of core count.
    #[serde(default)]
    pub max_colocation: Option<usize>,
    #[serde(default)]
    pub shared_inputs: Vec<SharedInput>,
    /// Optional placement hints embedded in the DAG file itself.
    /// Overridden by hints passed explicitly to `partition()`.
    #[serde(default)]
    pub hints: Option<PlacementHints>,
    pub nodes: Vec<SymbolicNode>,
}

fn default_true() -> bool {
    true
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedInput {
    pub path: String,
    pub source_node: u32,
}

/// A node in a SymbolicDag.
///
/// `deps` reference other node `id`s, including across machines.
/// `kind` is passed through as-is, except that `upstream_nodes: [<id>, ...]` inside
/// routing nodes (Aggregate / Bridge / Shuffle) is rewritten by the Partitioner to
/// the concrete `upstream: [<slot>, ...]` form after RecvSlot assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolicNode {
    pub id: String,
    #[serde(default)]
    pub deps: Vec<String>,
    /// Target host. `None` means the placer auto-assigns based on cluster capacity.
    /// Explicit `Some(id)` pins this node to that host (existing behaviour).
    #[serde(default)]
    pub node_id: Option<u32>,
    /// Required on Func/WasmVoid nodes that have cross-node consumers,
    /// since the Partitioner cannot auto-detect the output slot for those kinds.
    #[serde(default)]
    pub output_slot: Option<u32>,
    /// Expand this node into N identical copies named `{id}_0`, `{id}_1`, …
    /// Any other node that lists this id in its `deps` will have it replaced
    /// by all N expanded ids automatically.
    #[serde(default)]
    pub fanout: Option<usize>,
    /// Place one copy of this node on every machine in the cluster.
    /// Each copy is named `{id}_{machine_id}` and pinned to that machine.
    /// Dependencies on other `"all"` nodes resolve to the same-machine sibling;
    /// non-"all" node dependencies on an `"all"` template expand to all copies.
    /// `Input` nodes with this placement auto-populate `shared_inputs`.
    #[serde(default)]
    pub placement: Option<String>,
    #[serde(default)]
    pub barrier_group: Option<String>,
    pub kind: serde_json::Value,
}

impl SymbolicDag {
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| anyhow::anyhow!("parse SymbolicDag: {}", e))
    }
}
