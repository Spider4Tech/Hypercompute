/// hypercompute-proto: shared protocol types between server and CLI workers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

// ── Task ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// Run a shell command on the remote node.
    Shell { command: String, args: Vec<String>, timeout_secs: u64 },
    /// Fetch a URL and return the body.
    HttpFetch { url: String, method: String, body: Option<String> },
    /// Custom opaque payload; worker must know how to handle it by `tag`.
    Custom { tag: String, payload: Vec<u8> },
    /// MPI parallel job distributed across `np` nodes simultaneously.
    ///
    /// The server selects `np` eligible nodes, assigns each a rank, then
    /// sends an `MpiSlot` alongside this task to every recruited node.
    /// Each worker launches the program with its rank injected via env vars:
    ///   HC_MPI_RANK, HC_MPI_SIZE, HC_MPI_JOB_ID, HC_MPI_PEERS (JSON array).
    ///
    /// When `use_mpirun = true`, rank-0 invokes `mpirun --host <peers> …`
    /// and the result is collected from rank-0 only.
    /// When `use_mpirun = false` (pure-Rust / custom mode), each worker runs
    /// independently and reports back; the server aggregates all outputs.
    Mpi {
        /// Executable to run on every rank (must exist on each worker node).
        program: String,
        /// Arguments forwarded to the program unchanged.
        args: Vec<String>,
        /// Total number of MPI processes (= nodes recruited).
        np: u32,
        /// Extra environment variables injected on every rank.
        env: HashMap<String, String>,
        /// Wall-clock timeout in seconds for the whole job.
        timeout_secs: u64,
        /// Use system mpirun/mpiexec (requires OpenMPI or MPICH on workers).
        use_mpirun: bool,
    },
}

/// One node's assignment inside an MPI job.
/// Sent together with the `Task` in `ServerMessage::DispatchMpiSlot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MpiSlot {
    /// This node's rank (0 = root / result collector).
    pub rank: u32,
    /// Total number of ranks in the job.
    pub size: u32,
    /// Ordered peer list: index == rank. Each entry carries host+port+node_id.
    pub peers: Vec<MpiPeer>,
    /// Shared job UUID — identical for every slot of the same MPI job.
    pub job_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MpiPeer {
    /// Hostname or IP reachable from other nodes.
    pub host: String,
    /// TCP port that this peer's MPI bootstrap listener is bound to.
    pub port: u16,
    pub node_id: Uuid,
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
    /// For MPI tasks: minimum number of slots (logical CPUs) per node.
    pub mpi_slots_per_node: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Dispatched { node_id: Uuid },
    /// MPI jobs are dispatched to multiple nodes simultaneously.
    MpiDispatched { job_id: Uuid, node_ids: Vec<Uuid> },
    Running { node_id: Uuid, started_at: DateTime<Utc> },
    MpiRunning { job_id: Uuid, started_at: DateTime<Utc>, total_ranks: u32 },
    Completed { node_id: Uuid, duration_ms: u64 },
    /// MPI job completed: rank-0 stdout + per-rank exit codes.
    MpiCompleted { job_id: Uuid, duration_ms: u64, ranks_ok: u32, ranks_failed: u32 },
    Failed { reason: String },
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub submitted_at: DateTime<Utc>,
    pub priority: u8,
    pub kind: TaskKind,
    pub requirements: TaskRequirements,
    pub status: TaskStatus,
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

    pub fn is_mpi(&self) -> bool {
        matches!(self.kind, TaskKind::Mpi { .. })
    }
}

