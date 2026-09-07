//! Project registry (`/projects`), project overview (`/projects/:ident`), and
//! project settings (`/projects/:ident/settings`).

use super::home::{activity_view, agent_view, ActivityView, AgentView};
use super::{fmt_datetime, fmt_relative, render, PageChrome, Result};
use crate::routes::AppError;
use crate::{db, AppState};
use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Html,
};
use serde::Deserialize;
use tokio::task::spawn_blocking;

#[derive(Deserialize, Default)]
pub struct RegistryQuery {
    pub state: Option<String>,
    pub kind: Option<String>,
}

pub(crate) struct RegistryRow {
    pub ident: String,
    pub kind: String,
    pub archived: bool,
    pub repo_full_name: Option<String>,
    pub channel_name: String,
    pub has_room: bool,
    pub unanswered: i64,
    pub active_tasks: i64,
    pub in_progress: i64,
    pub todo: i64,
    pub last_activity_age: String,
    pub last_activity_when: String,
}

#[derive(Template)]
#[template(path = "projects.html")]
struct RegistryTemplate {
    head: String,
    shell_open: String,
    shell_close: String,
    state_filter: String,
    kind_filter: String,
    active_count: usize,
    archived_count: usize,
    rows: Vec<RegistryRow>,
}

pub async fn projects_registry_page(
    State(state): State<AppState>,
    Query(q): Query<RegistryQuery>,
) -> Result<Html<String>> {
    let db_handle = state.db.clone();
    let now = db::now_ms();
    let (theme, stats, last_activity) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        Ok((
            db::get_theme(&conn)?,
            db::list_project_stats_filtered(&conn, true)?,
            db::project_last_activity(&conn)?,
        ))
    })
    .await??;

    let state_filter = match q.state.as_deref() {
        Some("archived") => "archived",
        Some("all") => "all",
        _ => "active",
    }
    .to_string();
    let kind_filter = match q.kind.as_deref() {
        Some("repo") => "repo",
        Some("adhoc") => "adhoc",
        _ => "all",
    }
    .to_string();
    let active_count = stats.iter().filter(|s| s.archived_at.is_none()).count();
    let archived_count = stats.len() - active_count;

    let mut rows: Vec<RegistryRow> = stats
        .into_iter()
        .filter(|s| match state_filter.as_str() {
            "archived" => s.archived_at.is_some(),
            "all" => true,
            _ => s.archived_at.is_none(),
        })
        .filter(|s| kind_filter == "all" || s.kind == kind_filter)
        .map(|s| {
            let last = last_activity.get(&s.ident).copied();
            RegistryRow {
                last_activity_age: last
                    .map(|at| fmt_relative(now, at))
                    .unwrap_or_else(|| "never".to_string()),
                last_activity_when: last.map(fmt_datetime).unwrap_or_default(),
                archived: s.archived_at.is_some(),
                has_room: !s.room_id.is_empty(),
                unanswered: s.unread_count,
                active_tasks: s.todo_count + s.in_progress_count,
                in_progress: s.in_progress_count,
                todo: s.todo_count,
                ident: s.ident,
                kind: s.kind,
                repo_full_name: s.repo_full_name,
                channel_name: s.channel_name,
            }
        })
        .collect();
    // Most recently active first; never-active projects sink to the bottom.
    rows.sort_by(|a, b| {
        let la = last_activity.get(&a.ident).copied().unwrap_or(0);
        let lb = last_activity.get(&b.ident).copied().unwrap_or(0);
        lb.cmp(&la).then_with(|| a.ident.cmp(&b.ident))
    });

    let chrome = PageChrome::global(
        "agent-gateway — Projects",
        "Projects",
        "projects",
        &theme,
        "",
    );
    render(&RegistryTemplate {
        head: chrome.head,
        shell_open: chrome.shell_open,
        shell_close: chrome.shell_close,
        state_filter,
        kind_filter,
        active_count,
        archived_count,
        rows,
    })
}

/// Browser URL for a mapped repository.
pub(crate) fn repo_web_url(project: &db::Project) -> Option<String> {
    if let Some(canonical) = project.canonical_remote.as_deref() {
        return Some(format!("https://{canonical}"));
    }
    let full = project.repo_full_name.as_deref()?;
    let host = match project.repo_provider.as_deref().unwrap_or("github") {
        "github" => "github.com".to_string(),
        "gitlab" => "gitlab.com".to_string(),
        "bitbucket" => "bitbucket.org".to_string(),
        other => other.to_string(),
    };
    Some(format!("https://{host}/{full}"))
}

pub(crate) struct TaskRow {
    pub id: String,
    pub title: String,
    pub owner: Option<String>,
    pub age: String,
    pub comments: i64,
}

pub(crate) struct ArtifactRow {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub state: String,
    pub age: String,
}

pub(crate) struct LinkRow {
    pub id: i64,
    pub label: String,
    pub url: String,
}

