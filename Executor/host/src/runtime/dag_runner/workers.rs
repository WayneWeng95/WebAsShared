use anyhow::{anyhow, Result};
use std::path::Path;
use super::types::{DagNode, NodeKind};

// ─── One-shot WASM subprocess helpers ────────────────────────────────────────

/// Spawns `./host wasm-call <shm_path> <wasm_path> <func> <ret_type> <arg>` as a
/// child process and returns the handle.  The caller must `.wait()` on it.
pub(super) fn spawn_wasm_subprocess(
    node: &DagNode,
    shm_path: &str,
    wasm_path: &str,
) -> Result<std::process::Child> {
    let (func, ret_type, arg) = match &node.kind {
        NodeKind::WasmVoid(c)   => (c.func.as_str(), "void",   c.arg),
        NodeKind::WasmU32(c)    => (c.func.as_str(), "u32",    c.arg),
        NodeKind::WasmFatPtr(c) => (c.func.as_str(), "fatptr", c.arg),
        _ => return Err(anyhow!("[{}] not a subprocess WASM node", node.id)),
    };
    let exe = std::env::current_exe()
        .map_err(|e| anyhow!("cannot find current exe: {}", e))?;
    std::process::Command::new(exe)
        .arg("wasm-call")
        .arg(shm_path)
        .arg(wasm_path)
        .arg(func)
        .arg(ret_type)
        .arg(arg.to_string())
        .spawn()
        .map_err(|e| anyhow!("[{}] failed to spawn WASM worker: {}", node.id, e))
}

/// Spawns a Python runner subprocess (via `wasmtime run` or native `python3`)
/// for a `PyFunc` node and returns the child handle.  The caller must `.wait()`.
pub(super) fn spawn_python_subprocess(
    node: &DagNode,
    shm_path: &str,
    python_script: &str,
    python_wasm: Option<&str>,
) -> Result<std::process::Child> {
    let call = match &node.kind {
        NodeKind::PyFunc(c) => c,
        _ => return Err(anyhow!("[{}] not a PyFunc node", node.id)),
    };
    if python_script.is_empty() {
        return Err(anyhow!(
            "[{}] PyFunc requires 'python_script' to be set in the DAG JSON",
            node.id
        ));
    }
    if let Some(wasm_path) = python_wasm {
        let script_dir = Path::new(python_script)
            .parent()
            .unwrap_or(Path::new("."))
            .to_string_lossy();
        let shm_dir = Path::new(shm_path)
            .parent()
            .unwrap_or(Path::new("/dev/shm"))
            .to_string_lossy();
        let wasmtime_bin = std::env::var("WASMTIME").unwrap_or_else(|_| {
            let candidate = std::env::var("HOME")
                .map(|h| format!("{}/.wasmtime/bin/wasmtime", h))
                .unwrap_or_default();
            if !candidate.is_empty() && std::path::Path::new(&candidate).exists() {
                candidate
            } else {
                "wasmtime".to_owned()
            }
        });
        let mut cmd = std::process::Command::new(&wasmtime_bin);
        cmd.arg("run");
        if wasm_path.ends_with(".cwasm") {
            cmd.arg("--allow-precompiled");
        }
        cmd.arg("--env").arg(format!("SHM_PATH={}", shm_path))
            .arg("--env").arg(format!("WORKLOAD_FUNC={}", call.func))
            .arg("--env").arg(format!("WORKLOAD_ARG={}", call.arg));
        if let Some(a2) = call.arg2 {
            cmd.arg("--env").arg(format!("WORKLOAD_ARG2={}", a2));
        }
        cmd.arg("--dir").arg(shm_dir.as_ref());
        if script_dir != shm_dir {
            cmd.arg("--dir").arg(script_dir.as_ref());
        }
        cmd.arg(wasm_path)
            .arg("--")
            .arg(python_script);
        let mode = if wasm_path.ends_with(".cwasm") { "python.cwasm (AOT)" } else { "python.wasm" };
        println!("  PyFunc {}({}) via {}", call.func, call.arg, mode);
        cmd.spawn()
            .map_err(|e| anyhow!("[{}] failed to spawn wasmtime: {}", node.id, e))
    } else {
        let mut cmd = std::process::Command::new("python3");
        cmd.arg(python_script)
            .env("SHM_PATH",      shm_path)
            .env("WORKLOAD_FUNC", &call.func)
            .env("WORKLOAD_ARG",  call.arg.to_string());
        if let Some(a2) = call.arg2 {
            cmd.env("WORKLOAD_ARG2", a2.to_string());
        }
        println!("  PyFunc {}({}) via python3", call.func, call.arg);
        cmd.spawn()
            .map_err(|e| anyhow!("[{}] failed to spawn python3: {}", node.id, e))
    }
}

