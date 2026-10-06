//! Read-only history from the retired executor. No scheduling or agent launch.
use crate::db;
use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS execution_runs (
        id TEXT PRIMARY KEY,
        project_ident TEXT NOT NULL REFERENCES projects(ident),
        task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
        trigger TEXT NOT NULL,
        dedup_key TEXT UNIQUE NOT NULL,
        status TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        finished_at INTEGER,
        output TEXT NOT NULL DEFAULT ''
    );
    CREATE INDEX IF NOT EXISTS execution_queue ON execution_runs(status, created_at);
    CREATE TABLE IF NOT EXISTS execution_attempts (
        run_id TEXT NOT NULL REFERENCES execution_runs(id) ON DELETE CASCADE,
        position INTEGER NOT NULL,
        body TEXT NOT NULL,
        PRIMARY KEY(run_id, position)
    );
    CREATE TABLE IF NOT EXISTS task_subtasks (
        parent_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        child_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
        CHECK(parent_id != child_id)
    );
    CREATE INDEX IF NOT EXISTS task_subtasks_parent ON task_subtasks(parent_id);",
    )?;
    // Additive migration for gateways that already have execution history.
    let columns = conn
        .prepare("PRAGMA table_info(execution_runs)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (name, definition) in [
        ("started_at", "INTEGER"),
        ("updated_at", "INTEGER"),
        ("client", "TEXT"),
        ("model", "TEXT"),
        ("executor", "TEXT NOT NULL DEFAULT 'headless'"),
        ("progress", "TEXT NOT NULL DEFAULT ''"),
        ("summary", "TEXT NOT NULL DEFAULT ''"),
        ("summary_source", "TEXT NOT NULL DEFAULT 'missing'"),
        ("session_key", "TEXT"),
        ("last_sequence", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !columns.iter().any(|column| column == name) {
            conn.execute_batch(&format!(
                "ALTER TABLE execution_runs ADD COLUMN {name} {definition}"
            ))?;
        }
    }
    conn.execute("UPDATE execution_runs SET updated_at=COALESCE(finished_at,created_at) WHERE updated_at IS NULL", [])?;
    // Retire unfinished legacy runs without resuming their work or changing tasks.
    conn.execute("UPDATE execution_runs SET status='retired',finished_at=?1,updated_at=?1,progress='Legacy executor removed; no work resumed' WHERE finished_at IS NULL", [db::now_ms()])?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct Run {
    pub id: String,
    pub project_ident: String,
    pub task_id: Option<String>,
    pub trigger: String,
    pub status: String,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    pub output: String,
    pub attempts: Vec<serde_json::Value>,
    pub task_title: Option<String>,
    pub started_at: Option<i64>,
    pub updated_at: i64,
    pub client: Option<String>,
    pub model: Option<String>,
    pub executor: String,
    pub progress: String,
    pub summary: String,
    pub summary_source: String,
    pub session_key: Option<String>,
    pub last_sequence: i64,
}

pub fn list_runs(conn: &Connection, project: Option<&str>, task: Option<&str>) -> Result<Vec<Run>> {
    select_runs(conn, project, task)
}

fn select_runs(conn: &Connection, project: Option<&str>, task: Option<&str>) -> Result<Vec<Run>> {
    let mut statement = conn.prepare("SELECT r.id,r.project_ident,r.task_id,r.trigger,r.status,r.created_at,r.finished_at,r.output,t.title,r.started_at,COALESCE(r.updated_at,r.created_at),r.client,r.model,r.executor,r.progress,r.summary,r.summary_source,r.session_key,r.last_sequence FROM execution_runs r LEFT JOIN tasks t ON t.id=r.task_id WHERE (?1 IS NULL OR r.project_ident=?1) AND (?2 IS NULL OR r.task_id=?2) ORDER BY CASE WHEN r.finished_at IS NULL THEN 0 ELSE 1 END,r.created_at DESC,r.id DESC LIMIT 100")?;
    let mut runs = statement
        .query_map(params![project, task], |row| {
            Ok(Run {
                id: row.get(0)?,
                project_ident: row.get(1)?,
                task_id: row.get(2)?,
                trigger: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
                finished_at: row.get(6)?,
                output: row.get(7)?,
                attempts: vec![],
                task_title: row.get(8)?,
                started_at: row.get(9)?,
                updated_at: row.get(10)?,
                client: row.get(11)?,
                model: row.get(12)?,
                executor: row.get(13)?,
                progress: row.get(14)?,
                summary: row.get(15)?,
                summary_source: row.get(16)?,
                session_key: row.get(17)?,
                last_sequence: row.get(18)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for run in &mut runs {
        let mut statement =
            conn.prepare("SELECT body FROM execution_attempts WHERE run_id=?1 ORDER BY position")?;
        for body in statement.query_map([&run.id], |r| r.get::<_, String>(0))? {
            run.attempts.push(serde_json::from_str(&body?)?);
        }
    }
    Ok(runs)
}
