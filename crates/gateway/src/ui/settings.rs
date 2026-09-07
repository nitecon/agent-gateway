//! `GET /settings` — gateway-level settings: version, auth, channels,
//! retention, appearance. Project-level configuration lives on each project.

use super::{render, PageChrome, Result};
use crate::{db, AppState};
use askama::Template;
use axum::{extract::State, response::Html};
use tokio::task::spawn_blocking;

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    head: String,
    shell_open: String,
    shell_close: String,
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
}

pub async fn settings_page(State(state): State<AppState>) -> Result<Html<String>> {
    let db_handle = state.db.clone();
    let (theme, stats) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        Ok((
            db::get_theme(&conn)?,
            db::list_project_stats_filtered(&conn, true)?,
        ))
    })
    .await??;
    let archived_count = stats.iter().filter(|s| s.archived_at.is_some()).count();
    let mut plugins: Vec<String> = state.plugins.keys().cloned().collect();
    plugins.sort();
    let chrome = PageChrome::global(
        "agent-gateway — Settings",
        "Settings",
        "settings",
        &theme,
        "",
    );
    render(&SettingsTemplate {
        head: chrome.head,
        shell_open: chrome.shell_open,
        shell_close: chrome.shell_close,
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
    })
}
