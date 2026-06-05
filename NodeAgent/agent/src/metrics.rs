//! System and SHM metrics collection.
//!
//! Reads CPU usage from /proc/stat, RSS as the summed PRIVATE resident memory
//! of the executor process TREE (host + all fanned-out `wasm-call` workers,
//! SHM removed per process), and the SHM bump allocator offset from the
//! Superblock.  Total job memory = rss_bytes + shm_bump_offset (SHM once).

use anyhow::Result;
use scheduler::ScxNodeSnapshot;
use serde::{Deserialize, Serialize};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeMetrics {
    pub node_id: u32,
    pub timestamp_ms: u64,
    pub cpu_usage_pct: f32,
    /// Total PRIVATE resident memory (bytes) of the executor process tree —
    /// host + every fanned-out `wasm-call` worker — with the shared SHM removed
    /// (VmRSS − RssShmem per process).  The SHM is reported once in
    /// `shm_bump_offset`; total job memory = `rss_bytes + shm_bump_offset`.
    pub rss_bytes: u64,
    /// SHM bump-allocator high-water (bytes-as-u32 offset): the shared-memory
    /// footprint, counted once (excluded from `rss_bytes`).
    pub shm_bump_offset: u32,
    pub executor_running: bool,
    pub current_job_id: Option<String>,
    pub job_elapsed_ms: Option<u64>,
    /// Logical CPU count visible to this process (respects cgroup quotas).
    pub cpu_cores: u32,
    /// SCX sched_ext scheduler stats (None if SCX is not running or disabled).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scx: Option<ScxNodeSnapshot>,
}

/// State for computing CPU usage deltas between samples.
pub struct MetricsCollector {
    node_id: u32,
    prev_cpu_total: u64,
    prev_cpu_idle: u64,
    scx_client: Option<scheduler::ScxStatsClient>,
}

impl MetricsCollector {
    pub fn new(node_id: u32) -> Self {
        Self {
            node_id,
            prev_cpu_total: 0,
            prev_cpu_idle: 0,
            scx_client: None,
        }
    }

    /// Create a collector with SCX stats collection enabled.
    pub fn with_scx(node_id: u32, socket_path: Option<&str>) -> Self {
        Self {
            node_id,
            prev_cpu_total: 0,
            prev_cpu_idle: 0,
            scx_client: Some(scheduler::ScxStatsClient::new(socket_path)),
        }
    }

    /// Sample current system metrics.
    pub fn sample(
        &mut self,
        shm_path: Option<&str>,
        executor_running: bool,
        current_job_id: Option<&str>,
        job_elapsed_ms: Option<u64>,
        executor_pid: Option<u32>,
    ) -> NodeMetrics {
        let cpu_usage_pct = self.sample_cpu().unwrap_or(0.0);
        // RSS of the EXECUTOR subprocess (where the SHM + workloads live), not
        // the node-agent daemon itself — otherwise RSS stays flat at the
        // daemon's tiny footprint regardless of the job.  Falls back to self
        // when no executor pid is given (idle) or its /proc entry is gone.
        let rss_bytes = sample_rss(executor_pid).unwrap_or(0);
        let shm_bump_offset = shm_path
            .and_then(|p| read_shm_bump_offset(p).ok())
            .unwrap_or(0);

        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let scx = self.scx_client.as_ref().and_then(|c| c.fetch());

        let cpu_cores = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(0);

        NodeMetrics {
            node_id: self.node_id,
            timestamp_ms,
            cpu_usage_pct,
            rss_bytes,
            shm_bump_offset,
            executor_running,
            current_job_id: current_job_id.map(|s| s.to_string()),
            job_elapsed_ms,
            cpu_cores,
            scx,
        }
    }

    /// Read /proc/stat and compute CPU usage since last sample.
    fn sample_cpu(&mut self) -> Result<f32> {
        let content = fs::read_to_string("/proc/stat")?;
        let first_line = content.lines().next().unwrap_or("");
        // "cpu  user nice system idle iowait irq softirq steal guest guest_nice"
        let fields: Vec<u64> = first_line
            .split_whitespace()
            .skip(1) // skip "cpu"
            .filter_map(|s| s.parse().ok())
            .collect();

        if fields.len() < 4 {
            return Ok(0.0);
        }

        let idle = fields[3];
        let total: u64 = fields.iter().sum();

        let delta_total = total.saturating_sub(self.prev_cpu_total);
        let delta_idle = idle.saturating_sub(self.prev_cpu_idle);

        self.prev_cpu_total = total;
        self.prev_cpu_idle = idle;

        if delta_total == 0 {
            return Ok(0.0);
        }

        Ok(100.0 * (1.0 - delta_idle as f32 / delta_total as f32))
    }
}

