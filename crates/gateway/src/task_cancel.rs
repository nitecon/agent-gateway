//! Explicit task cancellation reports. The task row, comments, prior claimant
//! and lifecycle history are kept; a report never proves an executor stopped.
use crate::{
    db,
    projects::normalize_project_ident,
    routes::AppError,
    task_dispatch::Conflict,
    task_events::{self, AgentOrigin},
    AppState,
};
use anyhow::{bail, Result};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

const REASON_BYTES: usize = 16384;
const OLD_STATUS_CHECK: &str = "CHECK(status IN ('todo','in_progress','done'))";
const CANCEL_STATUS_CHECK: &str = "CHECK(status IN ('todo','in_progress','done','cancelled'))";
const NEW_STATUS_CHECK: &str =
    "CHECK(status IN ('todo','in_progress','user_review','done','cancelled'))";

/// Widen the tasks status CHECK once, then create the cancellation record table.
/// Runs before task lifecycle triggers are (re)created.
pub fn initialize(conn: &Connection) -> Result<()> {
    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='tasks'",
        [],
        |r| r.get(0),
    )?;
    if !sql.contains(NEW_STATUS_CHECK) {
        let old_check = if sql.contains(CANCEL_STATUS_CHECK) {
            CANCEL_STATUS_CHECK
        } else if sql.contains(OLD_STATUS_CHECK) {
            OLD_STATUS_CHECK
        } else {
            bail!("unrecognized tasks status constraint; refusing task status migration");
        };
        // Dropping `tasks` with foreign keys on would cascade-delete comments.
        conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
        let off: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
        if off != 0 {
            bail!("foreign keys must be off to rebuild tasks");
        }
        let indexes: Vec<String> = conn
            .prepare("SELECT sql FROM sqlite_master WHERE type='index' AND tbl_name='tasks' AND sql IS NOT NULL")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        // The stored name may be quoted after an earlier rename; keep only the body.
        let body = sql.replacen(old_check, NEW_STATUS_CHECK, 1);
        let create = format!(
            "CREATE TABLE tasks_cancel_rebuild {}",
            &body[body.find('(').unwrap_or(0)..]
        );
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "DROP TRIGGER IF EXISTS task_event_created;
             DROP TRIGGER IF EXISTS task_event_commented;
             DROP TRIGGER IF EXISTS task_event_completed;
             DROP TRIGGER IF EXISTS task_event_cancelled;",
        )?;
        tx.execute_batch(&create)?;
        tx.execute_batch(
            "INSERT INTO tasks_cancel_rebuild SELECT * FROM tasks;
             DROP TABLE tasks;
             ALTER TABLE tasks_cancel_rebuild RENAME TO tasks;",
        )?;
        for index in &indexes {
            tx.execute_batch(index)?;
        }
        let violations: i64 =
            tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
        if violations != 0 {
            bail!("tasks rebuild would leave {violations} foreign key violations");
        }
        tx.commit()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_cancellations (
            task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
            project_ident TEXT NOT NULL, reason TEXT NOT NULL,
            prior_status TEXT NOT NULL, prior_owner_agent_id TEXT,
            prior_owner_origin_json TEXT, prior_started_at INTEGER,
            reporter_origin_json TEXT NOT NULL, cancelled_at INTEGER NOT NULL
        );",
    )?;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cancellation {
    pub task_id: String,
    pub project_ident: String,
    pub reason: String,
    pub prior_status: String,
    pub prior_owner_agent_id: Option<String>,
    pub prior_owner_origin: Option<AgentOrigin>,
    pub prior_started_at: Option<i64>,
    pub reporter_origin: AgentOrigin,
    pub cancelled_at: i64,
    /// Always false: the gateway records the report, it cannot observe executors.
    pub executor_stop_confirmed: bool,
}

#[derive(Debug, Serialize)]
pub struct CancelResponse {
    pub task: db::Task,
    pub cancellation: Cancellation,
}

