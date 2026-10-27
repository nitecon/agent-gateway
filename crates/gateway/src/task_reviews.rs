//! Completion reviews are ordinary queued tasks, created atomically once.
use crate::{db, routes::AppError, task_dispatch::Conflict, task_events, AppState};
use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_reviews (
        source_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        source_done_at INTEGER NOT NULL,
        review_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        PRIMARY KEY(source_task_id,source_done_at));",
    )?;
    Ok(())
}

pub fn ensure_reviewer(
    labels: &[String],
    origin: Option<&task_events::AgentOrigin>,
    agent_id: Option<&str>,
) -> Result<()> {
    let excluded = labels
        .iter()
        .filter_map(|label| label.strip_prefix("review-exclude:"));
    for session in excluded {
        if origin.is_some_and(|origin| origin.session_id == session)
            || agent_id == Some(session)
            || (origin.is_none() && agent_id.is_some())
        {
            return Err(Conflict("completion reviews require a different native agent").into());
        }
    }
    Ok(())
}

pub fn preserve_metadata(current: &[String], next: &[String]) -> Result<()> {
    if current.iter().any(|label| label == "task-review") {
        let markers = |labels: &[String]| {
            labels
                .iter()
                .filter(|label| {
                    label.as_str() == "task-review"
                        || label.starts_with("review-of:")
                        || label.starts_with("review-exclude:")
                })
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
        };
        if markers(current) != markers(next) {
            return Err(
                Conflict("completion review source and reviewer exclusion are immutable").into(),
            );
        }
    }
    Ok(())
}

/// Caller supplies the existing task-mutation transaction and DB mutex.
pub fn enqueue(conn: &Connection, project: &str, source_id: &str) -> Result<Option<db::Task>> {
    let source = db::get_task_detail(conn, project, source_id)?
        .ok_or(Conflict("source task not found"))?
        .task;
    if source.labels.iter().any(|label| label == "task-review")
        || (source.kind == "delegated" && source.delegated_to_task_id.is_some())
    {
        return Ok(None);
    }
    if source.status != "done" {
        return Err(Conflict("source task is not completed").into());
    }
    let completed = source
        .done_at
        .ok_or(Conflict("source completion timestamp is missing"))?;
    let existing: Option<String> = conn
        .query_row(
            "SELECT review_task_id FROM task_reviews WHERE source_task_id=?1 AND source_done_at=?2",
            params![source.id, completed],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(db::get_task_detail(conn, project, &id)?.map(|detail| detail.task));
    }
    let mut labels = vec!["task-review".to_owned(), format!("review-of:{}", source.id)];
    labels.extend(
        source
            .labels
            .iter()
            .filter(|label| label.starts_with("os:"))
            .cloned(),
    );
    if let Some(owner) = source
        .owner_origin
        .as_ref()
        .map(|owner| owner.session_id.as_str())
        .or(source.owner_agent_id.as_deref())
    {
        labels.push(format!("review-exclude:{owner}"));
    }
    let specification = format!("Review the completed work for task {}: {}.\nRead that task and its associated comments for implementation and validation evidence. Inspect the actual changes. Follow repository instructions and KISS/YAGNI: report concrete introduced defects, not speculative enhancements or style preferences. Review only; do not implement changes. For confirmed actionable defects, check for duplicates and create normal queued finding tasks with concise evidence and suggested corrections. Post review results (including no findings) on the source task and this review task, then mark this review task done. Do not run the legacy reviews complete command. Completing a review does not create another review.", source.id, source.title);
    // This helper only inserts an unowned ordinary task; no dispatch protocol
    // record or submitting-agent claim is created.
    let task = db::insert_unassigned_dispatch_task(
        conn,
        project,
        &format!("Review: {}", source.title),
        None,
        Some(&specification),
        &labels,
        None,
        "amux-autoreview",
    )?;
    conn.execute(
        "INSERT INTO task_reviews VALUES(?1,?2,?3)",
        params![source.id, completed, task.id],
    )?;
    Ok(Some(task))
}

