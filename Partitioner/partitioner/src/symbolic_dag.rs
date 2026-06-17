use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::placer::PlacementHints;
use crate::policies::PlacementPolicy;

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
    /// Number of machines to partition across.  **Optional** — when a job is
    /// submitted, the coordinator fills this in from the live cluster size, so a
    /// SymbolicDag normally omits it and runs on however many nodes are online.
    /// An explicit value acts as an upper bound (the coordinator uses
    /// `min(total_nodes, live_nodes)`).  The standalone `partition` CLI defaults
    /// it via `--nodes N` (or 2) since it has no live cluster to query.
    #[serde(default)]
    pub total_nodes: Option<usize>,
    /// Optional hard cap on sandboxes per host.  `None` (default) means the
    /// placer uses the auto-derived limit from CPU cores and busy %.
    /// Set to `Some(n)` to override and cap at n regardless of core count.
    #[serde(default)]
    pub max_colocation: Option<usize>,
    #[serde(default)]
    pub shared_inputs: Vec<SharedInput>,
    /// Named placement policy embedded in the DAG file.
    /// Converted to `PlacementHints` when no live hints are available.
    /// Takes precedence over the raw `hints` field.
    #[serde(default)]
    pub placement_policy: Option<PlacementPolicy>,
    /// Raw placement hints (legacy).  Use `placement_policy` instead.
    /// Overridden by live hints from the coordinator or by `placement_policy`.
    #[serde(default)]
    pub hints: Option<PlacementHints>,
    /// Override `PARTITIONER_CONVERGE_ON_COORDINATOR` for this DAG: pin the
    /// convergence tail (final aggregate / reduce / Output) onto node 0 so the
    /// result co-locates with the input.  `None` → use the compile-time default.
    #[serde(default)]
    pub converge_on_coordinator: Option<bool>,
    /// Handshake protocol for the cross-node `RemoteSend`/`RemoteRecv` pairs this
    /// DAG generates: `"sender_init"` (default — omitted, preserves existing
    /// behaviour) or `"receiver_init"`. RI ("receiver announces first") avoids the
    /// SI rendezvous stall when a node gathers many transfers from many peers
    /// (e.g. finra's fan-in hub). `None` ⇒ emit no `protocol` field ⇒ executor
    /// default (`SenderInit`), so non-finra DAGs are byte-for-byte unchanged.
    #[serde(default)]
    pub remote_protocol: Option<String>,
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
    /// Breakability hint for this node's OUTPUT edge(s) when the cluster is
    /// partitioned across machines — the friendly way to steer where the cut
    /// lands.  Each node controls its own outgoing edge:
    ///   - `"avoid"`  → don't break here; keep the consumer on this node's host.
    ///   - `"prefer"` → a safe/cheap place to break; cuts gravitate here.
    ///   - omitted    → neutral.
    /// E.g. in a chain `A→B→C`, `B: "prefer"` makes the cut fall between B and
    /// C, while `A: "avoid"` keeps A→B co-located.  This is a strong preference,
    /// not a hard guarantee (it yields if a host runs out of capacity); for a
    /// hard guarantee pin both nodes to the same `node_id`.  Drives the placer's
    /// data-weighted dep-affinity (see `placer::edge_weight`).
    #[serde(default)]
    pub split: Option<SplitHint>,
    /// Advanced numeric override for the locality weight, used ONLY when `split`
    /// is absent.  Relative, unitless "data emitted on my output edge(s)";
    /// higher → the consumer is pulled harder onto this node's host, so cuts
    /// fall on lighter edges.  Absent → `1.0` (the original count-based
    /// dep-affinity).  Prefer `split` unless you need a specific ratio.
    #[serde(default)]
    pub out_weight: Option<f64>,
    pub kind: serde_json::Value,
}

/// Breakability intent for a node's output edge(s) — see `SymbolicNode::split`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitHint {
    /// Avoid breaking this node's output edge(s): keep the consumer local.
    Avoid,
    /// Prefer breaking here: this node's output is a safe cut point.
    Prefer,
}

impl SymbolicDag {
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| anyhow::anyhow!("parse SymbolicDag: {}", e))
    }
}
