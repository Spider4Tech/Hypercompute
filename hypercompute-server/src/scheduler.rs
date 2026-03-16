use hypercompute_proto::{
    MpiPeer, MpiSlot, NodeStatus, ServerMessage, TaskKind, TaskRequirements, TaskStatus,
};
use std::sync::Arc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::state::AppState;

// ── Main loop ─────────────────────────────────────────────────────────────────

pub async fn run(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(200));
    loop {
        interval.tick().await;
        dispatch_pending(&state).await;
    }
}

async fn dispatch_pending(state: &Arc<AppState>) {
    let mut queue = state.pending_tasks.lock().await;
    if queue.is_empty() { return; }

    let mut dispatched = Vec::new();

    'outer: for (idx, task) in queue.iter_mut().enumerate() {
        if task.is_mpi() {
            // MPI path: recruit np nodes simultaneously.
            let np = match &task.kind {
                TaskKind::Mpi { np, .. } => *np,
                _ => unreachable!(),
            };
            match recruit_mpi_nodes(state, &task.requirements, np) {
                None => {
                    debug!("MPI task {}: not enough eligible nodes ({} needed)", task.id, np);
                }
                Some(nodes) => {
                    let job_id = task.id; // reuse task id as job id for simplicity
                    let ok = dispatch_mpi_job(state, task, &nodes, job_id);
                    if ok { dispatched.push(idx); }
                }
            }
        } else {
            // Standard path.
            match score_nodes_for_task(state, &task.requirements) {
                None => debug!("No eligible node for task {}", task.id),
                Some((node_id, score)) => {
                    let msg = ServerMessage::DispatchTask { task: task.clone() };
                    let json = match serde_json::to_string(&msg) {
                        Ok(j) => j,
                        Err(e) => { warn!("Serialize error: {}", e); continue 'outer; }
                    };
                    if state.send_to_worker(&node_id, &json) {
                        info!("Dispatched task {} → node {} (score={:.3})", task.id, node_id, score);
                        task.status = TaskStatus::Dispatched { node_id };
                        if let Some(mut node) = state.nodes.get_mut(&node_id) {
                            node.stats.active_tasks += 1;
                        }
                        state.running_tasks.insert(task.id, task.clone());
                        dispatched.push(idx);
                    } else {
                        warn!("Worker channel dead for node {}", node_id);
                        if let Some(mut node) = state.nodes.get_mut(&node_id) {
                            node.status = NodeStatus::Offline;
                        }
                    }
                }
            }
        }
    }

    for idx in dispatched.into_iter().rev() { queue.remove(idx); }
}

// ── MPI dispatch ──────────────────────────────────────────────────────────────

/// Select `np` best distinct nodes for an MPI job.
fn recruit_mpi_nodes(
    state: &AppState,
    req: &TaskRequirements,
    np: u32,
) -> Option<Vec<(Uuid, f64)>> {
    let mut scored: Vec<(Uuid, f64)> = state.nodes.iter()
        .filter_map(|e| e.value().score_for(req).map(|s| (*e.key(), s)))
        .collect();

    if scored.len() < np as usize { return None; }

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Some(scored.into_iter().take(np as usize).collect())
}