pub async fn create(
    State(state): State<AppState>,
    Path((project, id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let task = tokio::task::spawn_blocking(move || -> Result<_> {
        let conn = state.db.lock().unwrap();
        task_events::mutation(&conn, None, |conn| enqueue(conn, &project, &id))
    })
    .await??;
    Ok(match task {
        Some(task) => Json(task).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn actor() -> task_events::AgentOrigin {
        task_events::AgentOrigin {
            session_id: uuid::Uuid::now_v7().to_string(),
            instance_id: uuid::Uuid::now_v7().to_string(),
            provider: "codex".into(),
            os: "linux".into(),
        }
    }
    fn update(
        conn: &Connection,
        id: &str,
        status: &str,
        actor: &task_events::AgentOrigin,
    ) -> Result<db::Task> {
        task_events::mutation(conn, Some(actor), |conn| {
            db::update_task(
                conn,
                "example",
                id,
                &db::TaskUpdate {
                    status: Some(status),
                    goal: None,
                    owner_agent_id: None,
                    rank: None,
                    title: None,
                    description: None,
                    details: None,
                    labels: None,
                    hostname: None,
                },
                Some(&actor.session_id),
            )?
            .ok_or_else(|| anyhow::anyhow!("missing fixture task"))
        })
    }
    fn fixture() -> (db::Db, db::Task, task_events::AgentOrigin) {
        let database = db::open(":memory:").unwrap();
        let conn = database.lock().unwrap();
        conn.execute("INSERT INTO projects(ident,channel_name,room_id,created_at) VALUES ('example','none','',0)", []).unwrap();
        let author = actor();
        let source = task_events::mutation(&conn, Some(&author), |conn| {
            db::insert_task(
                conn,
                "example",
                "Fix fixture",
                None,
                None,
                &["os:linux".into()],
                None,
                "author",
            )
        })
        .unwrap();
        update(&conn, &source.id, "in_progress", &author).unwrap();
        let source = update(&conn, &source.id, "done", &author).unwrap();
        drop(conn);
        (database, source, author)
    }
    #[test]
    fn review_is_unowned_deduplicated_independent_and_non_recursive() {
        let (database, source, author) = fixture();
        let conn = database.lock().unwrap();
        let review =
            task_events::mutation(&conn, None, |conn| enqueue(conn, "example", &source.id))
                .unwrap()
                .unwrap();
        assert_eq!(review.status, "todo");
        assert!(review.owner_origin.is_none() && review.owner_agent_id.is_none());
        assert!(review.labels.contains(&"os:linux".into()));
        assert!(update(&conn, &review.id, "in_progress", &author).is_err());
        assert_eq!(
            task_events::mutation(&conn, None, |conn| enqueue(conn, "example", &source.id))
                .unwrap()
                .unwrap()
                .id,
            review.id
        );
        let peer = actor();
        update(&conn, &review.id, "in_progress", &peer).unwrap();
        update(&conn, &review.id, "done", &peer).unwrap();
        assert!(
            task_events::mutation(&conn, None, |conn| enqueue(conn, "example", &review.id))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            task_events::mutation(&conn, None, |conn| enqueue(conn, "example", &source.id))
                .unwrap()
                .unwrap()
                .id,
            review.id
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM task_reviews", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn concurrent_hosts_create_one_review_and_rollback_leaves_no_orphan() {
        let (database, source, _) = fixture();
        {
            let conn = database.lock().unwrap();
            let result: Result<()> = task_events::mutation(&conn, None, |conn| {
                enqueue(conn, "example", &source.id)?;
                anyhow::bail!("simulate transaction failure")
            });
            assert!(result.is_err());
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let database = database.clone();
                let id = source.id.clone();
                std::thread::spawn(move || {
                    let conn = database.lock().unwrap();
                    task_events::mutation(&conn, None, |conn| enqueue(conn, "example", &id))
                        .unwrap()
                        .unwrap()
                        .id
                })
            })
            .collect();
        let ids: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(ids.iter().all(|id| id == &ids[0]));
    }
}
