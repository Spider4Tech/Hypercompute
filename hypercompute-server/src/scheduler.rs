use hypercompute_proto::{NodeStatus, ServerMessage, TaskRequirements, TaskStatus};
use std::sync::Arc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::state::AppState;

/// Main scheduler loop: dequeues tasks and dispatches them to the best node.
pub async fn run(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(200));
    loop {
        interval.tick().await;
        dispatch_pending(&state).await;
    }
}

async fn dispatch_pending(state: &Arc<AppState>) {
    let mut queue = state.pending_tasks.lock().await;
    if queue.is_empty() {
        return;
    }

    let mut dispatched_indices = Vec::new();

    'outer: for (idx, task) in queue.iter_mut().enumerate() {
        let best = score_nodes_for_task(state, &task.requirements);

        if let Some((node_id, score)) = best {
            let msg = ServerMessage::DispatchTask { task: task.clone() };
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(e) => {
                    warn!("Serialize error: {}", e);
                    continue 'outer;
                }
            };

            if state.send_to_worker(&node_id, &json) {
                info!("Dispatched task {} → node {} (score={:.3})", task.id, node_id, score);
                task.status = TaskStatus::Dispatched { node_id };
                if let Some(mut node) = state.nodes.get_mut(&node_id) {
                    node.stats.active_tasks += 1;
                }
                state.running_tasks.insert(task.id, task.clone());
                dispatched_indices.push(idx);
            } else {
                warn!("Worker channel dead for node {}, marking offline", node_id);
                if let Some(mut node) = state.nodes.get_mut(&node_id) {
                    node.status = NodeStatus::Offline;
                }
            }
        } else {
            debug!("No eligible node for task {} (requirements not met or no nodes online)", task.id);
        }
    }

    for idx in dispatched_indices.into_iter().rev() {
        queue.remove(idx);
    }
}

/// Returns the (node_id, score) of the best available node for a task's requirements.
pub fn score_nodes_for_task(
    state: &AppState,
    requirements: &TaskRequirements,
) -> Option<(Uuid, f64)> {
    state
        .nodes
        .iter()
        .filter_map(|entry| {
            let node = entry.value();
            node.score_for(requirements).map(|s| (*entry.key(), s))
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
}