// ─── WasmLoopWorker ───────────────────────────────────────────────────────────

/// A persistent `wasm-loop` subprocess worker.
///
/// Spawned once and reused across all rounds or stages.  Commands are sent via
/// stdin (`"arg0 arg1\n"`) and responses read from stdout (`"ok\n"`).
/// Dropping the worker closes stdin (EOF), which causes the process to exit.
///
/// Used by both `StreamPipeline` (scatter/gather across ticks) and
/// `WasmGrouping` (sequential stage-by-stage calls).
pub(super) struct WasmLoopWorker {
    pub(super) child:  std::process::Child,
    pub(super) stdin:  Option<std::io::BufWriter<std::process::ChildStdin>>,
    pub(super) stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl WasmLoopWorker {
    pub(super) fn spawn(func: &str, shm_path: &str, wasm_path: &str, node_id: &str) -> Result<Self> {
        let exe = std::env::current_exe()
            .map_err(|e| anyhow!("cannot find current exe: {}", e))?;
        let mut child = std::process::Command::new(exe)
            .args(["wasm-loop", shm_path, wasm_path, func])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("[{}] failed to spawn wasm-loop '{}': {}", node_id, func, e))?;
        let stdin  = std::io::BufWriter::new(child.stdin.take().unwrap());
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        Ok(WasmLoopWorker { child, stdin: Some(stdin), stdout })
    }

    /// Write a call command to the worker's stdin (non-blocking from the host side).
    /// The worker wakes from its blocked read(), executes the function, then sleeps again.
    pub(super) fn send(&mut self, arg0: u32, arg1: u32) -> Result<()> {
        use std::io::Write;
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{} {}", arg0, arg1)?;
        stdin.flush()?;
        Ok(())
    }

    /// Block until the worker writes its response line.
    pub(super) fn recv(&mut self) -> Result<()> {
        use std::io::BufRead;
        let mut line = String::new();
        self.stdout.read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed == "ok" { Ok(()) } else { Err(anyhow!("worker: {}", trimmed)) }
    }

    /// Close stdin (signals EOF → worker exits) and wait for the process.
    pub(super) fn finish(mut self) -> Result<()> {
        drop(self.stdin.take()); // closes pipe → worker's read() returns EOF
        let status = self.child.wait()?;
        if !status.success() {
            return Err(anyhow!("wasm-loop worker exited with {}", status));
        }
        Ok(())
    }
}

impl Drop for WasmLoopWorker {
    fn drop(&mut self) {
        drop(self.stdin.take()); // close stdin before waiting so worker can exit
        let _ = self.child.wait();
    }
}

// ─── PyLoopWorker ─────────────────────────────────────────────────────────────

