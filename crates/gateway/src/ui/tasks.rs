//! `GET /tasks` — the task board with a project switcher; `GET
//! /projects/:ident/tasks/:id` — dedicated task detail route; `/task-link/:ref`
//! redirects to it.

use super::{control_panel_close, control_panel_head, control_panel_open, he, Result};
use crate::routes::AppError;
use crate::{db, AppState};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use tokio::task::spawn_blocking;

#[derive(Deserialize, Default)]
pub struct TasksQuery {
    pub project: Option<String>,
}

/// GET /tasks — the three-column drag-and-drop board with a project switcher.
///
/// `?project=` selects the board; without it (or when the ident is unknown)
/// the project with the most active work is shown, matching the ordering of
/// `db::list_project_task_stats`.
pub async fn tasks_index_page(
    State(state): State<AppState>,
    Query(q): Query<TasksQuery>,
) -> Result<Html<String>> {
    let requested = q
        .project
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty());
    let db_handle = state.db.clone();
    let (theme, stats) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        Ok((db::get_theme(&conn)?, db::list_project_task_stats(&conn)?))
    })
    .await??;

    let selected = requested
        .filter(|p| stats.iter().any(|s| &s.ident == p))
        .or_else(|| stats.first().map(|s| s.ident.clone()));

    let content = match selected {
        None => r#"  <section class="nd-card"><div class="nd-card-body nd-text-muted">No projects registered yet.</div></section>"#.to_string(),
        Some(ident) => {
            let options = stats
                .iter()
                .map(|s| {
                    format!(
                        r#"<option value="{v}"{sel}>{v} · {todo} todo / {in_progress} in progress</option>"#,
                        v = he(&s.ident),
                        sel = if s.ident == ident { " selected" } else { "" },
                        todo = s.todo_count,
                        in_progress = s.in_progress_count,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let toolbar = format!(
                r#"    <label class="nd-flex nd-gap-sm" style="align-items:center">
      <span class="nd-text-xs nd-text-muted">Project</span>
      <select class="nd-input" onchange="if (this.value) window.location.href = '/tasks?project=' + encodeURIComponent(this.value)">{options}</select>
    </label>"#
            );
            crate::routes::tasks_board_content(&he(&ident), &toolbar)
        }
    };

    Ok(Html(format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head(
            "agent-gateway — Tasks",
            &theme,
            crate::routes::TASKS_BOARD_HEAD_EXTRA
        ),
        open = control_panel_open("Tasks", "tasks"),
        content = content,
        close = control_panel_close(),
    )))
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
