//! `GET /activity` — who is working where, and the event stream.

use super::home::{activity_view, agent_view, ActivityView, AgentView};
use super::{render, PageChrome, Result};
use crate::{db, AppState};
use askama::Template;
use axum::{
    extract::{Query, State},
    response::Html,
};
use serde::Deserialize;
use tokio::task::spawn_blocking;

#[derive(Deserialize, Default)]
pub struct ActivityQuery {
    pub project: Option<String>,
}

#[derive(Template)]
#[template(path = "activity.html")]
struct ActivityTemplate {
    head: String,
    shell_open: String,
    shell_close: String,
    project_filter: String,
    projects: Vec<String>,
    agents: Vec<AgentView>,
    activity: Vec<ActivityView>,
}

pub async fn activity_page(
    State(state): State<AppState>,
    Query(q): Query<ActivityQuery>,
) -> Result<Html<String>> {
    let now = db::now_ms();
    let project_filter = q
        .project
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .unwrap_or_default();
    let db_handle = state.db.clone();
    let pf = project_filter.clone();
    let (theme, projects, agents, activity) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        let filter = if pf.is_empty() {
            None
        } else {
            Some(pf.as_str())
        };
        let mut projects: Vec<String> = db::list_project_stats(&conn)?
            .into_iter()
            .map(|s| s.ident)
            .collect();
        projects.sort();
        Ok((
            db::get_theme(&conn)?,
            projects,
            db::list_agent_presence(&conn, now - 24 * 60 * 60 * 1000, filter)?,
            db::list_recent_activity(&conn, filter, 100)?,
        ))
    })
    .await??;
    let chrome = PageChrome::global(
        "agent-gateway — Activity",
        "Activity",
        "activity",
        &theme,
        "",
    );
    render(&ActivityTemplate {
        head: chrome.head,
        shell_open: chrome.shell_open,
        shell_close: chrome.shell_close,
        project_filter,
        projects,
        agents: agents.into_iter().map(|a| agent_view(now, a)).collect(),
        activity: activity
            .into_iter()
            .map(|e| activity_view(now, e))
            .collect(),
    })
}
