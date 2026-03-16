use anyhow::{anyhow, Result};
use hypercompute_proto::{MpiPeer, MpiSlot, Task, TaskKind, TaskOutput};
use std::collections::HashMap;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

// ── Public entry point ────────────────────────────────────────────────────────

/// Execute a standard (non-MPI) task.
pub async fn execute(task: &Task) -> Result<TaskOutput> {
    match &task.kind {
        TaskKind::Shell { command, args, timeout_secs } =>
            execute_shell(command, args, *timeout_secs).await,
        TaskKind::HttpFetch { url, method, body } =>
            execute_http(url, method, body.as_deref()).await,
        TaskKind::Custom { tag, payload } =>
            execute_custom(tag, payload).await,
        TaskKind::Mpi { .. } =>
            Err(anyhow!("Use execute_mpi_slot() for MPI tasks")),
    }
}

/// Execute this node's MPI slot.
///
/// Two modes:
/// - `use_mpirun = true` and `rank == 0`: rank-0 invokes `mpirun --host <peers> …`
///   All peer processes are spawned remotely via SSH by mpirun.
/// - `use_mpirun = false` (pure mode): every rank launches the program locally,
///   with MPI context injected via environment variables. Ranks discover each
///   other through `HC_MPI_PEERS` and communicate over plain TCP sockets.
pub async fn execute_mpi_slot(task: &Task, slot: &MpiSlot) -> Result<TaskOutput> {
    let (program, args, env, timeout_secs, use_mpirun) = match &task.kind {
        TaskKind::Mpi { program, args, env, timeout_secs, use_mpirun, .. } =>
            (program, args, env, *timeout_secs, *use_mpirun),
        _ => return Err(anyhow!("execute_mpi_slot called on non-MPI task")),
    };

    if use_mpirun && slot.rank == 0 {
        execute_mpirun(program, args, env, slot, timeout_secs).await
    } else if use_mpirun {
        // Non-root ranks in mpirun mode: spawned by mpirun on rank-0, nothing to do here.
        // The worker will receive an MpiRankResult from rank-0's mpirun invocation.
        // We return a placeholder; the real result comes from rank-0.
        info!("MPI rank {} in mpirun mode — delegated to rank-0's mpirun", slot.rank);
        Ok(TaskOutput::success(format!("rank {} delegated to mpirun on rank-0", slot.rank)))
    } else {
        execute_mpi_pure(program, args, env, slot, timeout_secs).await
    }
}

// ── mpirun mode (rank-0 only) ─────────────────────────────────────────────────

async fn execute_mpirun(
    program: &str,
    args: &[String],
    extra_env: &HashMap<String, String>,
    slot: &MpiSlot,
    timeout_secs: u64,
) -> Result<TaskOutput> {
    // Build the host string: "host0:slots,host1:slots,..."
    let hostlist = slot.peers.iter()
        .map(|p| format!("{}:1", p.host))
        .collect::<Vec<_>>()
        .join(",");

    let peers_json = serde_json::to_string(&slot.peers)
        .unwrap_or_default();

    let mpirun = find_mpirun()?;

    let mut cmd = Command::new(&mpirun);
    cmd.arg("--host").arg(&hostlist)
        .arg("-np").arg(slot.size.to_string())
        .arg("--map-by").arg("node");

    // Inject HC_ env vars into every rank via -x flags.
    let hc_vars = [
        ("HC_MPI_JOB_ID", slot.job_id.to_string()),
        ("HC_MPI_SIZE",   slot.size.to_string()),
        ("HC_MPI_PEERS",  peers_json),
    ];
    for (k, v) in &hc_vars {
        cmd.arg("-x").arg(format!("{}={}", k, v));
    }
    for (k, v) in extra_env {
        cmd.arg("-x").arg(format!("{}={}", k, v));
    }

    cmd.arg(program).args(args);

    info!("mpirun: {} --host {} -np {} {} {:?}", mpirun, hostlist, slot.size, program, args);

    let fut = cmd.output();
    let output = timeout(Duration::from_secs(timeout_secs), fut)
        .await
        .map_err(|_| anyhow!("MPI job timed out after {}s", timeout_secs))?
        .map_err(|e| anyhow!("mpirun failed to spawn: {}", e))?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if exit_code != 0 {
        warn!("mpirun exited with code {}: {}", exit_code, stderr.trim());
    }

    Ok(TaskOutput { exit_code, stdout, stderr, body: None, custom_result: None })
}

fn find_mpirun() -> Result<String> {
    for candidate in &["mpirun", "mpiexec", "mpirun.openmpi", "mpiexec.openmpi",
        "mpirun.mpich", "mpiexec.mpich"] {
        if std::process::Command::new("which")
            .arg(candidate)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Ok(candidate.to_string());
        }
    }
    Err(anyhow!("No mpirun/mpiexec found in PATH. Install OpenMPI or MPICH, or use use_mpirun=false."))
}

