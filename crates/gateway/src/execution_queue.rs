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

/// Incoming delegated targets are stored as normal tasks. The delegation row,
/// not the task kind or title, establishes that another project allocated work.
fn eligible_delegated_tasks(
    conn: &Connection,
    project: &str,
    task: Option<&str>,
) -> Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT t.id FROM tasks t
         WHERE t.project_ident=?1 AND (?2 IS NULL OR t.id=?2)
           AND t.status='todo' AND t.kind='normal' AND t.owner_agent_id IS NULL
           AND EXISTS (
             SELECT 1 FROM task_delegations d
             WHERE d.target_project_ident=t.project_ident AND d.target_task_id=t.id
               AND d.source_project_ident != d.target_project_ident
               AND d.completed_at IS NULL)
         ORDER BY t.created_at,t.id",
    )?;
    let tasks = statement
        .query_map(params![project, task], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(tasks)
}

/// Poll durable incoming delegations. Ordinary planning tasks and subtasks do
/// not authorize another unattended agent, regardless of their age.
pub fn schedule(conn: &Connection, now: i64) -> Result<()> {
    for project in db::all_projects(conn)? {
        if project.archived_at.is_some() {
            continue;
        }
        let policy = execution::project_settings(conn, &project.ident)?;
        if !policy.enabled {
            continue;
        }
        let trigger = if policy.on_task_received {
            "task"
        } else if let Some(seconds) = policy.cadence_seconds {
            let last: Option<i64> = conn.query_row("SELECT MAX(created_at) FROM execution_runs WHERE project_ident=?1 AND trigger='cadence'", [&project.ident], |r| r.get(0))?;
            let pending: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM execution_runs WHERE project_ident=?1 AND status IN ('queued','running'))", [&project.ident], |r| r.get(0))?;
            if pending
                || last.is_some_and(|last| {
                    now.saturating_sub(last) < (seconds as i64).saturating_mul(1000)
                })
            {
                continue;
            }
            "cadence"
        } else {
            continue;
        };
        for task in eligible_delegated_tasks(conn, &project.ident, None)? {
            // Both triggers share one dedup key; cadence cannot replay failed work.
            enqueue(
                conn,
                &project.ident,
                Some(&task),
                trigger,
                &format!("task:{task}"),
                now,
            )?;
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
        Some(task) => format!("A new task {task} was allocated to project {project}. Fetch its current detail and comments, evaluate whether completion is feasible within this repository's scope, and claim it before making changes. If another agent owns it or it is done, stop. This run is limited to this incoming delegated task. Do not claim unrelated ordinary planning tasks or launch work from a general task-board scan."),
        None => format!("Scheduled execution for project {project} is restricted to incoming delegated target tasks selected by the gateway. Each actual run receives a specific task ID. Without an assigned delegated task ID, stop; do not scan or claim ordinary planning tasks or subtasks."),
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
        let eligible = match task {
            Some(task) => !eligible_delegated_tasks(&conn, project, Some(task))?.is_empty(),
            None => false,
        };
        if !eligible {
            return finish(
                &conn,
                id,
                "cancelled",
                "Run requires an unclaimed, open incoming delegated task",
            );
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
        // Checkout can take time. Recheck eligibility before starting a client;
        // a fallback may continue a task claimed by the preceding attempt.
        {
            let conn = state.db.lock().unwrap();
            let current = execution::project_settings(&conn, project)?;
            let incoming = task
                .map(|task| db::get_delegation_by_target(&conn, project, task))
                .transpose()?
                .flatten()
                .is_some_and(|d| d.source_project_ident != project && d.completed_at.is_none());
            if !current.enabled
                || (trigger == "task" && !current.on_task_received)
                || (trigger == "cadence" && current.cadence_seconds.is_none())
                || !incoming
                || (position == 0 && eligible_delegated_tasks(&conn, project, task)?.is_empty())
            {
                return finish(
                    &conn,
                    id,
                    "cancelled",
                    "Execution disabled or incoming delegation no longer eligible",
                );
            }
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

    fn fixture() -> db::Db {
        let database = db::open(":memory:").unwrap();
        {
            let conn = database.lock().unwrap();
            for project in ["source", "sre"] {
                conn.execute("INSERT INTO projects(ident,channel_name,room_id,created_at) VALUES (?1,'discord','',0)", [project]).unwrap();
            }
        }
        database
    }

    fn ordinary(conn: &Connection, project: &str) -> db::Task {
        db::insert_task(
            conn,
            project,
            "Agent planning task",
            None,
            None,
            &[],
            None,
            "agent",
        )
        .unwrap()
    }

    fn incoming(conn: &Connection) -> db::Task {
        let target = ordinary(conn, "sre");
        let source = db::insert_delegated_task(
            conn,
            &db::DelegatedTaskInsert {
                project_ident: "source",
                title: "Delegated incident",
                description: None,
                details: None,
                labels: &[],
                hostname: None,
                reporter: "agent",
                target_project_ident: "sre",
                target_task_id: &target.id,
            },
        )
        .unwrap();
        db::insert_task_delegation(conn, "source", &source.id, "sre", &target.id, None, None)
            .unwrap();
        target
    }

    fn enable(conn: &Connection, on_task_received: bool, cadence_seconds: Option<u64>) {
        for project in ["sre", "source"] {
            execution::save_project_settings(
                conn,
                project,
                &execution::ProjectSettings {
                    enabled: true,
                    on_task_received,
                    cadence_seconds,
                    local_path: Some("/srv/repo".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn ordinary_tasks_and_subtasks_never_trigger_even_when_old() {
        let database = fixture();
        let conn = database.lock().unwrap();
        let parent = ordinary(&conn, "source");
        let child = ordinary(&conn, "sre");
        conn.execute(
            "INSERT INTO task_subtasks(parent_id,child_id) VALUES (?1,?2)",
            params![parent.id, child.id],
        )
        .unwrap();
        conn.execute("UPDATE tasks SET created_at=0", []).unwrap();
        for received in [true, false] {
            enable(&conn, received, Some(60));
            schedule(&conn, 2 * 86_400_000).unwrap();
            assert!(runs(&conn, "sre").unwrap().is_empty());
            assert!(runs(&conn, "source").unwrap().is_empty());
        }
    }

    #[test]
    fn incoming_trigger_is_opt_in_and_shared_across_triggers() {
        let database = fixture();
        let conn = database.lock().unwrap();
        let task = incoming(&conn);
        schedule(&conn, 1000).unwrap();
        assert!(runs(&conn, "sre").unwrap().is_empty());
        enable(&conn, true, None);
        schedule(&conn, 2000).unwrap();
        schedule(&conn, 3000).unwrap();
        let queued = runs(&conn, "sre").unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].task_id.as_deref(), Some(task.id.as_str()));
        assert!(runs(&conn, "source").unwrap().is_empty());
        finish(&conn, &queued[0].id, "failed", "client unavailable").unwrap();
        enable(&conn, false, Some(60));
        schedule(&conn, 100_000).unwrap();
        assert_eq!(runs(&conn, "sre").unwrap().len(), 1);
    }

    #[test]
    fn eligibility_requires_current_open_unclaimed_incoming_relationship() {
        let database = fixture();
        let conn = database.lock().unwrap();
        let task = incoming(&conn);
        assert_eq!(
            eligible_delegated_tasks(&conn, "sre", Some(&task.id)).unwrap(),
            vec![task.id.clone()]
        );
        assert!(eligible_delegated_tasks(&conn, "source", Some(&task.id))
            .unwrap()
            .is_empty());
        for assignment in [
            "status='done'",
            "status='in_progress'",
            "status='todo',owner_agent_id='working-agent'",
        ] {
            conn.execute(
                &format!("UPDATE tasks SET {assignment} WHERE id=?1"),
                [&task.id],
            )
            .unwrap();
            assert!(eligible_delegated_tasks(&conn, "sre", Some(&task.id))
                .unwrap()
                .is_empty());
        }
        conn.execute(
            "UPDATE tasks SET status='todo',owner_agent_id=NULL WHERE id=?1",
            [&task.id],
        )
        .unwrap();
        conn.execute("UPDATE task_delegations SET completed_at=1", [])
            .unwrap();
        assert!(eligible_delegated_tasks(&conn, "sre", Some(&task.id))
            .unwrap()
            .is_empty());
        conn.execute("DELETE FROM task_delegations", []).unwrap();
        assert!(eligible_delegated_tasks(&conn, "sre", Some(&task.id))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn cadence_requires_delegated_task_and_respects_interval_and_pending_work() {
        let database = fixture();
        let conn = database.lock().unwrap();
        enable(&conn, false, Some(60));
        schedule(&conn, 1000).unwrap();
        assert!(runs(&conn, "sre").unwrap().is_empty());
        let first = incoming(&conn);
        schedule(&conn, 1000).unwrap();
        let second = incoming(&conn);
        schedule(&conn, 100_000).unwrap();
        let queued = runs(&conn, "sre").unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].trigger, "cadence");
        assert_eq!(queued[0].task_id.as_deref(), Some(first.id.as_str()));
        finish(&conn, &queued[0].id, "completed", "reviewed").unwrap();
        schedule(&conn, 59_000).unwrap();
        assert_eq!(runs(&conn, "sre").unwrap().len(), 1);
        schedule(&conn, 61_000).unwrap();
        let queued = runs(&conn, "sre").unwrap();
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0].task_id.as_deref(), Some(second.id.as_str()));
    }
}
