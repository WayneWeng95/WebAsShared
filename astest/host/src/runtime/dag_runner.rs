//! DAG-based execution engine for stream routing scenarios.
//!
//! Reads a JSON DAG description, topologically sorts the nodes (Kahn's algorithm),
//! then runs each node in order against a single shared WASM instance backed by
//! a freshly-formatted SHM region.
//!
//! # JSON format
//! ```json
//! {
//!   "shm_path": "/dev/shm/my_dag_shm",
//!   "nodes": [
//!     { "id": "p0", "deps": [],            "kind": { "WasmVoid":   { "func": "produce_stream",      "arg": 0 } } },
//!     { "id": "p1", "deps": [],            "kind": { "WasmVoid":   { "func": "produce_stream",      "arg": 1 } } },
//!     { "id": "route", "deps": ["p0","p1"],"kind": { "Shuffle":    { "upstream":[0,1], "downstream":[2,3], "policy":{"type":"Modulo"} } } },
//!     { "id": "r2", "deps": ["route"],     "kind": { "WasmFatPtr": { "func": "dump_stream_records", "arg": 2 } } },
//!     { "id": "r3", "deps": ["route"],     "kind": { "WasmFatPtr": { "func": "dump_stream_records", "arg": 3 } } }
//!   ]
//! }
//! ```
//!
//! ## Node kinds
//! - `WasmVoid`   — calls `func(arg: u32) -> ()`
//! - `WasmU32`    — calls `func(arg: u32) -> u32`, prints the result
//! - `WasmFatPtr` — calls `func(arg: u32) -> u64`, decodes fat-pointer and prints all records
//! - `Bridge`     — `HostStream::bridge(from, to)` (zero-copy 1→1 wire)
//! - `Aggregate`  — `AggregateConnection::new(upstream, downstream).bridge()`
//! - `Shuffle`    — `ShuffleConnection::new(upstream, downstream, policy).bridge()`
//!
//! ## Shuffle policies
//! ```json
//! { "type": "Modulo" }
//! { "type": "RoundRobin" }
//! { "type": "FixedMap", "map": [[0,1],[1,0]], "default_slot": 0 }
//! ```

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use wasmtime::*;

use crate::policy::{FixedMapPartition, ModuloPartition, RoundRobinPartition};
use crate::routing::shuffle::{AggregateConnection, ShuffleConnection};
use crate::routing::stream::HostStream;
use crate::runtime::worker::{create_wasmtime_engine, setup_vma_environment, WorkerState};
use crate::shm::format_shared_memory;

const WASM_PATH: &str = "../target/wasm32-unknown-unknown/release/guest.wasm";

// ─── JSON schema ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct Dag {
    /// Path to the SHM file; created and formatted automatically.
    pub shm_path: String,
    pub nodes: Vec<DagNode>,
}

#[derive(Debug, Deserialize)]
pub struct DagNode {
    /// Unique node identifier used in `deps` references.
    pub id: String,
    /// IDs of nodes that must complete before this one runs.
    #[serde(default)]
    pub deps: Vec<String>,
    pub kind: NodeKind,
}

#[derive(Debug, Deserialize)]
pub enum NodeKind {
    /// Call `func(arg) -> ()` — fire-and-forget WASM invocation.
    WasmVoid(WasmCall),
    /// Call `func(arg) -> u32` — result is logged.
    WasmU32(WasmCall),
    /// Call `func(arg) -> u64` — fat-pointer result decoded and printed.
    WasmFatPtr(WasmCall),
    /// `HostStream::bridge(from, to)` — zero-copy 1→1 chain redirect.
    Bridge(BridgeParams),
    /// `AggregateConnection::new(upstream, downstream).bridge()` — N→1 merge.
    Aggregate(AggregateParams),
    /// `ShuffleConnection::new(upstream, downstream, policy).bridge()` — N→M routing.
    Shuffle(ShuffleParams),
}

#[derive(Debug, Deserialize)]
pub struct WasmCall {
    pub func: String,
    pub arg: u32,
}

#[derive(Debug, Deserialize)]
pub struct BridgeParams {
    pub from: usize,
    pub to: usize,
}

