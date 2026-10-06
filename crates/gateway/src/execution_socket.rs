//! Opt-in interactive executors. Durable assignments are never replayed on reconnect.
use crate::{
    db,
    execution::{self, Client, Executor},
    execution_queue as queue,
    routes::{require_admin, AppError},
    AppState,
};
use anyhow::{bail, Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS execution_sessions (
        session_key TEXT PRIMARY KEY, instance_id TEXT NOT NULL, connection_id TEXT NOT NULL,
        workspace_id TEXT NOT NULL, surface_id TEXT NOT NULL, session_id TEXT NOT NULL,
        project_ident TEXT NOT NULL REFERENCES projects(ident), client TEXT NOT NULL,
        model TEXT, cwd TEXT NOT NULL, state TEXT NOT NULL, connected INTEGER NOT NULL,
        last_seen_at INTEGER NOT NULL, UNIQUE(instance_id,surface_id,session_id)
    ); CREATE INDEX IF NOT EXISTS execution_session_connection ON execution_sessions(connection_id);")?;
    Ok(())
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInput {
    pub workspace_id: String,
    pub surface_id: String,
    pub session_id: String,
    pub project_ident: String,
    pub client: Client,
    pub model: Option<String>,
    pub cwd: String,
    pub state: SessionState,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Busy,
    WaitingInput,
    Exited,
}

impl SessionState {
    fn name(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::WaitingInput => "waiting_input",
            Self::Exited => "exited",
        }
    }
}

#[derive(Serialize)]
pub struct Session {
    pub session_key: String,
    pub instance_id: String,
    pub workspace_id: String,
    pub surface_id: String,
    pub session_id: String,
    pub project_ident: String,
    pub client: String,
    pub model: Option<String>,
    pub cwd: String,
    pub state: String,
    pub connected: bool,
    pub last_seen_at: i64,
    pub run_id: Option<String>,
}

