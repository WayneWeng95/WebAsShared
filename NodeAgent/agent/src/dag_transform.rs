//! DAG transformation: converts unified node kinds into native Executor kinds
//! based on the chosen execution mode (Rust/WASM vs Python).
//!
//! Unified kinds:
//!   `Func`     → `WasmVoid` (Rust) or `PyFunc` (Python)
//!   `Pipeline`  → `StreamPipeline` (Rust) or `PyPipeline` (Python)
//!   `Grouping`  → `WasmGrouping` (Rust) or `PyGrouping` (Python)

use anyhow::{Context, Result};
use serde_json::Value;

pub(crate) const DEFAULT_PYTHON_SCRIPT: &str = "Executor/py_guest/python/runner.py";
pub(crate) const DEFAULT_PYTHON_WASM: &str = "/opt/myapp/python-3.12.0.wasm";

/// Transform a unified DAG JSON for the target execution mode.
///
/// When `python_mode` is true:
///   - `Func` → `PyFunc`, `Pipeline` → `PyPipeline`, `Grouping` → `PyGrouping`
///   - `python_script` and `python_wasm` are injected
///   - `shm_path` gets a `py_` prefix on the filename
///   - `Output` paths get a `py_` prefix and `slot: 1` is added
///
/// When `python_mode` is false (default):
///   - `Func` → `WasmVoid`, `Pipeline` → `StreamPipeline`, `Grouping` → `WasmGrouping`
///   - Stage args converted: `arg`/`arg2` → `arg0`/`arg1`
pub fn transform_dag(
    dag_json: &str,
    python_mode: bool,
    python_script: Option<&str>,
    python_wasm: Option<&str>,
) -> Result<String> {
    let mut dag: Value = serde_json::from_str(dag_json).context("parse DAG JSON")?;

    // Check if DAG has any unified nodes (Func, Pipeline, Grouping).
    let has_unified_nodes = dag
        .get("nodes")
        .and_then(|v| v.as_array())
        .map(|nodes| {
            nodes.iter().any(|n| {
                let k = n.get("kind");
                k.and_then(|k| k.get("Func")).is_some()
                    || k.and_then(|k| k.get("Pipeline")).is_some()
                    || k.and_then(|k| k.get("Grouping")).is_some()
            })
        })
        .unwrap_or(false);

    if !has_unified_nodes {
        // Already in native format — return as-is.
        return Ok(dag_json.to_string());
    }

    if python_mode {
        let script = python_script.unwrap_or(DEFAULT_PYTHON_SCRIPT);
        let wasm = python_wasm.unwrap_or(DEFAULT_PYTHON_WASM);
        dag["python_script"] = Value::String(script.to_string());
        dag["python_wasm"] = Value::String(wasm.to_string());

        // Prefix shm_path: /dev/shm/finra_demo → /dev/shm/py_finra_demo
        if let Some(shm) = dag.get("shm_path").and_then(|v| v.as_str()).map(String::from) {
            if let Some(pos) = shm.rfind('/') {
                let (dir, name) = shm.split_at(pos + 1);
                dag["shm_path"] = Value::String(format!("{}py_{}", dir, name));
            }
        }
    }

    // Transform each node.
    if let Some(nodes) = dag.get_mut("nodes").and_then(|v| v.as_array_mut()) {
        for node in nodes.iter_mut() {
            transform_node(node, python_mode)?;
        }
    }

    serde_json::to_string_pretty(&dag).context("serialize transformed DAG")
}