#[derive(Debug, Deserialize)]
pub struct AggregateParams {
    pub upstream: Vec<usize>,
    pub downstream: usize,
}

#[derive(Debug, Deserialize)]
pub struct ShuffleParams {
    pub upstream: Vec<usize>,
    pub downstream: Vec<usize>,
    pub policy: PolicySpec,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum PolicySpec {
    Modulo,
    RoundRobin,
    FixedMap {
        /// Each inner array is `[upstream_id, slot_index]`.
        map: Vec<[usize; 2]>,
        #[serde(default)]
        default_slot: usize,
    },
}

// ─── Topological sort (Kahn's algorithm) ─────────────────────────────────────

fn topo_sort(nodes: &[DagNode]) -> Result<Vec<usize>> {
    let id_to_idx: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();

    let mut in_degree = vec![0usize; nodes.len()];
    // adj[i] = list of nodes that depend on node i (i must finish before them)
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];

    for (i, node) in nodes.iter().enumerate() {
        for dep_id in &node.deps {
            let dep_idx = *id_to_idx
                .get(dep_id.as_str())
                .ok_or_else(|| anyhow!("Unknown dep '{}' in node '{}'", dep_id, node.id))?;
            adj[dep_idx].push(i);
            in_degree[i] += 1;
        }
    }

    let mut queue: VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter(|(_, &d)| d == 0)
        .map(|(i, _)| i)
        .collect();

    let mut order = Vec::with_capacity(nodes.len());
    while let Some(n) = queue.pop_front() {
        order.push(n);
        for &m in &adj[n] {
            in_degree[m] -= 1;
            if in_degree[m] == 0 {
                queue.push_back(m);
            }
        }
    }

    if order.len() != nodes.len() {
        return Err(anyhow!("DAG contains a cycle — cannot execute"));
    }
    Ok(order)
}

// ─── Public entry points ──────────────────────────────────────────────────────

/// Load a DAG from a JSON **file** and execute it.
pub fn run_dag_file(json_path: &str) -> Result<()> {
    let json = std::fs::read_to_string(json_path)
        .map_err(|e| anyhow!("Cannot read DAG file '{}': {}", json_path, e))?;
    run_dag_json(&json)
}

/// Load a DAG from a JSON **string** and execute it.
pub fn run_dag_json(json: &str) -> Result<()> {
    let dag: Dag = serde_json::from_str(json)
        .map_err(|e| anyhow!("Invalid DAG JSON: {}", e))?;
    run_dag(&dag)
}

/// Execute a pre-parsed [`Dag`].
pub fn run_dag(dag: &Dag) -> Result<()> {
    println!("[DAG] Starting — shm: {}", dag.shm_path);

    // Format a fresh SHM region so prior data never leaks between runs.
    format_shared_memory(&dag.shm_path)?;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&dag.shm_path)?;

    let engine = create_wasmtime_engine()?;
    let mut store = Store::new(
        &engine,
        WorkerState { file: file.try_clone()?, splice_addr: 0 },
    );
    let mut linker = Linker::new(&engine);
    let memory = setup_vma_environment(&mut store, &mut linker, &file)?;

    let module = Module::from_file(&engine, WASM_PATH)?;
    let instance = linker.instantiate(&mut store, &module)?;

    // Topological order
    let order = topo_sort(&dag.nodes)?;
    let node_ids: Vec<&str> = order.iter().map(|&i| dag.nodes[i].id.as_str()).collect();
    println!("[DAG] Execution order: {}", node_ids.join(" → "));

    // Run each node
    for idx in order {
        let node = &dag.nodes[idx];
        println!("[DAG] ── Node: {} ──", node.id);
        execute_node(node, &mut store, &instance, &memory)?;
    }

    println!("[DAG] All nodes completed.");
    Ok(())
}

// ─── Node executor ────────────────────────────────────────────────────────────

