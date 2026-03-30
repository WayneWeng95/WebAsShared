//! Executor interface: spawn and monitor `host dag` subprocesses.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Handle to a running Executor process.
pub struct ExecutorHandle {
    child: Child,
    pub start_time: Instant,
    pub job_id: String,
    stdout_lines: Arc<Mutex<Vec<String>>>,
    stderr_lines: Arc<Mutex<Vec<String>>>,
}

/// Result of a completed executor run.
pub struct ExecutorResult {
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub success: bool,
}

impl ExecutorHandle {
    /// Spawn the Executor with the given DAG JSON.
    ///
    /// Writes `dag_json` to a temp file, then runs:
    ///   `<executor_bin> dag <tmp_file>`
    pub fn spawn(
        executor_bin: &Path,
        work_dir: &Path,
        dag_json: &str,
        job_id: &str,
    ) -> Result<Self> {
        // Write DAG JSON to a temp file in /tmp.
        let tmp_path = format!("/tmp/node_agent_dag_{}.json", job_id);
        std::fs::write(&tmp_path, dag_json)
            .with_context(|| format!("write temp DAG file: {}", tmp_path))?;

        let mut child = Command::new(executor_bin)
            .arg("dag")
            .arg(&tmp_path)
            .current_dir(work_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!(
                "spawn executor: {} dag {}",
                executor_bin.display(), tmp_path
            ))?;

        // Spawn threads to capture stdout/stderr without blocking.
        let stdout_lines = Arc::new(Mutex::new(Vec::new()));
        let stderr_lines = Arc::new(Mutex::new(Vec::new()));

        if let Some(stdout) = child.stdout.take() {
            let lines = Arc::clone(&stdout_lines);
            std::thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    if let Ok(line) = line {
                        let mut buf = lines.lock().unwrap();
                        // Keep only last 100 lines to bound memory.
                        if buf.len() >= 100 {
                            buf.remove(0);
                        }
                        buf.push(line);
                    }
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            let lines = Arc::clone(&stderr_lines);
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    if let Ok(line) = line {
                        let mut buf = lines.lock().unwrap();
                        if buf.len() >= 100 {
                            buf.remove(0);
                        }
                        buf.push(line);
                    }
                }
            });
        }

        Ok(Self {
            child,
            start_time: Instant::now(),
            job_id: job_id.to_string(),
            stdout_lines,
            stderr_lines,
        })
    }

    /// Check if the executor has finished (non-blocking).
    pub fn try_wait(&mut self) -> Result<Option<ExecutorResult>> {
        match self.child.try_wait().context("try_wait on executor")? {
            Some(status) => {
                let duration_ms = self.start_time.elapsed().as_millis() as u64;
                let exit_code = status.code();
                let stdout_tail = self.get_tail(&self.stdout_lines, 20);
                let stderr_tail = self.get_tail(&self.stderr_lines, 20);

                Ok(Some(ExecutorResult {
                    success: status.success(),
                    exit_code,
                    duration_ms,
                    stdout_tail,
                    stderr_tail,
                }))
            }
            None => Ok(None),
        }
    }

    /// Wait for the executor to finish (blocking).
    pub fn wait(mut self) -> Result<ExecutorResult> {
        let status = self.child.wait().context("wait on executor")?;
        let duration_ms = self.start_time.elapsed().as_millis() as u64;
        let exit_code = status.code();
        let stdout_tail = self.get_tail(&self.stdout_lines, 20);
        let stderr_tail = self.get_tail(&self.stderr_lines, 20);

        Ok(ExecutorResult {
            success: status.success(),
            exit_code,
            duration_ms,
            stdout_tail,
            stderr_tail,
        })
    }

    /// Get the PID of the executor process.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Get elapsed time since spawn.
    pub fn elapsed_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }

    /// Kill the executor process.
    pub fn kill(&mut self) -> Result<()> {
        self.child.kill().context("kill executor")?;
        Ok(())
    }

    fn get_tail(&self, lines: &Arc<Mutex<Vec<String>>>, n: usize) -> String {
        let buf = lines.lock().unwrap();
        let start = buf.len().saturating_sub(n);
        buf[start..].join("\n")
    }
}