fn transform_node(node: &mut Value, python_mode: bool) -> Result<()> {
    let kind = match node.get_mut("kind") {
        Some(k) => k,
        None => return Ok(()),
    };

    // ── Func → WasmVoid / PyFunc ──────────────────────────────────────────
    if let Some(func_params) = kind.get("Func").cloned() {
        let func_name = func_params
            .get("func")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if python_mode {
            let mut py = serde_json::Map::new();
            py.insert("func".to_string(), Value::String(func_name.to_string()));
            if let Some(a) = func_params.get("arg") {
                py.insert("arg".to_string(), a.clone());
            }
            if let Some(a2) = func_params.get("arg2") {
                py.insert("arg2".to_string(), a2.clone());
            }
            *kind = serde_json::json!({ "PyFunc": py });
        } else {
            let mut wasm = serde_json::Map::new();
            // Use wasm_func override if present, otherwise use func.
            let effective_func = func_params
                .get("wasm_func")
                .and_then(|v| v.as_str())
                .unwrap_or(func_name);
            wasm.insert("func".to_string(), Value::String(effective_func.to_string()));
            // Use wasm_arg if present, otherwise fall back to arg.
            let effective_arg = func_params.get("wasm_arg").or_else(|| func_params.get("arg"));
            if let Some(a) = effective_arg {
                wasm.insert("arg".to_string(), a.clone());
            }
            *kind = serde_json::json!({ "WasmVoid": wasm });
        }
        return Ok(());
    }

    // ── Pipeline → StreamPipeline / PyPipeline ────────────────────────────
    if let Some(pipeline_params) = kind.get("Pipeline").cloned() {
        let stages = pipeline_params.get("stages").cloned().unwrap_or(Value::Array(vec![]));
        let rounds = pipeline_params.get("rounds").cloned();

        if python_mode {
            let mut py = serde_json::Map::new();
            if let Some(r) = rounds {
                py.insert("rounds".to_string(), r);
            }
            py.insert("stages".to_string(), convert_stages(&stages, true));
            *kind = serde_json::json!({ "PyPipeline": py });
        } else {
            let mut wasm = serde_json::Map::new();
            if let Some(r) = rounds {
                wasm.insert("rounds".to_string(), r);
            }
            wasm.insert("stages".to_string(), convert_stages(&stages, false));
            *kind = serde_json::json!({ "StreamPipeline": wasm });
        }
        return Ok(());
    }

    // ── Grouping → WasmGrouping / PyGrouping ──────────────────────────────
    if let Some(grouping_params) = kind.get("Grouping").cloned() {
        let stages = grouping_params.get("stages").cloned().unwrap_or(Value::Array(vec![]));

        if python_mode {
            let mut py = serde_json::Map::new();
            py.insert("stages".to_string(), convert_stages(&stages, true));
            *kind = serde_json::json!({ "PyGrouping": py });
        } else {
            let mut wasm = serde_json::Map::new();
            wasm.insert("stages".to_string(), convert_stages(&stages, false));
            *kind = serde_json::json!({ "WasmGrouping": wasm });
        }
        return Ok(());
    }

    // ── Output path prefixing for Python mode ─────────────────────────────
    if python_mode {
        if let Some(output_params) = kind.get_mut("Output") {
            // Single path.
            if let Some(path) = output_params.get("path").and_then(|v| v.as_str()).map(String::from) {
                output_params["path"] = Value::String(prefix_filename(&path, "py_"));
            }
            // Multiple paths.
            if let Some(paths) = output_params.get("paths").and_then(|v| v.as_array()).cloned() {
                let prefixed: Vec<Value> = paths
                    .iter()
                    .filter_map(|p| p.as_str())
                    .map(|p| Value::String(prefix_path_dir(&p, "py_")))
                    .collect();
                output_params["paths"] = Value::Array(prefixed);
            }
            // Python guest writes to I/O slot 1.
            if output_params.get("slot").is_none() {
                output_params["slot"] = Value::Number(1.into());
            }
        }
    }

    Ok(())
}

/// Convert unified stage format to native format.
///
/// Unified: `{ "func": "...", "arg": N, "arg2": M, "wasm_func": "..." }`
/// Python:  `{ "func": "...", "arg": N, "arg2": M }`
/// Rust:    `{ "func": "...", "arg0": N, "arg1": M }`
fn convert_stages(stages: &Value, python_mode: bool) -> Value {
    let arr = match stages.as_array() {
        Some(a) => a,
        None => return stages.clone(),
    };

    let converted: Vec<Value> = arr
        .iter()
        .map(|stage| {
            let func = stage.get("func").and_then(|v| v.as_str()).unwrap_or("");
            let arg = stage.get("arg");
            let arg2 = stage.get("arg2");

            if python_mode {
                let mut s = serde_json::Map::new();
                s.insert("func".to_string(), Value::String(func.to_string()));
                if let Some(a) = arg {
                    s.insert("arg".to_string(), a.clone());
                }
                if let Some(a2) = arg2 {
                    s.insert("arg2".to_string(), a2.clone());
                }
                Value::Object(s)
            } else {
                let mut s = serde_json::Map::new();
                let effective_func = stage
                    .get("wasm_func")
                    .and_then(|v| v.as_str())
                    .unwrap_or(func);
                s.insert("func".to_string(), Value::String(effective_func.to_string()));
                // Rust stages use arg0/arg1 instead of arg/arg2.
                if let Some(a) = arg {
                    s.insert("arg0".to_string(), a.clone());
                }
                if let Some(a2) = arg2 {
                    s.insert("arg1".to_string(), a2.clone());
                }
                Value::Object(s)
            }
        })
        .collect();

    Value::Array(converted)
}

