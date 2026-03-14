use chrono::Utc;
use dashmap::DashMap;
use hypercompute_proto::{NodeInfo, NodeStatus, Task, TaskOutput, TaskStatus};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};
use uuid::Uuid;

/// A channel to send ServerMessage to a connected worker.
pub type WorkerTx = mpsc::UnboundedSender<String>;

pub struct AppState {
    /// All registered nodes, keyed by node_id.
    pub nodes: DashMap<Uuid, NodeInfo>,
    /// WebSocket sender channels for each connected worker.
    pub worker_channels: DashMap<Uuid, WorkerTx>,
    /// Pending (queued) tasks, protected by a Mutex to allow priority sorting.
    pub pending_tasks: Mutex<Vec<Task>>,
    /// Tasks currently dispatched or running, keyed by task_id.
    pub running_tasks: DashMap<Uuid, Task>,
    /// Completed tasks (success or failure), capped in memory.
    pub completed_tasks: Mutex<Vec<(Task, Option<TaskOutput>)>>,
    /// Track how many retries remain per task.
    pub retry_counts: DashMap<Uuid, u32>,

    pub node_timeout_secs: u64,
    pub task_timeout_secs: u64,
    pub max_retries: u32,
}

impl AppState {
    pub fn new(node_timeout_secs: u64, task_timeout_secs: u64, max_retries: u32) -> Self {
        Self {
            nodes: DashMap::new(),
            worker_channels: DashMap::new(),
            pending_tasks: Mutex::new(Vec::new()),
            running_tasks: DashMap::new(),
            completed_tasks: Mutex::new(Vec::new()),
            retry_counts: DashMap::new(),
            node_timeout_secs,
            task_timeout_secs,
            max_retries,
        }
    }

    /// Enqueue a new task.
    pub async fn enqueue(&self, task: Task) {
        self.retry_counts.insert(task.id, self.max_retries);
        let mut q = self.pending_tasks.lock().await;
        q.push(task);
        // Keep highest priority first.
        q.sort_by(|a, b| b.priority.cmp(&a.priority)
            .then(a.submitted_at.cmp(&b.submitted_at)));
    }

    /// Requeue a task (on failure, if retries remain).
    pub async fn requeue(&self, mut task: Task) {
        let retries = self.retry_counts.get(&task.id).map(|v| *v).unwrap_or(0);
        if retries == 0 {
            warn!("Task {} exhausted retries, marking failed", task.id);
            task.status = TaskStatus::Failed { reason: "max retries exceeded".into() };
            self.running_tasks.remove(&task.id);
            self.complete_task(task, None).await;
            return;
        }
        self.retry_counts.insert(task.id, retries - 1);
        self.running_tasks.remove(&task.id);
        task.status = TaskStatus::Queued;
        self.enqueue(task).await;
    }

    pub async fn complete_task(&self, task: Task, output: Option<TaskOutput>) {
        self.running_tasks.remove(&task.id);
        self.retry_counts.remove(&task.id);
        let mut done = self.completed_tasks.lock().await;
        done.push((task, output));
        // Keep last 10 000 completed tasks in memory.
        if done.len() > 10_000 {
            done.drain(0..1000);
        }
    }

    /// Send a JSON message to a specific worker.
    pub fn send_to_worker(&self, node_id: &Uuid, msg: &str) -> bool {
        if let Some(tx) = self.worker_channels.get(node_id) {
            tx.send(msg.to_string()).is_ok()
        } else {
            false
        }
    }
}

/// Background task: periodically evict nodes that haven't sent a heartbeat.
pub async fn monitor_nodes(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(
        tokio::time::Duration::from_secs(10)
    );
    loop {
        interval.tick().await;
        let cutoff = Utc::now()
            - chrono::Duration::seconds(state.node_timeout_secs as i64);

        let dead: Vec<Uuid> = state.nodes.iter()
            .filter(|n| n.last_seen < cutoff && n.status != NodeStatus::Offline)
            .map(|n| *n.key())
            .collect();

        for id in dead {
            if let Some(mut node) = state.nodes.get_mut(&id) {
                warn!("Node {} ({}) went offline (heartbeat timeout)", id, node.name);
                node.status = NodeStatus::Offline;
            }
            // Requeue any tasks dispatched to this node.
            let lost: Vec<Task> = state.running_tasks.iter()
                .filter(|t| matches!(&t.status,
                    TaskStatus::Dispatched { node_id } | TaskStatus::Running { node_id, .. }
                    if node_id == &id))
                .map(|t| t.clone())
                .collect();

            for task in lost {
                info!("Requeuing task {} due to node {} failure", task.id, id);
                state.requeue(task).await;
            }
        }

        // Also check for timed-out running tasks.
        let cutoff = Utc::now()
            - chrono::Duration::seconds(state.task_timeout_secs as i64);
        let timed_out: Vec<Task> = state.running_tasks.iter()
            .filter(|t| {
                if let TaskStatus::Running { started_at, .. } = &t.status {
                    started_at < &cutoff
                } else {
                    false
                }
            })
            .map(|t| t.clone())
            .collect();

        for task in timed_out {
            warn!("Task {} timed out", task.id);
            state.requeue(task).await;
        }
    }
}