// ── Pure-Rust MPI mode ────────────────────────────────────────────────────────

/// Pure mode: each node launches the program locally with env vars injected.
/// No system MPI required — ranks communicate over plain TCP if needed.
async fn execute_mpi_pure(
    program: &str,
    args: &[String],
    extra_env: &HashMap<String, String>,
    slot: &MpiSlot,
    timeout_secs: u64,
) -> Result<TaskOutput> {
    let peers_json = serde_json::to_string(&slot.peers)
        .unwrap_or_default();

    let rank0_host = slot.peers.get(0).map(|p| p.host.as_str()).unwrap_or("localhost");

    let mut cmd = Command::new(program);
    cmd.args(args)
        .env("HC_MPI_RANK",   slot.rank.to_string())
        .env("HC_MPI_SIZE",   slot.size.to_string())
        .env("HC_MPI_JOB_ID", slot.job_id.to_string())
        .env("HC_MPI_PEERS",  &peers_json)
        .env("HC_MPI_ROOT",   rank0_host)
        // Bootstrap port for rank-to-rank communication.
        .env("HC_MPI_PORT",   slot.peers.get(slot.rank as usize)
            .map(|p| p.port.to_string())
            .unwrap_or_else(|| "9900".into()));

    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    info!("MPI pure rank {}/{}: {} {:?}", slot.rank, slot.size, program, args);

    let fut = cmd.output();
    let output = timeout(Duration::from_secs(timeout_secs), fut)
        .await
        .map_err(|_| anyhow!("MPI rank {} timed out after {}s", slot.rank, timeout_secs))?
        .map_err(|e| anyhow!("Failed to spawn rank {}: {}", slot.rank, e))?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if exit_code != 0 {
        warn!("MPI rank {} exited {}: {}", slot.rank, exit_code, stderr.trim());
    } else {
        info!("MPI rank {}/{} completed OK", slot.rank, slot.size);
    }

    Ok(TaskOutput { exit_code, stdout, stderr, body: None, custom_result: None })
}

// ── Standard executors ────────────────────────────────────────────────────────

async fn execute_shell(command: &str, args: &[String], timeout_secs: u64) -> Result<TaskOutput> {
    debug!("Shell: {} {:?} (timeout={}s)", command, args, timeout_secs);
    let fut = Command::new(command).args(args).output();
    let output = timeout(Duration::from_secs(timeout_secs), fut)
        .await
        .map_err(|_| anyhow!("Command timed out after {}s", timeout_secs))?
        .map_err(|e| anyhow!("Failed to spawn '{}': {}", command, e))?;
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if exit_code != 0 { warn!("'{}' exited {}", command, exit_code); }
    Ok(TaskOutput { exit_code, stdout, stderr, body: None, custom_result: None })
}

async fn execute_http(url: &str, method: &str, body: Option<&str>) -> Result<TaskOutput> {
    debug!("HTTP {} {}", method, url);
    let client = reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?;
    let req = match method.to_uppercase().as_str() {
        "GET"    => client.get(url),
        "POST"   => { let mut r = client.post(url);   if let Some(b) = body { r = r.body(b.to_owned()); } r }
        "PUT"    => { let mut r = client.put(url);    if let Some(b) = body { r = r.body(b.to_owned()); } r }
        "DELETE" => client.delete(url),
        other    => return Err(anyhow!("Unsupported HTTP method: {}", other)),
    };
    let response = req.send().await.map_err(|e| anyhow!("HTTP failed: {}", e))?;
    let status   = response.status().as_u16() as i32;
    let bytes    = response.bytes().await.map_err(|e| anyhow!("Body read error: {}", e))?;
    let stdout   = String::from_utf8_lossy(&bytes).into_owned();
    Ok(TaskOutput { exit_code: status, stdout, stderr: String::new(), body: Some(bytes.to_vec()), custom_result: None })
}

async fn execute_custom(tag: &str, payload: &[u8]) -> Result<TaskOutput> {
    let handler = format!("hc-exec-{}", tag);
    debug!("Custom handler: {}", handler);
    let mut child = Command::new(&handler)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("Handler '{}' not found: {}", handler, e))?;
    if !payload.is_empty() {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload).await?;
        }
    }
    let output = timeout(Duration::from_secs(300), child.wait_with_output())
        .await
        .map_err(|_| anyhow!("Custom task timed out"))?
        .map_err(|e| anyhow!("Custom handler error: {}", e))?;
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout    = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr    = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok(TaskOutput { exit_code, stdout, stderr, body: None, custom_result: Some(output.stdout) })
}