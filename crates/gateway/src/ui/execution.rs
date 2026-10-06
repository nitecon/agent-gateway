use super::{render, PageChrome, Result};
use crate::{db, routes::require_admin, task_events, AppState};
use askama::Template;
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::Html,
};

#[derive(Template)]
#[template(path = "execution.html")]
struct StreamTemplate {
    head: String,
    shell_open: String,
    shell_close: String,
    project: String,
    task_id: String,
    projects: Vec<String>,
}

pub async fn page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<task_events::EventQuery>,
) -> Result<Html<String>> {
    require_admin(&headers)?;
    let conn = state.db.lock().unwrap();
    let theme = db::get_theme(&conn)?;
    let mut chrome = PageChrome::global(
        "Task stream",
        "Settings — Task stream",
        "settings",
        &theme,
        "",
    );
    chrome.shell_open = super::shell::control_panel_open_settings(
        "Settings — Task stream",
        "task-stream",
        true,
        crate::ui_auth::request_user(&headers).is_some(),
    );
    render(&StreamTemplate {
        head: chrome.head,
        shell_open: chrome.shell_open,
        shell_close: chrome.shell_close,
        project: query
            .project
            .map(|p| crate::projects::normalize_project_ident(&p))
            .unwrap_or_default(),
        task_id: query.task_id.unwrap_or_default(),
        projects: db::all_projects(&conn)?
            .into_iter()
            .filter(|p| p.archived_at.is_none())
            .map(|p| p.ident)
            .collect(),
    })
}
