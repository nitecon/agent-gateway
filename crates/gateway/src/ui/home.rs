//! `GET /` — the Home inbox: what needs a human, which agents are active,
//! and what happened recently.

use super::{fmt_datetime, fmt_relative, render, theme, PageChrome, Result};
use crate::{db, AppState};
use askama::Template;
use axum::{extract::State, response::Html};
use tokio::task::spawn_blocking;

pub(crate) struct NeedsYouView {
    pub kind_label: &'static str,
    pub chip_class: &'static str,
    pub project_ident: String,
    pub title: String,
    pub detail: String,
    pub age: String,
    pub href: String,
    pub message_id: Option<i64>,
}

pub(crate) struct AgentView {
    pub agent_id: String,
    pub project_ident: String,
    pub hostname: Option<String>,
    pub age: String,
    pub task_title: Option<String>,
    pub task_href: String,
    pub last_message: Option<String>,
}

pub(crate) struct ActivityView {
    pub age: String,
    pub when: String,
    pub kind: &'static str,
    pub project_ident: String,
    pub summary: String,
    pub state: String,
    pub href: String,
    pub actor: Option<String>,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {
    head: String,
    shell_open: String,
    shell_close: String,
    version: &'static str,
    update_available: Option<String>,
    needs_you: Vec<NeedsYouView>,
    agents: Vec<AgentView>,
    activity: Vec<ActivityView>,
}

pub(crate) fn needs_you_view(now: i64, item: db::NeedsYouItem) -> NeedsYouView {
    let (kind_label, chip_class) = match item.kind {
        "agent_update" => ("agent update", "gw-chip-ok"),
        "open_question" => ("waiting on agents", "gw-chip-warn"),
        "stalled_task" => ("stalled task", "gw-chip-danger"),
        _ => ("review decision", "gw-chip-warn"),
    };
    NeedsYouView {
        kind_label,
        chip_class,
        age: fmt_relative(now, item.at),
        project_ident: item.project_ident,
        title: item.title,
        detail: item.detail,
        href: item.href,
        message_id: item.message_id,
    }
}

pub(crate) fn agent_view(now: i64, a: db::AgentPresence) -> AgentView {
    AgentView {
        age: fmt_relative(now, a.last_seen),
        task_href: a
            .task_id
            .as_deref()
            .map(|id| format!("/projects/{}/tasks/{id}", a.project_ident))
            .unwrap_or_default(),
        agent_id: a.agent_id,
        project_ident: a.project_ident,
        hostname: a.hostname,
        task_title: a.task_title,
        last_message: a.last_message,
    }
}

pub(crate) fn activity_view(now: i64, ev: db::ActivityEvent) -> ActivityView {
    ActivityView {
        age: fmt_relative(now, ev.at),
        when: fmt_datetime(ev.at),
        kind: ev.kind,
        project_ident: ev.project_ident,
        summary: ev.summary,
        state: ev.state,
        href: ev.href,
        actor: ev.actor,
    }
}

pub async fn home_page(State(state): State<AppState>) -> Result<Html<String>> {
    let db_handle = state.db.clone();
    let now = db::now_ms();
    let (theme_name, needs_you, agents, activity) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        Ok((
            db::get_theme(&conn)?,
            db::list_needs_you(&conn, now, 50)?,
            db::list_agent_presence(&conn, now - 60 * 60 * 1000, None)?,
            db::list_recent_activity(&conn, None, 30)?,
        ))
    })
    .await??;
    let _ = theme(&state).await;
    let update_available = state.update_available.lock().unwrap().clone();
    let chrome = PageChrome::global("agent-gateway — Home", "Home", "home", &theme_name, "");
    render(&HomeTemplate {
        head: chrome.head,
        shell_open: chrome.shell_open,
        shell_close: chrome.shell_close,
        version: env!("AGENT_GATEWAY_VERSION"),
        update_available,
        needs_you: needs_you
            .into_iter()
            .map(|i| needs_you_view(now, i))
            .collect(),
        agents: agents.into_iter().map(|a| agent_view(now, a)).collect(),
        activity: activity
            .into_iter()
            .map(|e| activity_view(now, e))
            .collect(),
    })
}