/// Build peer list and send DispatchMpiSlot to every recruited node.
/// Returns true if all sends succeeded (false = partial failure, already logged).
fn dispatch_mpi_job(
    state: &AppState,
    task: &mut hypercompute_proto::Task,
    nodes: &[(Uuid, f64)],
    job_id: Uuid,
) -> bool {
    let size = nodes.len() as u32;

    // Build the peer list (rank order == nodes order).
    let peers: Vec<MpiPeer> = nodes.iter().enumerate().map(|(rank, (node_id, _))| {
        let host = state.nodes.get(node_id)
            .map(|n| n.capabilities.mpi_host.clone())
            .unwrap_or_else(|| "localhost".to_string());
        // Base port 9900 + rank as simple port assignment.
        let port = 9900u16 + rank as u16;
        MpiPeer { host, port, node_id: *node_id }
    }).collect();

    let node_ids: Vec<Uuid> = nodes.iter().map(|(id, _)| *id).collect();

    let mut all_ok = true;
    for (rank, (node_id, score)) in nodes.iter().enumerate() {
        let slot = MpiSlot {
            rank: rank as u32,
            size,
            peers: peers.clone(),
            job_id,
        };
        let msg = ServerMessage::DispatchMpiSlot { task: task.clone(), slot };
        let json = match serde_json::to_string(&msg) {
            Ok(j) => j,
            Err(e) => { warn!("MPI serialize error rank {}: {}", rank, e); all_ok = false; continue; }
        };
        if state.send_to_worker(node_id, &json) {
            info!("MPI job {} rank {}/{} → node {} (score={:.3})", job_id, rank, size, node_id, score);
            if let Some(mut node) = state.nodes.get_mut(node_id) {
                node.stats.active_tasks += 1;
            }
        } else {
            warn!("MPI: worker channel dead for node {} (rank {})", node_id, rank);
            if let Some(mut node) = state.nodes.get_mut(node_id) {
                node.status = NodeStatus::Offline;
            }
            all_ok = false;
        }
    }

    if all_ok {
        task.status = TaskStatus::MpiDispatched { job_id, node_ids };
        state.running_tasks.insert(task.id, task.clone());
        // Register the MPI job in the aggregator.
        state.mpi_jobs.insert(job_id, MpiJobState::new(task.id, size));
    } else {
        // Abort any that did receive the dispatch.
        abort_mpi_job(state, job_id, "one or more ranks failed to dispatch");
    }
    all_ok
}

/// Send AbortMpiJob to all nodes that are part of this job.
pub fn abort_mpi_job(state: &AppState, job_id: Uuid, reason: &str) {
    let msg = ServerMessage::AbortMpiJob { job_id, reason: reason.into() };
    if let Ok(json) = serde_json::to_string(&msg) {
        for entry in state.nodes.iter() {
            state.send_to_worker(entry.key(), &json);
        }
    }
}

// ── Standard node scoring ─────────────────────────────────────────────────────

pub fn score_nodes_for_task(
    state: &AppState,
    requirements: &TaskRequirements,
) -> Option<(Uuid, f64)> {
    state.nodes.iter()
        .filter_map(|e| e.value().score_for(requirements).map(|s| (*e.key(), s)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
}

// ── MPI job aggregation state ─────────────────────────────────────────────────

use hypercompute_proto::{MpiRankOutput, TaskOutput};

#[derive(Debug, Clone)]
pub struct MpiJobState {
    pub task_id: Uuid,
    pub total_ranks: u32,
    pub results: Vec<Option<(TaskOutput, Uuid)>>, // indexed by rank
    pub errors: Vec<Option<String>>,
    pub started_at: std::time::Instant,
}

impl MpiJobState {
    pub fn new(task_id: Uuid, total_ranks: u32) -> Self {
        Self {
            task_id,
            total_ranks,
            results: vec![None; total_ranks as usize],
            errors:  vec![None; total_ranks as usize],
            started_at: std::time::Instant::now(),
        }
    }

    pub fn record_result(&mut self, rank: u32, output: TaskOutput, node_id: Uuid) {
        if let Some(slot) = self.results.get_mut(rank as usize) {
            *slot = Some((output, node_id));
        }
    }

    pub fn record_error(&mut self, rank: u32, reason: String) {
        if let Some(slot) = self.errors.get_mut(rank as usize) {
            *slot = Some(reason);
        }
    }

    pub fn is_complete(&self) -> bool {
        self.results.iter().zip(self.errors.iter())
            .all(|(r, e)| r.is_some() || e.is_some())
    }

    pub fn duration_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    pub fn ranks_ok(&self) -> u32 {
        self.results.iter().filter(|r| r.is_some()).count() as u32
    }

    pub fn ranks_failed(&self) -> u32 {
        self.errors.iter().filter(|e| e.is_some()).count() as u32
    }

    pub fn into_rank_outputs(self) -> Vec<MpiRankOutput> {
        self.results.into_iter().enumerate()
            .filter_map(|(rank, opt)| opt.map(|(output, node_id)| MpiRankOutput {
                rank: rank as u32, node_id, output,
            }))
            .collect()
    }
}