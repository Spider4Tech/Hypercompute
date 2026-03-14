use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use chrono::Utc;
use futures::{sink::SinkExt, stream::StreamExt};
use hypercompute_proto::{
    NodeInfo, NodeStats, NodeStatus, TaskStatus, WorkerMessage,
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

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

    // Spawn a task to forward outgoing messages to the WebSocket.
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    let mut node_id: Option<Uuid> = None;

    while let Some(msg) = receiver.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(p)) => {
                // Pong is handled automatically by axum.
                let _ = p;
                continue;
            }
            Err(e) => {
                warn!("WebSocket error: {}", e);
                break;
            }
            _ => continue,
        };

        let worker_msg: WorkerMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                warn!("Invalid message from worker: {} — {}", e, text);
                continue;
            }
        };

        match worker_msg {
            WorkerMessage::Register { node_id: id, name, capabilities } => {
                info!("Worker registered: {} ({}) — region={}", id, name, capabilities.region);
                node_id = Some(id);
                state.worker_channels.insert(id, tx.clone());

                let stats = NodeStats {
                    cpu_used_pct: 0.0,
                    ram_free_mb: capabilities.total_ram_mb,
                    active_tasks: 0,
                    latency_ms: None,
                    reported_at: Utc::now(),
                };
                let info = NodeInfo {
                    id,
                    name,
                    capabilities,
                    stats,
                    status: NodeStatus::Online,
                    last_seen: Utc::now(),
                };
                state.nodes.insert(id, info);

                // Acknowledge.
                let ack = serde_json::to_string(
                    &hypercompute_proto::ServerMessage::Registered { node_id: id }
                ).unwrap();
                let _ = tx.send(ack);
            }

            WorkerMessage::Heartbeat { node_id: id, stats } => {
                if let Some(mut node) = state.nodes.get_mut(&id) {
                    node.stats = stats;
                    node.last_seen = Utc::now();
                    node.status = if node.stats.active_tasks > 0 {
                        NodeStatus::Busy
                    } else {
                        NodeStatus::Online
                    };
                }
            }

            WorkerMessage::TaskResult { task_id, node_id: nid, duration_ms, output } => {
                info!("Task {} completed by node {} in {}ms", task_id, nid, duration_ms);
                if let Some((_, mut task)) = state.running_tasks.remove(&task_id) {
                    task.status = TaskStatus::Completed { node_id: nid, duration_ms };
                    state.complete_task(task, Some(output)).await;
                }
                if let Some(mut node) = state.nodes.get_mut(&nid) {
                    node.stats.active_tasks = node.stats.active_tasks.saturating_sub(1);
                }
            }

            WorkerMessage::TaskError { task_id, node_id: nid, reason } => {
                warn!("Task {} failed on node {}: {}", task_id, nid, reason);
                if let Some((_, task)) = state.running_tasks.remove(&task_id) {
                    state.requeue(task).await;
                }
                if let Some(mut node) = state.nodes.get_mut(&nid) {
                    node.stats.active_tasks = node.stats.active_tasks.saturating_sub(1);
                }
            }

            WorkerMessage::Draining { node_id: id } => {
                info!("Node {} is draining", id);
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
        if let Some(mut node) = state.nodes.get_mut(&id) {
            node.status = NodeStatus::Offline;
        }

        // Requeue any tasks that were running on this node.
        let lost: Vec<_> = state.running_tasks.iter()
            .filter(|t| matches!(&t.status,
                TaskStatus::Dispatched { node_id } | TaskStatus::Running { node_id, .. }
                if node_id == &id))
            .map(|t| t.clone())
            .collect();

        for task in lost {
            warn!("Requeuing task {} after worker disconnect", task.id);
            state.requeue(task).await;
        }
    }

    send_task.abort();
}