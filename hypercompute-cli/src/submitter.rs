use anyhow::{anyhow, Result};
use clap::Args;
use hypercompute_proto::{
    NodesResponse, ServerStatusResponse, SubmitTaskRequest, SubmitTaskResponse,
    TaskKind, TaskRequirements, TaskStatus, TaskStatusResponse,
};
use std::collections::HashMap;
use uuid::Uuid;

// ── Submit args ───────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct SubmitArgs {
    #[command(subcommand)]
    pub kind: SubmitKind,

    #[arg(long, default_value_t = 128)]
    pub priority: u8,

    #[arg(long, default_value_t = 0.0)]
    pub min_cpu: f32,

    #[arg(long, default_value_t = 0)]
    pub min_ram: u64,

    #[arg(long, default_value_t = false)]
    pub gpu: bool,

    #[arg(long, value_delimiter = ',')]
    pub tags: Vec<String>,

    #[arg(long)]
    pub region: Option<String>,

    /// Wait for completion and print the result.
    #[arg(long, default_value_t = false)]
    pub wait: bool,
}

#[derive(clap::Subcommand, Debug)]
pub enum SubmitKind {
    /// Run a shell command on the best available node.
    Shell {
        command: String,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Fetch a URL from a worker node.
    Fetch {
        url: String,
        #[arg(long, default_value = "GET")]
        method: String,
        #[arg(long)]
        body: Option<String>,
    },
    /// Run an MPI parallel job across multiple nodes.
    ///
    /// Example (pure mode, 4 ranks):
    ///   hc submit mpi --np 4 -- /usr/local/bin/my_sim --steps 1000
    ///
    /// Example (mpirun mode, requires OpenMPI on all workers):
    ///   hc submit --wait mpi --np 8 --mpirun -- python3 train_distributed.py
    Mpi {
        /// The program to run on every rank.
        program: String,
        /// Arguments forwarded to the program.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
        /// Number of MPI processes (= nodes recruited).
        #[arg(long, default_value_t = 2)]
        np: u32,
        /// Use system mpirun/mpiexec (rank-0 spawns all ranks remotely via SSH).
        /// Without this flag, each worker launches its own rank independently.
        #[arg(long, default_value_t = false)]
        mpirun: bool,
        /// Extra environment variables for all ranks (KEY=VALUE, repeatable).
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Wall-clock timeout for the whole job in seconds.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
}

// ── submit ────────────────────────────────────────────────────────────────────

pub async fn submit(server: String, args: SubmitArgs) -> Result<()> {
    let (kind, extra_requirements) = build_task_kind(&args)?;

    let requirements = TaskRequirements {
        min_cpu_free_pct: args.min_cpu,
        min_ram_free_mb:  args.min_ram,
        requires_gpu:     args.gpu,
        required_tags:    args.tags.clone(),
        preferred_region: args.region.clone(),
        ..extra_requirements
    };

    let req = SubmitTaskRequest {
        kind,
        requirements,
        priority: args.priority,
        meta: HashMap::new(),
    };

    let client = reqwest::Client::new();
    let resp: SubmitTaskResponse = client
        .post(format!("{}/api/v1/tasks", server))
        .json(&req)
        .send().await?
        .error_for_status()?
        .json().await?;

    println!("Task submitted: {}", resp.task_id);
    println!("Status:         {:?}", resp.status);

    if args.wait {
        poll_until_done(&server, resp.task_id).await?;
    }
    Ok(())
}

fn build_task_kind(args: &SubmitArgs) -> Result<(TaskKind, TaskRequirements)> {
    let mut extra = TaskRequirements::default();
    let kind = match &args.kind {
        SubmitKind::Shell { command, args: cmd_args, timeout } =>
            TaskKind::Shell {
                command: command.clone(),
                args: cmd_args.clone(),
                timeout_secs: *timeout,
            },
        SubmitKind::Fetch { url, method, body } =>
            TaskKind::HttpFetch {
                url: url.clone(),
                method: method.clone(),
                body: body.clone(),
            },
        SubmitKind::Mpi { program, args: mpi_args, np, mpirun, env, timeout } => {
            // Parse KEY=VALUE env pairs.
            let mut env_map = HashMap::new();
            for pair in env {
                let (k, v) = pair.split_once('=')
                    .ok_or_else(|| anyhow!("--env must be KEY=VALUE, got: {}", pair))?;
                env_map.insert(k.to_string(), v.to_string());
            }
            // MPI jobs need at least np nodes — communicate this in requirements.
            extra.mpi_slots_per_node = Some(1);
            TaskKind::Mpi {
                program: program.clone(),
                args: mpi_args.clone(),
                np: *np,
                env: env_map,
                timeout_secs: *timeout,
                use_mpirun: *mpirun,
            }
        }
    };
    Ok((kind, extra))
}

// ── status ────────────────────────────────────────────────────────────────────

pub async fn status(server: String, task_id: String, wait: bool) -> Result<()> {
    let id: Uuid = task_id.parse().map_err(|_| anyhow!("Invalid task UUID"))?;
    if wait {
        poll_until_done(&server, id).await?;
    } else {
        let client = reqwest::Client::new();
        let resp: TaskStatusResponse = client
            .get(format!("{}/api/v1/tasks/{}", server, id))
            .send().await?
            .error_for_status()?
            .json().await?;
        print_task_status(&resp);
    }
    Ok(())
}

// ── polling ───────────────────────────────────────────────────────────────────

async fn poll_until_done(server: &str, id: Uuid) -> Result<()> {
    let client = reqwest::Client::new();
    println!("Waiting for task {}…", id);
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;
        let resp: TaskStatusResponse = client
            .get(format!("{}/api/v1/tasks/{}", server, id))
            .send().await?
            .error_for_status()?
            .json().await?;

        match &resp.task.status {
            TaskStatus::Completed { duration_ms, node_id } => {
                println!("✓ Completed in {}ms (node: {})", duration_ms, node_id);
                if let Some(output) = &resp.result {
                    if !output.stdout.is_empty() { println!("--- stdout ---\n{}", output.stdout.trim()); }
                    if !output.stderr.is_empty() { println!("--- stderr ---\n{}", output.stderr.trim()); }
                    println!("Exit code: {}", output.exit_code);
                }
                return Ok(());
            }
            TaskStatus::MpiCompleted { job_id, duration_ms, ranks_ok, ranks_failed } => {
                println!("✓ MPI job {} completed in {}ms  ({} ranks OK, {} failed)",
                         job_id, duration_ms, ranks_ok, ranks_failed);
                if let Some(results) = &resp.mpi_results {
                    for r in results {
                        println!("  rank {} (node {})  exit={}", r.rank, r.node_id, r.output.exit_code);
                        if !r.output.stdout.is_empty() {
                            for line in r.output.stdout.trim().lines() {
                                println!("    [rank {}] {}", r.rank, line);
                            }
                        }
                    }
                }
                if *ranks_failed > 0 {
                    return Err(anyhow!("{} MPI rank(s) failed", ranks_failed));
                }
                return Ok(());
            }
            TaskStatus::Failed { reason } => {
                eprintln!("✗ Failed: {}", reason);
                return Err(anyhow!("Task failed: {}", reason));
            }
            TaskStatus::TimedOut => {
                eprintln!("✗ Timed out");
                return Err(anyhow!("Task timed out"));
            }
            status => {
                println!("  status: {:?}", status);
            }
        }
    }
}

fn print_task_status(resp: &TaskStatusResponse) {
    println!("Task:     {}", resp.task.id);
    println!("Priority: {}", resp.task.priority);
    println!("Status:   {:?}", resp.task.status);
    if let Some(output) = &resp.result {
        println!("Exit:     {}", output.exit_code);
        if !output.stdout.is_empty() { println!("--- stdout ---\n{}", output.stdout.trim()); }
    }
    if let Some(mpi) = &resp.mpi_results {
        for r in mpi {
            println!("  rank {} node {} exit={}", r.rank, r.node_id, r.output.exit_code);
        }
    }
}

// ── nodes / info ──────────────────────────────────────────────────────────────

pub async fn list_nodes(server: String) -> Result<()> {
    let client = reqwest::Client::new();
    let resp: NodesResponse = client
        .get(format!("{}/api/v1/nodes", server))
        .send().await?.error_for_status()?.json().await?;
    if resp.nodes.is_empty() { println!("No nodes registered."); return Ok(()); }

    println!("{:<38} {:<20} {:<10} {:<8} {:<8} {:<6} {:<8} {:<6} MPI host",
             "NODE ID", "NAME", "STATUS", "CPU%used", "RAM free", "Tasks", "Region", "mpirun");
    println!("{}", "-".repeat(120));
    for node in &resp.nodes {
        println!("{:<38} {:<20} {:<10} {:<8.1} {:<8} {:<6} {:<8} {:<6} {}",
                 node.id, &node.name,
                 format!("{:?}", node.status),
                 node.stats.cpu_used_pct,
                 format!("{}MB", node.stats.ram_free_mb),
                 node.stats.active_tasks,
                 node.capabilities.region,
                 if node.capabilities.has_mpirun { "yes" } else { "no" },
                 node.capabilities.mpi_host,
        );
        if !node.capabilities.tags.is_empty() {
            println!("  tags: {}", node.capabilities.tags.join(", "));
        }
    }
    Ok(())
}

pub async fn server_info(server: String) -> Result<()> {
    let client = reqwest::Client::new();
    let resp: ServerStatusResponse = client
        .get(format!("{}/api/v1/status", server))
        .send().await?.error_for_status()?.json().await?;
    println!("HyperCompute Server v{}", resp.version);
    println!("  Online nodes:    {}", resp.online_nodes);
    println!("  Queued tasks:    {}", resp.queued_tasks);
    println!("  Running tasks:   {}", resp.running_tasks);
    println!("  Completed tasks: {}", resp.completed_tasks);
    Ok(())
}