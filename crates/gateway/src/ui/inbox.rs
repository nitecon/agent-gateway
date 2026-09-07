//! `GET /projects/:ident/inbox` — the threaded conversation view for one
//! project: filterable thread list, selected thread, composer, bulk resolve.

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

pub const INBOX_CSS: &str = r#"<style>
.gw-inbox { display: grid; grid-template-columns: minmax(16rem, 2fr) minmax(0, 3fr); gap: 1rem; align-items: start; }
.gw-inbox-list { max-height: calc(100vh - 14rem); overflow: auto; }
.gw-threads > li { padding: 0; display: block; }
.gw-thread-link { display: block; padding: 0.6rem 0.8rem; color: inherit; text-decoration: none; }
.gw-thread-link:hover { background: rgba(127,127,127,.08); }
.gw-thread-selected .gw-thread-link { background: rgba(37,99,235,.10); border-left: 3px solid var(--nd-primary, #2563eb); }
.gw-messages { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 0.75rem; }
.gw-msg { padding: 0.6rem 0.8rem; border-radius: 0.5rem; border: 1px solid var(--nd-border, rgba(127,127,127,.25)); }
.gw-msg-human { border-left: 3px solid #2563eb; }
.gw-msg-agent { border-left: 3px solid #16a34a; }
.gw-msg-bot, .gw-msg-webhook { border-left: 3px solid #d97706; opacity: 0.9; }
.gw-msg-system { border-left: 3px solid #6b7280; }
.gw-msg-head { display: flex; gap: 0.5rem; align-items: center; flex-wrap: wrap; font-size: 0.85rem; }
.gw-msg-subject { font-weight: 600; margin-top: 0.25rem; }
.gw-msg-body { white-space: pre-wrap; overflow-wrap: anywhere; font-size: 0.85rem; margin: 0.35rem 0 0; background: transparent; padding: 0; }
.gw-composer textarea { width: 100%; box-sizing: border-box; }
.gw-state-open { border-color: #d97706; color: #d97706; }
.gw-state-acknowledged { border-color: #2563eb; color: #2563eb; }
.gw-state-answered { border-color: #16a34a; color: #16a34a; }
.gw-state-resolved { opacity: 0.6; }
@media (max-width: 900px) { .gw-inbox { grid-template-columns: 1fr; } .gw-inbox-list { max-height: none; } }
</style>"#;

#[derive(Deserialize, Default)]
pub struct InboxQuery {
    pub kind: Option<String>,
    pub state: Option<String>,
    pub thread: Option<i64>,
}

pub(crate) struct ThreadRow {
    pub id: i64,
    pub title: String,
    pub author: String,
    pub author_kind: String,
    pub state: String,
    pub state_class: String,
    pub age: String,
    pub reply_count: i64,
    pub ack_count: i64,
    pub selected: bool,
}

pub(crate) struct MessageRow {
    pub author: String,
    pub author_kind: String,
    pub hostname: Option<String>,
    pub kind: String,
    pub subject: Option<String>,
    pub content: String,
    pub age: String,
    pub when: String,
}

pub(crate) struct ThreadPane {
    pub id: i64,
    pub title: String,
    pub state: String,
    pub state_class: String,
    pub resolved: bool,
    pub resolved_by: Option<String>,
    pub messages: Vec<MessageRow>,
    pub confirmations: Vec<String>,
}

#[derive(Template)]
#[template(path = "inbox.html")]
struct InboxTemplate {
    head: String,
    shell_open: String,
    shell_close: String,
    ident: String,
    kind_filter: String,
    state_filter: String,
    counts: db::InboxCounts,
    bulk_kinds_json: String,
    week_ago_ms: i64,
    threads: Vec<ThreadRow>,
    thread: Option<ThreadPane>,
}

/// Author kinds behind each filter key.
pub(crate) fn kinds_for_filter(kind: &str) -> &'static [&'static str] {
    match kind {
        "human" => &["human"],
        "agent" => &["agent"],
        "alerts" => &["bot", "webhook"],
        "system" => &["system"],
        "all" => &[],
        _ => &["human", "agent"],
    }
}

fn state_class(state: &str) -> String {
    format!("gw-state-{state}")
}

fn author_label(source: &str, author_kind: &str, agent_id: Option<&str>) -> String {
    match (source, agent_id) {
        ("agent", Some(agent)) | ("system", Some(agent)) => agent.to_string(),
        ("agent", None) => "agent".to_string(),
        ("system", None) => "gateway".to_string(),
        _ => match author_kind {
            "bot" => "bot".to_string(),
            "webhook" => "webhook".to_string(),
            _ => "human".to_string(),
        },
    }
}

fn message_row(now: i64, m: &db::Message) -> MessageRow {
    let author_kind = m
        .author_kind
        .clone()
        .unwrap_or_else(|| match m.source.as_str() {
            "user" => "human".to_string(),
            other => other.to_string(),
        });
    MessageRow {
        author: author_label(&m.source, &author_kind, m.agent_id.as_deref()),
        author_kind,
        hostname: m.hostname.clone(),
        kind: m.message_type.clone(),
        subject: m.subject.clone().filter(|s| !s.trim().is_empty()),
        content: m.content.clone(),
        age: fmt_relative(now, m.sent_at),
        when: fmt_datetime(m.sent_at),
    }
}

pub async fn inbox_page(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    Query(q): Query<InboxQuery>,
) -> Result<Html<String>> {
    let kind_filter = q
        .kind
        .as_deref()
        .filter(|k| matches!(*k, "human" | "agent" | "alerts" | "system" | "all"))
        .unwrap_or("conversation")
        .to_string();
    let state_filter = q
        .state
        .as_deref()
        .filter(|s| {
            matches!(
                *s,
                "open" | "acknowledged" | "answered" | "resolved" | "all"
            )
        })
        .unwrap_or("unresolved")
        .to_string();
    let now = db::now_ms();
    let db_handle = state.db.clone();
    let lookup = ident.clone();
    let kf = kind_filter.clone();
    let sf = state_filter.clone();
    let selected = q.thread;
    let (theme, project, counts, threads, thread) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        let Some(project) = db::get_project(&conn, &lookup)? else {
            return Ok((
                db::get_theme(&conn)?,
                None,
                db::InboxCounts::default(),
                vec![],
                None,
            ));
        };
        let counts = db::inbox_counts(&conn, &project.ident)?;
        let threads = db::list_message_threads(
            &conn,
            &project.ident,
            &db::ThreadFilter {
                state: Some(sf.as_str()),
                kinds: kinds_for_filter(&kf),
                agent_id: None,
                limit: 100,
                offset: 0,
            },
        )?;
        let chosen = selected.or_else(|| threads.first().map(|t| t.id));
        let thread = match chosen {
            Some(id) => db::get_thread(&conn, &project.ident, id)?,
            None => None,
        };
        Ok((
            db::get_theme(&conn)?,
            Some(project),
            counts,
            threads,
            thread,
        ))
    })
    .await??;
    let project = project.ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("project '{ident}' not found"),
        )
    })?;
    let ident = project.ident;
    let selected_id = thread.as_ref().map(|t| t.root.id);

    let thread_rows = threads
        .into_iter()
        .map(|t| ThreadRow {
            title: t
                .subject
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| {
                    let first = t.preview.lines().next().unwrap_or("").trim();
                    let mut s: String = first.chars().take(90).collect();
                    if first.chars().count() > 90 {
                        s.push('…');
                    }
                    if s.is_empty() {
                        "(no content)".to_string()
                    } else {
                        s
                    }
                }),
            author: author_label(&t.source, &t.author_kind, t.agent_id.as_deref()),
            state_class: state_class(&t.state),
            age: fmt_relative(now, t.last_activity_at),
            selected: Some(t.id) == selected_id,
            id: t.id,
            author_kind: t.author_kind,
            state: t.state,
            reply_count: t.reply_count,
            ack_count: t.ack_count,
        })
        .collect();

    let thread_pane = thread.map(|t| {
        let mut messages = vec![message_row(now, &t.root)];
        messages.extend(t.replies.iter().map(|m| message_row(now, m)));
        ThreadPane {
            id: t.root.id,
            title: t
                .root
                .subject
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| {
                    t.root
                        .content
                        .lines()
                        .next()
                        .unwrap_or("(no content)")
                        .chars()
                        .take(120)
                        .collect()
                }),
            state_class: state_class(&t.state),
            state: t.state,
            resolved: t.root.resolved_at.is_some(),
            resolved_by: t.root.resolved_by.clone(),
            messages,
            confirmations: t.confirmations.into_iter().map(|(a, _)| a).collect(),
        }
    });

    let bulk_kinds_json = kinds_for_filter(&kind_filter)
        .iter()
        .map(|k| format!("\"{k}\""))
        .collect::<Vec<_>>()
        .join(",");
    let chrome = PageChrome {
        head: super::control_panel_head(
            &format!("agent-gateway — {ident} inbox"),
            &theme,
            &format!("{}{}", INBOX_CSS, super::live_meta(20)),
        ),
        shell_open: super::control_panel_open_project("Inbox", &ident, "inbox"),
        shell_close: super::control_panel_close(),
    };
    render(&InboxTemplate {
        head: chrome.head,
        shell_open: chrome.shell_open,
        shell_close: chrome.shell_close,
        ident,
        kind_filter,
        state_filter,
        counts,
        bulk_kinds_json,
        week_ago_ms: now - 7 * 24 * 60 * 60 * 1000,
        threads: thread_rows,
        thread: thread_pane,
    })
}
