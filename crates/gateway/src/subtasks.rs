use crate::{db, routes::AppError, AppState};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::{params, Connection};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSubtask {
    pub title: String,
    pub description: Option<String>,
    pub specification: Option<String>,
    pub target_project_ident: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
}

pub fn list(conn: &Connection, parent_id: &str) -> anyhow::Result<Vec<db::Task>> {
    let mut statement = conn.prepare("SELECT t.id,t.project_ident FROM task_subtasks s JOIN tasks t ON t.id=s.child_id WHERE s.parent_id=?1 ORDER BY t.created_at,t.id")?;
    let refs = statement
        .query_map([parent_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    refs.into_iter()
        .map(|(id, project)| db::get_task_detail(conn, &project, &id).map(|t| t.unwrap().task))
        .collect()
}

pub async fn get(
    State(state): State<AppState>,
    Path((project, id)): Path<(String, String)>,
) -> Result<Json<Vec<db::Task>>, AppError> {
    let conn = state.db.lock().unwrap();
    if db::get_task_detail(&conn, &project, &id)?.is_none() {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            "parent task not found".into(),
        ));
    }
    Ok(Json(list(&conn, &id)?))
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((project, id)): Path<(String, String)>,
    Json(request): Json<CreateSubtask>,
) -> Result<Json<db::Task>, AppError> {
    let conn = state.db.lock().unwrap();
    let parent = db::get_task_detail(&conn, &project, &id)?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "parent task not found".into()))?;
    if parent.task.status == "done" {
        return Err(AppError(
            StatusCode::CONFLICT,
            "reopen the parent before adding subtasks".into(),
        ));
    }
    if request.title.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "title must be non-empty".into(),
        ));
    }
    let target = request.target_project_ident.as_deref().unwrap_or(&project);
    let target = db::get_project(&conn, target)?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "target project not found".into()))?;
    if target.archived_at.is_some() {
        return Err(AppError(
            StatusCode::CONFLICT,
            "target project is archived".into(),
        ));
    }
    let reporter = crate::routes::resolve_identity(None, &headers);
    let tx = conn.unchecked_transaction()?;
    let task = db::insert_task(
        &tx,
        &target.ident,
        request.title.trim(),
        request.description.as_deref(),
        request.specification.as_deref(),
        &request.labels,
        None,
        &reporter,
    )?;
    tx.execute(
        "INSERT INTO task_subtasks(parent_id,child_id) VALUES (?1,?2)",
        params![id, task.id],
    )?;
    tx.commit()?;
    Ok(Json(task))
}
