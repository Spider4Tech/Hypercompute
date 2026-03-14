use anyhow::{anyhow, Result};
use clap::Args;
use hypercompute_proto::{
    NodesResponse, ServerStatusResponse, SubmitTaskRequest, SubmitTaskResponse,
    TaskKind, TaskRequirements, TaskStatus, TaskStatusResponse,
};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Args, Debug)]
pub struct SubmitArgs {
    #[command(subcommand)]
    pub kind: SubmitKind,

    /// Task priority (0-255, default 128).
    #[arg(long, default_value_t = 128)]
    pub priority: u8,

    /// Minimum free CPU percentage required on the worker.
    #[arg(long, default_value_t = 0.0)]
    pub min_cpu: f32,

    /// Minimum free RAM in MB required on the worker.
    #[arg(long, default_value_t = 0)]
    pub min_ram: u64,

    /// Require a GPU on the worker.
    #[arg(long, default_value_t = false)]
    pub gpu: bool,

    /// Required worker tags (comma-separated).
    #[arg(long, value_delimiter = ',')]
    pub tags: Vec<String>,

    /// Preferred region for the worker.
    #[arg(long)]
    pub region: Option<String>,

    /// Wait for the task to complete and print the result.
    #[arg(long, default_value_t = false)]
    pub wait: bool,
}

#[derive(clap::Subcommand, Debug)]
pub enum SubmitKind {
    /// Run a shell command.
    Shell {
        /// The command to run (e.g. "ls").
        command: String,
        /// Arguments to pass to the command.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
        /// Timeout in seconds.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Fetch a URL.
    Fetch {
        url: String,
        #[arg(long, default_value = "GET")]
        method: String,
        #[arg(long)]
        body: Option<String>,
    },
}

pub async fn submit(server: String, args: SubmitArgs) -> Result<()> {
    let kind = match &args.kind {
        SubmitKind::Shell { command, args: cmd_args, timeout } => TaskKind::Shell {
            command: command.clone(),
            args: cmd_args.clone(),
            timeout_secs: *timeout,
        },
        SubmitKind::Fetch { url, method, body } => TaskKind::HttpFetch {
            url: url.clone(),
            method: method.clone(),
            body: body.clone(),
        },
    };

    let req = SubmitTaskRequest {
        kind,
        requirements: TaskRequirements {
            min_cpu_free_pct: args.min_cpu,
            min_ram_free_mb: args.min_ram,
            requires_gpu: args.gpu,
            required_tags: args.tags.clone(),
            preferred_region: args.region.clone(),
        },
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
    println!("Status: {:?}", resp.status);

    if args.wait {
        poll_until_done(&server, resp.task_id).await?;
    }

    Ok(())
}

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

async fn poll_until_done(server: &str, id: Uuid) -> Result<()> {
    let client = reqwest::Client::new();
    println!("Waiting for task {}...", id);

    loop {
        // Small delay before each poll — gives the worker time to execute and report back.
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
                    if !output.stdout.is_empty() {
                        println!("--- stdout ---\n{}", output.stdout.trim());
                    }
                    if !output.stderr.is_empty() {
                        println!("--- stderr ---\n{}", output.stderr.trim());
                    }
                    println!("Exit code: {}", output.exit_code);
                }
                return Ok(());
            }
            TaskStatus::Failed { reason } => {
                eprintln!("✗ Task failed: {}", reason);
                return Err(anyhow!("Task failed: {}", reason));
            }
            TaskStatus::TimedOut => {
                eprintln!("✗ Task timed out");
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
        if !output.stdout.is_empty() {
            println!("--- stdout ---\n{}", output.stdout.trim());
        }
    }
}

pub async fn list_nodes(server: String) -> Result<()> {
    let client = reqwest::Client::new();
    let resp: NodesResponse = client
        .get(format!("{}/api/v1/nodes", server))
        .send().await?
        .error_for_status()?
        .json().await?;

    if resp.nodes.is_empty() {
        println!("No nodes registered.");
        return Ok(());
    }

    println!("{:<38} {:<20} {:<10} {:<8} {:<8} {:<6} {:<8}",
             "NODE ID", "NAME", "STATUS", "CPU%used", "RAM free", "Tasks", "Region");
    println!("{}", "-".repeat(104));

    for node in &resp.nodes {
        println!("{:<38} {:<20} {:<10} {:<8.1} {:<8} {:<6} {:<8}",
                 node.id,
                 &node.name,
                 format!("{:?}", node.status),
                 node.stats.cpu_used_pct,
                 format!("{}MB", node.stats.ram_free_mb),
                 node.stats.active_tasks,
                 node.capabilities.region,
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
        .send().await?
        .error_for_status()?
        .json().await?;

    println!("HyperCompute Server v{}", resp.version);
    println!("  Online nodes:    {}", resp.online_nodes);
    println!("  Queued tasks:    {}", resp.queued_tasks);
    println!("  Running tasks:   {}", resp.running_tasks);
    println!("  Completed tasks: {}", resp.completed_tasks);
    Ok(())
}