#[derive(Template)]
#[template(path = "project_overview.html")]
struct OverviewTemplate {
    head: String,
    shell_open: String,
    shell_close: String,
    ident: String,
    kind: String,
    channel: String,
    repo_full_name: String,
    repo_url: Option<String>,
    questions_open: i64,
    agent_updates_open: i64,
    todo: i64,
    done: i64,
    doc_count: i64,
    memory_count: i64,
    in_progress: Vec<TaskRow>,
    artifacts: Vec<ArtifactRow>,
    links: Vec<LinkRow>,
    agents: Vec<AgentView>,
    activity: Vec<ActivityView>,
}

fn not_found(ident: &str) -> AppError {
    AppError(
        StatusCode::NOT_FOUND,
        format!("project '{ident}' not found"),
    )
}

pub async fn project_overview_page(
    State(state): State<AppState>,
    Path(ident): Path<String>,
) -> Result<Html<String>> {
    let db_handle = state.db.clone();
    let now = db::now_ms();
    let lookup = ident.clone();
    let (theme, overview) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        Ok((
            db::get_theme(&conn)?,
            db::project_overview(&conn, &lookup, now)?,
        ))
    })
    .await??;
    let ov = overview.ok_or_else(|| not_found(&ident))?;
    let ident = ov.project.ident.clone();
    let chrome = PageChrome::project(
        &format!("agent-gateway — {ident}"),
        &ident,
        &ident,
        "overview",
        &theme,
    );
    let channel = if ov.project.room_id.is_empty() {
        format!("{} (no room yet)", ov.project.channel_name)
    } else {
        ov.project.channel_name.clone()
    };
    render(&OverviewTemplate {
        head: chrome.head,
        shell_open: chrome.shell_open,
        shell_close: chrome.shell_close,
        repo_url: repo_web_url(&ov.project),
        repo_full_name: ov.project.repo_full_name.clone().unwrap_or_default(),
        kind: ov.project.kind.clone(),
        channel,
        questions_open: ov.questions_open,
        agent_updates_open: ov.agent_updates_open,
        todo: ov.stats.todo_count,
        done: ov.stats.done_count,
        doc_count: ov.stats.api_doc_count,
        memory_count: ov.stats.memory_count,
        in_progress: ov
            .in_progress
            .into_iter()
            .map(|t| TaskRow {
                age: fmt_relative(now, t.updated_at),
                id: t.id,
                title: t.title,
                owner: t.owner_agent_id,
                comments: t.comment_count,
            })
            .collect(),
        artifacts: ov
            .recent_artifacts
            .into_iter()
            .map(|a| ArtifactRow {
                age: fmt_relative(now, a.updated_at),
                state: format!("{} · {}", a.lifecycle_state, a.review_state),
                id: a.artifact_id,
                kind: a.kind,
                title: a.title,
            })
            .collect(),
        links: ov
            .links
            .into_iter()
            .map(|l| LinkRow {
                id: l.id,
                label: l.label,
                url: l.url,
            })
            .collect(),
        agents: ov.agents.into_iter().map(|a| agent_view(now, a)).collect(),
        activity: ov
            .activity
            .into_iter()
            .map(|e| activity_view(now, e))
            .collect(),
        ident,
    })
}

#[derive(Template)]
#[template(path = "project_settings.html")]
struct ProjectSettingsTemplate {
    head: String,
    shell_open: String,
    shell_close: String,
    ident: String,
    kind: String,
    canonical_remote: Option<String>,
    created: String,
    provider: String,
    namespace: String,
    repo_name: String,
    repo_url: Option<String>,
    channel: String,
    room_id: String,
    has_room: bool,
    links: Vec<LinkRow>,
    archived: bool,
}

pub async fn project_settings_page(
    State(state): State<AppState>,
    Path(ident): Path<String>,
) -> Result<Html<String>> {
    let db_handle = state.db.clone();
    let lookup = ident.clone();
    let (theme, project, links) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        let project = db::get_project(&conn, &lookup)?;
        let links = match project.as_ref() {
            Some(p) => db::list_project_links(&conn, &p.ident)?,
            None => Vec::new(),
        };
        Ok((db::get_theme(&conn)?, project, links))
    })
    .await??;
    let project = project.ok_or_else(|| not_found(&ident))?;
    let ident = project.ident.clone();
    let chrome = PageChrome::project(
        &format!("agent-gateway — {ident} settings"),
        "Settings",
        &ident,
        "settings",
        &theme,
    );
    render(&ProjectSettingsTemplate {
        head: chrome.head,
        shell_open: chrome.shell_open,
        shell_close: chrome.shell_close,
        kind: project.kind.clone(),
        canonical_remote: project.canonical_remote.clone(),
        created: fmt_datetime(project.created_at),
        provider: project
            .repo_provider
            .clone()
            .unwrap_or_else(|| "github".to_string()),
        namespace: project.repo_namespace.clone().unwrap_or_default(),
        repo_name: project.repo_name.clone().unwrap_or_default(),
        repo_url: repo_web_url(&project),
        channel: project.channel_name.clone(),
        has_room: !project.room_id.is_empty(),
        room_id: project.room_id.clone(),
        links: links
            .into_iter()
            .map(|l| LinkRow {
                id: l.id,
                label: l.label,
                url: l.url,
            })
            .collect(),
        archived: project.archived_at.is_some(),
        ident,
    })
}
