//! Persisted, opt-in execution policy. A local mapping never grants checkout permission.
use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use crate::db;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Client {
    Claude,
    Codex,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub client: Client,
    /// None uses the client's configured default model.
    pub model: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub data_directory: Option<String>,
    /// Ordered attempts, including multiple models for the same client.
    pub candidates: Vec<Candidate>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Templates {
    pub task: String,
    pub cadence: String,
}

impl Default for Templates {
    fn default() -> Self {
        let task = crate::execution_queue::default_prompt("{{project}}", Some("{{task_id}}"));
        Self {
            cadence: format!(
                "This is a scheduled execution of an incoming delegated task.\n{task}"
            ),
            task,
        }
    }
}

impl Templates {
    pub fn render(&self, project: &str, task: &str, trigger: &str) -> String {
        let template = if trigger == "cadence" {
            &self.cadence
        } else {
            &self.task
        };
        template
            .replace("{{project}}", project)
            .replace("{{task_id}}", task)
    }
}

pub fn templates(conn: &Connection) -> Result<Templates> {
    Ok(db::get_setting(conn, "execution.templates")?
        .map(|json| serde_json::from_str(&json))
        .transpose()?
        .unwrap_or_default())
}

pub fn save_templates(conn: &Connection, value: &Templates) -> Result<()> {
    if value.task.trim().is_empty() || value.cadence.trim().is_empty() {
        bail!("base instructions must not be empty");
    }
    db::set_setting(conn, "execution.templates", &serde_json::to_string(value)?)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectSettings {
    pub local_path: Option<String>,
    pub allow_checkout: bool,
    pub clone_url: Option<String>,
    pub enabled: bool,
    pub on_task_received: bool,
    pub cadence_seconds: Option<u64>,
    /// Empty inherits the gateway candidate list.
    pub candidates: Vec<Candidate>,
    /// Interactive execution waits for a registered cmux session; it never falls back to headless.
    pub executor: Executor,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Executor {
    #[default]
    Headless,
    Cmux,
}

fn validate_path(value: Option<&str>, name: &str) -> Result<()> {
    if let Some(value) = value {
        if value.trim().is_empty() || !Path::new(value).is_absolute() {
            bail!("{name} must be an absolute path or null");
        }
    }
    Ok(())
}

fn validate_candidates(candidates: &[Candidate]) -> Result<()> {
    if candidates.len() > 16 {
        bail!("at most 16 client/model candidates are allowed");
    }
    for candidate in candidates {
        if candidate
            .model
            .as_ref()
            .is_some_and(|m| m.trim().is_empty() || m.starts_with('-'))
        {
            bail!("model must be a non-empty model name or null");
        }
    }
    Ok(())
}

pub fn settings(conn: &Connection) -> Result<Settings> {
    Ok(db::get_setting(conn, "execution")?
        .map(|json| serde_json::from_str(&json))
        .transpose()?
        .unwrap_or_default())
}

pub fn save_settings(conn: &Connection, value: &Settings) -> Result<()> {
    validate_path(value.data_directory.as_deref(), "data_directory")?;
    validate_candidates(&value.candidates)?;
    db::set_setting(conn, "execution", &serde_json::to_string(value)?)
}

fn project_key(ident: &str) -> String {
    format!(
        "execution.project.{}",
        crate::projects::normalize_project_ident(ident)
    )
}

pub fn project_settings(conn: &Connection, ident: &str) -> Result<ProjectSettings> {
    if db::get_project(conn, ident)?.is_none() {
        bail!("project not found: {ident}");
    }
    Ok(db::get_setting(conn, &project_key(ident))?
        .map(|json| serde_json::from_str(&json))
        .transpose()?
        .unwrap_or_default())
}

pub fn save_project_settings(
    conn: &Connection,
    ident: &str,
    value: &ProjectSettings,
) -> Result<()> {
    project_settings(conn, ident)?;
    validate_path(value.local_path.as_deref(), "local_path")?;
    validate_candidates(&value.candidates)?;
    if let Some(url) = value.clone_url.as_deref() {
        validate_clone_url(url)?;
    }
    if value
        .cadence_seconds
        .is_some_and(|seconds| !(60..=31_536_000).contains(&seconds))
    {
        bail!("cadence_seconds must be between 60 and 31536000 or null");
    }
    if value.executor == Executor::Headless
        && value.enabled
        && value.local_path.is_none()
        && !value.allow_checkout
    {
        bail!("execution requires a local mapping or checkout permission");
    }
    if value.executor == Executor::Headless
        && value.enabled
        && value.local_path.is_none()
        && (value.clone_url.is_none() || settings(conn)?.data_directory.is_none())
    {
        bail!("automatic checkout requires clone_url and a gateway data_directory");
    }
    db::set_setting(conn, &project_key(ident), &serde_json::to_string(value)?)
}

fn validate_clone_url(url: &str) -> Result<()> {
    // Permit network transports only: local mappings are the explicit local-disk mechanism.
    let https = url
        .strip_prefix("https://")
        .is_some_and(|s| s.contains('/') && !s.starts_with('/'));
    let ssh = url
        .strip_prefix("ssh://")
        .is_some_and(|s| s.contains('/') && !s.starts_with('/'));
    let scp = url.split_once('@').is_some_and(|(user, host_path)| {
        !user.is_empty()
            && !user.contains([':', '/'])
            && host_path.split_once(':').is_some_and(|(host, path)| {
                !host.is_empty() && !host.contains('/') && !path.is_empty()
            })
    });
    if url.chars().any(char::is_whitespace) || url.starts_with('-') || !(https || ssh || scp) {
        bail!("clone_url must be an HTTPS or SSH Git URL");
    }
    Ok(())
}

async fn git_output(directory: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    let output = tokio::time::timeout(Duration::from_secs(600), command.output())
        .await
        .context("git operation timed out")??;
    if !output.status.success() {
        bail!(
            "git operation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

async fn repository_root(path: &Path) -> Result<PathBuf> {
    let canonical = tokio::fs::canonicalize(path)
        .await
        .context("repository path is unavailable")?;
    let root = git_output(Some(&canonical), &["rev-parse", "--show-toplevel"]).await?;
    let root = tokio::fs::canonicalize(root).await?;
    if root != canonical {
        bail!("local_path must point to the repository root");
    }
    Ok(root)
}

/// Called only by the runner, after reloading the current project policy.
pub async fn resolve_repository(
    ident: &str,
    global: &Settings,
    policy: &ProjectSettings,
) -> Result<PathBuf> {
    if !policy.enabled {
        bail!("agent execution is disabled for this project");
    }
    if let Some(path) = policy.local_path.as_deref() {
        return repository_root(Path::new(path)).await;
    }
    if !policy.allow_checkout {
        bail!("automatic checkout is disabled for this project");
    }
    let url = policy
        .clone_url
        .as_deref()
        .context("clone_url is required")?;
    validate_clone_url(url)?;
    let directory = global
        .data_directory
        .as_deref()
        .context("data_directory is required")?;
    validate_path(Some(directory), "data_directory")?;
    tokio::fs::create_dir_all(directory).await?;
    let directory = tokio::fs::canonicalize(directory).await?;
    // Hash the full identity: repository names may contain path separators or collide.
    use sha2::{Digest, Sha256};
    let name = hex::encode(Sha256::digest(ident.as_bytes()));
    let destination = directory.join(name);
    if tokio::fs::try_exists(&destination).await? {
        let root = repository_root(&destination).await?;
        if root.parent() != Some(directory.as_path()) {
            bail!("managed checkout escapes data_directory");
        }
        if git_output(Some(&root), &["config", "--get", "remote.origin.url"]).await? != url {
            bail!("managed checkout origin differs from clone_url");
        }
        return Ok(root);
    }
    let staging = directory.join(format!(".checkout-{}", uuid::Uuid::now_v7()));
    let result = git_output(
        None,
        &[
            "clone",
            "--",
            url,
            staging.to_str().context("non-UTF8 data directory")?,
        ],
    )
    .await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(error);
    }
    if let Err(error) = tokio::fs::rename(&staging, &destination).await {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(error.into());
    }
    repository_root(&destination).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mapping_takes_precedence_and_never_falls_back_to_checkout() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("execution-test-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        git_output(Some(&root), &["init", "--quiet"]).await.unwrap();
        let mut policy = ProjectSettings {
            enabled: true,
            local_path: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let global = Settings::default();
        assert_eq!(
            resolve_repository("test", &global, &policy).await.unwrap(),
            root.canonicalize().unwrap()
        );
        policy.enabled = false;
        assert!(resolve_repository("test", &global, &policy).await.is_err());
        policy.enabled = true;
        policy.allow_checkout = true;
        policy.clone_url = Some("https://example.invalid/repo.git".into());
        let missing = root.join("missing");
        policy.local_path = Some(missing.to_string_lossy().into_owned());
        assert!(resolve_repository("test", &global, &policy).await.is_err());
        assert!(!missing.exists());
        tokio::fs::create_dir_all(root.join("nested"))
            .await
            .unwrap();
        assert!(repository_root(&root.join("nested")).await.is_err());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[test]
    fn clone_requires_network_transport() {
        for url in [
            "https://github.com/org/repo.git",
            "ssh://git@example.com/org/repo",
            "git@example.com:org/repo",
        ] {
            validate_clone_url(url).unwrap();
        }
        for url in [
            "/local/repo",
            "file:///local/repo",
            "--upload-pack=evil",
            "ext::command",
            "ext::command@host:path",
            "https://host/a b",
        ] {
            assert!(validate_clone_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn default_policy_does_not_execute_or_checkout() {
        let policy: ProjectSettings = serde_json::from_str("{}").unwrap();
        assert!(!policy.enabled && !policy.allow_checkout && !policy.on_task_received);
        assert_eq!(policy.cadence_seconds, None);
        assert!(serde_json::from_str::<ProjectSettings>(r#"{"allow_chekout":true}"#).is_err());
    }

    #[test]
    fn settings_persist_model_fallback_order() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            .unwrap();
        let expected = Settings {
            data_directory: Some("/srv/agent-repositories".into()),
            candidates: vec![
                Candidate {
                    client: Client::Claude,
                    model: Some("primary-model".into()),
                },
                Candidate {
                    client: Client::Claude,
                    model: Some("fallback-model".into()),
                },
                Candidate {
                    client: Client::Codex,
                    model: None,
                },
            ],
        };
        save_settings(&conn, &expected).unwrap();
        assert_eq!(settings(&conn).unwrap(), expected);
        let invalid = Settings {
            data_directory: Some("relative".into()),
            ..expected.clone()
        };
        assert!(save_settings(&conn, &invalid).is_err());
        assert_eq!(settings(&conn).unwrap(), expected);
    }
}
