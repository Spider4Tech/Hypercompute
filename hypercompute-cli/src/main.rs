mod executor;
mod submitter;
mod worker;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(name = "hc", about = "HyperCompute CLI — worker and client")]
struct Cli {
    /// HyperCompute server URL (e.g. http://localhost:7700).
    #[arg(long, env = "HC_SERVER", global = true, default_value = "http://localhost:7700")]
    server: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start this machine as a HyperCompute worker node.
    Worker(worker::WorkerArgs),
    /// Submit a task to the cluster.
    Submit(submitter::SubmitArgs),
    /// Query the status of a task.
    Status {
        /// Task UUID.
        task_id: String,
        /// Poll until the task completes.
        #[arg(long)]
        wait: bool,
    },
    /// List all known worker nodes.
    Nodes,
    /// Show cluster-wide statistics.
    Info,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "hc=debug".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Worker(args) => worker::run(cli.server, args).await,
        Commands::Submit(args) => submitter::submit(cli.server, args).await,
        Commands::Status { task_id, wait } => submitter::status(cli.server, task_id, wait).await,
        Commands::Nodes => submitter::list_nodes(cli.server).await,
        Commands::Info => submitter::server_info(cli.server).await,
    }
}