fn list(conn: &Connection, connection: Option<&str>) -> Result<Vec<Session>> {
    let mut stmt = conn.prepare("SELECT s.session_key,s.instance_id,s.workspace_id,s.surface_id,s.session_id,s.project_ident,s.client,s.model,s.cwd,s.state,s.connected,s.last_seen_at,(SELECT id FROM execution_runs WHERE session_key=s.session_key AND finished_at IS NULL LIMIT 1) FROM execution_sessions s WHERE (?1 IS NULL OR connection_id=?1) ORDER BY s.connected DESC,s.last_seen_at DESC LIMIT 100")?;
    let values = stmt
        .query_map([connection], |r| {
            Ok(Session {
                session_key: r.get(0)?,
                instance_id: r.get(1)?,
                workspace_id: r.get(2)?,
                surface_id: r.get(3)?,
                session_id: r.get(4)?,
                project_ident: r.get(5)?,
                client: r.get(6)?,
                model: r.get(7)?,
                cwd: r.get(8)?,
                state: r.get(9)?,
                connected: r.get(10)?,
                last_seen_at: r.get(11)?,
                run_id: r.get(12)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(values)
}

pub async fn sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Vec<Session>>, AppError> {
    require_admin(&headers)?;
    Ok(Json(list(&state.db.lock().unwrap(), None)?))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    Register {
        protocol_version: u32,
        instance_id: String,
        sessions: Vec<SessionInput>,
    },
    Heartbeat,
    Accepted {
        run_id: String,
        session_key: String,
    },
    Report(Report),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    run_id: String,
    session_key: String,
    state: ReportState,
    message: String,
    summary: Option<String>,
    sequence: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReportState {
    Running,
    WaitingInput,
    Finished,
    Failed,
}

fn identity(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        bail!("invalid session identity");
    }
    Ok(())
}

fn register(
    conn: &Connection,
    connection: &str,
    instance: &str,
    sessions: &[SessionInput],
) -> Result<Value> {
    identity(instance)?;
    if sessions.len() > 64 {
        bail!("at most 64 sessions may be registered");
    }
    let tx = conn.unchecked_transaction()?;
    let collision: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM execution_sessions WHERE instance_id=?1 AND connection_id!=?2 AND connected=1 AND last_seen_at>?3)",params![instance,connection,db::now_ms()-60_000],|r|r.get(0))?;
    if collision {
        bail!("instance already connected");
    }
    tx.execute(
        "UPDATE execution_sessions SET connected=0 WHERE instance_id=?1",
        [instance],
    )?;
    let mut seen = std::collections::HashSet::new();
    for session in sessions {
        for value in [
            &session.workspace_id,
            &session.surface_id,
            &session.session_id,
            &session.project_ident,
            &session.cwd,
        ] {
            identity(value)?;
        }
        if !std::path::Path::new(&session.cwd).is_absolute() {
            bail!("session cwd must be absolute");
        }
        if !seen.insert((&session.surface_id, &session.session_id)) {
            bail!("duplicate session");
        }
        if db::get_project(&tx, &session.project_ident)?.is_none() {
            bail!("project not found: {}", session.project_ident);
        }
        if session.model.as_ref().is_some_and(|m| m.len() > 512) {
            bail!("model too long");
        }
        let existing: Option<(String,String,String)> = tx.query_row("SELECT session_key,project_ident,cwd FROM execution_sessions WHERE instance_id=?1 AND surface_id=?2 AND session_id=?3",params![instance,session.surface_id,session.session_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let key = existing
            .as_ref()
            .map(|s| s.0.clone())
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM execution_runs WHERE session_key=?1 AND finished_at IS NULL)",[&key],|r|r.get(0))?;
        if active
            && existing
                .as_ref()
                .is_some_and(|s| s.1 != session.project_ident || s.2 != session.cwd)
        {
            bail!("cannot remap a session with an active assignment");
        }
        if active && session.state == SessionState::Exited {
            tx.execute("UPDATE execution_runs SET status='needs_attention',progress='Assigned agent session exited; reconcile task',updated_at=?2 WHERE session_key=?1 AND finished_at IS NULL",params![key,db::now_ms()])?;
        }
        tx.execute("INSERT INTO execution_sessions(session_key,instance_id,connection_id,workspace_id,surface_id,session_id,project_ident,client,model,cwd,state,connected,last_seen_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,1,?12) ON CONFLICT(instance_id,surface_id,session_id) DO UPDATE SET connection_id=excluded.connection_id,workspace_id=excluded.workspace_id,project_ident=excluded.project_ident,client=excluded.client,model=excluded.model,cwd=excluded.cwd,state=excluded.state,connected=1,last_seen_at=excluded.last_seen_at",params![key,instance,connection,session.workspace_id,session.surface_id,session.session_id,session.project_ident,session.client.executable(),session.model,session.cwd,session.state.name(),db::now_ms()])?;
    }
    // Missing surfaces may have exited while offline; keep their assignments visible.
    tx.execute("UPDATE execution_runs SET status='needs_attention',progress='Assigned session is absent from registration; reconcile before continuing',updated_at=?2 WHERE finished_at IS NULL AND session_key IN (SELECT session_key FROM execution_sessions WHERE instance_id=?1 AND connected=0)",params![instance,db::now_ms()])?;
    let result =
        json!({"type":"registered","protocol_version":1,"sessions":list(&tx,Some(connection))?});
    tx.commit()?;
    Ok(result)
}

fn owned_run(conn: &Connection, connection: &str, key: &str, run_id: &str) -> Result<queue::Run> {
    let project: Option<String> = conn.query_row("SELECT project_ident FROM execution_sessions WHERE session_key=?1 AND connection_id=?2 AND connected=1",params![key,connection],|r|r.get(0)).optional()?;
    let project = project.context("session not registered on this connection")?;
    queue::run(conn, &project, run_id)?
        .filter(|run| run.session_key.as_deref() == Some(key))
        .context("assignment not found for session")
}

fn accept(conn: &Connection, connection: &str, key: &str, run_id: &str) -> Result<Value> {
    let run = owned_run(conn, connection, key, run_id)?;
    if run.finished_at.is_some() {
        return Ok(json!({"type":"accepted","run_id":run_id,"status":run.status}));
    }
    if run.started_at.is_none() {
        let task = run.task_id.as_deref().context("assignment has no task")?;
        let policy = execution::project_settings(conn, &run.project_ident)?;
        if !policy.enabled
            || policy.executor != Executor::Cmux
            || queue::eligible_delegated_tasks(conn, &run.project_ident, Some(task))?.is_empty()
        {
            queue::finish(conn, run_id, "cancelled", "Assignment no longer eligible")?;
            bail!("assignment no longer eligible");
        }
        conn.execute("UPDATE execution_runs SET status='running',started_at=?2,updated_at=?2,progress='Interactive agent accepted task' WHERE id=?1",params![run_id,db::now_ms()])?;
    }
    conn.execute(
        "UPDATE execution_sessions SET state='busy' WHERE session_key=?1",
        [key],
    )?;
    Ok(
        json!({"type":"accepted","run_id":run_id,"status":if run.started_at.is_none() { "running" } else { run.status.as_str() }}),
    )
}

fn report(conn: &Connection, connection: &str, value: &Report, api_key: &str) -> Result<Value> {
    let Report {
        run_id,
        session_key: key,
        state,
        message,
        summary,
        sequence,
    } = value;
    let run = owned_run(conn, connection, key, run_id)?;
    if *sequence <= 0 {
        bail!("report sequence must be positive");
    }
    if run.finished_at.is_some() || *sequence <= run.last_sequence {
        return Ok(
            json!({"type":"recorded","run_id":run_id,"status":run.status,"sequence":run.last_sequence}),
        );
    }
    if run.started_at.is_none() {
        bail!("accept assignment before reporting or executing");
    }
    let message = queue::redacted(message, api_key, 4096);
    queue::progress(conn, run_id, &message)?;
    if let Some(summary) = summary {
        queue::summary(
            conn,
            run_id,
            &queue::redacted(summary, api_key, 16384),
            "agent_report",
        )?;
    }
    let (status, session_state) = match state {
        ReportState::Running => ("running", "busy"),
        ReportState::WaitingInput => ("waiting_input", "waiting_input"),
        ReportState::Finished | ReportState::Failed => {
            let done = run
                .task_id
                .as_ref()
                .map(|id| db::get_task_detail(conn, &run.project_ident, id))
                .transpose()?
                .flatten()
                .is_some_and(|t| t.task.status == "done");
            let status = if done {
                "completed"
            } else if matches!(state, ReportState::Failed) {
                "failed"
            } else {
                "needs_attention"
            };
            queue::finish(
                conn,
                run_id,
                status,
                if done {
                    "Task completed"
                } else {
                    "Interactive execution ended; task remains open"
                },
            )?;
            (status, "idle")
        }
    };
    conn.execute(
        "UPDATE execution_runs SET status=?2,last_sequence=?3 WHERE id=?1",
        params![run_id, status, sequence],
    )?;
    conn.execute(
        "UPDATE execution_sessions SET state=?2 WHERE session_key=?1",
        params![key, session_state],
    )?;
    Ok(json!({"type":"recorded","run_id":run_id,"status":status,"sequence":sequence}))
}

fn offer(conn: &Connection, connection: &str) -> Result<Option<Value>> {
    queue::schedule(conn, db::now_ms())?;
    for session in list(conn, Some(connection))? {
        if !session.connected || session.state != "idle" || session.run_id.is_some() {
            continue;
        }
        let surface_active: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM execution_runs r JOIN execution_sessions s ON s.session_key=r.session_key WHERE s.instance_id=?1 AND s.surface_id=?2 AND r.finished_at IS NULL)",params![session.instance_id,session.surface_id],|r|r.get(0))?;
        if surface_active {
            continue;
        }
        let policy = execution::project_settings(conn, &session.project_ident)?;
        let project =
            db::get_project(conn, &session.project_ident)?.context("project not found")?;
        if project.archived_at.is_some() || !policy.enabled || policy.executor != Executor::Cmux {
            continue;
        }
        let next: Option<(String,String,String)> = conn.query_row("SELECT id,task_id,trigger FROM execution_runs WHERE project_ident=?1 AND executor='cmux' AND status='queued' ORDER BY created_at,id LIMIT 1",[&session.project_ident],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((run_id, task_id, trigger)) = next else {
            continue;
        };
        if queue::eligible_delegated_tasks(conn, &session.project_ident, Some(&task_id))?.is_empty()
        {
            queue::finish(conn, &run_id, "cancelled", "Delegation no longer eligible")?;
            continue;
        }
        if (trigger == "task" && !policy.on_task_received)
            || (trigger == "cadence" && policy.cadence_seconds.is_none())
        {
            continue;
        }
        conn.execute("UPDATE execution_runs SET status='assigned',session_key=?2,client=?3,model=?4,updated_at=?5,progress='Waiting for interactive agent acceptance' WHERE id=?1 AND status='queued'",params![run_id,session.session_key,session.client,session.model,db::now_ms()])?;
        let prompt = format!(
            "{}{}",
            execution::templates(conn)?.render(&session.project_ident, &task_id, &trigger),
            queue::reporting_prompt()
        );
        return Ok(Some(
            json!({"type":"assignment","run_id":run_id,"task_id":task_id,"project_ident":session.project_ident,"session_key":session.session_key,"workspace_id":session.workspace_id,"surface_id":session.surface_id,"session_id":session.session_id,"action":"execute","prompt":prompt}),
        ));
    }
    Ok(None)
}

fn disconnect(conn: &Connection, connection: &str) -> Result<()> {
    conn.execute(
        "UPDATE execution_sessions SET connected=0 WHERE connection_id=?1",
        [connection],
    )?;
    conn.execute("UPDATE execution_runs SET status='needs_attention',progress='Interactive connection lost; reconcile assignment before continuing',updated_at=?2 WHERE finished_at IS NULL AND session_key IN (SELECT session_key FROM execution_sessions WHERE connection_id=?1)",params![connection,db::now_ms()])?;
    Ok(())
}

async fn send(socket: &mut WebSocket, value: Value) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(value.to_string().into())),
    )
    .await??;
    Ok(())
}

pub async fn connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> std::result::Result<Response, AppError> {
    // This is a native executor channel. Browser cookies never grant registration authority.
    let bearer = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    if bearer != Some(state.api_key.as_str()) || state.api_key.is_empty() {
        return Err(AppError(
            StatusCode::UNAUTHORIZED,
            "executor bearer token required".into(),
        ));
    }
    Ok(ws
        .max_message_size(65536)
        .max_frame_size(65536)
        .on_upgrade(move |socket| serve(socket, state)))
}

async fn serve(mut socket: WebSocket, state: AppState) {
    let connection = uuid::Uuid::now_v7().to_string();
    let mut registered = false;
    let mut last_seen = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        let response = tokio::select! {
            message = socket.recv() => {
                let Some(Ok(message)) = message else { break; };
                last_seen = tokio::time::Instant::now();
                let Message::Text(text) = message else { if matches!(message,Message::Close(_)) { break; } continue; };
                let result: Result<Value> = (|| {
                    let input: Input = serde_json::from_str(&text)?;
                    let conn = state.db.lock().unwrap();
                    if !registered && !matches!(input,Input::Register{..}) { bail!("register first"); }
                    conn.execute("UPDATE execution_sessions SET last_seen_at=?2 WHERE connection_id=?1 AND connected=1",params![connection,db::now_ms()])?;
                    match input {
                        Input::Register { protocol_version,instance_id,sessions } => {
                            if protocol_version!=1 { bail!("unsupported protocol version"); }
                            let value = register(&conn,&connection,&instance_id,&sessions)?;
                            registered = true;
                            Ok(value)
                        }
                        Input::Heartbeat => Ok(json!({"type":"heartbeat_ack"})),
                        Input::Accepted {run_id,session_key} => accept(&conn,&connection,&session_key,&run_id),
                        Input::Report(value) => report(&conn, &connection, &value, &state.api_key),
                    }
                })();
                Some(result)
            }
            _ = tick.tick() => {
                if last_seen.elapsed()>Duration::from_secs(60) { break; }
                if registered {
                    let result = offer(&state.db.lock().unwrap(),&connection);
                    match result { Ok(Some(value)) => Some(Ok(value)), Ok(None) => Some(Ok(json!({"type":"heartbeat"}))), Err(error) => Some(Err(error)) }
                } else { None }
            }
        };
        if let Some(response) = response {
            let value = response.unwrap_or_else(|error|json!({"type":"error","message":queue::redacted(&error.to_string(),&state.api_key,4096)}));
            if send(&mut socket, value).await.is_err() {
                break;
            }
        }
    }
    if let Err(error) = disconnect(&state.db.lock().unwrap(), &connection) {
        tracing::warn!(%error,"Could not record executor disconnect");
    }
}

