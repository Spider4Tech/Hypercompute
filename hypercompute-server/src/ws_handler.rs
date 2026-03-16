use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    State,
};
use axum::response::IntoResponse;
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use hypercompute_proto::{
    NodeInfo, NodeStats, NodeStatus, TaskStatus, WorkerMessage,
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::scheduler::abort_mpi_job;
use crate::state::AppState;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(Message::Text(msg)).await.is_err() { break; }
        }
    });

    let mut node_id: Option<Uuid> = None;

    while let Some(msg) = receiver.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) => break,
            Err(e) => { warn!("WS error: {}", e); break; }
            _ => continue,
        };

        let wm: WorkerMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => { warn!("Bad worker message: {}", e); continue; }
        };

        match wm {
            // ── Registration ──────────────────────────────────────────────────
            WorkerMessage::Register { node_id: id, name, capabilities } => {
                info!("Worker registered: {} ({})  mpi_host={}  mpirun={}",
                    id, name, capabilities.mpi_host, capabilities.has_mpirun);
                node_id = Some(id);
                state.worker_channels.insert(id, tx.clone());

                let stats = NodeStats {
                    cpu_used_pct: 0.0,
                    ram_free_mb: capabilities.total_ram_mb,
                    active_tasks: 0,
                    latency_ms: None,
                    reported_at: Utc::now(),
                };
                state.nodes.insert(id, NodeInfo {
                    id, name, capabilities, stats,
                    status: NodeStatus::Online,
                    last_seen: Utc::now(),
                });
                let ack = serde_json::to_string(
                    &hypercompute_proto::ServerMessage::Registered { node_id: id }
                ).unwrap();
                let _ = tx.send(ack);
            }

            // ── Heartbeat ─────────────────────────────────────────────────────
            WorkerMessage::Heartbeat { node_id: id, stats } => {
                if let Some(mut node) = state.nodes.get_mut(&id) {
                    node.stats = stats;
                    node.last_seen = Utc::now();
                    node.status = if node.stats.active_tasks > 0 { NodeStatus::Busy } else { NodeStatus::Online };
                }
            }

            // ── Standard task result ──────────────────────────────────────────
            WorkerMessage::TaskResult { task_id, node_id: nid, duration_ms, output } => {
                info!("Task {} completed by node {} in {}ms", task_id, nid, duration_ms);
                if let Some((_, mut task)) = state.running_tasks.remove(&task_id) {
                    task.status = TaskStatus::Completed { node_id: nid, duration_ms };
                    state.complete_task(task, Some(output), None).await;
                }
                decrement_active(&state, &nid);
            }

            WorkerMessage::TaskError { task_id, node_id: nid, reason } => {
                warn!("Task {} failed on node {}: {}", task_id, nid, reason);
                if let Some((_, task)) = state.running_tasks.remove(&task_id) {
                    state.requeue(task).await;
                }
                decrement_active(&state, &nid);
            }

            // ── MPI rank result ───────────────────────────────────────────────
            WorkerMessage::MpiRankResult { job_id, task_id, node_id: nid, rank, duration_ms, output } => {
                info!("MPI rank {}/{} result for job {} (node {} {}ms)", rank, job_id, job_id, nid, duration_ms);
                decrement_active(&state, &nid);

                let complete = {
                    let mut job = match state.mpi_jobs.get_mut(&job_id) {
                        Some(j) => j,
                        None => { warn!("Unknown MPI job {}", job_id); continue; }
                    };
                    job.record_result(rank, output, nid);
                    job.is_complete()
                };

                if complete {
                    finalize_mpi_job(&state, job_id, task_id).await;
                }
            }

            WorkerMessage::MpiRankError { job_id, task_id, node_id: nid, rank, reason } => {
                warn!("MPI rank {} error for job {}: {}", rank, job_id, reason);
                decrement_active(&state, &nid);

                let complete = {
                    let mut job = match state.mpi_jobs.get_mut(&job_id) {
                        Some(j) => j,
                        None => { warn!("Unknown MPI job {}", job_id); continue; }
                    };
                    job.record_error(rank, reason.clone());
                    job.is_complete()
                };

                if complete {
                    finalize_mpi_job(&state, job_id, task_id).await;
                } else {
                    // Abort remaining ranks immediately — no point waiting.
                    abort_mpi_job(&state, job_id, &format!("rank {} failed: {}", rank, reason));
                }
            }

            WorkerMessage::Draining { node_id: id } => {
                info!("Node {} draining", id);
                if let Some(mut node) = state.nodes.get_mut(&id) {
                    node.status = NodeStatus::Draining;
                }
            }
        }
    }

    // Cleanup on disconnect.
    if let Some(id) = node_id {
        info!("Worker {} disconnected", id);
        state.worker_channels.remove(&id);
        if let Some(mut node) = state.nodes.get_mut(&id) { node.status = NodeStatus::Offline; }

        let lost: Vec<_> = state.running_tasks.iter()
            .filter(|t| matches!(&t.status,
                TaskStatus::Dispatched { node_id } | TaskStatus::Running { node_id, .. }
                if node_id == &id))
            .map(|t| t.clone()).collect();
        for task in lost { state.requeue(task).await; }

        let mpi_lost: Vec<_> = state.running_tasks.iter()
            .filter(|t| matches!(&t.status,
                TaskStatus::MpiDispatched { node_ids, .. } if node_ids.contains(&id)))
            .map(|t| t.clone()).collect();
        for task in mpi_lost {
            warn!("Requeuing MPI task {} (node {} disconnected)", task.id, id);
            state.mpi_jobs.remove(&task.id);
            state.requeue(task).await;
        }
    }

    send_task.abort();
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn decrement_active(state: &Arc<AppState>, node_id: &Uuid) {
    if let Some(mut node) = state.nodes.get_mut(node_id) {
        node.stats.active_tasks = node.stats.active_tasks.saturating_sub(1);
    }
}

async fn finalize_mpi_job(state: &Arc<AppState>, job_id: Uuid, task_id: Uuid) {
    let job_state = match state.mpi_jobs.remove(&job_id) {
        Some((_, j)) => j,
        None => return,
    };

    let duration_ms   = job_state.duration_ms();
    let ranks_ok      = job_state.ranks_ok();
    let ranks_failed  = job_state.ranks_failed();
    let mpi_results   = job_state.into_rank_outputs();

    info!("MPI job {} finalized: {} ok, {} failed, {}ms", job_id, ranks_ok, ranks_failed, duration_ms);

    if let Some((_, mut task)) = state.running_tasks.remove(&task_id) {
        task.status = TaskStatus::MpiCompleted { job_id, duration_ms, ranks_ok, ranks_failed };
        state.complete_task(task, None, Some(mpi_results)).await;
    }
}