//! ClusterDag: a single JSON file describing the entire distributed workflow.
//!
//! The coordinator splits it into per-node `Dag` JSONs (compatible with the
//! Executor's `run_dag_file()`) by injecting RDMA config and node-specific
//! SHM paths.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A distributed DAG definition that spans multiple machines.
///
/// The `node_dags` map assigns each machine (by node_id) its own list of DAG
/// nodes.  The coordinator generates per-node Dag JSONs from this, filling in
/// RDMA config from the live cluster membership.
#[derive(Debug, Deserialize)]
pub struct ClusterDag {
    /// SHM path prefix.  Each node gets `"{prefix}_n{node_id}"`.
    pub shm_path_prefix: String,

    /// Optional path to the WASM module.
    #[serde(default)]
    pub wasm_path: Option<String>,

    /// Path to the Python runner script.
    #[serde(default)]
    pub python_script: Option<String>,

    /// Path to the Python WASM binary.
    #[serde(default)]
    pub python_wasm: Option<String>,

    /// Log level: "debug", "info", "warn", "error", "off".
    #[serde(default)]
    pub log_level: Option<String>,

    /// Execution mode: "one_shot" (default) or "reset".
    #[serde(default)]
    pub mode: Option<String>,

    /// Number of runs in reset mode.
    #[serde(default)]
    pub runs: Option<u32>,

    /// Enable RDMA transfer (default: true).
    #[serde(default = "default_true")]
    pub transfer: bool,

    /// Files that the coordinator should stage to workers before execution.
    /// Produced by the Partitioner from a SymbolicDag's shared_inputs field.
    #[serde(default)]
    pub shared_inputs: Vec<SharedInput>,

    /// Per-node DAG node lists.  Key is the node_id (as string in JSON).
    /// Each entry becomes the `nodes` array in that node's per-node Dag.
    pub node_dags: HashMap<String, Vec<serde_json::Value>>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedInput {
    pub path: String,
    pub source_node: u32,
}

/// A per-node Dag JSON that the Executor understands.
#[derive(Debug, Serialize)]
pub struct PerNodeDag {
    pub shm_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wasm_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub python_script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub python_wasm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runs: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rdma: Option<RdmaSection>,
    pub nodes: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct RdmaSection {
    pub node_id: usize,
    pub total: usize,
    pub ips: Vec<String>,
    pub transfer: bool,
}

impl ClusterDag {
    /// Parse a ClusterDag from a JSON string.
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("parse ClusterDag JSON")
    }

    /// Split this ClusterDag into per-node Dag JSONs.
    ///
    /// `ips` is the live cluster IP list (from agent config).
    /// Returns a map: node_id -> serialized Dag JSON string.
    pub fn split(&self, ips: &[String]) -> Result<HashMap<u32, String>> {
        let mut result = HashMap::new();

        // The RDMA mesh must span exactly the nodes that are part of THIS job —
        // i.e. the node ids present in `node_dags` — not the full cluster roster.
        // Otherwise, when the job is partitioned across fewer nodes than the
        // configured `ips` list (e.g. a 4-node roster running a 2-node job), the
        // full RC QP mesh would wait forever for the absent nodes to join.
        // node ids are a contiguous prefix 0..job_nodes, so ips[0..job_nodes] are
        // the right addresses.
        let job_nodes = self
            .node_dags
            .keys()
            .filter_map(|k| k.parse::<u32>().ok())
            .max()
            .map(|m| m as usize + 1)
            .unwrap_or(0);
        if job_nodes > ips.len() {
            bail!("job spans {} nodes but only {} cluster IPs are configured", job_nodes, ips.len());
        }
        let total = job_nodes;
        let mesh_ips: Vec<String> = ips[..total].to_vec();

        // Determine if RDMA is needed: check if any node_dag has RemoteSend/RemoteRecv nodes.
        let needs_rdma = self.has_remote_nodes();

        for (node_id_str, nodes) in &self.node_dags {
            let node_id: u32 = node_id_str.parse()
                .with_context(|| format!("invalid node_id key: {}", node_id_str))?;

            if node_id as usize >= total {
                bail!(
                    "node_id {} in ClusterDag exceeds job size {}",
                    node_id, total
                );
            }

            let rdma = if needs_rdma {
                Some(RdmaSection {
                    node_id: node_id as usize,
                    total,
                    ips: mesh_ips.clone(),
                    transfer: self.transfer,
                })
            } else {
                None
            };

            let dag = PerNodeDag {
                shm_path: format!("{}_n{}", self.shm_path_prefix, node_id),
                wasm_path: self.wasm_path.clone(),
                python_script: self.python_script.clone(),
                python_wasm: self.python_wasm.clone(),
                log_level: self.log_level.clone(),
                mode: self.mode.clone(),
                runs: self.runs,
                rdma,
                nodes: nodes.clone(),
            };

            let json = serde_json::to_string_pretty(&dag)
                .with_context(|| format!("serialize per-node DAG for node {}", node_id))?;
            result.insert(node_id, json);
        }

        Ok(result)
    }

