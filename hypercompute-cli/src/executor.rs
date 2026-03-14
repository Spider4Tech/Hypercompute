use anyhow::{anyhow, Result};
use hypercompute_proto::{Task, TaskKind, TaskOutput};
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{debug, warn};

/// Execute a task and return its output.
pub async fn execute(task: &Task) -> Result<TaskOutput> {
    match &task.kind {
        TaskKind::Shell { command, args, timeout_secs } => {
            execute_shell(command, args, *timeout_secs).await
        }
        TaskKind::HttpFetch { url, method, body } => {
            execute_http(url, method, body.as_deref()).await
        }
        TaskKind::Custom { tag, payload } => {
            execute_custom(tag, payload).await
        }
    }
}

async fn execute_shell(command: &str, args: &[String], timeout_secs: u64) -> Result<TaskOutput> {
    debug!("Shell: {} {:?} (timeout={}s)", command, args, timeout_secs);

    let fut = Command::new(command)
        .args(args)
        .output();

    let output = timeout(Duration::from_secs(timeout_secs), fut)
        .await
        .map_err(|_| anyhow!("Command timed out after {}s", timeout_secs))?
        .map_err(|e| anyhow!("Failed to spawn command '{}': {}", command, e))?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if exit_code != 0 {
        warn!("Command '{}' exited with code {}", command, exit_code);
    }

    Ok(TaskOutput {
        exit_code,
        stdout,
        stderr,
        body: None,
        custom_result: None,
    })
}

async fn execute_http(url: &str, method: &str, body: Option<&str>) -> Result<TaskOutput> {
    debug!("HTTP {} {}", method, url);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;

    let req = match method.to_uppercase().as_str() {
        "GET" => client.get(url),
        "POST" => {
            let mut r = client.post(url);
            if let Some(b) = body {
                r = r.body(b.to_string());
            }
            r
        }
        "PUT" => {
            let mut r = client.put(url);
            if let Some(b) = body {
                r = r.body(b.to_string());
            }
            r
        }
        "DELETE" => client.delete(url),
        other => return Err(anyhow!("Unsupported HTTP method: {}", other)),
    };

    let response = req.send().await
        .map_err(|e| anyhow!("HTTP request failed: {}", e))?;

    let status = response.status().as_u16() as i32;
    let body_bytes = response.bytes().await
        .map_err(|e| anyhow!("Failed to read response body: {}", e))?;

    let stdout = String::from_utf8_lossy(&body_bytes).into_owned();

    Ok(TaskOutput {
        exit_code: status,
        stdout,
        stderr: String::new(),
        body: Some(body_bytes.to_vec()),
        custom_result: None,
    })
}

async fn execute_custom(tag: &str, payload: &[u8]) -> Result<TaskOutput> {
    // Custom task execution: look for a handler binary named `hc-exec-{tag}` in PATH.
    let handler = format!("hc-exec-{}", tag);
    debug!("Custom task handler: {}", handler);

    let mut child = Command::new(&handler)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("Custom handler '{}' not found or failed to start: {}", handler, e))?;

    // Write payload to stdin.
    if !payload.is_empty() {
        if let Some(stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let mut stdin = stdin;
            stdin.write_all(payload).await?;
            // stdin dropped here closes it.
        }
    }

    let output = timeout(Duration::from_secs(300), child.wait_with_output())
        .await
        .map_err(|_| anyhow!("Custom task timed out"))?
        .map_err(|e| anyhow!("Custom handler error: {}", e))?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    Ok(TaskOutput {
        exit_code,
        stdout,
        stderr,
        body: None,
        custom_result: Some(output.stdout),
    })
}