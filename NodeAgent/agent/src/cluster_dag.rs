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

    /// Per-node DAG node lists.  Key is the node_id (as string in JSON).
    /// Each entry becomes the `nodes` array in that node's per-node Dag.
    pub node_dags: HashMap<String, Vec<serde_json::Value>>,
}

fn default_true() -> bool { true }

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
        let total = ips.len();
        let mut result = HashMap::new();

        // Determine if RDMA is needed: check if any node_dag has RemoteSend/RemoteRecv nodes.
        let needs_rdma = self.has_remote_nodes();

        for (node_id_str, nodes) in &self.node_dags {
            let node_id: u32 = node_id_str.parse()
                .with_context(|| format!("invalid node_id key: {}", node_id_str))?;

            if node_id as usize >= total {
                bail!(
                    "node_id {} in ClusterDag exceeds cluster size {}",
                    node_id, total
                );
            }

            let rdma = if needs_rdma {
                Some(RdmaSection {
                    node_id: node_id as usize,
                    total,
                    ips: ips.to_vec(),
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

    /// Check if any node's DAG contains RemoteSend or RemoteRecv nodes.
    fn has_remote_nodes(&self) -> bool {
        for nodes in self.node_dags.values() {
            for node in nodes {
                if let Some(kind) = node.get("kind") {
                    let kind_str = kind.to_string();
                    if kind_str.contains("RemoteSend") || kind_str.contains("RemoteRecv")
                        || kind_str.contains("RemoteAtomicFetchAdd")
                        || kind_str.contains("RemoteAtomicCmpSwap")
                        || kind_str.contains("RemoteAtomicPush")
                    {
                        return true;
                    }
                }
            }
        }
        false
    }
}