/// Prefix the filename component of a path: /tmp/foo.txt → /tmp/py_foo.txt
fn prefix_filename(path: &str, prefix: &str) -> String {
    if let Some(pos) = path.rfind('/') {
        let (dir, name) = path.split_at(pos + 1);
        format!("{}{}{}", dir, prefix, name)
    } else {
        format!("{}{}", prefix, path)
    }
}

/// Prefix the last directory component: /tmp/img_out/foo.pgm → /tmp/py_img_out/foo.pgm
fn prefix_path_dir(path: &str, prefix: &str) -> String {
    // For paths like /tmp/img_pipeline_out/file.pgm, prefix the directory name.
    let parts: Vec<&str> = path.rsplitn(2, '/').collect();
    if parts.len() == 2 {
        let filename = parts[0];
        let dir_path = parts[1];
        // Prefix the last directory component.
        if let Some(pos) = dir_path.rfind('/') {
            let (parent, dir_name) = dir_path.split_at(pos + 1);
            format!("{}{}{}/{}", parent, prefix, dir_name, filename)
        } else {
            format!("{}{}/{}", prefix, dir_path, filename)
        }
    } else {
        path.to_string()
    }
}

/// Transform a ClusterDag JSON for Python execution.
///
/// Converts native node kinds to their Python equivalents:
///   `WasmVoid` → `PyFunc`, `StreamPipeline` → `PyPipeline`, `WasmGrouping` → `PyGrouping`
///
/// Also injects `python_script` / `python_wasm` and prefixes `shm_path_prefix`.
pub fn transform_cluster_dag(
    cluster_dag_json: &str,
    python_script: Option<&str>,
    python_wasm: Option<&str>,
) -> Result<String> {
    let mut dag: Value = serde_json::from_str(cluster_dag_json).context("parse ClusterDag JSON")?;

    let script = python_script.unwrap_or(DEFAULT_PYTHON_SCRIPT);
    let wasm = python_wasm.unwrap_or(DEFAULT_PYTHON_WASM);
    dag["python_script"] = Value::String(script.to_string());
    dag["python_wasm"] = Value::String(wasm.to_string());

    // Prefix shm_path_prefix: /dev/shm/rdma_wc → /dev/shm/py_rdma_wc
    if let Some(prefix) = dag.get("shm_path_prefix").and_then(|v| v.as_str()).map(String::from) {
        dag["shm_path_prefix"] = Value::String(prefix_filename(&prefix, "py_"));
    }

    // Transform each node in every node_dag.
    if let Some(node_dags) = dag.get_mut("node_dags").and_then(|v| v.as_object_mut()) {
        for (_node_id, nodes) in node_dags.iter_mut() {
            if let Some(arr) = nodes.as_array_mut() {
                for node in arr.iter_mut() {
                    transform_node_to_python(node)?;
                }
            }
        }
    }

    serde_json::to_string_pretty(&dag).context("serialize transformed ClusterDag")
}

