//! One durable result subscriber, independent of executor assignment.
use crate::{db, routes::AppError, task_dispatch::Target, task_events, task_targets, AppState};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};

pub fn initialize(conn: &Connection) -> anyhow::Result<()> {
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tasks') WHERE name='requester_json')",
        [],
        |r| r.get(0),
    )?;
    if !present {
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN requester_json TEXT")?;
    }
    Ok(())
}

pub fn read(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<Target>> {
    row.get::<_, Option<String>>(index)?
        .map(|s| {
            serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()
}

/// Uses ordinary presence only; no separate dispatch credential/provisioning.
/// Called under the same transaction/DB lock as the task write.
pub fn validate(
    conn: &Connection,
    project: &str,
    labels: &[String],
    target: Option<&Target>,
) -> anyhow::Result<()> {
    let Some(target) = target else { return Ok(()) };
    if task_events::current_origin(conn)?.is_some_and(|o| o != target.origin) {
        return Err(task_events::OwnershipConflict.into());
    }
    if !task_targets::connected(conn, project, target)? {
        return Err(crate::task_dispatch::Conflict(
            "requester must be a current same-project task target",
        )
        .into());
    }
    if db::task_required_os(labels)?.is_some_and(|os| os != target.origin.os) {
        return Err(crate::task_dispatch::Conflict(
            "requester OS does not match the task os: label",
        )
        .into());
    }
    Ok(())
}

pub async fn capabilities() -> Json<Value> {
    Json(json!({"requester_subscriptions":1}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetRequester {
    // Value makes the field required while still accepting explicit null.
    pub requester: Value,
}

pub async fn set(
    State(state): State<AppState>,
    Path((project, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<SetRequester>,
) -> Result<Json<db::Task>, AppError> {
    let origin = crate::routes::task_creation_origin(&headers)?;
    let target: Option<Target> = serde_json::from_value(req.requester)
        .map_err(|e| AppError(axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;
    let conn = state.db.lock().unwrap();
    if db::get_task_detail(&conn, &project, &id)?.is_none() {
        return Err(AppError(
            axum::http::StatusCode::NOT_FOUND,
            "task not found".into(),
        ));
    }
    let task = task_events::mutation(&conn, origin.as_ref(), |conn| {
        let old = db::get_task_detail(conn, &project, &id)?
            .ok_or_else(|| anyhow::anyhow!("task not found"))?
            .task;
        if let Some(origin) = origin.as_ref() {
            if old.owner_origin.as_ref() != Some(origin)
                && old.requester.as_ref().map(|r| &r.origin) != Some(origin)
            {
                return Err(task_events::OwnershipConflict.into());
            }
        }
        validate(conn, &old.project_ident, &old.labels, target.as_ref())?;
        conn.execute(
            "UPDATE tasks SET requester_json=?1, updated_at=?2 WHERE id=?3 AND project_ident=?4",
            params![
                target.as_ref().map(serde_json::to_string).transpose()?,
                db::now_ms(),
                id,
                old.project_ident
            ],
        )?;
        Ok(db::get_task_detail(conn, &project, &id)?.unwrap().task)
    })?;
    Ok(Json(task))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_rows_migrate_without_requester_or_rewriting_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tasks(id TEXT PRIMARY KEY); INSERT INTO tasks VALUES('legacy');",
        )
        .unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        assert_eq!(
            conn.query_row("SELECT id,requester_json FROM tasks", [], |row| Ok((
                row.get::<_, String>(0)?,
                read(row, 1)?
            )))
            .unwrap(),
            ("legacy".into(), None)
        );
    }
}