/// A persistent Python loop worker.
///
/// Spawns `runner.py --loop` (via `python3` or `wasmtime run python.wasm`) once
/// and reuses the process across all rounds or stages.  Each call sends
/// `"func arg [arg2]\n"` to stdin and reads `"ok\n"` back.  The process sleeps
/// (blocked on stdin read) between calls.
///
/// Used by both `PyPipeline` (scatter/gather across ticks) and `PyGrouping`
/// (sequential stage-by-stage calls via the blocking [`call`] method).
pub(super) struct PyLoopWorker {
    child:  std::process::Child,
    stdin:  Option<std::io::BufWriter<std::process::ChildStdin>>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl PyLoopWorker {
    pub(super) fn spawn(
        shm_path: &str,
        python_script: &str,
        python_wasm: Option<&str>,
        node_id: &str,
    ) -> Result<Self> {
        if python_script.is_empty() {
            return Err(anyhow!(
                "[{}] Python loop worker requires 'python_script' to be set in the DAG JSON",
                node_id
            ));
        }
        let mut child = if let Some(wasm_path) = python_wasm {
            let script_dir = Path::new(python_script)
                .parent().unwrap_or(Path::new("."))
                .to_string_lossy();
            let shm_dir = Path::new(shm_path)
                .parent().unwrap_or(Path::new("/dev/shm"))
                .to_string_lossy();
            let wasmtime_bin = std::env::var("WASMTIME").unwrap_or_else(|_| {
                let candidate = std::env::var("HOME")
                    .map(|h| format!("{}/.wasmtime/bin/wasmtime", h))
                    .unwrap_or_default();
                if !candidate.is_empty() && std::path::Path::new(&candidate).exists() {
                    candidate
                } else {
                    "wasmtime".to_owned()
                }
            });
            let mut cmd = std::process::Command::new(&wasmtime_bin);
            cmd.arg("run");
            if wasm_path.ends_with(".cwasm") {
                cmd.arg("--allow-precompiled");
            }
            cmd.arg("--env").arg(format!("SHM_PATH={}", shm_path))
                .arg("--dir").arg(shm_dir.as_ref());
            if script_dir != shm_dir {
                cmd.arg("--dir").arg(script_dir.as_ref());
            }
            cmd.arg(wasm_path).arg("--").arg(python_script).arg("--loop");
            cmd.stdin(std::process::Stdio::piped())
               .stdout(std::process::Stdio::piped())
               .spawn()
               .map_err(|e| anyhow!("[{}] failed to spawn wasmtime loop: {}", node_id, e))?
        } else {
            std::process::Command::new("python3")
                .arg(python_script)
                .arg("--loop")
                .env("SHM_PATH", shm_path)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| anyhow!("[{}] failed to spawn python3 loop: {}", node_id, e))?
        };
        let stdin  = std::io::BufWriter::new(child.stdin.take().unwrap());
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        Ok(PyLoopWorker { child, stdin: Some(stdin), stdout })
    }

    /// Send a stage call and wait for the response (blocking).
    pub(super) fn call(&mut self, func: &str, arg: u32, arg2: Option<u32>) -> Result<()> {
        self.call_async(func, arg, arg2)?;
        self.recv()
    }

    /// Write a call command to the worker's stdin without waiting for the response.
    /// Use [`recv`] afterwards to collect the result.  Together they form a
    /// scatter/gather pair for concurrent multi-stage tick dispatch.
    pub(super) fn call_async(&mut self, func: &str, arg: u32, arg2: Option<u32>) -> Result<()> {
        use std::io::Write;
        let stdin = self.stdin.as_mut().unwrap();
        if let Some(a2) = arg2 {
            writeln!(stdin, "{} {} {}", func, arg, a2)?;
        } else {
            writeln!(stdin, "{} {}", func, arg)?;
        }
        stdin.flush()?;
        Ok(())
    }

    /// Block until the worker writes its response line.
    pub(super) fn recv(&mut self) -> Result<()> {
        use std::io::BufRead;
        let mut line = String::new();
        self.stdout.read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed == "ok" { Ok(()) } else { Err(anyhow!("py worker: {}", trimmed)) }
    }

    /// Close stdin (signals EOF → worker exits) and wait for the process.
    pub(super) fn finish(mut self) -> Result<()> {
        drop(self.stdin.take());
        let status = self.child.wait()?;
        if !status.success() {
            return Err(anyhow!("python loop worker exited with {}", status));
        }
        Ok(())
    }
}

impl Drop for PyLoopWorker {
    fn drop(&mut self) {
        drop(self.stdin.take()); // close stdin before waiting so worker can exit
        let _ = self.child.wait();
    }
}