/// Convert a native Executor node kind to its Python equivalent.
fn transform_node_to_python(node: &mut Value) -> Result<()> {
    let kind = match node.get_mut("kind") {
        Some(k) => k,
        None => return Ok(()),
    };

    // WasmVoid { func, arg } → PyFunc { func, arg }
    if let Some(params) = kind.get("WasmVoid").cloned() {
        *kind = serde_json::json!({ "PyFunc": params });
        return Ok(());
    }

    // StreamPipeline → PyPipeline (convert stage arg naming: arg0→arg, arg1→arg2)
    if let Some(mut params) = kind.get("StreamPipeline").cloned() {
        if let Some(stages) = params.get_mut("stages").and_then(|v| v.as_array_mut()) {
            for stage in stages.iter_mut() {
                rename_stage_args_to_python(stage);
            }
        }
        *kind = serde_json::json!({ "PyPipeline": params });
        return Ok(());
    }

    // WasmGrouping → PyGrouping
    if let Some(mut params) = kind.get("WasmGrouping").cloned() {
        if let Some(stages) = params.get_mut("stages").and_then(|v| v.as_array_mut()) {
            for stage in stages.iter_mut() {
                rename_stage_args_to_python(stage);
            }
        }
        *kind = serde_json::json!({ "PyGrouping": params });
        return Ok(());
    }

    // Output: prefix path with py_, add slot: 1
    if let Some(output_params) = kind.get_mut("Output") {
        if let Some(path) = output_params.get("path").and_then(|v| v.as_str()).map(String::from) {
            output_params["path"] = Value::String(prefix_filename(&path, "py_"));
        }
        if output_params.get("slot").is_none() {
            output_params["slot"] = Value::Number(1.into());
        }
    }

    Ok(())
}

/// Rename Rust-style stage args (arg0, arg1) to Python-style (arg, arg2).
fn rename_stage_args_to_python(stage: &mut Value) {
    if let Some(obj) = stage.as_object_mut() {
        if let Some(a0) = obj.remove("arg0") {
            obj.insert("arg".to_string(), a0);
        }
        if let Some(a1) = obj.remove("arg1") {
            obj.insert("arg2".to_string(), a1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_func_to_wasm_void() {
        let dag = r#"{
            "shm_path": "/dev/shm/test",
            "wasm_path": "test.wasm",
            "nodes": [
                { "id": "a", "deps": [], "kind": { "Func": { "func": "my_func", "arg": 10, "arg2": 110 } } }
            ]
        }"#;
        let result = transform_dag(dag, false, None, None).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let kind = &v["nodes"][0]["kind"];
        assert!(kind.get("WasmVoid").is_some());
        assert_eq!(kind["WasmVoid"]["arg"], 10);
    }

    #[test]
    fn test_func_to_pyfunc() {
        let dag = r#"{
            "shm_path": "/dev/shm/test",
            "wasm_path": "test.wasm",
            "nodes": [
                { "id": "a", "deps": [], "kind": { "Func": { "func": "my_func", "arg": 10, "arg2": 110 } } }
            ]
        }"#;
        let result = transform_dag(dag, true, None, None).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["shm_path"], "/dev/shm/py_test");
        assert!(v["python_script"].is_string());
        let kind = &v["nodes"][0]["kind"];
        assert!(kind.get("PyFunc").is_some());
        assert_eq!(kind["PyFunc"]["arg"], 10);
        assert_eq!(kind["PyFunc"]["arg2"], 110);
    }

    #[test]
    fn test_wasm_arg_override() {
        let dag = r#"{
            "shm_path": "/dev/shm/test",
            "wasm_path": "test.wasm",
            "nodes": [
                { "id": "a", "deps": [], "kind": { "Func": { "func": "f", "arg": 200, "arg2": 3, "wasm_arg": 3 } } }
            ]
        }"#;
        let result = transform_dag(dag, false, None, None).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["nodes"][0]["kind"]["WasmVoid"]["arg"], 3);
    }

    #[test]
    fn test_pipeline_to_stream_pipeline() {
        let dag = r#"{
            "shm_path": "/dev/shm/test",
            "wasm_path": "test.wasm",
            "nodes": [
                { "id": "p", "deps": [], "kind": { "Pipeline": {
                    "rounds": 3,
                    "stages": [
                        { "func": "load", "arg": 10, "arg2": 20 },
                        { "func": "process", "arg": 20, "arg2": 30 }
                    ]
                }}}
            ]
        }"#;
        let result = transform_dag(dag, false, None, None).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let kind = &v["nodes"][0]["kind"];
        assert!(kind.get("StreamPipeline").is_some());
        let stages = &kind["StreamPipeline"]["stages"];
        assert_eq!(stages[0]["arg0"], 10);
        assert_eq!(stages[0]["arg1"], 20);
    }

    #[test]
    fn test_output_path_prefix() {
        assert_eq!(prefix_filename("/tmp/result.txt", "py_"), "/tmp/py_result.txt");
        assert_eq!(
            prefix_path_dir("/tmp/img_out/file.pgm", "py_"),
            "/tmp/py_img_out/file.pgm"
        );
    }
}
