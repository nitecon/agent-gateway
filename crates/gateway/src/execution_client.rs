//! Headless client discovery and invocation. No shell interpolation of prompts or models.
use crate::execution::{Candidate, Client};
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

impl Client {
    pub fn executable(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Serialize)]
pub struct DiscoveredClient {
    pub client: Client,
    pub path: Option<PathBuf>,
}

pub fn executable_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|path| {
            let Ok(meta) = path.metadata() else {
                return false;
            };
            if !meta.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        })
}

pub fn discover() -> Vec<DiscoveredClient> {
    [Client::Claude, Client::Codex]
        .into_iter()
        .map(|client| DiscoveredClient {
            path: executable_on_path(client.executable()),
            client,
        })
        .collect()
}

#[derive(Debug, Serialize)]
pub struct Attempt {
    pub candidate: Candidate,
    pub success: bool,
    pub output: String,
}

async fn capture(mut stream: impl AsyncRead + Unpin) -> String {
    // Drain all output so a verbose client cannot block; retain only a bounded tail.
    let mut tail = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.extend_from_slice(&buffer[..n]);
                if tail.len() > 32768 {
                    tail.drain(..tail.len() - 32768);
                }
            }
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}

pub async fn attempt(
    candidate: &Candidate,
    executable: &Path,
    directory: &Path,
    prompt: &str,
    timeout: Duration,
    gateway: Option<(&str, &str)>,
) -> Result<Attempt> {
    let mut command = Command::new(executable);
    if let Some((url, key)) = gateway {
        command
            .env("GATEWAY_EXECUTION_URL", url)
            .env("GATEWAY_EXECUTION_API_KEY", key);
    }
    match candidate.client {
        Client::Claude => {
            command.args([
                "--print",
                "--output-format",
                "json",
                "--permission-mode",
                "dontAsk",
                "--allowedTools",
                "Bash,Read,Edit,Write,Glob,Grep",
            ]);
        }
        Client::Codex => {
            command.args([
                "exec",
                "--sandbox",
                "workspace-write",
                "-c",
                "approval_policy=\"never\"",
                "-c",
                "sandbox_workspace_write.network_access=true",
                "--color",
                "never",
            ]);
        }
    }
    if let Some(model) = &candidate.model {
        command.args(["--model", model]);
    }
    if candidate.client == Client::Codex {
        command.arg("-");
    }
    command
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("start headless client")?;
    let pid = child.id();
    let stdout = tokio::spawn(capture(child.stdout.take().unwrap()));
    let stderr = tokio::spawn(capture(child.stderr.take().unwrap()));
    let mut input = child.stdin.take().unwrap();
    let prompt = prompt.to_owned();
    let writer = tokio::spawn(async move {
        input.write_all(prompt.as_bytes()).await?;
        input.shutdown().await
    });
    let result = tokio::time::timeout(timeout, child.wait()).await;
    // Stop remaining children before advancing to the next candidate.
    #[cfg(unix)]
    if let Some(pid) = pid {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let (success, status) = match result {
        Ok(Ok(status)) => (status.success(), format!("exit: {status}")),
        Ok(Err(error)) => {
            let _ = child.kill().await;
            (false, format!("wait failed: {error}"))
        }
        Err(_) => {
            let _ = child.kill().await;
            (false, "execution timed out".into())
        }
    };
    writer.abort();
    // A broken client may leave inherited pipes open; never stall the queue on output.
    let out = tokio::time::timeout(Duration::from_secs(2), stdout)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let err = tokio::time::timeout(Duration::from_secs(2), stderr)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let reported_error = candidate.client == Client::Claude
        && serde_json::from_str::<serde_json::Value>(&out)
            .ok()
            .and_then(|v| v.get("is_error").and_then(|v| v.as_bool()))
            .unwrap_or(false);
    Ok(Attempt {
        candidate: candidate.clone(),
        success: success && !reported_error,
        output: format!("{status}\n{out}\n{err}"),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn headless_process_receives_cwd_prompt_model_and_reports_failures() {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("client-test-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let directory = directory.canonicalize().unwrap();
        let executable = directory.join("client");
        std::fs::write(&executable, "#!/bin/sh\npwd\nprintf '%s\\n' \"$@\"\ncat\n").unwrap();
        tokio::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let candidate = Candidate {
            client: Client::Codex,
            model: Some("custom-model".into()),
        };
        let result = attempt(
            &candidate,
            &executable,
            &directory,
            "task prompt $(must remain text)",
            Duration::from_secs(3),
            None,
        )
        .await
        .unwrap();
        assert!(result.success);
        assert!(result.output.contains(directory.to_str().unwrap()));
        assert!(result.output.contains("custom-model"));
        assert!(result.output.contains("task prompt $(must remain text)"));
        std::fs::write(&executable, "#!/bin/sh\ncat >/dev/null\nexit 2\n").unwrap();
        assert!(
            !attempt(
                &candidate,
                &executable,
                &directory,
                "task",
                Duration::from_secs(3),
                None
            )
            .await
            .unwrap()
            .success
        );
        std::fs::write(&executable, "#!/bin/sh\nsleep 30\n").unwrap();
        let result = attempt(
            &candidate,
            &executable,
            &directory,
            "task",
            Duration::from_millis(100),
            None,
        )
        .await
        .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("timed out"));
        tokio::fs::remove_dir_all(directory).await.unwrap();
    }
}
