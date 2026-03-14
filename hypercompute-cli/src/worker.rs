use anyhow::Result;
use clap::Args;
use futures::{SinkExt, StreamExt};
use hypercompute_proto::{NodeCapabilities, NodeStats, ServerMessage, WorkerMessage};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::executor;

#[derive(Args, Debug)]
pub struct WorkerArgs {
    /// Human-readable name for this node (defaults to hostname).
    #[arg(long)]
    name: Option<String>,

    /// Comma-separated capability tags (e.g. python3,ffmpeg,docker).
    #[arg(long, value_delimiter = ',')]
    tags: Vec<String>,

    /// Geographic region label.
    #[arg(long, env = "HC_REGION", default_value = "default")]
    region: String,

    /// Heartbeat interval in seconds.
    #[arg(long, default_value_t = 10)]
    heartbeat_secs: u64,

    /// Maximum concurrent tasks to accept.
    #[arg(long, default_value_t = 4)]
    max_tasks: u32,
}

pub async fn run(server: String, args: WorkerArgs) -> Result<()> {
    let node_id = Uuid::new_v4();
    let name = args.name.clone().unwrap_or_else(|| {
        hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| format!("node-{}", &node_id.to_string()[..8]))
    });

    let capabilities = build_capabilities(&args, &name);

    // Convert http(s) to ws(s)
    let ws_url = server
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1)
        + "/ws";

    info!("Connecting to {} as '{}' ({})", ws_url, name, node_id);

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let s = Arc::clone(&shutdown);
        ctrlc::set_handler(move || {
            info!("Shutdown requested — draining...");
            s.store(true, Ordering::SeqCst);
        })?;
    }

    // Retry loop with back-off.
    let mut backoff = Duration::from_secs(1);
    loop {
        match connect_async(&ws_url).await {
            Ok((stream, _)) => {
                backoff = Duration::from_secs(1);
                info!("Connected to server");
                run_session(
                    stream,
                    node_id,
                    name.clone(),
                    capabilities.clone(),
                    args.heartbeat_secs,
                    args.max_tasks,
                    Arc::clone(&shutdown),
                )
                    .await;
            }
            Err(e) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                warn!("Connection failed: {} — retrying in {}s", e, backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
    }

    info!("Worker shut down cleanly");
    Ok(())
}

async fn run_session(
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    node_id: Uuid,
    name: String,
    capabilities: NodeCapabilities,
    heartbeat_secs: u64,
    max_tasks: u32,
    shutdown: Arc<AtomicBool>,
) {
    let (mut ws_tx, mut ws_rx) = stream.split();
    let active_tasks = Arc::new(std::sync::atomic::AtomicU32::new(0));

    // Send registration message.
    let reg = WorkerMessage::Register {
        node_id,
        name: name.clone(),
        capabilities: capabilities.clone(),
    };
    if let Ok(json) = serde_json::to_string(&reg) {
        let _ = ws_tx.send(Message::Text(json)).await;
    }

    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Outbound pump.
    let pump_handle = {
        let mut ws_tx = ws_tx;
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if ws_tx.send(Message::Text(msg)).await.is_err() {
                    break;
                }
            }
        })
    };

    // Heartbeat ticker.
    let hb_handle = {
        let tx = out_tx.clone();
        let caps = capabilities.clone();
        let active = Arc::clone(&active_tasks);
        let sd = Arc::clone(&shutdown);
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(heartbeat_secs));
            loop {
                ticker.tick().await;
                let stats = collect_stats(&caps, active.load(Ordering::SeqCst));
                let hb = WorkerMessage::Heartbeat { node_id, stats };
                if let Ok(json) = serde_json::to_string(&hb) {
                    if tx.send(json).is_err() { break; }
                }
                if sd.load(Ordering::SeqCst) {
                    // Send drain notice then break.
                    let drain = WorkerMessage::Draining { node_id };
                    if let Ok(json) = serde_json::to_string(&drain) {
                        let _ = tx.send(json);
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    break;
                }
            }
        })
    };

    // Inbound message loop.
    while let Some(msg) = ws_rx.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) => break,
            Err(e) => { error!("WS error: {}", e); break; }
            _ => continue,
        };

        let server_msg: ServerMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => { warn!("Bad server message: {} — {}", e, text); continue; }
        };

        match server_msg {
            ServerMessage::Registered { node_id: id } => {
                info!("Registered with server as {}", id);
            }

            ServerMessage::DispatchTask { task } => {
                let count = active_tasks.load(Ordering::SeqCst);
                if count >= max_tasks {
                    // Refuse: report error back (server should not have dispatched here).
                    let err = WorkerMessage::TaskError {
                        task_id: task.id,
                        node_id,
                        reason: format!("Worker at capacity ({}/{})", count, max_tasks),
                    };
                    if let Ok(json) = serde_json::to_string(&err) {
                        let _ = out_tx.send(json);
                    }
                    continue;
                }

                active_tasks.fetch_add(1, Ordering::SeqCst);
                info!("Received task {} ({:?})", task.id, std::mem::discriminant(&task.kind));

                let tx = out_tx.clone();
                let active = Arc::clone(&active_tasks);
                tokio::spawn(async move {
                    let task_id = task.id;
                    let started = std::time::Instant::now();
                    match executor::execute(&task).await {
                        Ok(output) => {
                            let duration_ms = started.elapsed().as_millis() as u64;
                            let result = WorkerMessage::TaskResult {
                                task_id,
                                node_id,
                                duration_ms,
                                output,
                            };
                            if let Ok(json) = serde_json::to_string(&result) {
                                let _ = tx.send(json);
                            }
                        }
                        Err(e) => {
                            let err = WorkerMessage::TaskError {
                                task_id,
                                node_id,
                                reason: e.to_string(),
                            };
                            if let Ok(json) = serde_json::to_string(&err) {
                                let _ = tx.send(json);
                            }
                        }
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }

            ServerMessage::CancelTask { task_id } => {
                info!("Cancel requested for task {} (not yet implemented for running tasks)", task_id);
            }

            ServerMessage::ServerShutdown => {
                info!("Server is shutting down");
                break;
            }
        }
    }

    hb_handle.abort();
    pump_handle.abort();
}

fn build_capabilities(args: &WorkerArgs, _name: &str) -> NodeCapabilities {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_all();

    let cpu_cores = sys.cpus().len() as u32;
    let total_ram_mb = sys.total_memory() / 1024 / 1024;
    let os = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);

    // Very naive GPU detection: look for nvidia-smi or /dev/dri.
    let has_gpu = std::path::Path::new("/dev/dri").exists()
        || std::process::Command::new("nvidia-smi").output().map(|o| o.status.success()).unwrap_or(false);

    NodeCapabilities {
        cpu_cores,
        total_ram_mb,
        has_gpu,
        tags: args.tags.clone(),
        region: args.region.clone(),
        os,
    }
}

fn collect_stats(_caps: &NodeCapabilities, active_tasks: u32) -> NodeStats {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_cpu();
    sys.refresh_memory();

    let cpu_used_pct = sys.global_cpu_info().cpu_usage();
    let ram_free_mb = sys.available_memory() / 1024 / 1024;

    NodeStats {
        cpu_used_pct,
        ram_free_mb,
        active_tasks,
        latency_ms: None,
        reported_at: chrono::Utc::now(),
    }
}