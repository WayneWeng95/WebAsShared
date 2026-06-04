//! Length-prefixed JSON protocol for NodeAgent control plane.
//!
//! Wire format: `[4-byte big-endian length][JSON payload]`.
//! All messages are serialized as `Message { kind, payload }`.

use node_agent_common as common;
use anyhow::{bail, Context, Result};
use scheduler::ScxNodeSnapshot;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpStream;

// ── Message types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub kind: MessageKind,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    // Coordinator -> Worker
    AssignJob,
    AbortJob,
    Ping,

    // Worker -> Coordinator
    Ready,
    JobStarted,
    JobCompleted,
    JobFailed,
    Metrics,
    Pong,

    // Client -> Coordinator
    SubmitJob,
    StatusQuery,

    // Coordinator -> Client
    SubmitAck,
    StatusResponse,
    JobResult,

    // Coordinator -> Worker (file staging, before AssignJob)
    StageFiles,
    // Worker -> Coordinator
    StageFilesAck,

    // Coordinator -> Worker: offer RDMA MR for shared input files
    InputShareOffer,
    // Worker -> Coordinator: accept (send back worker QP info)
    InputShareAccept,
    // Coordinator -> Worker: QPs are in RTR/RTS, proceed with RDMA READ
    InputShareGo,
    // Worker -> Coordinator: RDMA READ complete, files written to disk
    InputShareDone,
}

// ── Payload types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignJobPayload {
    pub job_id: String,
    pub dag_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyPayload {
    pub node_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStartedPayload {
    pub job_id: String,
    pub executor_pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobCompletedPayload {
    pub job_id: String,
    pub duration_ms: u64,
    pub stdout_tail: String,
    /// Output files produced by the job, sent back to the coordinator.
    #[serde(default)]
    pub result_files: Vec<StagedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobFailedPayload {
    pub job_id: String,
    pub exit_code: Option<i32>,
    pub stderr_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsPayload {
    pub node_id: u32,
    pub timestamp_ms: u64,
    pub cpu_usage_pct: f32,
    pub rss_bytes: u64,
    pub shm_bump_offset: u32,
    pub executor_running: bool,
    pub current_job_id: Option<String>,
    pub job_elapsed_ms: Option<u64>,
    /// SCX sched_ext scheduler stats (None if unavailable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scx: Option<ScxNodeSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitJobPayload {
    /// The submitted DAG JSON — either a raw `SymbolicDag` (`nodes` key) or an
    /// already-partitioned `ClusterDag` (`node_dags` key). The coordinator
    /// resolves which it is, partitions symbolic DAGs against the live cluster,
    /// and applies the mode transform below before assigning per-node work.
    pub cluster_dag_json: String,
    /// Execution mode + Python runtime injected by the coordinator's transform.
    /// Defaulted so older clients (which pre-transformed) still deserialize.
    #[serde(default)]
    pub python_mode: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python_script: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python_wasm: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponsePayload {
    pub workers: Vec<WorkerStatus>,
    pub current_job: Option<String>,
    /// Cluster-wide SCX scheduler stats (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scx_cluster: Option<scheduler::ScxClusterView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub node_id: u32,
    pub connected: bool,
    pub running_job: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResultPayload {
    pub job_id: String,
    pub success: bool,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageFilesPayload {
    pub job_id: String,
    pub files: Vec<StagedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedFile {
    pub rel_path: String,
    #[serde(with = "serde_bytes_base64")]
    pub data: Vec<u8>,
}

/// One file entry inside an InputShareOffer: describes where in the flat MR the file lives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputFileEntry {
    pub path: String,
    pub offset: u64,
    pub len: u64,
}

/// Coordinator → Worker: expose the shared-input MR and coordinator QP info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputShareOfferPayload {
    pub job_id: String,
    /// Coordinator's QP number.
    pub qpn: u32,
    /// Coordinator's initial packet sequence number.
    pub psn: u32,
    /// Coordinator's GID (16 bytes, JSON-encoded as an array of u8).
    pub gid: Vec<u8>,
    /// Coordinator's LID (0 for RoCE).
    pub lid: u16,
    /// Remote key of the coordinator's MR.
    pub rkey: u32,
    /// Base virtual address of the coordinator's MR.
    pub addr: u64,
    /// Total size of the MR (sum of all file lengths).
    pub total_len: u64,
    /// Per-file offset/length table into the flat MR.
    pub files: Vec<InputFileEntry>,
}

/// Worker → Coordinator: reply with the worker's QP info and receive-buffer
/// MR so the coordinator can RDMA-WRITE the file data into the worker's buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputShareAcceptPayload {
    pub job_id: String,
    pub worker_id: u32,
    pub qpn: u32,
    pub psn: u32,
    pub gid: Vec<u8>,
    pub lid: u16,
    /// Remote key of the worker's receive buffer MR (coordinator writes here).
    pub rkey: u32,
    /// Base virtual address of the worker's receive buffer MR.
    pub addr: u64,
}

/// Base64 serde shim so file bytes survive JSON transit without escaping issues.
mod serde_bytes_base64 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64_encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        base64_decode(&s).map_err(serde::de::Error::custom)
    }

    fn base64_encode(data: &[u8]) -> String {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as usize;
            let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
            let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[(n >> 18) & 0x3f] as char);
            out.push(TABLE[(n >> 12) & 0x3f] as char);
            if chunk.len() > 1 { out.push(TABLE[(n >> 6) & 0x3f] as char); } else { out.push('='); }
            if chunk.len() > 2 { out.push(TABLE[n & 0x3f] as char); } else { out.push('='); }
        }
        out
    }

    fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
        let s = s.trim_end_matches('=');
        let mut out = Vec::with_capacity(s.len() * 3 / 4);
        let mut buf: u32 = 0;
        let mut bits = 0u32;
        for ch in s.bytes() {
            let v = match ch {
                b'A'..=b'Z' => ch - b'A',
                b'a'..=b'z' => ch - b'a' + 26,
                b'0'..=b'9' => ch - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => return Err(format!("invalid base64 char: {}", ch as char)),
            } as u32;
            buf = (buf << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
                buf &= (1 << bits) - 1;
            }
        }
        Ok(out)
    }
}

// ── Wire helpers ─────────────────────────────────────────────────────────────

/// Send a message over a TCP stream (length-prefixed JSON).
pub fn send_message(stream: &mut TcpStream, msg: &Message) -> Result<()> {
    let json = serde_json::to_vec(msg).context("serialize message")?;
    let len = json.len() as u32;
    if len > common::MAX_MSG_SIZE {
        bail!("message too large: {} bytes", len);
    }
    stream.write_all(&len.to_be_bytes()).context("write length prefix")?;
    stream.write_all(&json).context("write payload")?;
    stream.flush().context("flush")?;
    Ok(())
}

/// Receive a message from a TCP stream (length-prefixed JSON).
pub fn recv_message(stream: &mut TcpStream) -> Result<Message> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).context("read length prefix")?;
    let len = u32::from_be_bytes(len_buf);
    if len > common::MAX_MSG_SIZE {
        bail!("message too large: {} bytes", len);
    }
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).context("read payload")?;
    serde_json::from_slice(&buf).context("deserialize message")
}

/// Construct a `Message` from kind and a serializable payload.
pub fn make_message<T: Serialize>(kind: MessageKind, payload: &T) -> Result<Message> {
    Ok(Message {
        kind,
        payload: serde_json::to_value(payload)?,
    })
}

/// Construct a `Message` with no payload (null).
pub fn make_signal(kind: MessageKind) -> Message {
    Message {
        kind,
        payload: serde_json::Value::Null,
    }
}
