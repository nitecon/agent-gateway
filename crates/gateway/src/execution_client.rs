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
    pub summary: Option<String>,
    pub summary_complete: bool,
}

#[derive(Clone)]
pub struct Observer {
    pub db: crate::db::Db,
    pub run_id: String,
    pub api_key: String,
}

impl Observer {
    fn progress(&self, message: &str) {
        let message = crate::execution_queue::redacted(message, &self.api_key, 4096);
        if let Err(error) =
            crate::execution_queue::progress(&self.db.lock().unwrap(), &self.run_id, &message)
        {
            tracing::warn!(run_id=%self.run_id, %error, "Could not record execution progress");
        }
    }
}

#[derive(Default)]
struct Capture {
    output: String,
    summary: Option<String>,
    reported_error: bool,
    final_received: bool,
    skipped_event: bool,
}

fn observe_line(line: &[u8], result: &mut Capture, observer: Option<&Observer>) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
        return;
    };
    let kind = value["type"].as_str().unwrap_or("");
    let progress = match kind {
        "thread.started" | "turn.started" => Some("Agent started working".to_owned()),
        "turn.failed" => {
            result.reported_error = true;
            Some("Agent turn failed".to_owned())
        }
        "item.started" | "item.completed" => {
            let item = &value["item"];
            match item["type"].as_str().unwrap_or("") {
                "agent_message" if kind == "item.completed" => {
                    result.summary = item["text"].as_str().map(|text| {
                        crate::execution_queue::redacted(
                            text,
                            observer.map(|o| o.api_key.as_str()).unwrap_or(""),
                            16384,
                        )
                    });
                    Some("Agent response received".to_owned())
                }
                "command_execution" => Some(
                    if kind == "item.started" {
                        "Running command"
                    } else {
                        "Command finished"
                    }
                    .to_owned(),
                ),
                "file_change" => Some("Updating files".to_owned()),
                "mcp_tool_call" => Some("Using external tool".to_owned()),
                "web_search" => Some("Searching documentation".to_owned()),
                _ => None,
            }
        }
        "assistant" => value["message"]["content"].as_array().and_then(|blocks| {
            blocks
                .iter()
                .find(|block| block["type"] == "tool_use")
                .map(|block| format!("Using {}", block["name"].as_str().unwrap_or("tool")))
        }),
        "result" => {
            result.final_received = true;
            result.summary = value["result"].as_str().map(|text| {
                crate::execution_queue::redacted(
                    text,
                    observer.map(|o| o.api_key.as_str()).unwrap_or(""),
                    16384,
                )
            });
            result.reported_error |= value["is_error"].as_bool().unwrap_or(false);
            Some("Agent result received".to_owned())
        }
        "turn.completed" => {
            result.final_received = true;
            None
        }
        "system" if value["subtype"] == "permission_denied" => {
            Some("A tool permission was denied".to_owned())
        }
        "system" if value["subtype"] == "api_retry" => {
            Some("Client is retrying its provider connection".to_owned())
        }
        _ => None,
    };
    if let (Some(observer), Some(message)) = (observer, progress) {
        observer.progress(&message);
    }
}

async fn capture(
    mut stream: impl AsyncRead + Unpin,
    observer: Option<Observer>,
    structured: bool,
) -> Capture {
    // Drain all output so a verbose client cannot block; retain only a bounded tail.
    let mut tail = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut line = Vec::new();
    let mut oversized = false;
    let mut result = Capture::default();
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.extend_from_slice(&buffer[..n]);
                if tail.len() > 32768 {
                    tail.drain(..tail.len() - 32768);
                }
                if structured {
                    for byte in &buffer[..n] {
                        if *byte == b'\n' {
                            if !oversized {
                                observe_line(&line, &mut result, observer.as_ref());
                            }
                            line.clear();
                            oversized = false;
                        } else if !oversized {
                            if line.len() < 65536 {
                                line.push(*byte);
                            } else {
                                line.clear();
                                oversized = true;
                                result.skipped_event = true;
                            }
                        }
                    }
                }
            }
        }
    }
    if structured && !oversized && !line.is_empty() {
        observe_line(&line, &mut result, observer.as_ref());
    }
    result.output = String::from_utf8_lossy(&tail).into_owned();
    result
}

pub async fn attempt(
    candidate: &Candidate,
    executable: &Path,
    directory: &Path,
    prompt: &str,
    timeout: Duration,
    gateway: Option<(&str, &str)>,
    observer: Option<Observer>,
) -> Result<Attempt> {
    let mut command = Command::new(executable);
    if let Some((url, key)) = gateway {
        command
            .env("GATEWAY_EXECUTION_URL", url)
            .env("GATEWAY_EXECUTION_API_KEY", key);
    }
    if let Some(observer) = &observer {
        command.env("GATEWAY_EXECUTION_RUN_ID", &observer.run_id);
    }
    match candidate.client {
        Client::Claude => {
            command.args([
                "--print",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "dontAsk",
                "--allowedTools",
                "Bash,Read,Edit,Write,Glob,Grep",
            ]);
        }
        Client::Codex => {
            command.args([
                "exec",
                "--json",
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
    let mut stdout = tokio::spawn(capture(
        child.stdout.take().unwrap(),
        observer.clone(),
        true,
    ));
    let mut stderr = tokio::spawn(capture(child.stderr.take().unwrap(), None, false));
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
    let out = tokio::time::timeout(Duration::from_secs(2), &mut stdout)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let err = tokio::time::timeout(Duration::from_secs(2), &mut stderr)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    stdout.abort();
    stderr.abort();
    Ok(Attempt {
        candidate: candidate.clone(),
        success: success && !out.reported_error,
        output: format!("{status}\n{}\n{}", out.output, err.output),
        summary: out.summary,
        summary_complete: out.final_received && !out.skipped_event,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn structured_capture_handles_fragmentation_bounds_and_provider_failures() {
        let (mut writer, reader) = tokio::io::duplex(128);
        let capture_task = tokio::spawn(capture(reader, None, true));
        writer.write_all(b"{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Verified ").await.unwrap();
        writer.write_all("変更\"}}\n".as_bytes()).await.unwrap();
        writer.write_all(&vec![b'x'; 100_000]).await.unwrap();
        writer
            .write_all(b"\n{\"type\":\"turn.failed\"}\n")
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        let result = capture_task.await.unwrap();
        assert!(result.reported_error);
        assert_eq!(result.summary.as_deref(), Some("Verified 変更"));
        assert!(result.output.len() <= 32768);
        let mut result = Capture::default();
        observe_line(
            br#"{"type":"result","result":"Work blocked","is_error":true}"#,
            &mut result,
            None,
        );
        assert!(result.reported_error);
        assert_eq!(result.summary.as_deref(), Some("Work blocked"));
    }

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
                None,
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
            None,
        )
        .await
        .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("timed out"));
        tokio::fs::remove_dir_all(directory).await.unwrap();
    }
}
