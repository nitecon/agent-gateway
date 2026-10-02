//! `GET /settings/{section}` — gateway-level settings: version, auth, channels,
//! retention, appearance. Project-level configuration lives on each project.

use super::{render, PageChrome, Result};
use crate::{db, AppState};
use askama::Template;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, Redirect},
};
use tokio::task::spawn_blocking;

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    head: String,
    shell_open: String,
    shell_close: String,
    section: String,
    version: &'static str,
    update_available: Option<String>,
    ui_auth_enabled: bool,
    default_channel: String,
    plugins: Vec<String>,
    retention_days: u64,
    bot_retention_days: u64,
    theme_name: String,
    project_count: usize,
    archived_count: usize,
    current_user: Option<crate::ui_auth::SessionUser>,
    users: Vec<UserRow>,
}

pub(crate) struct UserRow {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub disabled: bool,
    pub is_self: bool,
    pub last_login: String,
}

pub async fn settings_page() -> Redirect {
    Redirect::to("/settings/gateway")
}

pub async fn section_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(section): Path<String>,
) -> Result<Html<String>> {
    let title = match section.as_str() {
        "gateway" => "Gateway",
        "account" => "Your account",
        "users" => "Users",
        "retention" => "Retention",
        "appearance" => "Appearance",
        "projects" => "Projects",
        _ => {
            return Err(crate::routes::AppError(
                StatusCode::NOT_FOUND,
                "settings section not found".into(),
            ))
        }
    };
    let current_user = crate::ui_auth::request_user(&headers);
    // Bearer callers never reach pages; no session means login is disabled,
    // in which case the page acts as an administrator.
    let is_admin = current_user.as_ref().map(|u| u.is_admin()).unwrap_or(true);
    if section == "users" {
        crate::routes::require_admin(&headers)?;
    }
    if section == "account" && current_user.is_none() {
        return Err(crate::routes::AppError(
            StatusCode::NOT_FOUND,
            "no signed-in account".into(),
        ));
    }
    let db_handle = state.db.clone();
    let now = db::now_ms();
    let (theme, stats, users) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        Ok((
            db::get_theme(&conn)?,
            db::list_project_stats_filtered(&conn, true)?,
            if is_admin {
                db::list_users(&conn)?
            } else {
                Vec::new()
            },
        ))
    })
    .await??;
    let self_id = current_user.as_ref().map(|u| u.id).unwrap_or(0);
    let users = users
        .into_iter()
        .map(|u| UserRow {
            id: u.id,
            is_self: u.id == self_id,
            disabled: u.disabled_at.is_some(),
            last_login: u
                .last_login_at
                .map(|at| super::fmt_relative(now, at))
                .unwrap_or_else(|| "never".to_string()),
            username: u.username,
            display_name: u.display_name,
            role: u.role,
        })
        .collect();
    let archived_count = stats.iter().filter(|s| s.archived_at.is_some()).count();
    let mut plugins: Vec<String> = state.plugins.keys().cloned().collect();
    plugins.sort();
    let chrome = PageChrome::global(
        &format!("agent-gateway — Settings — {title}"),
        &format!("Settings — {title}"),
        "settings",
        &theme,
        "",
    );
    render(&SettingsTemplate {
        head: chrome.head,
        shell_open: super::shell::control_panel_open_settings(
            &format!("Settings — {title}"),
            &section,
            is_admin,
            current_user.is_some(),
        ),
        shell_close: chrome.shell_close,
        section,
        version: env!("AGENT_GATEWAY_VERSION"),
        update_available: state.update_available.lock().unwrap().clone(),
        ui_auth_enabled: state.ui_auth_enabled,
        default_channel: state.default_channel.clone(),
        plugins,
        retention_days: state.retention_days,
        bot_retention_days: state.bot_retention_days,
        theme_name: theme,
        project_count: stats.len() - archived_count,
        archived_count,
        current_user,
        users,
    })
}
