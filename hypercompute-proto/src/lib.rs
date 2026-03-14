//! hypercompute-proto: shared protocol types between server and CLI workers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

// ── Task ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// Run a shell command on the remote node.
    Shell { command: String, args: Vec<String>, timeout_secs: u64 },
    /// Fetch a URL and return the body.
    HttpFetch { url: String, method: String, body: Option<String> },
    /// Custom opaque payload; worker must know how to handle it by `tag`.
    Custom { tag: String, payload: Vec<u8> },
}

/// Capabilities that a task may require from the executing node.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskRequirements {
    /// Minimum free CPU percentage needed.
    pub min_cpu_free_pct: f32,
    /// Minimum free RAM in megabytes.
    pub min_ram_free_mb: u64,
    /// If true, node must have a GPU.
    pub requires_gpu: bool,
    /// Tags the node must declare (e.g. "python", "ffmpeg").
    pub required_tags: Vec<String>,
    /// Preferred region, if any (soft constraint).
    pub preferred_region: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Dispatched { node_id: Uuid },
    Running { node_id: Uuid, started_at: DateTime<Utc> },
    Completed { node_id: Uuid, duration_ms: u64 },
    Failed { reason: String },
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub submitted_at: DateTime<Utc>,
    pub priority: u8, // 0 = lowest, 255 = highest
    pub kind: TaskKind,
    pub requirements: TaskRequirements,
    pub status: TaskStatus,
    /// Arbitrary metadata from the submitter.
    pub meta: HashMap<String, String>,
}

impl Task {
    pub fn new(kind: TaskKind, requirements: TaskRequirements, priority: u8) -> Self {
        Self {
            id: Uuid::new_v4(),
            submitted_at: Utc::now(),
            priority,
            kind,
            requirements,
            status: TaskStatus::Queued,
            meta: HashMap::new(),
        }
    }
}

// ── Node ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCapabilities {
    /// Total logical CPUs.
    pub cpu_cores: u32,
    /// Total RAM in MB.
    pub total_ram_mb: u64,
    /// Whether a GPU is present.
    pub has_gpu: bool,
    /// Declared feature tags (e.g. ["python3", "ffmpeg", "docker"]).
    pub tags: Vec<String>,
    /// Geographic region label (e.g. "eu-west", "us-east").
    pub region: String,
    /// OS identifier (e.g. "linux-x86_64").
    pub os: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStats {
    /// CPU usage percentage (0-100).
    pub cpu_used_pct: f32,
    /// Free RAM in MB.
    pub ram_free_mb: u64,
    /// Current number of tasks being executed.
    pub active_tasks: u32,
    /// Round-trip latency to server in milliseconds (filled by server).
    pub latency_ms: Option<u64>,
    pub reported_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Online,
    Busy,
    Draining, // graceful shutdown in progress
    Offline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: Uuid,
    pub name: String,
    pub capabilities: NodeCapabilities,
    pub stats: NodeStats,
    pub status: NodeStatus,
    pub last_seen: DateTime<Utc>,
}

impl NodeInfo {
    /// Compute a score for this node against a task's requirements.
    /// Higher is better. Returns None if node cannot satisfy hard requirements.
    pub fn score_for(&self, req: &TaskRequirements) -> Option<f64> {
        // Hard constraints
        if self.status == NodeStatus::Offline || self.status == NodeStatus::Draining {
            return None;
        }
        let cpu_free_pct = 100.0 - self.stats.cpu_used_pct;
        if cpu_free_pct < req.min_cpu_free_pct {
            return None;
        }
        if self.stats.ram_free_mb < req.min_ram_free_mb {
            return None;
        }
        if req.requires_gpu && !self.capabilities.has_gpu {
            return None;
        }
        let tags_ok = req.required_tags.iter().all(|t| self.capabilities.tags.contains(t));
        if !tags_ok {
            return None;
        }

        // Weighted scoring (all components in [0, 1])
        let cpu_score = (cpu_free_pct / 100.0) as f64;
        let ram_max = self.capabilities.total_ram_mb as f64;
        let ram_score = if ram_max > 0.0 {
            (self.stats.ram_free_mb as f64 / ram_max).min(1.0)
        } else {
            0.0
        };
        let load_score = if self.stats.active_tasks == 0 {
            1.0
        } else {
            1.0 / (1.0 + self.stats.active_tasks as f64)
        };
        let latency_score = match self.stats.latency_ms {
            None => 0.5,
            Some(ms) => 1.0 - (ms as f64 / 500.0).min(1.0),
        };
        let region_bonus = match &req.preferred_region {
            Some(r) if r == &self.capabilities.region => 0.1,
            _ => 0.0,
        };

        let score = cpu_score * 0.35
            + ram_score * 0.25
            + load_score * 0.25
            + latency_score * 0.10
            + region_bonus * 0.05;

        Some(score)
    }
}

// ── WebSocket messages ───────────────────────────────────────────────────────

/// Messages sent FROM the worker CLI to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMessage {
    /// Initial registration on connect.
    Register {
        node_id: Uuid,
        name: String,
        capabilities: NodeCapabilities,
    },
    /// Periodic heartbeat with current stats.
    Heartbeat {
        node_id: Uuid,
        stats: NodeStats,
    },
    /// Task completed successfully.
    TaskResult {
        task_id: Uuid,
        node_id: Uuid,
        duration_ms: u64,
        output: TaskOutput,
    },
    /// Task failed.
    TaskError {
        task_id: Uuid,
        node_id: Uuid,
        reason: String,
    },
    /// Worker is about to shut down gracefully.
    Draining {
        node_id: Uuid,
    },
}

/// Messages sent FROM the server to the worker CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Acknowledge registration.
    Registered { node_id: Uuid },
    /// Dispatch a task to this worker.
    DispatchTask { task: Box<Task> },
    /// Cancel a previously dispatched task.
    CancelTask { task_id: Uuid },
    /// Server is shutting down.
    ServerShutdown,
}

// ── REST API types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitTaskRequest {
    pub kind: TaskKind,
    #[serde(default)]
    pub requirements: TaskRequirements,
    #[serde(default = "default_priority")]
    pub priority: u8,
    #[serde(default)]
    pub meta: HashMap<String, String>,
}

fn default_priority() -> u8 { 128 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitTaskResponse {
    pub task_id: Uuid,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStatusResponse {
    pub task: Task,
    pub result: Option<TaskOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodesResponse {
    pub nodes: Vec<NodeInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatusResponse {
    pub version: String,
    pub queued_tasks: usize,
    pub running_tasks: usize,
    pub completed_tasks: usize,
    pub online_nodes: usize,
}

// ── Task output ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutput {
    /// Exit code for shell tasks, HTTP status for fetches, etc.
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// For HTTP tasks, the response body.
    pub body: Option<Vec<u8>>,
    /// For custom tasks, an opaque result payload.
    pub custom_result: Option<Vec<u8>>,
}

impl TaskOutput {
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            exit_code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
            body: None,
            custom_result: None,
        }
    }

    pub fn failure(exit_code: i32, stderr: impl Into<String>) -> Self {
        Self {
            exit_code,
            stdout: String::new(),
            stderr: stderr.into(),
            body: None,
            custom_result: None,
        }
    }
}