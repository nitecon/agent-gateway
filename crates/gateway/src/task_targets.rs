//! Ordinary agent availability uses the existing gateway authentication only.
use crate::{db, routes::AppError, task_dispatch::Target, task_events, AppState};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const TTL_MS: i64 = 120_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Routing {
    #[default]
    Auto,
    Unassigned,
    Broadcast,
    Session {
        target: Target,
    },
}

pub fn initialize(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_target_presence (
        project_ident TEXT NOT NULL REFERENCES projects(ident),
        instance_id TEXT NOT NULL, session_id TEXT NOT NULL,
        origin_json TEXT NOT NULL, generation TEXT NOT NULL, seen_at INTEGER NOT NULL, canonical_remote TEXT NOT NULL,
        PRIMARY KEY(project_ident,instance_id,session_id)
    )",
    )?;
    for column in ["display_name", "hostname"] {
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('task_target_presence') WHERE name=?1)",
            [column],
            |r| r.get(0),
        )?;
        if !present {
            conn.execute_batch(&format!(
                "ALTER TABLE task_target_presence ADD COLUMN {column} TEXT"
            ))?;
        }
    }
    Ok(())
}

pub fn connected(conn: &Connection, project: &str, target: &Target) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_target_presence s JOIN projects p ON p.ident=s.project_ident AND (p.canonical_remote=s.canonical_remote OR (p.kind='adhoc' AND p.canonical_remote IS NULL AND s.canonical_remote='')) WHERE
        s.project_ident=?1 AND instance_id=?2 AND session_id=?3 AND origin_json=?4
        AND generation=?5 AND seen_at>=?6)",
        params![
            project,
            target.origin.instance_id,
            target.origin.session_id,
            serde_json::to_string(&target.origin)?,
            target.generation,
            db::now_ms() - TTL_MS
        ],
        |r| r.get(0),
    )?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    pub display_name: Option<String>,
    pub hostname: Option<String>,
    pub canonical_remote: Option<String>,
    pub generation: String,
}

pub async fn heartbeat(
    State(state): State<AppState>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(req): Json<Heartbeat>,
) -> Result<Json<Value>, AppError> {
    let origin = task_events::origin(&headers)?.ok_or_else(|| {
        AppError(
            StatusCode::BAD_REQUEST,
            "native agent origin required".into(),
        )
    })?;
    let generation = uuid::Uuid::parse_str(&req.generation)
        .map_err(|_| AppError(StatusCode::BAD_REQUEST, "generation must be a UUID".into()))?
        .to_string();
    let display_name = label(req.display_name)?;
    let hostname = label(req.hostname)?;
    let project = crate::projects::normalize_project_ident(&project);
    let conn = state.db.lock().unwrap();
    let remote = require_binding(&conn, &project, req.canonical_remote.as_deref())?;
    let seen = db::now_ms();
    conn.execute("INSERT INTO task_target_presence(project_ident,instance_id,session_id,origin_json,generation,seen_at,canonical_remote,display_name,hostname)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(project_ident,instance_id,session_id)
        DO UPDATE SET origin_json=excluded.origin_json,generation=excluded.generation,seen_at=excluded.seen_at,canonical_remote=excluded.canonical_remote,display_name=excluded.display_name,hostname=excluded.hostname",
        params![project,origin.instance_id,origin.session_id,serde_json::to_string(&origin)?,generation,seen,remote.as_deref().unwrap_or(""),display_name,hostname])?;
    Ok(Json(
        json!({"target":Target{origin,generation},"seen_at":seen,"expires_at":seen+TTL_MS,"canonical_remote":remote,"display_name":display_name,"hostname":hostname}),
    ))
}

pub async fn list(
    State(state): State<AppState>,
    Path(project): Path<String>,
) -> Result<Json<Value>, AppError> {
    let project = crate::projects::normalize_project_ident(&project);
    let conn = state.db.lock().unwrap();
    if db::get_project(&conn, &project)?.is_none() {
        return Err(AppError(StatusCode::NOT_FOUND, "project not found".into()));
    }
    let mut stmt = conn.prepare(
        "SELECT s.origin_json,s.generation,s.seen_at,s.display_name,s.hostname FROM task_target_presence s JOIN projects p ON p.ident=s.project_ident AND (p.canonical_remote=s.canonical_remote OR (p.kind='adhoc' AND p.canonical_remote IS NULL AND s.canonical_remote=''))
        WHERE s.project_ident=?1 AND seen_at>=?2 ORDER BY instance_id,session_id",
    )?;
    let records = stmt
        .query_map(params![project, db::now_ms() - TTL_MS], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let targets=records.into_iter().map(|(origin,generation,seen,display_name,hostname)| -> anyhow::Result<Value> {
        Ok(json!({"target":Target{origin:serde_json::from_str(&origin)?,generation},"seen_at":seen,"expires_at":seen+TTL_MS,"display_name":display_name,"hostname":hostname}))
    }).collect::<anyhow::Result<Vec<_>>>()?;
    let remote = db::get_project(&conn, &project)?.and_then(|p| p.canonical_remote);
    Ok(Json(json!({"targets":targets,"canonical_remote":remote})))
}

/// Remote-backed projects require an exact repository match. Explicit ad-hoc
/// project bindings may omit the remote only while the registry has none.
pub fn require_binding(
    conn: &Connection,
    project: &str,
    supplied: Option<&str>,
) -> Result<Option<String>, AppError> {
    let project = db::get_project(conn, project)?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "project not found".into()))?;
    let supplied = supplied.map(str::trim).filter(|s| !s.is_empty());
    let Some(remote) = project.canonical_remote else {
        if project.kind == db::PROJECT_KIND_ADHOC && supplied.is_none() {
            return Ok(None);
        }
        return Err(AppError(StatusCode::CONFLICT, "project binding requires a matching canonical repository or an explicit ad-hoc project without a remote".into()));
    };
    let actual = supplied.and_then(crate::projects::parse_remote);
    let expected = crate::projects::parse_remote(&remote);
    if actual.is_none()
        || expected.is_none()
        || actual.map(|r| r.canonical) != expected.map(|r| r.canonical)
    {
        return Err(AppError(
            StatusCode::CONFLICT,
            "canonical_remote must match the project repository".into(),
        ));
    }
    Ok(Some(remote))
}

fn label(value: Option<String>) -> Result<Option<String>, AppError> {
    let value = value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if value
        .as_ref()
        .is_some_and(|s| s.len() > 128 || s.chars().any(char::is_control))
    {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "display_name and hostname must be at most 128 UTF-8 bytes with no control characters"
                .into(),
        ));
    }
    Ok(value)
}