pub fn recover(conn: &Connection) -> Result<()> {
    conn.execute("UPDATE execution_sessions SET connected=0", [])?;
    conn.execute("UPDATE execution_runs SET status='needs_attention',progress='Gateway restarted; reconcile interactive assignment',updated_at=?1 WHERE executor='cmux' AND finished_at IS NULL AND session_key IS NOT NULL",[db::now_ms()])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (db::Db, String, SessionInput) {
        let db = db::open(":memory:").unwrap();
        let task;
        {
            let conn = db.lock().unwrap();
            for project in ["source", "target"] {
                conn.execute("INSERT INTO projects(ident,channel_name,room_id,created_at) VALUES (?1,'discord','',0)",[project]).unwrap();
            }
            task = db::insert_task(
                &conn,
                "target",
                "Delegated work",
                None,
                None,
                &[],
                None,
                "test",
            )
            .unwrap();
            let source = db::insert_delegated_task(
                &conn,
                &db::DelegatedTaskInsert {
                    project_ident: "source",
                    title: "Delegation",
                    description: None,
                    details: None,
                    labels: &[],
                    hostname: None,
                    reporter: "test",
                    target_project_ident: "target",
                    target_task_id: &task.id,
                },
            )
            .unwrap();
            db::insert_task_delegation(&conn, "source", &source.id, "target", &task.id, None, None)
                .unwrap();
            execution::save_project_settings(
                &conn,
                "target",
                &execution::ProjectSettings {
                    enabled: true,
                    on_task_received: true,
                    executor: Executor::Cmux,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        (
            db,
            task.id,
            SessionInput {
                workspace_id: "workspace".into(),
                surface_id: "surface".into(),
                session_id: "native-session".into(),
                project_ident: "target".into(),
                client: Client::Codex,
                model: Some("test-model".into()),
                cwd: "/home/test/repo".into(),
                state: SessionState::Idle,
            },
        )
    }

    fn update(run: &str, key: &str, state: ReportState) -> Report {
        let sequence = match state {
            ReportState::Running => 1,
            ReportState::WaitingInput => 2,
            ReportState::Finished => 3,
            ReportState::Failed => 4,
        };
        Report {
            sequence,
            run_id: run.into(),
            session_key: key.into(),
            state,
            message: "Working secret-key".into(),
            summary: Some("Updated code; tests passed; secret-key".into()),
        }
    }

    #[test]
    fn assignments_are_opt_in_and_reserved_once_with_exact_session_ownership() {
        let (db, task, mut session) = fixture();
        let conn = db.lock().unwrap();
        session.state = SessionState::Busy;
        register(
            &conn,
            "connection",
            "instance",
            std::slice::from_ref(&session),
        )
        .unwrap();
        assert!(offer(&conn, "connection").unwrap().is_none());
        session.state = SessionState::Idle;
        register(
            &conn,
            "connection",
            "instance",
            std::slice::from_ref(&session),
        )
        .unwrap();
        let assignment = offer(&conn, "connection").unwrap().unwrap();
        let run = assignment["run_id"].as_str().unwrap();
        let key = assignment["session_key"].as_str().unwrap();
        assert_eq!(assignment["task_id"], task);
        assert_eq!(assignment["surface_id"], session.surface_id);
        assert!(offer(&conn, "connection").unwrap().is_none());
        assert!(accept(&conn, "other-connection", key, run).is_err());
        assert!(report(
            &conn,
            "connection",
            &update(run, key, ReportState::Running),
            "secret-key"
        )
        .is_err());
        accept(&conn, "connection", key, run).unwrap();
        let started = queue::run(&conn, "target", run)
            .unwrap()
            .unwrap()
            .started_at;
        accept(&conn, "connection", key, run).unwrap();
        assert_eq!(
            queue::run(&conn, "target", run)
                .unwrap()
                .unwrap()
                .started_at,
            started
        );
        report(
            &conn,
            "connection",
            &update(run, key, ReportState::WaitingInput),
            "secret-key",
        )
        .unwrap();
        let waiting = queue::run(&conn, "target", run).unwrap().unwrap();
        assert_eq!(waiting.status, "waiting_input");
        assert!(waiting.progress.contains("[redacted]"));
        assert!(!waiting.summary.contains("secret-key"));
        // Replayed older progress cannot undo a question after reconnect.
        let replay = report(
            &conn,
            "connection",
            &update(run, key, ReportState::Running),
            "secret-key",
        )
        .unwrap();
        assert_eq!(replay["sequence"], 2);
        assert_eq!(
            queue::run(&conn, "target", run).unwrap().unwrap().status,
            "waiting_input"
        );
        report(
            &conn,
            "connection",
            &update(run, key, ReportState::Finished),
            "secret-key",
        )
        .unwrap();
        let finished = queue::run(&conn, "target", run).unwrap().unwrap();
        assert_eq!(finished.status, "needs_attention");
        assert!(finished.finished_at.is_some());
        assert_eq!(
            db::get_task_detail(&conn, "target", &task)
                .unwrap()
                .unwrap()
                .task
                .status,
            "todo"
        );
        report(
            &conn,
            "connection",
            &update(run, key, ReportState::Failed),
            "secret-key",
        )
        .unwrap();
        assert_eq!(
            queue::run(&conn, "target", run)
                .unwrap()
                .unwrap()
                .finished_at,
            finished.finished_at
        );
    }

    #[test]
    fn disconnect_and_restart_require_reconciliation_and_never_reoffer_work() {
        let (db, _, session) = fixture();
        let conn = db.lock().unwrap();
        register(&conn, "old", "instance", std::slice::from_ref(&session)).unwrap();
        let assignment = offer(&conn, "old").unwrap().unwrap();
        let run = assignment["run_id"].as_str().unwrap();
        let key = assignment["session_key"].as_str().unwrap();
        assert!(register(&conn, "new", "instance", std::slice::from_ref(&session)).is_err());
        accept(&conn, "old", key, run).unwrap();
        disconnect(&conn, "old").unwrap();
        assert!(queue::run(&conn, "target", run)
            .unwrap()
            .unwrap()
            .finished_at
            .is_none());
        let registration =
            register(&conn, "new", "instance", std::slice::from_ref(&session)).unwrap();
        assert_eq!(registration["sessions"][0]["run_id"], run);
        assert!(offer(&conn, "new").unwrap().is_none());
        assert!(report(&conn, "old", &update(run, key, ReportState::Running), "").is_err());
        disconnect(&conn, "old").unwrap(); // A late old socket cannot disconnect its replacement.
        assert!(list(&conn, Some("new")).unwrap()[0].connected);
        recover(&conn).unwrap();
        assert_eq!(
            queue::run(&conn, "target", run).unwrap().unwrap().status,
            "needs_attention"
        );
        register(&conn, "third", "instance", &[session]).unwrap();
        report(&conn, "third", &update(run, key, ReportState::Running), "").unwrap();
        assert_eq!(
            queue::run(&conn, "target", run).unwrap().unwrap().status,
            "running"
        );
        assert!(offer(&conn, "third").unwrap().is_none());
    }

    #[test]
    fn stale_task_cannot_start_and_finished_turn_only_completes_an_already_done_task() {
        let (db, task, session) = fixture();
        let conn = db.lock().unwrap();
        register(&conn, "connection", "instance", &[session]).unwrap();
        let assignment = offer(&conn, "connection").unwrap().unwrap();
        let run = assignment["run_id"].as_str().unwrap();
        let key = assignment["session_key"].as_str().unwrap();
        conn.execute(
            "UPDATE tasks SET owner_agent_id='another-agent' WHERE id=?1",
            [&task],
        )
        .unwrap();
        assert!(accept(&conn, "connection", key, run).is_err());
        assert_eq!(
            queue::run(&conn, "target", run).unwrap().unwrap().status,
            "cancelled"
        );
        // Exercise an acknowledged task whose canonical task transition is complete.
        conn.execute(
            "UPDATE execution_runs SET status='running',finished_at=NULL,started_at=1 WHERE id=?1",
            [run],
        )
        .unwrap();
        conn.execute("UPDATE tasks SET status='done' WHERE id=?1", [&task])
            .unwrap();
        report(
            &conn,
            "connection",
            &update(run, key, ReportState::Finished),
            "",
        )
        .unwrap();
        assert_eq!(
            queue::run(&conn, "target", run).unwrap().unwrap().status,
            "completed"
        );
    }

    #[test]
    fn registration_validates_entire_snapshot_and_keeps_active_mapping_stable() {
        let (db, _, session) = fixture();
        let conn = db.lock().unwrap();
        register(
            &conn,
            "connection",
            "instance",
            std::slice::from_ref(&session),
        )
        .unwrap();
        let mut invalid = session.clone();
        invalid.project_ident = "unknown".into();
        assert!(register(&conn, "connection", "instance", &[session.clone(), invalid]).is_err());
        assert_eq!(list(&conn, Some("connection")).unwrap().len(), 1);
        assert!(list(&conn, Some("connection")).unwrap()[0].connected);
        offer(&conn, "connection").unwrap();
        let mut changed = session.clone();
        changed.cwd = "/different/repo".into();
        assert!(register(&conn, "connection", "instance", &[changed]).is_err());
        register(&conn, "connection", "instance", &[]).unwrap();
        assert!(!list(&conn, Some("connection")).unwrap()[0].connected);
        assert_eq!(
            queue::runs(&conn, "target").unwrap()[0].status,
            "needs_attention"
        );
    }
}
