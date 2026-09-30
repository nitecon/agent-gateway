use super::{render, PageChrome, Result};
use crate::{db, execution, execution_client, execution_queue, routes::require_admin, AppState};
use askama::Template;
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::Html,
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ExecutionQuery {
    pub project: Option<String>,
}

#[derive(Template)]
#[template(path = "execution.html")]
struct ExecutionTemplate {
    head: String,
    shell_open: String,
    shell_close: String,
    project: String,
    settings_json: String,
    clients: Vec<String>,
    runs: Vec<execution_queue::Run>,
    task_template: String,
    cadence_template: String,
}

pub async fn page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ExecutionQuery>,
) -> Result<Html<String>> {
    require_admin(&headers)?;
    let conn = state.db.lock().unwrap();
    let project = query
        .project
        .map(|p| crate::projects::normalize_project_ident(&p))
        .unwrap_or_default();
    let (settings_json, runs) = if project.is_empty() {
        (
            serde_json::to_string(&execution::settings(&conn)?)?,
            Vec::new(),
        )
    } else {
        (
            serde_json::to_string(&execution::project_settings(&conn, &project)?)?,
            execution_queue::runs(&conn, &project)?,
        )
    };
    let theme = db::get_theme(&conn)?;
    let chrome = if project.is_empty() {
        PageChrome::global("Agent execution", "Agent execution", "settings", &theme, "")
    } else {
        PageChrome::project(
            "Agent execution",
            "Agent execution",
            &project,
            "settings",
            &theme,
        )
    };
    render(&ExecutionTemplate {
        head: chrome.head,
        shell_open: chrome.shell_open,
        shell_close: chrome.shell_close,
        project: project.clone(),
        settings_json,
        runs,
        task_template: execution_queue::prompt(
            if project.is_empty() {
                "PROJECT"
            } else {
                &project
            },
            Some("TASK_ID"),
        ),
        cadence_template: execution_queue::prompt(
            if project.is_empty() {
                "PROJECT"
            } else {
                &project
            },
            None,
        ),
        clients: execution_client::discover()
            .into_iter()
            .map(|c| {
                format!(
                    "{}: {}",
                    c.client.executable(),
                    c.path
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "not found on gateway PATH".into())
                )
            })
            .collect(),
    })
}
