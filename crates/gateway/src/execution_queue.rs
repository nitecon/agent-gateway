use crate::{
    db,
    execution::{self, Candidate, Client},
    execution_client, AppState,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::time::Duration;

type PendingRun = (String, String, Option<String>, String);

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
}

pub fn runs(conn: &Connection, project: &str) -> Result<Vec<Run>> {
    let mut statement = conn.prepare("SELECT id, project_ident, task_id, trigger, status, created_at, finished_at, output FROM execution_runs WHERE project_ident=?1 ORDER BY created_at DESC, id DESC LIMIT 100")?;
    let mut runs = statement
        .query_map([project], |row| {
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

fn enqueue(
    conn: &Connection,
    project: &str,
    task: Option<&str>,
    trigger: &str,
    key: &str,
    now: i64,
) -> Result<()> {
    conn.execute("INSERT OR IGNORE INTO execution_runs (id,project_ident,task_id,trigger,dedup_key,status,created_at) VALUES (?1,?2,?3,?4,?5,'queued',?6)",
        params![uuid::Uuid::now_v7().to_string(), project, task, trigger, key, now])?;
    Ok(())
}

/// Poll durable tasks, including delegated and artifact-generated tasks; no delivery can
/// be lost between committing a task and notifying an in-memory worker.
pub fn schedule(conn: &Connection, now: i64) -> Result<()> {
    for project in db::all_projects(conn)? {
        if project.archived_at.is_some() {
            continue;
        }
        let policy = execution::project_settings(conn, &project.ident)?;
        if !policy.enabled {
            continue;
        }
        if policy.on_task_received {
            let mut statement = conn.prepare("SELECT id FROM tasks WHERE project_ident=?1 AND status='todo' AND kind='normal' AND owner_agent_id IS NULL ORDER BY created_at")?;
            let tasks = statement
                .query_map([&project.ident], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for task in tasks {
                enqueue(
                    conn,
                    &project.ident,
                    Some(&task),
                    "task",
                    &format!("task:{task}"),
                    now,
                )?;
            }
        }
        if let Some(seconds) = policy.cadence_seconds {
            let last: Option<i64> = conn.query_row("SELECT MAX(created_at) FROM execution_runs WHERE project_ident=?1 AND trigger='cadence'", [&project.ident], |r| r.get(0))?;
            let pending: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM execution_runs WHERE project_ident=?1 AND status IN ('queued','running'))", [&project.ident], |r| r.get(0))?;
            if !pending
                && last.is_none_or(|last| {
                    now.saturating_sub(last) >= (seconds as i64).saturating_mul(1000)
                })
            {
                enqueue(
                    conn,
                    &project.ident,
                    None,
                    "cadence",
                    &format!("cadence:{}:{now}", project.ident),
                    now,
                )?;
            }
        }
    }
    Ok(())
}

fn finish(conn: &Connection, id: &str, status: &str, message: &str) -> Result<()> {
    conn.execute(
        "UPDATE execution_runs SET status=?2, finished_at=?3, output=?4 WHERE id=?1",
        params![id, status, db::now_ms(), message],
    )?;
    Ok(())
}

pub fn prompt(project: &str, task: Option<&str>) -> String {
    let assignment = match task {
        Some(task) => format!("A new task {task} was allocated to project {project}. Fetch its current detail and comments, evaluate whether completion is feasible within this repository's scope, and claim it before making changes. If another agent owns it or it is done, stop."),
        None => format!("Perform the scheduled task review for project {project}. Inspect its task board, evaluate pending work within this repository's scope, claim feasible work and complete it. If there is no actionable work, report that and stop."),
    };
    format!("{assignment}\nYou are running in the project's repository root. Read and obey AGENTS.md and repository instructions. Use agent-tools and memory from this directory for project context. Verify that those tools point to this gateway and project before mutating tickets. The authoritative gateway URL is in GATEWAY_EXECUTION_URL and bearer token in GATEWAY_EXECUTION_API_KEY; never print the token. If CLI configuration does not match, use the gateway REST API at /v1/projects/{project}/tasks and /tasks/{{id}}, with Authorization: Bearer and a consistent X-Agent-Id. Claim using PATCH status=in_progress. Add progress with POST /tasks/{{id}}/comments. Evaluate scope and feasibility first. For work belonging elsewhere, use cross-project task delegation rather than modifying another repository. Create additional review/testing tasks with POST /v1/projects/{project}/tasks/{{parent_id}}/subtasks using title, description, specification, optional target_project_ident and labels. GET that same subtask endpoint to check reviews; wait for required reviews/testing before closing the parent. On a blocker, leave the task open with an actionable comment. After implementation and relevant verification, mark the ticket done using agent-tools tasks done or PATCH status=done; never mark unfinished work complete. A prior execution attempt may have partially changed files: inspect current state and avoid repeating completed side effects. Stay within the requested task; do not push, deploy or publish unless the task authorizes it.")
}

async fn execute(
    state: &AppState,
    url: &str,
    id: &str,
    project: &str,
    task: Option<&str>,
    trigger: &str,
) -> Result<()> {
    let (global, policy) = {
        let conn = state.db.lock().unwrap();
        let project_record =
            db::get_project(&conn, project)?.context("project no longer exists")?;
        let policy = execution::project_settings(&conn, project)?;
        if project_record.archived_at.is_some()
            || !policy.enabled
            || (trigger == "task" && !policy.on_task_received)
            || (trigger == "cadence" && policy.cadence_seconds.is_none())
        {
            return finish(
                &conn,
                id,
                "cancelled",
                "Project execution or trigger disabled",
            );
        }
        if let Some(task) = task {
            let detail =
                db::get_task_detail(&conn, project, task)?.context("task no longer exists")?;
            if detail.task.status != "todo" || detail.task.owner_agent_id.is_some() {
                return finish(
                    &conn,
                    id,
                    "cancelled",
                    "Task is already claimed or complete",
                );
            }
        } else if trigger == "task" {
            return finish(&conn, id, "cancelled", "Task was deleted");
        }
        (execution::settings(&conn)?, policy)
    };
    let directory = execution::resolve_repository(project, &global, &policy).await?;
    let candidates = if !policy.candidates.is_empty() {
        policy.candidates
    } else if !global.candidates.is_empty() {
        global.candidates
    } else {
        vec![
            Candidate {
                client: Client::Claude,
                model: None,
            },
            Candidate {
                client: Client::Codex,
                model: None,
            },
        ]
    };
    for (position, candidate) in candidates.iter().enumerate() {
        // Re-check opt-in before every fallback, too.
        if !execution::project_settings(&state.db.lock().unwrap(), project)?.enabled {
            return finish(
                &state.db.lock().unwrap(),
                id,
                "cancelled",
                "Execution disabled",
            );
        }
        let outcome = match execution_client::executable_on_path(candidate.client.executable()) {
            Some(path) => {
                execution_client::attempt(
                    candidate,
                    &path,
                    &directory,
                    &prompt(project, task),
                    Duration::from_secs(1800),
                    Some((url, &state.api_key)),
                )
                .await
            }
            None => Err(anyhow::anyhow!(
                "{} not found on PATH",
                candidate.client.executable()
            )),
        };
        let mut attempt = outcome.unwrap_or_else(|error| execution_client::Attempt {
            candidate: candidate.clone(),
            success: false,
            output: error.to_string(),
        });
        if !state.api_key.is_empty() {
            attempt.output = attempt.output.replace(&state.api_key, "[redacted]");
        }
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO execution_attempts(run_id,position,body) VALUES (?1,?2,?3)",
            params![id, position as i64, serde_json::to_string(&attempt)?],
        )?;
        let done = task
            .map(|task| {
                db::get_task_detail(&conn, project, task)
                    .map(|d| d.is_some_and(|d| d.task.status == "done"))
            })
            .transpose()?
            .unwrap_or(false);
        if done {
            return finish(&conn, id, "completed", "Task completed");
        }
        if attempt.success {
            return finish(
                &conn,
                id,
                if task.is_some() {
                    "needs_attention"
                } else {
                    "completed"
                },
                if task.is_some() {
                    "Client exited successfully but task remains open; inspect task and attempt output"
                } else {
                    "Scheduled review completed"
                },
            );
        }
    }
    finish(
        &state.db.lock().unwrap(),
        id,
        "failed",
        "All configured client/model attempts failed",
    )
}

/// One worker deliberately serializes repository writes, including projects mapped
/// to the same checkout. Runs survive restart; interrupted runs require review.
pub fn start(state: AppState, url: String) -> Result<()> {
    state.db.lock().unwrap().execute("UPDATE execution_runs SET status='interrupted', finished_at=?1, output='Gateway restarted during execution; inspect repository and task before retrying' WHERE status='running'", [db::now_ms()])?;
    tokio::spawn(async move {
        loop {
            let next = (|| -> Result<Option<PendingRun>> {
                let conn = state.db.lock().unwrap();
                schedule(&conn, db::now_ms())?;
                let next: Option<PendingRun> = conn.query_row("SELECT id,project_ident,task_id,trigger FROM execution_runs WHERE status='queued' ORDER BY created_at,id LIMIT 1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
                if let Some((id, ..)) = &next {
                    conn.execute(
                        "UPDATE execution_runs SET status='running' WHERE id=?1",
                        [id],
                    )?;
                }
                Ok(next)
            })();
            match next {
                Ok(Some((id, project, task, trigger))) => {
                    if let Err(error) =
                        execute(&state, &url, &id, &project, task.as_deref(), &trigger).await
                    {
                        let _ =
                            finish(&state.db.lock().unwrap(), &id, "failed", &error.to_string());
                        tracing::warn!(run_id=%id, "Agent execution failed: {error}");
                    }
                }
                Ok(None) => tokio::time::sleep(Duration::from_secs(5)).await,
                Err(error) => {
                    tracing::warn!("Agent execution queue error: {error}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_task_trigger_is_opt_in_and_deduplicated() {
        let database = db::open(":memory:").unwrap();
        let conn = database.lock().unwrap();
        conn.execute("INSERT INTO projects(ident,channel_name,room_id,created_at) VALUES ('sre','discord','',0)", []).unwrap();
        let task = db::insert_task(
            &conn,
            "sre",
            "Review assigned incident",
            None,
            None,
            &[],
            None,
            "source",
        )
        .unwrap();
        schedule(&conn, 1000).unwrap();
        assert!(runs(&conn, "sre").unwrap().is_empty());
        let policy = execution::ProjectSettings {
            enabled: true,
            on_task_received: true,
            local_path: Some("/srv/sre".into()),
            ..Default::default()
        };
        execution::save_project_settings(&conn, "sre", &policy).unwrap();
        schedule(&conn, 2000).unwrap();
        schedule(&conn, 3000).unwrap();
        let queued = runs(&conn, "sre").unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].task_id.as_deref(), Some(task.id.as_str()));
        finish(&conn, &queued[0].id, "failed", "client unavailable").unwrap();
        schedule(&conn, 4000).unwrap();
        assert_eq!(runs(&conn, "sre").unwrap().len(), 1);
    }

    #[test]
    fn cadence_respects_interval_and_outstanding_work() {
        let database = db::open(":memory:").unwrap();
        let conn = database.lock().unwrap();
        conn.execute("INSERT INTO projects(ident,channel_name,room_id,created_at) VALUES ('sre','discord','',0)", []).unwrap();
        let policy = execution::ProjectSettings {
            enabled: true,
            cadence_seconds: Some(60),
            local_path: Some("/srv/sre".into()),
            ..Default::default()
        };
        execution::save_project_settings(&conn, "sre", &policy).unwrap();
        schedule(&conn, 1000).unwrap();
        schedule(&conn, 100_000).unwrap();
        let queued = runs(&conn, "sre").unwrap();
        assert_eq!(queued.len(), 1);
        finish(&conn, &queued[0].id, "completed", "reviewed").unwrap();
        schedule(&conn, 59_000).unwrap();
        assert_eq!(runs(&conn, "sre").unwrap().len(), 1);
        schedule(&conn, 61_000).unwrap();
        assert_eq!(runs(&conn, "sre").unwrap().len(), 2);
    }
}