pub fn get(conn: &Connection, project: &str, task_id: &str) -> Result<Option<Cancellation>> {
    Ok(conn
        .query_row(
            "SELECT task_id,project_ident,reason,prior_status,prior_owner_agent_id,
                    prior_owner_origin_json,prior_started_at,reporter_origin_json,cancelled_at
             FROM task_cancellations WHERE task_id=?1 AND project_ident=?2",
            params![task_id, project],
            |r| {
                Ok(Cancellation {
                    task_id: r.get(0)?,
                    project_ident: r.get(1)?,
                    reason: r.get(2)?,
                    prior_status: r.get(3)?,
                    prior_owner_agent_id: r.get(4)?,
                    prior_owner_origin: task_events::read_origin(r, 5)?,
                    prior_started_at: r.get(6)?,
                    reporter_origin: task_events::read_origin(r, 7)?.ok_or(
                        rusqlite::Error::InvalidColumnType(
                            7,
                            "reporter_origin_json".into(),
                            rusqlite::types::Type::Null,
                        ),
                    )?,
                    cancelled_at: r.get(8)?,
                    executor_stop_confirmed: false,
                })
            },
        )
        .optional()?)
}

/// Cancel one task as `actor`. `Ok(None)` means the task is not in `project`.
/// Eligibility: an unowned TODO task may be cancelled by any exact-origin
/// caller (the same set that may claim it); an owned task only by the exact
/// claiming session+instance. Owned tasks without bound origin are refused.
pub fn cancel(
    conn: &Connection,
    project: &str,
    task_id: &str,
    actor: &AgentOrigin,
    reason: &str,
) -> Result<Option<CancelResponse>> {
    let project = normalize_project_ident(project);
    let reason = reason.trim();
    task_events::mutation(conn, Some(actor), |conn| {
        let Some(before) = db::get_task_detail(conn, &project, task_id)?.map(|d| d.task) else {
            return Ok(None);
        };
        if before.status == "cancelled" {
            let existing = get(conn, &project, task_id)?
                .ok_or(Conflict("cancelled task has no cancellation record"))?;
            // Exact replay by the same reporter acknowledges the same record.
            if existing.reporter_origin == *actor && existing.reason == reason {
                return Ok(Some(CancelResponse {
                    task: before,
                    cancellation: existing,
                }));
            }
            return Err(Conflict("task is already cancelled").into());
        }
        if before.status == "user_review" {
            return Err(Conflict("tasks in user review require a user decision").into());
        }
        if before.status == "done" {
            return Err(Conflict("completed tasks cannot be cancelled").into());
        }
        if crate::task_dispatch::managed(conn, task_id)? {
            return Err(Conflict("dispatch tasks require the dispatch cancel operation").into());
        }
        let owned = before.owner_agent_id.is_some() || before.status == "in_progress";
        match &before.owner_origin {
            Some(owner)
                if owner.session_id != actor.session_id
                    || owner.instance_id != actor.instance_id =>
            {
                return Err(task_events::OwnershipConflict.into())
            }
            None if owned => return Err(task_events::OwnershipConflict.into()),
            _ => {}
        }
        let now = db::now_ms();
        conn.execute(
            "INSERT INTO task_cancellations(task_id,project_ident,reason,prior_status,
                prior_owner_agent_id,prior_owner_origin_json,prior_started_at,
                reporter_origin_json,cancelled_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                task_id,
                project,
                reason,
                before.status,
                before.owner_agent_id,
                before
                    .owner_origin
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                before.started_at,
                serde_json::to_string(actor)?,
                now
            ],
        )?;
        // Owner, owner origin and started_at stay as prior-claim provenance.
        conn.execute(
            "UPDATE tasks SET status='cancelled',updated_at=?1 WHERE id=?2 AND project_ident=?3",
            params![now, task_id, project],
        )?;
        let task = db::get_task_detail(conn, &project, task_id)?
            .ok_or_else(|| anyhow::anyhow!("task vanished during cancellation"))?
            .task;
        let cancellation = get(conn, &project, task_id)?
            .ok_or_else(|| anyhow::anyhow!("cancellation record missing"))?;
        Ok(Some(CancelResponse { task, cancellation }))
    })
}

