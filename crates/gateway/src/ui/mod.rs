//! Human-facing control panel.
//!
//! Every page shares the shell in [`shell`] (sidebar, header, project tab
//! strip). New pages are askama templates under `templates/` that extend
//! `base.html`; older pages still assemble markup in `routes.rs` but render
//! the same shell through [`shell::control_panel_open`] so the navigation is
//! defined once.

pub mod home;
pub mod projects;
pub mod settings;
pub mod shell;

pub use shell::{
    control_panel_close, control_panel_head, control_panel_open, control_panel_open_project, he,
};

use crate::routes::AppError;
use crate::AppState;
use askama::Template;
use axum::http::StatusCode;
use axum::response::Html;

pub(crate) type Result<T> = std::result::Result<T, AppError>;

/// Render a template into an HTML response, mapping template errors to 500.
pub(crate) fn render<T: Template>(template: &T) -> Result<Html<String>> {
    template
        .render()
        .map(Html)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("render: {e}")))
}

/// Load the stored theme; falls back to the default on any error.
pub(crate) async fn theme(state: &AppState) -> String {
    let db = state.db.clone();
    tokio::task::spawn_blocking(move || {
        let conn = db.lock().unwrap();
        crate::db::get_theme(&conn).unwrap_or_else(|_| crate::db::DEFAULT_THEME.to_string())
    })
    .await
    .unwrap_or_else(|_| crate::db::DEFAULT_THEME.to_string())
}

/// Human-friendly age: "just now", "5m ago", "3h ago", "2d ago", "3w ago".
pub fn fmt_relative(now_ms: i64, at_ms: i64) -> String {
    let delta = now_ms - at_ms;
    if delta < 0 {
        return "just now".to_string();
    }
    let secs = delta / 1000;
    match secs {
        s if s < 45 => "just now".to_string(),
        s if s < 3600 => format!("{}m ago", (s / 60).max(1)),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 14 * 86_400 => format!("{}d ago", s / 86_400),
        s if s < 60 * 86_400 => format!("{}w ago", s / (7 * 86_400)),
        s => format!("{}mo ago", s / (30 * 86_400)),
    }
}

/// UTC timestamp, `YYYY-MM-DD HH:MM`.
pub fn fmt_datetime(at_ms: i64) -> String {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_millis_opt(at_ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

/// Base fields every templated page needs.
pub(crate) struct PageChrome {
    pub head: String,
    pub shell_open: String,
    pub shell_close: String,
}

impl PageChrome {
    pub fn global(title: &str, page_title: &str, active: &str, theme: &str, extra: &str) -> Self {
        Self {
            head: control_panel_head(title, theme, extra),
            shell_open: control_panel_open(page_title, active),
            shell_close: control_panel_close(),
        }
    }

    pub fn project(title: &str, page_title: &str, ident: &str, tab: &str, theme: &str) -> Self {
        Self {
            head: control_panel_head(title, theme, ""),
            shell_open: control_panel_open_project(page_title, ident, tab),
            shell_close: control_panel_close(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time_buckets() {
        let now = 10_000_000_000;
        assert_eq!(fmt_relative(now, now), "just now");
        assert_eq!(fmt_relative(now, now - 5 * 60_000), "5m ago");
        assert_eq!(fmt_relative(now, now - 3 * 3_600_000), "3h ago");
        assert_eq!(fmt_relative(now, now - 2 * 86_400_000), "2d ago");
        assert_eq!(fmt_relative(now, now - 21 * 86_400_000), "3w ago");
        assert_eq!(fmt_relative(now, now + 5), "just now");
    }
}