/// Total PRIVATE resident memory of the executor process tree: the host
/// executor `executor_pid` PLUS all its descendants — the fanned-out
/// `wasm-call` workers, each with its own wasmtime runtime, JIT'd guest, and
/// guest heap (which dominate the footprint, ~95% in practice).
///
/// "Private" = `VmRSS − RssShmem` per process, so the shared SHM is subtracted
/// from EVERY process and contributes nothing here — it is reported exactly
/// once, separately, via `shm_bump_offset`.  Thus
/// `total job memory = rss_bytes + shm_bump_offset` with no double-counting.
///
/// Falls back to this process's own private RSS when no executor pid is given
/// (idle) or the executor tree has already exited.
fn sample_rss(executor_pid: Option<u32>) -> Result<u64> {
    let root = match executor_pid {
        Some(p) => p,
        None => return Ok(proc_private_kb(std::process::id()).unwrap_or((0, 0)).1 * 1024),
    };

    // Snapshot (pid → ppid, private_kb) for every live process.  Racy reads
    // (a process exits mid-scan) are simply skipped.
    use std::collections::{HashMap, HashSet, VecDeque};
    let mut ppid_of: HashMap<u32, u32> = HashMap::new();
    let mut priv_of: HashMap<u32, u64> = HashMap::new();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for e in entries.flatten() {
            let pid = match e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) {
                Some(p) => p,
                None => continue,
            };
            if let Some((ppid, priv_kb)) = proc_private_kb(pid) {
                ppid_of.insert(pid, ppid);
                priv_of.insert(pid, priv_kb);
                children.entry(ppid).or_default().push(pid);
            }
        }
    }

    // BFS the subtree rooted at the executor and sum private RSS.
    let mut total_kb = 0u64;
    let mut seen: HashSet<u32> = HashSet::new();
    let mut q: VecDeque<u32> = VecDeque::new();
    q.push_back(root);
    seen.insert(root);
    while let Some(pid) = q.pop_front() {
        if let Some(&pk) = priv_of.get(&pid) {
            total_kb += pk;
        }
        if let Some(kids) = children.get(&pid) {
            for &k in kids {
                if seen.insert(k) {
                    q.push_back(k);
                }
            }
        }
    }

    if total_kb == 0 {
        // Executor tree already gone — fall back to self so the sample succeeds.
        total_kb = proc_private_kb(std::process::id()).unwrap_or((0, 0)).1;
    }
    Ok(total_kb * 1024)
}

/// Read `(PPid, VmRSS − RssShmem)` in kB from `/proc/<pid>/status`.
/// Returns `None` if the process is gone or has no VmRSS (kernel threads).
fn proc_private_kb(pid: u32) -> Option<(u32, u64)> {
    let content = fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    let (mut ppid, mut vmrss, mut shmem, mut has_rss) = (0u32, 0u64, 0u64, false);
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("PPid:") {
            ppid = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("VmRSS:") {
            vmrss = v.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0);
            has_rss = true;
        } else if let Some(v) = line.strip_prefix("RssShmem:") {
            shmem = v.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0);
        }
    }
    if !has_rss {
        return None;
    }
    Some((ppid, vmrss.saturating_sub(shmem)))
}

/// Read the bump_allocator field from the SHM Superblock.
///
/// The Superblock layout (from common/src/lib.rs):
///   offset 0: magic (u32)
///   offset 4: bump_allocator (AtomicU32)
///
/// We read the raw bytes at offset 4 as a little-endian u32.
fn read_shm_bump_offset(shm_path: &str) -> Result<u32> {
    let data = fs::read(shm_path)?;
    if data.len() < 8 {
        return Ok(0);
    }
    // bump_allocator is at offset 4 (after the 4-byte magic field).
    let bytes: [u8; 4] = [data[4], data[5], data[6], data[7]];
    Ok(u32::from_le_bytes(bytes))
}

/// Append a metrics record to a JSON-lines log file.
pub fn append_metrics_log(path: &str, metrics: &NodeMetrics) -> Result<()> {
    use std::io::Write;
    let json = serde_json::to_string(metrics)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", json)?;
    Ok(())
}
