use chrono::Utc;
use dashmap::DashMap;
use hypercompute_proto::{
    MpiRankOutput, NodeInfo, NodeStatus, Task, TaskOutput, TaskStatus,
};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};
use uuid::Uuid;

use crate::scheduler::MpiJobState;

pub type WorkerTx = mpsc::UnboundedSender<String>;

pub struct AppState {
    pub nodes: DashMap<Uuid, NodeInfo>,
    pub worker_channels: DashMap<Uuid, WorkerTx>,
    pub pending_tasks: Mutex<Vec<Task>>,
    pub running_tasks: DashMap<Uuid, Task>,
    pub completed_tasks: Mutex<Vec<(Task, Option<TaskOutput>, Option<Vec<MpiRankOutput>>)>>,
    pub retry_counts: DashMap<Uuid, u32>,
    /// Live MPI job aggregation: job_id → state.
    pub mpi_jobs: DashMap<Uuid, MpiJobState>,

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
            mpi_jobs: DashMap::new(),
            node_timeout_secs,
            task_timeout_secs,
            max_retries,
        }
    }

    pub async fn enqueue(&self, task: Task) {
        self.retry_counts.insert(task.id, self.max_retries);
        let mut q = self.pending_tasks.lock().await;
        q.push(task);
        q.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.submitted_at.cmp(&b.submitted_at)));
    }

    pub async fn requeue(&self, mut task: Task) {
        let retries = self.retry_counts.get(&task.id).map(|v| *v).unwrap_or(0);
        if retries == 0 {
            warn!("Task {} exhausted retries", task.id);
            task.status = TaskStatus::Failed { reason: "max retries exceeded".into() };
            self.running_tasks.remove(&task.id);
            self.complete_task(task, None, None).await;
            return;
        }
        self.retry_counts.insert(task.id, retries - 1);
        self.running_tasks.remove(&task.id);
        task.status = TaskStatus::Queued;
        self.enqueue(task).await;
    }

    pub async fn complete_task(
        &self,
        task: Task,
        output: Option<TaskOutput>,
        mpi_results: Option<Vec<MpiRankOutput>>,
    ) {
        self.running_tasks.remove(&task.id);
        self.retry_counts.remove(&task.id);
        let mut done = self.completed_tasks.lock().await;
        done.push((task, output, mpi_results));
        if done.len() > 10_000 { done.drain(0..1000); }
    }

    pub fn send_to_worker(&self, node_id: &Uuid, msg: &str) -> bool {
        if let Some(tx) = self.worker_channels.get(node_id) { tx.send(msg.to_string()).is_ok() }
        else { false }
    }
}

pub async fn monitor_nodes(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
    loop {
        interval.tick().await;
        let cutoff = Utc::now() - chrono::Duration::seconds(state.node_timeout_secs as i64);

        let dead: Vec<Uuid> = state.nodes.iter()
            .filter(|n| n.last_seen < cutoff && n.status != NodeStatus::Offline)
            .map(|n| *n.key()).collect();

        for id in dead {
            if let Some(mut node) = state.nodes.get_mut(&id) {
                warn!("Node {} ({}) timed out", id, node.name);
                node.status = NodeStatus::Offline;
            }

            // Requeue tasks dispatched to this node.
            let lost: Vec<Task> = state.running_tasks.iter()
                .filter(|t| matches!(&t.status,
                    TaskStatus::Dispatched { node_id } | TaskStatus::Running { node_id, .. }
                    if node_id == &id))
                .map(|t| t.clone()).collect();
            for task in lost { state.requeue(task).await; }

            // FIX: Use separate checks or map to common type to avoid E0271
            let affected_jobs: Vec<Uuid> = state.mpi_jobs.iter()
                .filter(|j| {
                    j.results.iter().any(|r| r.is_some()) || j.errors.iter().any(|e| e.is_some())
                })
                .map(|j| *j.key()).collect();

            // Drop or use affected_jobs as needed...
            drop(affected_jobs);

            let mpi_lost: Vec<Task> = state.running_tasks.iter()
                .filter(|t| matches!(&t.status, TaskStatus::MpiDispatched { node_ids, .. }
                    if node_ids.contains(&id)))
                .map(|t| t.clone()).collect();

            for task in mpi_lost {
                warn!("Requeuing MPI task {} due to node {} failure", task.id, id);
                state.mpi_jobs.remove(&task.id);
                state.requeue(task).await;
            }
        }

        let cutoff2 = Utc::now() - chrono::Duration::seconds(state.task_timeout_secs as i64);
        let timed_out: Vec<Task> = state.running_tasks.iter()
            .filter(|t| matches!(&t.status, TaskStatus::Running { started_at, .. } if started_at < &cutoff2))
            .map(|t| t.clone()).collect();
        for task in timed_out { state.requeue(task).await; }
    }
}