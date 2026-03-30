//! Length-prefixed JSON protocol for NodeAgent control plane.
//!
//! Wire format: `[4-byte big-endian length][JSON payload]`.
//! All messages are serialized as `Message { kind, payload }`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpStream;

/// Maximum message size: 64 MiB (generous for large DAG JSONs).
const MAX_MSG_SIZE: u32 = 64 * 1024 * 1024;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitJobPayload {
    pub cluster_dag_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponsePayload {
    pub workers: Vec<WorkerStatus>,
    pub current_job: Option<String>,
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

// ── Wire helpers ─────────────────────────────────────────────────────────────

/// Send a message over a TCP stream (length-prefixed JSON).
pub fn send_message(stream: &mut TcpStream, msg: &Message) -> Result<()> {
    let json = serde_json::to_vec(msg).context("serialize message")?;
    let len = json.len() as u32;
    if len > MAX_MSG_SIZE {
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
    if len > MAX_MSG_SIZE {
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
