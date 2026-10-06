//! Durable task lifecycle notifications. Clients own terminal routing and delivery.
use crate::{
    db, execution_history,
    routes::{require_admin, AppError},
    AppState,
};
use anyhow::{bail, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

const FRAME_BYTES: usize = 65536;

/// Triggers keep lifecycle events in the same transaction as every task mutation,
/// including browser/API changes, delegation mirrors and generated subtasks.
pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS task_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL,
        project_ident TEXT NOT NULL, canonical_remote TEXT, task_json TEXT NOT NULL,
        comment_json TEXT, created_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS task_events_project ON task_events(project_ident,id);
    CREATE TABLE IF NOT EXISTS task_stream_consumers (
        consumer_id TEXT PRIMARY KEY, cursor INTEGER NOT NULL, connection_id TEXT,
        connected INTEGER NOT NULL DEFAULT 0, last_seen_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS task_event_deliveries (
        consumer_id TEXT NOT NULL REFERENCES task_stream_consumers(consumer_id),
        event_id INTEGER NOT NULL REFERENCES task_events(id), status TEXT NOT NULL,
        workspace_id TEXT, surface_id TEXT, message TEXT NOT NULL DEFAULT '',
        summary TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL,
        PRIMARY KEY(consumer_id,event_id)
    );
    DELETE FROM settings WHERE key='execution' OR key='execution.templates' OR key GLOB 'execution.project.*';")?;
    let task = task_json("NEW");
    let commented_task = task_json("t");
    let comment = comment_json("NEW");
    let latest_comment = comment_json("c");
    conn.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS task_event_created AFTER INSERT ON tasks BEGIN
        INSERT INTO task_events(kind,project_ident,canonical_remote,task_json,created_at)
        VALUES ('task_created',NEW.project_ident,(SELECT canonical_remote FROM projects WHERE ident=NEW.project_ident),{task},NEW.created_at);
    END;
    CREATE TRIGGER IF NOT EXISTS task_event_commented AFTER INSERT ON task_comments BEGIN
        INSERT INTO task_events(kind,project_ident,canonical_remote,task_json,comment_json,created_at)
        SELECT 'task_commented',t.project_ident,p.canonical_remote,{commented_task},{comment},NEW.created_at
        FROM tasks t JOIN projects p ON p.ident=t.project_ident WHERE t.id=NEW.task_id;
    END;
    CREATE TRIGGER IF NOT EXISTS task_event_completed AFTER UPDATE OF status ON tasks
    WHEN NEW.status='done' AND OLD.status!='done' BEGIN
        INSERT INTO task_events(kind,project_ident,canonical_remote,task_json,comment_json,created_at)
        VALUES ('task_completed',NEW.project_ident,(SELECT canonical_remote FROM projects WHERE ident=NEW.project_ident),{task},
        (SELECT {latest_comment} FROM task_comments c WHERE c.task_id=NEW.id AND c.author_type!='system' ORDER BY c.created_at DESC,c.id DESC LIMIT 1),NEW.updated_at);
    END;"))?;
    Ok(())
}

fn task_json(alias: &str) -> String {
    let fields = [
        "id",
        "project_ident",
        "title",
        "description",
        "details",
        "status",
        "rank",
        "hostname",
        "owner_agent_id",
        "reporter",
        "created_at",
        "updated_at",
        "started_at",
        "done_at",
        "kind",
        "delegated_to_project_ident",
        "delegated_to_task_id",
    ];
    let mut entries: Vec<String> = fields
        .iter()
        .map(|field| format!("'{field}',{alias}.{field}"))
        .collect();
    entries.push(format!(
        "'labels',json(CASE WHEN json_valid({alias}.labels) THEN {alias}.labels ELSE '[]' END)"
    ));
    format!("json_object({})", entries.join(","))
}

fn comment_json(alias: &str) -> String {
    let entries: Vec<String> = [
        "id",
        "task_id",
        "author",
        "author_type",
        "content",
        "created_at",
    ]
    .iter()
    .map(|field| format!("'{field}',{alias}.{field}"))
    .collect();
    format!("json_object({})", entries.join(","))
}

#[derive(Serialize)]
pub struct Event {
    pub id: i64,
    pub kind: String,
    pub project_ident: String,
    pub canonical_remote: Option<String>,
    pub task: Value,
    pub comment: Option<Value>,
    pub delegation: Option<Value>,
    pub created_at: i64,
    pub truncated: bool,
    pub deliveries: Vec<Delivery>,
}

#[derive(Serialize)]
pub struct Delivery {
    pub consumer_id: String,
    pub event_id: i64,
    pub status: String,
    pub workspace_id: Option<String>,
    pub surface_id: Option<String>,
    pub message: String,
    pub summary: String,
    pub updated_at: i64,
}

#[derive(Default, Deserialize)]
pub struct EventQuery {
    pub after_event_id: Option<i64>,
    pub project: Option<String>,
    pub task_id: Option<String>,
}

/// Bounded, ordered replay; absence of a cursor selects the newest events for UI.
pub fn list(conn: &Connection, query: &EventQuery) -> Result<Vec<Event>> {
    let project = query.project.as_deref().filter(|s| !s.is_empty());
    let mut stmt = conn.prepare("SELECT id,kind,project_ident,canonical_remote,task_json,comment_json,created_at FROM task_events
        WHERE (?1 IS NULL OR id>?1) AND (?2 IS NULL OR project_ident=?2)
        AND (?3 IS NULL OR json_extract(task_json,'$.id')=?3)
        ORDER BY CASE WHEN ?1 IS NOT NULL THEN id END ASC,id DESC LIMIT 100")?;
    let records = stmt
        .query_map(params![query.after_event_id, project, query.task_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut events = Vec::new();
    for (id, kind, project_ident, canonical_remote, task, comment, created_at) in records {
        let task: Value = serde_json::from_str(&task)?;
        let task_id = task["id"].as_str().unwrap_or_default();
        let delegation = db::get_delegation_by_target(conn, &project_ident, task_id)?
            .or(db::get_delegation_by_source(conn, &project_ident, task_id)?);
        events.push(Event {
            id,
            kind,
            project_ident,
            canonical_remote,
            task,
            comment: comment.map(|s| serde_json::from_str(&s)).transpose()?,
            delegation: delegation.map(serde_json::to_value).transpose()?,
            created_at,
            truncated: false,
            deliveries: deliveries(conn, id)?,
        });
    }
    Ok(events)
}

fn deliveries(conn: &Connection, event: i64) -> Result<Vec<Delivery>> {
    let mut stmt = conn.prepare("SELECT consumer_id,event_id,status,workspace_id,surface_id,message,summary,updated_at FROM task_event_deliveries WHERE event_id=?1 ORDER BY updated_at DESC LIMIT 100")?;
    let values = stmt
        .query_map([event], |r| {
            Ok(Delivery {
                consumer_id: r.get(0)?,
                event_id: r.get(1)?,
                status: r.get(2)?,
                workspace_id: r.get(3)?,
                surface_id: r.get(4)?,
                message: r.get(5)?,
                summary: r.get(6)?,
                updated_at: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(values)
}

pub async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EventQuery>,
) -> std::result::Result<Json<Vec<Event>>, AppError> {
    require_admin(&headers)?;
    let mut events = list(&state.db.lock().unwrap(), &query)?;
    for event in &mut events {
        bound_event(event, &state.api_key)?;
    }
    Ok(Json(events))
}

pub async fn history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EventQuery>,
) -> std::result::Result<Json<Vec<execution_history::Run>>, AppError> {
    require_admin(&headers)?;
    Ok(Json(execution_history::list_runs(
        &state.db.lock().unwrap(),
        query.project.as_deref(),
        query.task_id.as_deref(),
    )?))
}

pub async fn consumers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Vec<Value>>, AppError> {
    require_admin(&headers)?;
    let conn = state.db.lock().unwrap();
    let mut stmt = conn.prepare("SELECT consumer_id,cursor,connected,last_seen_at FROM task_stream_consumers ORDER BY last_seen_at DESC LIMIT 100")?;
    let values = stmt.query_map([],|r|Ok(json!({"consumer_id":redacted(&r.get::<_,String>(0)?,&state.api_key,128),"cursor":r.get::<_,i64>(1)?,"connected":r.get::<_,bool>(2)?,"last_seen_at":r.get::<_,i64>(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Json(values))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    Subscribe {
        protocol_version: u32,
        consumer_id: String,
        after_event_id: Option<i64>,
    },
    Ack {
        event_id: i64,
        status: String,
        workspace_id: Option<String>,
        surface_id: Option<String>,
        message: Option<String>,
        summary: Option<String>,
    },
    Heartbeat,
}

fn identity(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        bail!("invalid identity");
    }
    Ok(())
}

fn tail(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT COALESCE(MAX(id),0) FROM task_events", [], |r| {
            r.get(0)
        })?,
    )
}

fn subscribe(
    conn: &Connection,
    connection: &str,
    consumer: &str,
    after: Option<i64>,
) -> Result<i64> {
    identity(consumer)?;
    let now = db::now_ms();
    let end = tail(conn)?;
    let old: Option<(i64, bool, i64)> = conn
        .query_row(
            "SELECT cursor,connected,last_seen_at FROM task_stream_consumers WHERE consumer_id=?1",
            [consumer],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if old.is_some_and(|(_, connected, last_seen)| connected && now - last_seen < 60000) {
        bail!("consumer already connected");
    }
    let stored = old.map(|s| s.0).unwrap_or(end);
    let cursor = after.unwrap_or(stored);
    if cursor < 0 || cursor > end || (old.is_some() && cursor > stored) {
        bail!("invalid replay cursor");
    }
    conn.execute("INSERT INTO task_stream_consumers(consumer_id,cursor,connection_id,connected,last_seen_at) VALUES (?1,?2,?3,1,?4)
        ON CONFLICT(consumer_id) DO UPDATE SET connection_id=excluded.connection_id,connected=1,last_seen_at=excluded.last_seen_at",params![consumer,if old.is_some(){stored}else{cursor},connection,now])?;
    Ok(cursor)
}

fn owns(conn: &Connection, connection: &str, consumer: &str) -> Result<()> {
    if !conn.query_row("SELECT EXISTS(SELECT 1 FROM task_stream_consumers WHERE consumer_id=?1 AND connection_id=?2 AND connected=1)",params![consumer,connection],|r|r.get::<_,bool>(0))? { bail!("connection no longer owns consumer"); }
    Ok(())
}

fn offer(
    conn: &Connection,
    connection: &str,
    consumer: &str,
    cursor: i64,
    key: &str,
) -> Result<Option<Value>> {
    owns(conn, connection, consumer)?;
    let Some(mut event) = list(
        conn,
        &EventQuery {
            after_event_id: Some(cursor),
            ..Default::default()
        },
    )?
    .into_iter()
    .next() else {
        return Ok(None);
    };
    // Do not send other consumers' receipt metadata to the terminal client.
    event.deliveries.clear();
    bound_event(&mut event, key)?;
    conn.execute("INSERT OR IGNORE INTO task_event_deliveries(consumer_id,event_id,status,updated_at) VALUES (?1,?2,'offered',?3)",params![consumer,event.id,db::now_ms()])?;
    Ok(Some(json!({"type":"event","event":event})))
}

fn terminal(status: &str) -> bool {
    matches!(status, "injected" | "skipped" | "failed" | "uncertain")
}

#[allow(clippy::too_many_arguments)]
fn acknowledge(
    conn: &Connection,
    connection: &str,
    consumer: &str,
    id: i64,
    status: &str,
    workspace: Option<&str>,
    surface: Option<&str>,
    message: Option<&str>,
    summary: Option<&str>,
    key: &str,
) -> Result<Value> {
    owns(conn, connection, consumer)?;
    if !matches!(
        status,
        "received" | "queued" | "injected" | "skipped" | "failed" | "uncertain"
    ) {
        bail!("invalid delivery status");
    }
    for ident in [workspace, surface].into_iter().flatten() {
        identity(ident)?;
    }
    let previous: Option<String> = conn
        .query_row(
            "SELECT status FROM task_event_deliveries WHERE consumer_id=?1 AND event_id=?2",
            params![consumer, id],
            |r| r.get(0),
        )
        .optional()?;
    let previous = previous.ok_or_else(|| anyhow::anyhow!("event was not offered to consumer"))?;
    // Receipts describe delivery only. Never claim or complete a canonical task.
    let recorded = if terminal(&previous) || (previous == "queued" && status == "received") {
        previous.as_str()
    } else {
        status
    };
    let tx = conn.unchecked_transaction()?;
    if recorded == status && !terminal(&previous) {
        tx.execute("UPDATE task_event_deliveries SET status=?3,workspace_id=COALESCE(?4,workspace_id),surface_id=COALESCE(?5,surface_id),message=?6,summary=?7,updated_at=?8 WHERE consumer_id=?1 AND event_id=?2",params![consumer,id,status,workspace,surface,redacted(message.unwrap_or_default(),key,4096),redacted(summary.unwrap_or_default(),key,16384),db::now_ms()])?;
    }
    tx.execute("UPDATE task_stream_consumers SET cursor=MAX(cursor,?3),last_seen_at=?4 WHERE consumer_id=?1 AND connection_id=?2",params![consumer,connection,id,db::now_ms()])?;
    tx.commit()?;
    Ok(json!({"type":"recorded","event_id":id,"status":recorded}))
}

pub fn recover(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE task_stream_consumers SET connected=0,connection_id=NULL",
        [],
    )?;
    Ok(())
}

fn disconnect(conn: &Connection, connection: &str) -> Result<()> {
    conn.execute(
        "UPDATE task_stream_consumers SET connected=0,connection_id=NULL WHERE connection_id=?1",
        [connection],
    )?;
    Ok(())
}

pub async fn connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> std::result::Result<Response, AppError> {
    let bearer = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    if state.api_key.is_empty() || bearer != Some(state.api_key.as_str()) {
        return Err(AppError(
            StatusCode::UNAUTHORIZED,
            "stream bearer token required".into(),
        ));
    }
    Ok(ws
        .max_message_size(FRAME_BYTES)
        .max_frame_size(FRAME_BYTES)
        .on_upgrade(move |socket| serve(socket, state)))
}

async fn send(socket: &mut WebSocket, value: Value) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(value.to_string().into())),
    )
    .await??;
    Ok(())
}

async fn serve(mut socket: WebSocket, state: AppState) {
    let connection = uuid::Uuid::now_v7().to_string();
    let mut consumer: Option<String> = None;
    let mut cursor = 0;
    let mut inflight: Option<i64> = None;
    let mut last_seen = tokio::time::Instant::now();
    let mut heartbeat = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        let response: Option<Result<Value>> = tokio::select! {
            message=socket.recv()=>{
                let Some(Ok(message))=message else {break;};
                if matches!(message,Message::Close(_)){break;}
                let Message::Text(text)=message else {continue;};
                last_seen=tokio::time::Instant::now();
                Some((||{
                    let input:Input=serde_json::from_str(&text)?;
                    let conn=state.db.lock().unwrap();
                    if let Some(id)=&consumer {
                        owns(&conn,&connection,id)?;
                        conn.execute("UPDATE task_stream_consumers SET last_seen_at=?2 WHERE connection_id=?1",params![connection,db::now_ms()])?;
                    }
                    match input {
                        Input::Subscribe{protocol_version,consumer_id,after_event_id}=>{
                            if consumer.is_some(){bail!("already subscribed");}
                            if protocol_version!=1{bail!("unsupported protocol version");}
                            cursor=subscribe(&conn,&connection,&consumer_id,after_event_id)?;
                            consumer=Some(consumer_id.clone());
                            Ok(json!({"type":"subscribed","protocol_version":1,"consumer_id":consumer_id,"cursor":cursor,"latest_event_id":tail(&conn)?}))
                        }
                        Input::Heartbeat=>{if consumer.is_none(){bail!("subscribe first");} Ok(json!({"type":"heartbeat_ack"}))}
                        Input::Ack{event_id,status,workspace_id,surface_id,message,summary}=>{
                            let id=consumer.as_deref().ok_or_else(||anyhow::anyhow!("subscribe first"))?;
                            if event_id>cursor && inflight!=Some(event_id){bail!("event is not in flight");}
                            let value=acknowledge(&conn,&connection,id,event_id,&status,workspace_id.as_deref(),surface_id.as_deref(),message.as_deref(),summary.as_deref(),&state.api_key)?;
                            if inflight==Some(event_id){cursor=event_id;inflight=None;}
                            Ok(value)
                        }
                    }
                })())
            }
            _=tick.tick()=>{
                if last_seen.elapsed()>Duration::from_secs(60){break;}
                if let Some(id)=&consumer {
                    if owns(&state.db.lock().unwrap(),&connection,id).is_err(){break;}
                    if inflight.is_none() {
                        let result=offer(&state.db.lock().unwrap(),&connection,id,cursor,&state.api_key);
                        match result {
                            Ok(Some(value))=>{inflight=value["event"]["id"].as_i64();Some(Ok(value))}
                            Ok(None)=>None,
                            Err(_)=>break,
                        }
                    } else {None}
                } else {None}
            }
        };
        if let Some(result) = response {
            let value = result.unwrap_or_else(
                |e| json!({"type":"error","message":redacted(&e.to_string(),&state.api_key,4096)}),
            );
            if send(&mut socket, value).await.is_err() {
                break;
            }
        }
        if consumer.is_some() && heartbeat.elapsed() > Duration::from_secs(20) {
            if send(&mut socket, json!({"type":"heartbeat"}))
                .await
                .is_err()
            {
                break;
            }
            heartbeat = tokio::time::Instant::now();
        }
    }
    if let Err(error) = disconnect(&state.db.lock().unwrap(), &connection) {
        tracing::warn!(%error,"Could not record stream disconnect");
    }
}

fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit.saturating_sub(16);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &text[..end])
}

fn redacted(text: &str, key: &str, limit: usize) -> String {
    bounded(
        &if key.is_empty() {
            text.to_string()
        } else {
            text.replace(key, "[redacted]")
        },
        limit,
    )
}

fn bound_value(value: &mut Value, key: &str, limit: usize) -> bool {
    let mut changed = false;
    match value {
        Value::String(s) => {
            let clean = if key.is_empty() {
                s.clone()
            } else {
                s.replace(key, "[redacted]")
            };
            changed = clean.len() > limit;
            *s = bounded(&clean, limit);
        }
        Value::Array(items) => {
            if items.len() > 32 {
                items.truncate(32);
                changed = true;
            }
            for item in items {
                changed |= bound_value(item, key, limit);
            }
        }
        Value::Object(fields) => {
            for field in fields.values_mut() {
                changed |= bound_value(field, key, limit);
            }
        }
        _ => {}
    }
    changed
}

fn bound_event(event: &mut Event, key: &str) -> Result<()> {
    // Bound the notification itself; REST may include multiple independent
    // client receipts. The WebSocket omits receipts entirely.
    let mut receipts = std::mem::take(&mut event.deliveries);
    for receipt in &mut receipts {
        receipt.consumer_id = redacted(&receipt.consumer_id, key, 128);
        receipt.workspace_id = receipt
            .workspace_id
            .as_deref()
            .map(|s| redacted(s, key, 128));
        receipt.surface_id = receipt.surface_id.as_deref().map(|s| redacted(s, key, 128));
        receipt.message = redacted(&receipt.message, key, 4096);
        receipt.summary = redacted(&receipt.summary, key, 16384);
    }
    for limit in [16384, 4096, 512] {
        event.project_ident = redacted(&event.project_ident, key, 512);
        event.canonical_remote = event
            .canonical_remote
            .as_deref()
            .map(|s| redacted(s, key, 1024));
        event.truncated |= bound_value(&mut event.task, key, limit);
        if let Some(comment) = &mut event.comment {
            event.truncated |= bound_value(comment, key, limit);
        }
        if let Some(delegation) = &mut event.delegation {
            event.truncated |= bound_value(delegation, key, 512);
        }
        if serde_json::to_vec(event)?.len() < FRAME_BYTES - 1024 {
            event.deliveries = receipts;
            return Ok(());
        }
    }
    bail!("event exceeds wire limit; inspect task through REST");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> db::Db {
        let database = db::open(":memory:").unwrap();
        let conn = database.lock().unwrap();
        conn.execute("INSERT INTO projects(ident,channel_name,room_id,created_at,canonical_remote) VALUES ('example','none','',0,'github.com/owner/example')", []).unwrap();
        drop(conn);
        database
    }

    fn task(conn: &Connection) -> db::Task {
        db::insert_task(
            conn,
            "example",
            "Fixture task",
            Some("Description"),
            Some("Specification"),
            &[],
            None,
            "fixture-agent",
        )
        .unwrap()
    }

    #[test]
    fn lifecycle_snapshots_are_atomic_and_survive_task_deletion() {
        let database = database();
        let conn = database.lock().unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        task(&tx);
        tx.rollback().unwrap();
        assert_eq!(tail(&conn).unwrap(), 0);
        let task = task(&conn);
        db::insert_comment(&conn, &task.id, "human", "user", "Question").unwrap();
        db::insert_comment(
            &conn,
            &task.id,
            "fixture-agent",
            "agent",
            "Implemented and verified",
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET status='done',updated_at=10 WHERE id=?1",
            [&task.id],
        )
        .unwrap();
        conn.execute("UPDATE tasks SET status='done' WHERE id=?1", [&task.id])
            .unwrap();
        let events = list(
            &conn,
            &EventQuery {
                after_event_id: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            [
                "task_created",
                "task_commented",
                "task_commented",
                "task_completed"
            ]
        );
        assert_eq!(events[0].task["status"], "todo");
        assert_eq!(events[3].task["status"], "done");
        assert_eq!(
            events[3].comment.as_ref().unwrap()["content"],
            "Implemented and verified"
        );
        assert_eq!(
            events[0].canonical_remote.as_deref(),
            Some("github.com/owner/example")
        );
        db::delete_task(&conn, "example", &task.id).unwrap();
        assert_eq!(list(&conn, &EventQuery::default()).unwrap().len(), 4);
    }

    #[test]
    fn delegated_source_is_identified_before_creation_is_streamed() {
        let database = database();
        let conn = database.lock().unwrap();
        let source = db::insert_delegated_task(
            &conn,
            &db::DelegatedTaskInsert {
                project_ident: "example",
                title: "Outgoing",
                description: None,
                details: None,
                labels: &[],
                hostname: None,
                reporter: "test",
                target_project_ident: "other",
                target_task_id: "target-id",
            },
        )
        .unwrap();
        let event = list(&conn, &EventQuery::default()).unwrap().pop().unwrap();
        assert_eq!(event.task["kind"], "delegated");
        assert_eq!(event.task["delegated_to_task_id"], "target-id");
        assert_eq!(source.kind, "delegated");
    }

    #[test]
    fn consumer_replay_fencing_and_idempotent_receipts_do_not_change_tasks() {
        let database = database();
        let conn = database.lock().unwrap();
        let first = task(&conn);
        assert_eq!(subscribe(&conn, "a", "cmux", None).unwrap(), 1);
        assert!(subscribe(&conn, "b", "cmux", None).is_err());
        let second = task(&conn);
        assert!(
            acknowledge(&conn, "a", "cmux", 1, "injected", None, None, None, None, "secret")
                .is_err()
        );
        let offered = offer(&conn, "a", "cmux", 1, "secret").unwrap().unwrap();
        let id = offered["event"]["id"].as_i64().unwrap();
        assert_eq!(
            acknowledge(
                &conn,
                "a",
                "cmux",
                id,
                "queued",
                None,
                None,
                Some("secret"),
                None,
                "secret"
            )
            .unwrap()["status"],
            "queued"
        );
        assert_eq!(
            acknowledge(&conn, "a", "cmux", id, "received", None, None, None, None, "secret")
                .unwrap()["status"],
            "queued"
        );
        assert_eq!(
            acknowledge(
                &conn,
                "a",
                "cmux",
                id,
                "injected",
                Some("workspace"),
                Some("surface"),
                Some("Sent"),
                Some("secret outcome"),
                "secret"
            )
            .unwrap()["status"],
            "injected"
        );
        assert_eq!(
            acknowledge(&conn, "a", "cmux", id, "queued", None, None, None, None, "secret")
                .unwrap()["status"],
            "injected"
        );
        let receipt = deliveries(&conn, id).unwrap().pop().unwrap();
        assert_eq!(receipt.summary, "[redacted] outcome");
        assert_eq!(
            db::get_task_detail(&conn, "example", &first.id)
                .unwrap()
                .unwrap()
                .task
                .status,
            "todo"
        );
        assert_eq!(
            db::get_task_detail(&conn, "example", &second.id)
                .unwrap()
                .unwrap()
                .task
                .status,
            "todo"
        );
        recover(&conn).unwrap();
        assert_eq!(subscribe(&conn, "b", "cmux", None).unwrap(), id);
        assert!(owns(&conn, "a", "cmux").is_err());
        disconnect(&conn, "a").unwrap();
        owns(&conn, "b", "cmux").unwrap();
        assert!(offer(&conn, "b", "cmux", id, "secret").unwrap().is_none());
        disconnect(&conn, "b").unwrap();
        assert_eq!(subscribe(&conn, "c", "cmux", Some(0)).unwrap(), 0);
        assert_eq!(
            offer(&conn, "c", "cmux", 0, "secret").unwrap().unwrap()["event"]["id"],
            1
        );
    }

    #[test]
    fn payloads_bound_unicode_controls_and_redact_before_truncating() {
        let database = database();
        let conn = database.lock().unwrap();
        let task = task(&conn);
        let text = "secret\n🦀".repeat(30000);
        db::insert_comment(&conn, &task.id, &text, "agent", &text).unwrap();
        let mut event = list(&conn, &EventQuery::default()).unwrap().remove(0);
        bound_event(&mut event, "secret").unwrap();
        let bytes = serde_json::to_vec(&event).unwrap();
        assert!(bytes.len() < FRAME_BYTES - 1024);
        assert!(!String::from_utf8(bytes).unwrap().contains("secret"));
        assert!(event.truncated);
    }

    #[test]
    fn legacy_cleanup_preserves_history_and_subtasks() {
        let database = database();
        let conn = database.lock().unwrap();
        let task = task(&conn);
        db::set_setting(&conn, "execution.project.example", "{}").unwrap();
        db::set_setting(&conn, "execution", "{}").unwrap();
        db::set_setting(&conn, "execution.templates", "{}").unwrap();
        db::set_setting(&conn, "unrelated", "keep").unwrap();
        conn.execute("INSERT INTO execution_runs(id,project_ident,task_id,trigger,dedup_key,status,created_at,summary) VALUES ('old','example',?1,'task','old','running',1,'Existing summary')",[&task.id]).unwrap();
        execution_history::initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        assert!(db::get_setting(&conn, "execution.project.example")
            .unwrap()
            .is_none());
        assert!(db::get_setting(&conn, "execution").unwrap().is_none());
        assert!(db::get_setting(&conn, "execution.templates")
            .unwrap()
            .is_none());
        assert_eq!(
            db::get_setting(&conn, "unrelated").unwrap().as_deref(),
            Some("keep")
        );
        let run = execution_history::list_runs(&conn, None, None)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(run.status, "retired");
        assert_eq!(run.summary, "Existing summary");
        assert!(run.finished_at.is_some());
    }
}
