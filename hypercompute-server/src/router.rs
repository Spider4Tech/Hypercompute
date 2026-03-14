use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use hypercompute_proto::{
    NodesResponse, ServerStatusResponse, SubmitTaskRequest, SubmitTaskResponse,
    Task, TaskStatusResponse,
};
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

use crate::{state::AppState, ws_handler::ws_handler};

pub fn build(state: Arc<AppState>) -> Router {
    Router::new()
        // Worker WebSocket endpoint
        .route("/ws", get(ws_handler))
        // REST API
        .route("/api/v1/tasks", post(submit_task))
        .route("/api/v1/tasks/:id", get(get_task))
        .route("/api/v1/tasks/:id/cancel", post(cancel_task))
        .route("/api/v1/nodes", get(list_nodes))
        .route("/api/v1/status", get(server_status))
        .route("/health", get(health))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// POST /api/v1/tasks — submit a new task.
async fn submit_task(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SubmitTaskRequest>,
) -> impl IntoResponse {
    let task = Task::new(req.kind, req.requirements, req.priority);
    let resp = SubmitTaskResponse {
        task_id: task.id,
        status: task.status.clone(),
    };
    state.enqueue(task).await;
    (StatusCode::ACCEPTED, Json(resp))
}

/// GET /api/v1/tasks/:id — query task status and result.
async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    // Check pending queue.
    {
        let q = state.pending_tasks.lock().await;
        if let Some(task) = q.iter().find(|t| t.id == id) {
            return Ok(Json(TaskStatusResponse { task: task.clone(), result: None }));
        }
    }

    // Check running.
    if let Some(task) = state.running_tasks.get(&id) {
        return Ok(Json(TaskStatusResponse { task: task.clone(), result: None }));
    }

    // Check completed.
    {
        let done = state.completed_tasks.lock().await;
        if let Some((task, output)) = done.iter().find(|(t, _)| t.id == id) {
            return Ok(Json(TaskStatusResponse {
                task: task.clone(),
                result: output.clone(),
            }));
        }
    }

    Err(StatusCode::NOT_FOUND)
}

/// POST /api/v1/tasks/:id/cancel — cancel a queued task.
async fn cancel_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let mut q = state.pending_tasks.lock().await;
    if let Some(pos) = q.iter().position(|t| t.id == id) {
        q.remove(pos);
        return StatusCode::OK;
    }
    StatusCode::NOT_FOUND
}

/// GET /api/v1/nodes — list all known nodes.
async fn list_nodes(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let nodes = state.nodes.iter().map(|n| n.clone()).collect();
    Json(NodesResponse { nodes })
}

/// GET /api/v1/status — server-wide statistics.
async fn server_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let queued = state.pending_tasks.lock().await.len();
    let running = state.running_tasks.len();
    let completed = state.completed_tasks.lock().await.len();
    let online = state.nodes.iter()
        .filter(|n| matches!(n.status, hypercompute_proto::NodeStatus::Online | hypercompute_proto::NodeStatus::Busy))
        .count();

    Json(ServerStatusResponse {
        version: env!("CARGO_PKG_VERSION").into(),
        queued_tasks: queued,
        running_tasks: running,
        completed_tasks: completed,
        online_nodes: online,
    })
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}