    /// Check if any node's DAG needs the RDMA mesh — either an explicit
    /// RemoteSend/RemoteRecv/RemoteAtomic node, OR a node with EMBEDDED RDMA
    /// (a `StreamPipeline`/`StreamOutput` carrying `rdma_send`/`rdma_recv`, as
    /// produced by Mode-2 return and the Phase-2 auto-split).  Without the latter
    /// the cross-node streaming nodes would run with `dag.rdma` unset and fail
    /// ("requires dag.rdma to be configured").
    fn has_remote_nodes(&self) -> bool {
        for nodes in self.node_dags.values() {
            for node in nodes {
                if let Some(kind) = node.get("kind") {
                    let kind_str = kind.to_string();
                    if kind_str.contains("RemoteSend") || kind_str.contains("RemoteRecv")
                        || kind_str.contains("RemoteAtomicFetchAdd")
                        || kind_str.contains("RemoteAtomicCmpSwap")
                        || kind_str.contains("RemoteAtomicPush")
                        || kind_str.contains("rdma_send") || kind_str.contains("rdma_recv")
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// For each (logical) node id, the set of input-file paths its DAG actually
    /// loads (the `path` of every `Input` node). The coordinator uses this to
    /// stage each shared-input file only to the workers that read it, instead of
    /// broadcasting every file to every node.
    pub fn input_paths_by_node(&self) -> HashMap<u32, std::collections::HashSet<String>> {
        let mut map: HashMap<u32, std::collections::HashSet<String>> = HashMap::new();
        for (node_id_str, nodes) in &self.node_dags {
            let node_id: u32 = match node_id_str.parse() { Ok(v) => v, Err(_) => continue };
            let set = map.entry(node_id).or_default();
            for node in nodes {
                if let Some(path) = node
                    .get("kind")
                    .and_then(|k| k.get("Input"))
                    .and_then(|i| i.get("path"))
                    .and_then(|p| p.as_str())
                {
                    set.insert(path.to_string());
                }
            }
        }
        map
    }

    /// For each (logical) node id, the per-file data-parallel `slice` ([lo,hi]
    /// fractions) its `Input` nodes declare — defaulting to the whole file [0,1]
    /// when absent. The coordinator uses this to RDMA-stage each worker ONLY its
    /// line-aligned slice window (not the whole file), then rewrites the slice to
    /// [0,1] so the executor loads the whole (already-sliced) staged file.
    pub fn input_slices_by_node(&self) -> HashMap<u32, HashMap<String, [f64; 2]>> {
        let mut map: HashMap<u32, HashMap<String, [f64; 2]>> = HashMap::new();
        for (node_id_str, nodes) in &self.node_dags {
            let node_id: u32 = match node_id_str.parse() { Ok(v) => v, Err(_) => continue };
            let m = map.entry(node_id).or_default();
            for node in nodes {
                if let Some(inp) = node.get("kind").and_then(|k| k.get("Input")) {
                    if let Some(path) = inp.get("path").and_then(|p| p.as_str()) {
                        // binary/chunk_bytes inputs ignore slice (load_slice is skipped) → keep whole.
                        let binary = inp.get("binary").and_then(|b| b.as_bool()).unwrap_or(false);
                        let has_chunk = inp.get("chunk_bytes").map(|c| !c.is_null()).unwrap_or(false);
                        let slice = inp.get("slice").and_then(|s| s.as_array()).and_then(|a| {
                            if a.len() == 2 {
                                Some([a[0].as_f64().unwrap_or(0.0), a[1].as_f64().unwrap_or(1.0)])
                            } else { None }
                        });
                        let win = if binary || has_chunk { [0.0, 1.0] } else { slice.unwrap_or([0.0, 1.0]) };
                        m.insert(path.to_string(), win);
                    }
                }
            }
        }
        map
    }
}