fn execute_node(
    node: &DagNode,
    store: &mut Store<WorkerState>,
    instance: &Instance,
    memory: &Memory,
) -> Result<()> {
    let splice_addr = store.data().splice_addr;
    let base_ptr = memory.data_ptr(&*store);

    match &node.kind {
        // ── WASM: void return ────────────────────────────────────────────────
        NodeKind::WasmVoid(call) => {
            let func = instance.get_typed_func::<u32, ()>(&mut *store, &call.func)
                .map_err(|e| anyhow!("[{}] no WASM export '{}': {}", node.id, call.func, e))?;
            func.call(&mut *store, call.arg)?;
            println!("  {}({}) → ()", call.func, call.arg);
        }

        // ── WASM: u32 return ─────────────────────────────────────────────────
        NodeKind::WasmU32(call) => {
            let func = instance.get_typed_func::<u32, u32>(&mut *store, &call.func)
                .map_err(|e| anyhow!("[{}] no WASM export '{}': {}", node.id, call.func, e))?;
            let result = func.call(&mut *store, call.arg)?;
            println!("  {}({}) → {}", call.func, call.arg, result);
        }

        // ── WASM: fat-pointer return — prints all records ─────────────────────
        NodeKind::WasmFatPtr(call) => {
            let func = instance.get_typed_func::<u32, u64>(&mut *store, &call.func)
                .map_err(|e| anyhow!("[{}] no WASM export '{}': {}", node.id, call.func, e))?;
            let packed = func.call(&mut *store, call.arg)?;
            if packed > 0 {
                let ptr = (packed >> 32) as usize;
                let len = (packed & 0xFFFF_FFFF) as usize;
                let raw = unsafe { std::slice::from_raw_parts(base_ptr.add(ptr), len) };
                let text = String::from_utf8_lossy(raw);
                let lines: Vec<&str> = text.lines().collect();
                println!("  {}({}) → {} records:", call.func, call.arg, lines.len());
                // Print first 5 and last 5 to keep output readable for large dumps.
                let show_all = lines.len() <= 10;
                for (i, line) in lines.iter().enumerate() {
                    if show_all || i < 5 || i >= lines.len().saturating_sub(5) {
                        println!("    [{:4}] {}", i, line);
                    } else if i == 5 {
                        println!("    ... {} records omitted ...", lines.len() - 10);
                    }
                }
            } else {
                println!("  {}({}) → (empty)", call.func, call.arg);
            }
        }

        // ── Host routing: HostStream 1→1 bridge ──────────────────────────────
        NodeKind::Bridge(p) => {
            let ok = HostStream::new(splice_addr).bridge(p.from, p.to);
            println!(
                "  HostStream::bridge({} → {}): {}",
                p.from, p.to,
                if ok { "OK" } else { "FAIL (source slot empty)" }
            );
        }

        // ── Host routing: AggregateConnection N→1 ────────────────────────────
        NodeKind::Aggregate(p) => {
            AggregateConnection::new(&p.upstream, p.downstream).bridge(splice_addr);
            println!("  AggregateConnection({:?} → {}): done", p.upstream, p.downstream);
        }

        // ── Host routing: ShuffleConnection N→M ──────────────────────────────
        NodeKind::Shuffle(p) => {
            let policy_name = match &p.policy {
                PolicySpec::Modulo => {
                    ShuffleConnection::new(&p.upstream, &p.downstream, ModuloPartition)
                        .bridge(splice_addr);
                    "Modulo"
                }
                PolicySpec::RoundRobin => {
                    ShuffleConnection::new(&p.upstream, &p.downstream, RoundRobinPartition::new())
                        .bridge(splice_addr);
                    "RoundRobin"
                }
                PolicySpec::FixedMap { map, default_slot } => {
                    let hmap: HashMap<usize, usize> =
                        map.iter().map(|&[k, v]| (k, v)).collect();
                    ShuffleConnection::new(
                        &p.upstream,
                        &p.downstream,
                        FixedMapPartition::new(hmap, *default_slot),
                    ).bridge(splice_addr);
                    "FixedMap"
                }
            };
            println!(
                "  ShuffleConnection({:?} → {:?}, {}): done",
                p.upstream, p.downstream, policy_name
            );
        }
    }

    Ok(())
}
