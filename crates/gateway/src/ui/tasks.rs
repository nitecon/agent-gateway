//! `GET /tasks` — cross-project task list; `GET /projects/:ident/tasks/:id` —
//! dedicated task detail route; `/task-link/:ref` redirects to it.

use super::{fmt_datetime, fmt_relative, render, PageChrome, Result};
use crate::routes::AppError;
use crate::{db, AppState};
use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use tokio::task::spawn_blocking;

#[derive(Deserialize, Default)]
pub struct TasksQuery {
    pub status: Option<String>,
    pub project: Option<String>,
}

pub(crate) struct TaskListRow {
    pub id: String,
    pub project_ident: String,
    pub title: String,
    pub status: String,
    pub status_class: &'static str,
    pub kind: String,
    pub labels: Vec<String>,
    pub owner: Option<String>,
    pub age: String,
    pub when: String,
    pub comments: i64,
}

pub(crate) struct ProjectTaskRow {
    pub ident: String,
    pub todo: i64,
    pub in_progress: i64,
    pub done: i64,
}

#[derive(Template)]
#[template(path = "tasks.html")]
struct TasksTemplate {
    head: String,
    shell_open: String,
    shell_close: String,
    status_filter: String,
    project_filter: String,
    project_qs: String,
    rows: Vec<TaskListRow>,
    project_stats: Vec<ProjectTaskRow>,
}

fn status_class(status: &str) -> &'static str {
    match status {
        "in_progress" => "gw-chip-ok",
        "done" => "",
        _ => "gw-chip-warn",
    }
}

pub async fn tasks_index_page(
    State(state): State<AppState>,
    Query(q): Query<TasksQuery>,
) -> Result<Html<String>> {
    let status_filter = q
        .status
        .as_deref()
        .filter(|s| matches!(*s, "open" | "in_progress" | "todo" | "done"))
        .unwrap_or("open")
        .to_string();
    let project_filter = q
        .project
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .unwrap_or_default();
    let statuses: Vec<String> = match status_filter.as_str() {
        "in_progress" => vec!["in_progress".into()],
        "todo" => vec!["todo".into()],
        "done" => vec!["done".into()],
        _ => vec!["in_progress".into(), "todo".into()],
    };
    let now = db::now_ms();
    let db_handle = state.db.clone();
    let pf = project_filter.clone();
    let (theme, tasks, stats) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        let filter = if pf.is_empty() {
            None
        } else {
            Some(pf.as_str())
        };
        Ok((
            db::get_theme(&conn)?,
            db::list_tasks_across_projects(&conn, &statuses, filter, 300)?,
            db::list_project_task_stats(&conn)?,
        ))
    })
    .await??;
    let rows = tasks
        .into_iter()
        .map(|t| TaskListRow {
            status_class: status_class(&t.task.status),
            age: fmt_relative(now, t.task.updated_at),
            when: fmt_datetime(t.task.updated_at),
            id: t.task.id,
            project_ident: t.project_ident,
            title: t.task.title,
            status: t.task.status,
            kind: t.task.kind,
            labels: t.task.labels,
            owner: t.task.owner_agent_id,
            comments: t.task.comment_count,
        })
        .collect();
    let project_stats = stats
        .into_iter()
        .filter(|p| p.todo_count + p.in_progress_count + p.done_count > 0)
        .map(|p| ProjectTaskRow {
            ident: p.ident,
            todo: p.todo_count,
            in_progress: p.in_progress_count,
            done: p.done_count,
        })
        .collect();
    let chrome = PageChrome::global("agent-gateway — Tasks", "Tasks", "tasks", &theme, "");
    render(&TasksTemplate {
        head: chrome.head,
        shell_open: chrome.shell_open,
        shell_close: chrome.shell_close,
        project_qs: if project_filter.is_empty() {
            String::new()
        } else {
            format!("&project={project_filter}")
        },
        status_filter,
        project_filter,
        rows,
        project_stats,
    })
}

/// GET /projects/:ident/tasks/:id — canonical task detail page.
pub async fn task_detail_page(
    State(state): State<AppState>,
    Path((ident, task_id)): Path<(String, String)>,
) -> Result<Html<String>> {
    let db_handle = state.db.clone();
    let lookup_ident = ident.clone();
    let lookup_id = task_id.clone();
    let (theme, detail) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        db::reclaim_stale_tasks(&conn, &lookup_ident)?;
        Ok((
            db::get_theme(&conn)?,
            db::get_task_detail(&conn, &lookup_ident, &lookup_id)?,
        ))
    })
    .await??;
    let detail = detail.ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("task '{task_id}' not found in project '{ident}'"),
        )
    })?;
    Ok(Html(crate::routes::render_task_link_page(&detail, &theme)))
}

/// GET /task-link/:ref — resolve a short task id prefix across projects and
/// redirect to the canonical task route.
pub async fn task_link_redirect(
    State(state): State<AppState>,
    Path(task_ref): Path<String>,
) -> Result<Response> {
    let task_ref = task_ref.to_ascii_lowercase();
    if task_ref.len() < 8
        || task_ref.len() > 36
        || !task_ref.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "task reference must be 8-36 hexadecimal/UUID characters".into(),
        ));
    }
    let db_handle = state.db.clone();
    let matches = spawn_blocking(move || {
        let conn = db_handle.lock().unwrap();
        db::find_tasks_by_id_prefix(&conn, &task_ref, 2)
    })
    .await??;
    match matches.as_slice() {
        [] => Err(AppError(
            StatusCode::NOT_FOUND,
            "no task matches that reference".into(),
        )),
        [task] => Ok(Redirect::permanent(&format!(
            "/projects/{}/tasks/{}",
            task.project_ident, task.id
        ))
        .into_response()),
        _ => Err(AppError(
            StatusCode::CONFLICT,
            "task reference is ambiguous; use more of the task ID".into(),
        )),
    }
}
