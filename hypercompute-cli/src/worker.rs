use clap::Args;
use futures::{SinkExt, StreamExt};
use hypercompute_proto::{
    MpiPeer, NodeCapabilities, NodeStats, ServerMessage, WorkerMessage,
};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use uuid::Uuid;
use anyhow::Result;

use crate::executor;

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct WorkerArgs {
    /// Human-readable name (defaults to hostname).
    #[arg(long)]
    name: Option<String>,

    /// Capability tags (comma-separated).
    #[arg(long, value_delimiter = ',')]
    tags: Vec<String>,

    /// Geographic region label.
    #[arg(long, env = "HC_REGION", default_value = "default")]
    region: String,

    /// Heartbeat interval in seconds.
    #[arg(long, default_value_t = 10)]
    heartbeat_secs: u64,

    /// Maximum concurrent tasks.
    #[arg(long, default_value_t = 4)]
    max_tasks: u32,

    /// Host/IP that other cluster nodes can reach this machine on (for MPI).
    /// Defaults to the machine's hostname.
    #[arg(long, env = "HC_MPI_HOST")]
    mpi_host: Option<String>,

    /// Base TCP port for MPI bootstrap listeners. Ranks on this node use
    /// ports starting here (mpi_port + rank_local_offset).
    #[arg(long, env = "HC_MPI_PORT", default_value_t = 9900)]
    mpi_port: u16,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(server: String, args: WorkerArgs) -> Result<()> {
    let node_id = Uuid::new_v4();
    let name = args.name.clone().unwrap_or_else(|| {
        hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| format!("node-{}", &node_id.to_string()[..8]))
    });

    let capabilities = build_capabilities(&args, &name);

    let ws_url = server
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1)
        + "/ws";

    info!("Connecting to {} as '{}' ({})", ws_url, name, node_id);
    info!("MPI host: {}  base port: {}", capabilities.mpi_host, args.mpi_port);
    if capabilities.has_mpirun {
        info!("mpirun/mpiexec detected — mpirun mode available");
    } else {
        info!("No mpirun detected — pure-Rust MPI mode only");
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let s = Arc::clone(&shutdown);
        ctrlc::set_handler(move || {
            info!("Shutdown requested — draining…");
            s.store(true, Ordering::SeqCst);
        })?;
    }

    let mut backoff = Duration::from_secs(1);
    loop {
        match connect_async(&ws_url).await {
            Ok((stream, _)) => {
                backoff = Duration::from_secs(1);
                info!("Connected");
                run_session(stream, node_id, name.clone(), capabilities.clone(),
                            args.heartbeat_secs, args.max_tasks, Arc::clone(&shutdown)).await;
            }
            Err(e) => {
                if shutdown.load(Ordering::SeqCst) { break; }
                warn!("Connection failed: {} — retry in {}s", e, backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
        if shutdown.load(Ordering::SeqCst) { break; }
    }
    info!("Worker shut down cleanly");
    Ok(())
}

// ── Session ───────────────────────────────────────────────────────────────────

async fn run_session(
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>
    >,
    node_id: Uuid,
    name: String,
    capabilities: NodeCapabilities,
    heartbeat_secs: u64,
    max_tasks: u32,
    shutdown: Arc<AtomicBool>,
) {
    let (mut ws_tx, mut ws_rx) = stream.split();
    let active = Arc::new(AtomicU32::new(0));
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Outbound pump.
    let pump = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_tx.send(Message::Text(msg)).await.is_err() { break; }
        }
    });

    // Register.
    send_json(&out_tx, &WorkerMessage::Register {
        node_id, name: name.clone(), capabilities: capabilities.clone(),
    });

    // Heartbeat.
    let hb = {
        let tx = out_tx.clone();
        let caps = capabilities.clone();
        let active = Arc::clone(&active);
        let sd = Arc::clone(&shutdown);
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(heartbeat_secs));
            loop {
                ticker.tick().await;
                let stats = collect_stats(active.load(Ordering::SeqCst));
                send_json(&tx, &WorkerMessage::Heartbeat { node_id, stats });
                if sd.load(Ordering::SeqCst) {
                    send_json(&tx, &WorkerMessage::Draining { node_id });
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    break;
                }
            }
        })
    };

    // Message loop.
    while let Some(msg) = ws_rx.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) => break,
            Err(e) => { error!("WS error: {}", e); break; }
            _ => continue,
        };

        let server_msg: ServerMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => { warn!("Bad server message: {} — {}", e, &text[..text.len().min(120)]); continue; }
        };

        match server_msg {
            ServerMessage::Registered { node_id: id } => {
                info!("Registered as {}", id);
            }

            // ── Standard task ─────────────────────────────────────────────────
            ServerMessage::DispatchTask { task } => {
                if active.load(Ordering::SeqCst) >= max_tasks {
                    send_json(&out_tx, &WorkerMessage::TaskError {
                        task_id: task.id, node_id,
                        reason: format!("worker at capacity ({}/{})", active.load(Ordering::SeqCst), max_tasks),
                    });
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                info!("Received task {}", task.id);
                let tx = out_tx.clone();
                let ac = Arc::clone(&active);
                tokio::spawn(async move {
                    let t0 = std::time::Instant::now();
                    match executor::execute(&task).await {
                        Ok(output) => send_json(&tx, &WorkerMessage::TaskResult {
                            task_id: task.id, node_id,
                            duration_ms: t0.elapsed().as_millis() as u64,
                            output,
                        }),
                        Err(e) => send_json(&tx, &WorkerMessage::TaskError {
                            task_id: task.id, node_id, reason: e.to_string(),
                        }),
                    }
                    ac.fetch_sub(1, Ordering::SeqCst);
                });
            }

            // ── MPI slot ──────────────────────────────────────────────────────
            ServerMessage::DispatchMpiSlot { task, slot } => {
                if active.load(Ordering::SeqCst) >= max_tasks {
                    send_json(&out_tx, &WorkerMessage::MpiRankError {
                        job_id: slot.job_id, task_id: task.id, node_id,
                        rank: slot.rank,
                        reason: format!("worker at capacity ({}/{})", active.load(Ordering::SeqCst), max_tasks),
                    });
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                info!("Received MPI slot rank {}/{} for job {}", slot.rank, slot.size, slot.job_id);
                let tx = out_tx.clone();
                let ac = Arc::clone(&active);
                tokio::spawn(async move {
                    let t0 = std::time::Instant::now();
                    match executor::execute_mpi_slot(&task, &slot).await {
                        Ok(output) => {
                            send_json(&tx, &WorkerMessage::MpiRankResult {
                                job_id: slot.job_id, task_id: task.id, node_id,
                                rank: slot.rank,
                                duration_ms: t0.elapsed().as_millis() as u64,
                                output,
                            });
                        }
                        Err(e) => {
                            send_json(&tx, &WorkerMessage::MpiRankError {
                                job_id: slot.job_id, task_id: task.id, node_id,
                                rank: slot.rank, reason: e.to_string(),
                            });
                        }
                    }
                    ac.fetch_sub(1, Ordering::SeqCst);
                });
            }

            ServerMessage::CancelTask { task_id } => {
                info!("Cancel requested for {} (in-flight cancellation not yet implemented)", task_id);
            }
            ServerMessage::AbortMpiJob { job_id, reason } => {
                warn!("MPI job {} aborted by server: {}", job_id, reason);
                // TODO: kill spawned process if tracked
            }
            ServerMessage::ServerShutdown => {
                info!("Server shutting down");
                break;
            }
        }
    }

    hb.abort();
    pump.abort();
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn send_json(tx: &tokio::sync::mpsc::UnboundedSender<String>, msg: &impl serde::Serialize) {
    if let Ok(json) = serde_json::to_string(msg) {
        let _ = tx.send(json);
    }
}

fn build_capabilities(args: &WorkerArgs, _name: &str) -> NodeCapabilities {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_all();

    let cpu_cores    = sys.cpus().len() as u32;
    let total_ram_mb = sys.total_memory() / 1024 / 1024;
    let os           = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);

    let has_gpu = std::path::Path::new("/dev/dri").exists()
        || std::process::Command::new("nvidia-smi")
        .output().map(|o| o.status.success()).unwrap_or(false);

    let has_mpirun = ["mpirun", "mpiexec", "mpirun.openmpi", "mpiexec.mpich"]
        .iter()
        .any(|cmd| std::process::Command::new("which")
            .arg(cmd).output()
            .map(|o| o.status.success()).unwrap_or(false));

    let mpi_host = args.mpi_host.clone().unwrap_or_else(|| {
        hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "localhost".into())
    });

    NodeCapabilities {
        cpu_cores,
        total_ram_mb,
        has_gpu,
        tags: args.tags.clone(),
        region: args.region.clone(),
        os,
        mpi_host,
        has_mpirun,
    }
}

fn collect_stats(active_tasks: u32) -> NodeStats {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_cpu();
    sys.refresh_memory();
    NodeStats {
        cpu_used_pct: sys.global_cpu_info().cpu_usage(),
        ram_free_mb:  sys.available_memory() / 1024 / 1024,
        active_tasks,
        latency_ms:   None,
        reported_at:  chrono::Utc::now(),
    }
}