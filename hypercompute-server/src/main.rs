mod router;
mod scheduler;
mod state;
mod ws_handler;

use anyhow::Result;
use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use state::AppState;

#[derive(Parser, Debug)]
#[command(name = "hc-server", about = "HyperCompute coordination server")]
struct Args {
    #[arg(long, env = "HC_BIND", default_value = "0.0.0.0:7700")]
    bind: SocketAddr,
    #[arg(long, env = "HC_NODE_TIMEOUT", default_value_t = 30)]
    node_timeout_secs: u64,
    #[arg(long, env = "HC_TASK_TIMEOUT", default_value_t = 300)]
    task_timeout_secs: u64,
    #[arg(long, env = "HC_MAX_RETRIES", default_value_t = 3)]
    max_retries: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "hc_server=debug,tower_http=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();
    let state = Arc::new(AppState::new(
        args.node_timeout_secs,
        args.task_timeout_secs,
        args.max_retries,
    ));

    {
        let s = Arc::clone(&state);
        tokio::spawn(async move { scheduler::run(s).await });
    }
    {
        let s = Arc::clone(&state);
        tokio::spawn(async move { state::monitor_nodes(s).await });
    }

    let app = router::build(Arc::clone(&state));
    info!("HyperCompute Server (MPI-enabled) listening on {}", args.bind);
    axum::serve(tokio::net::TcpListener::bind(args.bind).await?, app).await?;
    Ok(())
}