fn not_found(project: &str, task_id: &str) -> AppError {
    AppError(
        StatusCode::NOT_FOUND,
        format!("task '{task_id}' not found in project '{project}'"),
    )
}

/// `POST /v1/projects/{ident}/tasks/{id}/cancellation`
pub async fn create_handler(
    State(state): State<AppState>,
    Path((project, task_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<CancelRequest>,
) -> Result<Json<CancelResponse>, AppError> {
    let actor = task_events::origin(&headers)?.ok_or_else(|| {
        AppError(
            StatusCode::BAD_REQUEST,
            "exact agent origin required".into(),
        )
    })?;
    let reason = req.reason.trim();
    if reason.is_empty() || reason.len() > REASON_BYTES {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("reason must be 1..={REASON_BYTES} bytes"),
        ));
    }
    let database = state.db.clone();
    tokio::task::spawn_blocking(move || {
        let conn = database.lock().unwrap();
        cancel(&conn, &project, &task_id, &actor, &req.reason)?
            .map(Json)
            .ok_or_else(|| not_found(&project, &task_id))
    })
    .await?
}

/// `GET /v1/projects/{ident}/tasks/{id}/cancellation`
pub async fn get_handler(
    State(state): State<AppState>,
    Path((project, task_id)): Path<(String, String)>,
) -> Result<Json<Cancellation>, AppError> {
    let project = normalize_project_ident(&project);
    let database = state.db.clone();
    tokio::task::spawn_blocking(move || {
        let conn = database.lock().unwrap();
        get(&conn, &project, &task_id)?
            .map(Json)
            .ok_or_else(|| not_found(&project, &task_id))
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
        routing::{get, patch},
        Router,
    };
    use serde_json::{json, Value};
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };
    use tower::ServiceExt;

    fn origin(n: u8) -> AgentOrigin {
        AgentOrigin {
            session_id: format!("{n:08x}-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
            instance_id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into(),
            provider: "claude".into(),
            os: "linux".into(),
        }
    }

    fn database() -> db::Db {
        let database = db::open(":memory:").unwrap();
        database
            .lock()
            .unwrap()
            .execute("INSERT INTO projects(ident,channel_name,room_id,created_at) VALUES ('example','none','',0)", [])
            .unwrap();
        database
    }

    fn task(conn: &Connection) -> db::Task {
        db::insert_task(conn, "example", "Fixture", None, None, &[], None, "fixture").unwrap()
    }

    fn claim(conn: &Connection, id: &str, actor: &AgentOrigin) {
        task_events::mutation(conn, Some(actor), |conn| {
            db::reorder_tasks_in_column(
                conn,
                "example",
                "in_progress",
                &[id.to_string()],
                Some("owner-agent"),
            )
        })
        .unwrap();
    }

    fn conflict(err: anyhow::Error) -> bool {
        err.downcast_ref::<Conflict>().is_some()
            || err
                .downcast_ref::<task_events::OwnershipConflict>()
                .is_some()
    }

    #[test]
    fn migration_widens_status_and_preserves_rows_comments_and_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE projects (ident TEXT PRIMARY KEY);
             CREATE TABLE tasks (
                id TEXT PRIMARY KEY, project_ident TEXT NOT NULL REFERENCES projects(ident),
                status TEXT NOT NULL DEFAULT 'todo'
                       CHECK(status IN ('todo','in_progress','done')));
             ALTER TABLE tasks ADD COLUMN owner_origin_json TEXT;
             CREATE INDEX idx_tasks_project_status ON tasks(project_ident, status);
             CREATE TABLE task_comments (id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE);
             INSERT INTO projects VALUES ('example');
             INSERT INTO tasks VALUES ('t1','example','in_progress','{}');
             INSERT INTO task_comments VALUES ('c1','t1');",
        )
        .unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap(); // idempotent
        let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(
            count("SELECT COUNT(*) FROM task_comments WHERE task_id='t1'"),
            1
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM tasks WHERE owner_origin_json='{}'"),
            1
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM sqlite_master WHERE name='idx_tasks_project_status'"),
            1
        );
        assert_eq!(count("PRAGMA foreign_keys"), 1);
        conn.execute("UPDATE tasks SET status='user_review'", [])
            .unwrap();
        conn.execute("UPDATE tasks SET status='cancelled'", [])
            .unwrap();
        assert!(conn.execute("UPDATE tasks SET status='bogus'", []).is_err());
        // Foreign keys still point at the rebuilt table.
        assert!(conn
            .execute("INSERT INTO task_comments VALUES ('c2','missing')", [])
            .is_err());
    }

    #[test]
    fn full_schema_upgrade_keeps_history_and_lifecycle_triggers() {
        for old_check in [OLD_STATUS_CHECK, CANCEL_STATUS_CHECK] {
            full_schema_upgrade(old_check);
        }
    }

    fn full_schema_upgrade(old_check: &str) {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/task-cancel-fixture"
        );
        std::fs::create_dir_all(dir).unwrap();
        let path = format!("{dir}/upgrade-{}.db", uuid::Uuid::now_v7());
        let id = {
            let database = db::open(&path).unwrap();
            let conn = database.lock().unwrap();
            conn.execute("INSERT INTO projects(ident,channel_name,room_id,created_at) VALUES ('example','none','',0)", []).unwrap();
            let t = task(&conn);
            db::insert_comment(&conn, &t.id, "fixture", "agent", "history").unwrap();
            // Recreate the pre-cancellation constraint with the real full schema.
            let sql: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name='tasks'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
            conn.execute_batch(&sql.replacen(NEW_STATUS_CHECK, old_check, 1).replacen(
                "CREATE TABLE tasks",
                "CREATE TABLE tasks_old_shape",
                1,
            ))
            .unwrap();
            conn.execute_batch("DROP TRIGGER task_event_created; DROP TRIGGER task_event_commented; DROP TRIGGER task_event_completed; DROP TRIGGER task_event_cancelled;
                INSERT INTO tasks_old_shape SELECT * FROM tasks; DROP TABLE tasks; ALTER TABLE tasks_old_shape RENAME TO tasks; PRAGMA foreign_keys=ON;").unwrap();
            task_events::initialize(&conn).unwrap();
            t.id
        };
        let database = db::open(&path).unwrap();
        let conn = database.lock().unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='tasks'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains(NEW_STATUS_CHECK));
        assert_eq!(
            db::get_task_detail(&conn, "example", &id)
                .unwrap()
                .unwrap()
                .comments
                .len(),
            1
        );
        let violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0);
        cancel(&conn, "example", &id, &origin(1), "stop")
            .unwrap()
            .unwrap();
        let kinds: Vec<String> = conn
            .prepare("SELECT kind FROM task_events ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(kinds, ["task_created", "task_commented", "task_cancelled"]);
        drop(conn);
        drop(database);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn todo_task_cancel_records_reporter_and_lifecycle_event() {
        let database = database();
        let conn = database.lock().unwrap();
        let t = task(&conn);
        db::insert_comment(&conn, &t.id, "fixture", "agent", "history").unwrap();
        let reporter = origin(1);
        let out = cancel(&conn, "example", &t.id, &reporter, " no longer needed ")
            .unwrap()
            .unwrap();
        assert_eq!(out.task.status, "cancelled");
        assert_eq!(out.task.done_at, None);
        assert_eq!(out.cancellation.reason, "no longer needed");
        assert_eq!(out.cancellation.prior_status, "todo");
        assert_eq!(out.cancellation.reporter_origin, reporter);
        assert!(!out.cancellation.executor_stop_confirmed);
        let detail = db::get_task_detail(&conn, "example", &t.id)
            .unwrap()
            .unwrap();
        assert_eq!(detail.comments.len(), 1);
        let (kind, actor): (String, String) = conn
            .query_row("SELECT kind,origin_json FROM task_events WHERE task_json->>'id'=?1 ORDER BY id DESC LIMIT 1", [&t.id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(kind, "task_cancelled");
        assert_eq!(
            serde_json::from_str::<AgentOrigin>(&actor).unwrap(),
            reporter
        );
        let completed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_events WHERE kind='task_completed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(completed, 0);
    }

    #[test]
    fn claimed_task_only_cancelled_by_exact_owner_and_keeps_prior_claim() {
        let database = database();
        let conn = database.lock().unwrap();
        let t = task(&conn);
        let owner = origin(1);
        claim(&conn, &t.id, &owner);
        let before = db::get_task_detail(&conn, "example", &t.id)
            .unwrap()
            .unwrap()
            .task;
        let mut same_session_other_instance = owner.clone();
        same_session_other_instance.instance_id = "cccccccc-cccc-4ccc-8ccc-cccccccccccc".into();
        for other in [origin(2), same_session_other_instance] {
            assert!(conflict(
                cancel(&conn, "example", &t.id, &other, "x").err().unwrap()
            ));
        }
        let out = cancel(&conn, "example", &t.id, &owner, "stop")
            .unwrap()
            .unwrap();
        assert_eq!(out.cancellation.prior_status, "in_progress");
        assert_eq!(out.cancellation.prior_owner_origin.as_ref(), Some(&owner));
        assert_eq!(
            out.cancellation.prior_owner_agent_id.as_deref(),
            Some("owner-agent")
        );
        assert_eq!(out.cancellation.prior_started_at, before.started_at);
        assert_eq!(out.task.owner_origin.as_ref(), Some(&owner));
        assert_eq!(out.task.owner_agent_id, before.owner_agent_id);
        assert_eq!(out.task.started_at, before.started_at);
    }

    #[test]
    fn unbound_owned_done_and_dispatch_tasks_are_refused() {
        let database = database();
        let conn = database.lock().unwrap();
        let legacy = task(&conn);
        conn.execute(
            "UPDATE tasks SET owner_agent_id='legacy' WHERE id=?1",
            [&legacy.id],
        )
        .unwrap();
        assert!(conflict(
            cancel(&conn, "example", &legacy.id, &origin(1), "x")
                .err()
                .unwrap()
        ));
        let done = task(&conn);
        conn.execute("UPDATE tasks SET status='done' WHERE id=?1", [&done.id])
            .unwrap();
        assert!(conflict(
            cancel(&conn, "example", &done.id, &origin(1), "x")
                .err()
                .unwrap()
        ));
        let managed = task(&conn);
        conn.execute("INSERT INTO task_dispatches(task_id,project_ident,document_json) VALUES (?1,'example','{}')", [&managed.id]).unwrap();
        assert!(conflict(
            cancel(&conn, "example", &managed.id, &origin(1), "x")
                .err()
                .unwrap()
        ));
        assert!(cancel(&conn, "example", "missing", &origin(1), "x")
            .unwrap()
            .is_none());
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM task_cancellations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn replay_is_idempotent_and_cancelled_is_terminal() {
        let database = database();
        let conn = database.lock().unwrap();
        let t = task(&conn);
        let reporter = origin(1);
        let first = cancel(&conn, "example", &t.id, &reporter, "stop")
            .unwrap()
            .unwrap();
        let again = cancel(&conn, "example", &t.id, &reporter, "stop")
            .unwrap()
            .unwrap();
        assert_eq!(first.cancellation, again.cancellation);
        assert!(conflict(
            cancel(&conn, "example", &t.id, &reporter, "other")
                .err()
                .unwrap()
        ));
        assert!(conflict(
            cancel(&conn, "example", &t.id, &origin(2), "stop")
                .err()
                .unwrap()
        ));
        for status in ["todo", "in_progress", "done"] {
            let err = task_events::mutation(&conn, Some(&reporter), |conn| {
                db::reorder_tasks_in_column(
                    conn,
                    "example",
                    status,
                    std::slice::from_ref(&t.id),
                    None,
                )
            });
            assert!(conflict(err.err().unwrap()), "{status}");
        }
        let status: String = conn
            .query_row("SELECT status FROM tasks WHERE id=?1", [&t.id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "cancelled");
    }

    fn state(database: db::Db) -> AppState {
        AppState {
            db: database,
            plugins: Arc::new(HashMap::new()),
            #[cfg(feature = "whatsapp")]
            whatsapp: None,
            default_channel: "discord".into(),
            api_key: "fixture-bearer".into(),
            ui_auth_enabled: true,
            retention_days: 30,
            bot_retention_days: 7,
            artifact_operations: db::ArtifactOperationsEnvelope::production_defaults(),
            artifact_body_schema_enabled: true,
            artifact_auth_enforced: false,
            update_available: Arc::new(Mutex::new(None)),
        }
    }

    async fn call(
        app: &Router,
        method: &str,
        path: &str,
        actor: Option<&AgentOrigin>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(a) = actor {
            req = req
                .header("x-agent-session-id", &a.session_id)
                .header("x-agent-instance-id", &a.instance_id)
                .header("x-agent-provider", &a.provider)
                .header("x-agent-os", &a.os);
        }
        let res = app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn http_contract() {
        let database = database();
        let id = task(&database.lock().unwrap()).id;
        let app = Router::new()
            .route(
                "/v1/projects/{ident}/tasks/{id}/cancellation",
                get(get_handler).post(create_handler),
            )
            .route(
                "/v1/projects/{ident}/tasks/{id}",
                patch(crate::routes::update_task_handler),
            )
            .with_state(state(database));
        let path = format!("/v1/projects/example/tasks/{id}/cancellation");
        let a = origin(1);
        assert_eq!(
            call(&app, "POST", &path, None, json!({"reason":"x"}))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(&app, "POST", &path, Some(&a), json!({"reason":"  "}))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(
                &app,
                "POST",
                &path,
                Some(&a),
                json!({"reason":"x","force":true})
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            call(&app, "GET", &path, None, Value::Null).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &app,
                "POST",
                "/v1/projects/example/tasks/missing/cancellation",
                Some(&a),
                json!({"reason":"x"})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let (status, body) = call(&app, "POST", &path, Some(&a), json!({"reason":"stop"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["task"]["status"], "cancelled");
        assert_eq!(
            body["cancellation"]["reporter_origin"]["session_id"],
            a.session_id
        );
        assert_eq!(body["cancellation"]["executor_stop_confirmed"], false);
        let (status, got) = call(&app, "GET", &path, None, Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got, body["cancellation"]);
        assert_eq!(
            call(&app, "POST", &path, Some(&a), json!({"reason":"stop"}))
                .await
                .1,
            body
        );
        assert_eq!(
            call(
                &app,
                "POST",
                &path,
                Some(&origin(2)),
                json!({"reason":"stop"})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let task_path = format!("/v1/projects/example/tasks/{id}");
        for status in ["todo", "in_progress", "done"] {
            let (code, _) = call(
                &app,
                "PATCH",
                &task_path,
                Some(&a),
                json!({"status": status}),
            )
            .await;
            assert_eq!(code, StatusCode::CONFLICT, "{status}");
        }
    }
}