// ── Node ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCapabilities {
    pub cpu_cores: u32,
    pub total_ram_mb: u64,
    pub has_gpu: bool,
    pub tags: Vec<String>,
    pub region: String,
    pub os: String,
    /// Host/IP reachable from other cluster nodes (for MPI peer connections).
    pub mpi_host: String,
    /// Whether mpirun/mpiexec is available on this node.
    pub has_mpirun: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStats {
    pub cpu_used_pct: f32,
    pub ram_free_mb: u64,
    pub active_tasks: u32,
    pub latency_ms: Option<u64>,
    pub reported_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Online,
    Busy,
    Draining,
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
    /// Score this node for a task. Returns None if hard requirements unmet.
    pub fn score_for(&self, req: &TaskRequirements) -> Option<f64> {
        if self.status == NodeStatus::Offline || self.status == NodeStatus::Draining {
            return None;
        }
        let cpu_free_pct = 100.0 - self.stats.cpu_used_pct;
        if cpu_free_pct < req.min_cpu_free_pct { return None; }
        if self.stats.ram_free_mb < req.min_ram_free_mb { return None; }
        if req.requires_gpu && !self.capabilities.has_gpu { return None; }
        let tags_ok = req.required_tags.iter().all(|t| self.capabilities.tags.contains(t));
        if !tags_ok { return None; }

        let cpu_score   = (cpu_free_pct / 100.0) as f64;
        let ram_max     = self.capabilities.total_ram_mb as f64;
        let ram_score   = if ram_max > 0.0 { (self.stats.ram_free_mb as f64 / ram_max).min(1.0) } else { 0.0 };
        let load_score  = if self.stats.active_tasks == 0 { 1.0 } else { 1.0 / (1.0 + self.stats.active_tasks as f64) };
        let lat_score   = match self.stats.latency_ms { None => 0.5, Some(ms) => 1.0 - (ms as f64 / 500.0).min(1.0) };
        let region_bonus = match &req.preferred_region { Some(r) if r == &self.capabilities.region => 0.1, _ => 0.0 };

        Some(cpu_score * 0.35 + ram_score * 0.25 + load_score * 0.25 + lat_score * 0.10 + region_bonus * 0.05)
    }
}

// ── WebSocket messages ────────────────────────────────────────────────────────

/// Messages FROM worker → server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMessage {
    Register    { node_id: Uuid, name: String, capabilities: NodeCapabilities },
    Heartbeat   { node_id: Uuid, stats: NodeStats },
    TaskResult  { task_id: Uuid, node_id: Uuid, duration_ms: u64, output: TaskOutput },
    TaskError   { task_id: Uuid, node_id: Uuid, reason: String },
    /// Report from one MPI rank once it finishes.
    MpiRankResult {
        job_id: Uuid,
        task_id: Uuid,
        node_id: Uuid,
        rank: u32,
        duration_ms: u64,
        output: TaskOutput,
    },
    MpiRankError {
        job_id: Uuid,
        task_id: Uuid,
        node_id: Uuid,
        rank: u32,
        reason: String,
    },
    Draining    { node_id: Uuid },
}

/// Messages FROM server → worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Registered    { node_id: Uuid },
    DispatchTask  { task: Task },
    /// Dispatch an MPI slot: same task + this node's rank assignment.
    DispatchMpiSlot { task: Task, slot: MpiSlot },
    CancelTask    { task_id: Uuid },
    /// Abort an MPI job on this node (another rank failed).
    AbortMpiJob   { job_id: Uuid, reason: String },
    ServerShutdown,
}

// ── REST API types ─────────────────────────────────────────────────────────────

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
    /// For MPI jobs: per-rank outputs.
    pub mpi_results: Option<Vec<MpiRankOutput>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MpiRankOutput {
    pub rank: u32,
    pub node_id: Uuid,
    pub output: TaskOutput,
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
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub body: Option<Vec<u8>>,
    pub custom_result: Option<Vec<u8>>,
}

impl TaskOutput {
    pub fn success(stdout: impl Into<String>) -> Self {
        Self { exit_code: 0, stdout: stdout.into(), stderr: String::new(), body: None, custom_result: None }
    }
    pub fn failure(exit_code: i32, stderr: impl Into<String>) -> Self {
        Self { exit_code, stdout: String::new(), stderr: stderr.into(), body: None, custom_result: None }
    }
}