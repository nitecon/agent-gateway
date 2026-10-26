//! Opt-in, credential-bound task dispatch. Legacy task headers remain provenance.
use crate::{
    db,
    routes::AppError,
    task_events::{self, AgentOrigin},
    AppState,
};
use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const SESSION_TTL_MS: i64 = 120_000;

#[derive(Debug)]
pub struct Conflict(pub &'static str);
impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Conflict {}

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS task_machines (
        project_ident TEXT NOT NULL REFERENCES projects(ident), instance_id TEXT NOT NULL,
        os TEXT NOT NULL, key_hash TEXT NOT NULL UNIQUE, revoked INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(project_ident,instance_id)
    );
    CREATE TABLE IF NOT EXISTS task_sessions (
        project_ident TEXT NOT NULL, instance_id TEXT NOT NULL, session_id TEXT NOT NULL,
        origin_json TEXT NOT NULL, generation TEXT NOT NULL, session_key_hash TEXT NOT NULL, seen_at INTEGER NOT NULL,
        PRIMARY KEY(project_ident,instance_id,session_id,generation),
        FOREIGN KEY(project_ident,instance_id) REFERENCES task_machines(project_ident,instance_id)
    );
    CREATE TABLE IF NOT EXISTS task_dispatches (
        task_id TEXT PRIMARY KEY REFERENCES tasks(id), project_ident TEXT NOT NULL,
        document_json TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS task_dispatch_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL,
        project_ident TEXT NOT NULL, instance_id TEXT NOT NULL, session_id TEXT NOT NULL,
        operation TEXT NOT NULL, authentication_json TEXT NOT NULL, document_json TEXT NOT NULL, created_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS dispatch_recovery ON task_dispatch_events(project_ident,instance_id,id);
    CREATE TABLE IF NOT EXISTS task_dispatch_requests (
        project_ident TEXT NOT NULL, instance_id TEXT NOT NULL, session_id TEXT NOT NULL,
        request_id TEXT NOT NULL, fingerprint TEXT NOT NULL, response_json TEXT NOT NULL,
        PRIMARY KEY(project_ident,instance_id,session_id,request_id)
    );")?;
    Ok(())
}

fn uuid(value: &str) -> Result<String> {
    Ok(uuid::Uuid::parse_str(value)?.to_string())
}
fn hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

/// Local operator provisioning only; never called by an HTTP registration.
/// Reissuing revokes the previous key and expires leases, preserving session-secret fencing.
pub fn provision(conn: &Connection, project: &str, instance: &str, os: &str) -> Result<String> {
    let project = crate::projects::normalize_project_ident(project);
    let instance = uuid(instance)?;
    anyhow::ensure!(matches!(os, "linux" | "windows" | "macos"), "invalid OS");
    anyhow::ensure!(
        db::get_project(conn, &project)?.is_some(),
        "project not found"
    );
    let mut secret = [0u8; 32];
    OsRng.fill_bytes(&mut secret);
    let key = hex::encode(secret);
    let tx = conn.unchecked_transaction()?;
    tx.execute("INSERT INTO task_machines(project_ident,instance_id,os,key_hash) VALUES (?1,?2,?3,?4)
        ON CONFLICT(project_ident,instance_id) DO UPDATE SET os=excluded.os,key_hash=excluded.key_hash,revoked=0",
        params![project,instance,os,hash(&key)])?;
    tx.execute(
        "UPDATE task_sessions SET seen_at=0 WHERE project_ident=?1 AND instance_id=?2",
        params![project, instance],
    )?;
    tx.commit()?;
    Ok(key)
}

pub fn revoke(conn: &Connection, project: &str, instance: &str) -> Result<()> {
    conn.execute(
        "UPDATE task_machines SET revoked=1 WHERE project_ident=?1 AND instance_id=?2",
        params![
            crate::projects::normalize_project_ident(project),
            uuid(instance)?
        ],
    )?;
    Ok(())
}

fn authenticate(
    conn: &Connection,
    project: &str,
    headers: &HeaderMap,
) -> Result<AgentOrigin, AppError> {
    let origin = task_events::origin(headers)?.ok_or_else(|| {
        AppError(
            StatusCode::BAD_REQUEST,
            "exact agent origin required".into(),
        )
    })?;
    let keys = headers.get_all("x-gateway-machine-key");
    let key = if keys.iter().count() == 1 {
        keys.iter().next().and_then(|v| v.to_str().ok())
    } else {
        None
    };
    let Some(key) = key.filter(|key| key.len() == 64) else {
        return Err(AppError(
            StatusCode::UNAUTHORIZED,
            "machine credential required".into(),
        ));
    };
    let authenticated: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM task_machines
        WHERE project_ident=?1 AND instance_id=?2 AND os=?3 AND key_hash=?4 AND revoked=0)",
            params![project, origin.instance_id, origin.os, hash(key)],
            |r| r.get(0),
        )
        .map_err(AppError::from)?;
    if !authenticated {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            "machine credential does not authorize this project, instance and OS".into(),
        ));
    }
    Ok(origin)
}

fn session_key(headers: &HeaderMap) -> Result<String, AppError> {
    let values = headers.get_all("x-gateway-session-key");
    if values.iter().count() != 1 {
        return Err(AppError(
            StatusCode::UNAUTHORIZED,
            "one session credential required".into(),
        ));
    }
    let key = values
        .iter()
        .next()
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() == 64 && hex::decode(v).is_ok())
        .ok_or_else(|| {
            AppError(
                StatusCode::UNAUTHORIZED,
                "session credential must be 32 random bytes encoded as hex".into(),
            )
        })?;
    Ok(hash(key))
}

fn authenticate_session(
    conn: &Connection,
    project: &str,
    actor: &AgentOrigin,
    generation: &str,
    headers: &HeaderMap,
) -> Result<(), AppError> {
    let key = session_key(headers)?;
    let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM task_sessions WHERE project_ident=?1 AND instance_id=?2 AND session_id=?3 AND generation=?4 AND session_key_hash=?5)",
        params![project,actor.instance_id,actor.session_id,generation,key],|r|r.get(0))?;
    if !valid {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            "session credential/generation mismatch".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub origin: AgentOrigin,
    /// Immutable executor generation, distinct from the stable native conversation.
    pub generation: String,
}

fn registered(conn: &Connection, project: &str, target: &Target, live: bool) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_sessions s JOIN task_machines m
        ON m.project_ident=s.project_ident AND m.instance_id=s.instance_id
        WHERE s.project_ident=?1 AND s.instance_id=?2 AND s.session_id=?3
        AND s.origin_json=?4 AND s.generation=?5 AND m.revoked=0 AND m.os=?6
        AND (?7=0 OR s.seen_at>=?8))",
        params![
            project,
            target.origin.instance_id,
            target.origin.session_id,
            serde_json::to_string(&target.origin)?,
            target.generation,
            target.origin.os,
            live,
            db::now_ms() - SESSION_TTL_MS
        ],
        |r| r.get(0),
    )?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Register {
    pub generation: String,
}

pub async fn register(
    State(state): State<AppState>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(req): Json<Register>,
) -> Result<Json<Value>, AppError> {
    let project = crate::projects::normalize_project_ident(&project);
    let generation = uuid(&req.generation)
        .map_err(|_| AppError(StatusCode::BAD_REQUEST, "generation must be a UUID".into()))?;
    let conn = state.db.lock().unwrap();
    let origin = authenticate(&conn, &project, &headers)?;
    let key = session_key(&headers)?;
    let existing: Option<(String,String,String)> = conn.query_row("SELECT origin_json,generation,session_key_hash FROM task_sessions WHERE project_ident=?1 AND instance_id=?2 AND session_id=?3 AND generation=?4",
        params![project,origin.instance_id,origin.session_id,generation], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let raw = serde_json::to_string(&origin)?;
    if existing
        .as_ref()
        .is_some_and(|(o, g, k)| o != &raw || g != &generation || k != &key)
    {
        return Err(Conflict(
            "session registration and credential are immutable within a generation",
        )
        .into());
    }
    let now = db::now_ms();
    conn.execute("INSERT INTO task_sessions(project_ident,instance_id,session_id,origin_json,generation,session_key_hash,seen_at) VALUES (?1,?2,?3,?4,?5,?6,?7)
        ON CONFLICT(project_ident,instance_id,session_id,generation) DO UPDATE SET seen_at=excluded.seen_at",
        params![project,origin.instance_id,origin.session_id,raw,generation,key,now])?;
    Ok(Json(
        json!({"target":Target{origin,generation},"seen_at":now,"expires_at":now+SESSION_TTL_MS}),
    ))
}

pub async fn sessions(
    State(state): State<AppState>,
    Path(project): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let project = crate::projects::normalize_project_ident(&project);
    let conn = state.db.lock().unwrap();
    authenticate(&conn, &project, &headers)?;
    let mut stmt = conn.prepare("SELECT s.origin_json,s.generation,s.seen_at FROM task_sessions s JOIN task_machines m
        ON m.project_ident=s.project_ident AND m.instance_id=s.instance_id
        WHERE s.project_ident=?1 AND m.revoked=0 AND s.seen_at>=?2 ORDER BY s.instance_id,s.session_id")?;
    let records = stmt
        .query_map(params![project, db::now_ms() - SESSION_TTL_MS], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let records = records.into_iter().map(|(origin,generation,seen)| -> Result<Value> {
        Ok(json!({"target":Target{origin:serde_json::from_str(&origin)?,generation},"seen_at":seen,"expires_at":seen+SESSION_TTL_MS}))
    }).collect::<Result<Vec<_>>>()?;
    Ok(Json(json!({"sessions":records})))
}

/// A durable machine address carries no process/session identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineTarget {
    pub instance_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionedTarget {
    pub instance_id: String,
    pub os: String,
}

fn machine_target(
    conn: &Connection,
    project: &str,
    target: &MachineTarget,
    required_os: Option<&str>,
) -> Result<ProvisionedTarget> {
    let instance =
        uuid(&target.instance_id).map_err(|_| Conflict("machine instance_id must be a UUID"))?;
    let os: Option<String> = conn
        .query_row(
            "SELECT os FROM task_machines WHERE project_ident=?1 AND instance_id=?2 AND revoked=0",
            params![project, instance],
            |row| row.get(0),
        )
        .optional()?;
    let os = os.ok_or(Conflict(
        "target machine is not provisioned/eligible in project",
    ))?;
    if required_os.is_some_and(|required| required != os) {
        return Err(Conflict("target machine does not match required OS").into());
    }
    Ok(ProvisionedTarget {
        instance_id: instance,
        os,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dispatch {
    pub task_id: String,
    pub project_ident: String,
    pub source_project_ident: String,
    pub requester: Target,
    pub target: Option<Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_target: Option<ProvisionedTarget>,
    pub required_os: Option<String>,
    pub required_provider: Option<String>,
    pub state: String,
    pub claim_id: Option<String>,
    pub received_at: Option<i64>,
    pub claimed_at: Option<i64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub execution_id: Option<String>,
    pub cancel_requested_at: Option<i64>,
    pub cancel_reason: Option<String>,
    pub result: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delegate {
    pub request_id: String,
    pub generation: String,
    pub target_project_ident: String,
    pub target: Option<Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_target: Option<MachineTarget>,
    pub title: String,
    pub specification: String,
    pub required_os: Option<String>,
    pub required_provider: Option<String>,
}

/// Validate an ordinary task reassignment against the existing live directory.
pub fn connected_target(conn: &Connection, project: &str, target: &Target) -> Result<()> {
    if crate::task_targets::connected(conn, project, target)? {
        return Ok(());
    }
    eligible(conn, project, target, None, None)
}

fn eligible(
    conn: &Connection,
    project: &str,
    target: &Target,
    os: Option<&str>,
    provider: Option<&str>,
) -> Result<()> {
    if !registered(conn, project, target, true)?
        || os.is_some_and(|os| os != target.origin.os)
        || provider.is_some_and(|p| p != target.origin.provider)
    {
        return Err(
            Conflict("target is unavailable or ineligible for the required OS/provider").into(),
        );
    }
    Ok(())
}

pub fn managed(conn: &Connection, task_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_dispatches WHERE task_id=?1)",
        [task_id],
        |r| r.get(0),
    )?)
}

fn load(conn: &Connection, project: &str, id: &str) -> Result<Dispatch> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT document_json FROM task_dispatches WHERE task_id=?2 AND (project_ident=?1 OR json_extract(document_json,'$.source_project_ident')=?1)",
            params![project, id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(serde_json::from_str(
        &raw.ok_or(Conflict("dispatch not found in project"))?,
    )?)
}

fn save(
    conn: &Connection,
    dispatch: &Dispatch,
    actor: &AgentOrigin,
    auth_project: &str,
    generation: &str,
    operation: &str,
) -> Result<Value> {
    // These fields come from the provisioned server row, after credential validation.
    let machine:Value=conn.query_row("SELECT project_ident,instance_id,os,key_hash FROM task_machines WHERE project_ident=?1 AND instance_id=?2 AND revoked=0",
        params![auth_project,actor.instance_id],|r|Ok(json!({"project_ident":r.get::<_,String>(0)?,"instance_id":r.get::<_,String>(1)?,"os":r.get::<_,String>(2)?,"credential_id":r.get::<_,String>(3)?})))?;
    let authentication = json!({"machine":machine,"session":actor,"generation":generation});
    let raw = serde_json::to_string(dispatch)?;
    conn.execute(
        "INSERT INTO task_dispatches(task_id,project_ident,document_json) VALUES (?1,?2,?3)
        ON CONFLICT(task_id) DO UPDATE SET document_json=excluded.document_json",
        params![dispatch.task_id, dispatch.project_ident, raw],
    )?;
    conn.execute("INSERT INTO task_dispatch_events(task_id,project_ident,instance_id,session_id,operation,authentication_json,document_json,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![dispatch.task_id,dispatch.project_ident,actor.instance_id,actor.session_id,operation,serde_json::to_string(&authentication)?,raw,db::now_ms()])?;
    let task = db::get_task_detail(conn, &dispatch.project_ident, &dispatch.task_id)?
        .ok_or(Conflict("dispatch task missing"))?
        .task;
    Ok(json!({"dispatch":dispatch,"task":task,"authentication":authentication}))
}

/// Request uniqueness covers every operation for this exact authenticated actor.
/// Persisted successful responses replay even when the session lease has expired.
fn dedup(
    conn: &Connection,
    project: &str,
    actor: &AgentOrigin,
    request: &str,
    fingerprint: &str,
    apply: impl FnOnce(&Connection) -> Result<Value>,
) -> Result<Value> {
    let request = uuid(request).map_err(|_| Conflict("request_id must be a UUID"))?;
    let prior: Option<(String, String)> = conn
        .query_row(
            "SELECT fingerprint,response_json FROM task_dispatch_requests
        WHERE project_ident=?1 AND instance_id=?2 AND session_id=?3 AND request_id=?4",
            params![project, actor.instance_id, actor.session_id, request],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((prior, response)) = prior {
        if prior != fingerprint {
            return Err(Conflict("request_id already used with different input").into());
        }
        return Ok(serde_json::from_str(&response)?);
    }
    let response = apply(conn)?;
    conn.execute("INSERT INTO task_dispatch_requests(project_ident,instance_id,session_id,request_id,fingerprint,response_json) VALUES (?1,?2,?3,?4,?5,?6)",
        params![project,actor.instance_id,actor.session_id,request,fingerprint,serde_json::to_string(&response)?])?;
    Ok(response)
}

fn delegate(conn: &Connection, source: &str, actor: &AgentOrigin, req: &Delegate) -> Result<Value> {
    let fingerprint = serde_json::to_string(&("delegate", actor, req))?;
    task_events::mutation(conn, Some(actor), |conn| {
        dedup(conn, source, actor, &req.request_id, &fingerprint, |conn| {
            let requester = Target {
                origin: actor.clone(),
                generation: req.generation.clone(),
            };
            if !registered(conn, source, &requester, true)? {
                return Err(Conflict("requester session is not registered/live").into());
            }
            let target_project =
                crate::projects::normalize_project_ident(&req.target_project_ident);
            let machine_target = match (&req.target, &req.machine_target) {
                (Some(target), None) => {
                    eligible(
                        conn,
                        &target_project,
                        target,
                        req.required_os.as_deref(),
                        req.required_provider.as_deref(),
                    )?;
                    None
                }
                (None, Some(machine)) => {
                    if req
                        .required_provider
                        .as_deref()
                        .is_some_and(|p| !matches!(p, "codex" | "claude"))
                    {
                        return Err(Conflict("required_provider must be codex or claude").into());
                    }
                    Some(machine_target(
                        conn,
                        &target_project,
                        machine,
                        req.required_os.as_deref(),
                    )?)
                }
                _ => return Err(Conflict("provide exactly one target or machine_target").into()),
            };
            if req.title.trim().is_empty()
                || req.title.len() > 512
                || req.specification.len() > 65536
            {
                return Err(Conflict("title/specification invalid or too large").into());
            }
            let task = db::insert_unassigned_dispatch_task(
                conn,
                &target_project,
                req.title.trim(),
                None,
                Some(&req.specification),
                &[],
                None,
                &actor.session_id,
            )?;
            let dispatch = Dispatch {
                task_id: task.id,
                project_ident: target_project,
                source_project_ident: source.into(),
                requester,
                target: req.target.clone(),
                machine_target,
                required_os: req.required_os.clone(),
                required_provider: req.required_provider.clone(),
                state: if req.machine_target.is_some() {
                    "pending_machine"
                } else {
                    "pending"
                }
                .into(),
                claim_id: None,
                received_at: None,
                claimed_at: None,
                started_at: None,
                finished_at: None,
                execution_id: None,
                cancel_requested_at: None,
                cancel_reason: None,
                result: None,
            };
            save(conn, &dispatch, actor, source, &req.generation, "delegate")
        })
    })
}

pub async fn delegate_handler(
    State(state): State<AppState>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(req): Json<Delegate>,
) -> Result<Json<Value>, AppError> {
    let project = crate::projects::normalize_project_ident(&project);
    let database = state.db.clone();
    tokio::task::spawn_blocking(move || {
        let conn = database.lock().unwrap();
        let actor = authenticate(&conn, &project, &headers)?;
        authenticate_session(&conn, &project, &actor, &req.generation, &headers)?;
        Ok(Json(delegate(&conn, &project, &actor, &req)?))
    })
    .await?
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub request_id: String,
    pub generation: String,
    pub operation: String,
    pub claim_id: Option<String>,
    pub execution_id: Option<String>,
    pub result: Option<String>,
    pub target: Option<Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_target: Option<MachineTarget>,
}

fn terminal(state: &str) -> bool {
    matches!(state, "succeeded" | "failed" | "cancelled")
}

fn action(
    conn: &Connection,
    project: &str,
    id: &str,
    actor: &AgentOrigin,
    req: &Action,
) -> Result<Value> {
    let fingerprint = serde_json::to_string(&("action", id, actor, req))?;
    task_events::mutation(conn, Some(actor), |conn| {
        dedup(
            conn,
            project,
            actor,
            &req.request_id,
            &fingerprint,
            |conn| {
                let mut dispatch = load(conn, project, id)?;
                let caller = Target {
                    origin: actor.clone(),
                    generation: req.generation.clone(),
                };
                if !registered(conn, project, &caller, true)? {
                    return Err(Conflict("caller session is not registered/live").into());
                }
                let is_target =
                    dispatch.project_ident == project && dispatch.target.as_ref() == Some(&caller);
                let machine_receive = req.operation == "receive"
                    && dispatch.state == "pending_machine"
                    && dispatch.project_ident == project
                    && dispatch.target.is_none()
                    && dispatch
                        .machine_target
                        .as_ref()
                        .is_some_and(|m| m.instance_id == actor.instance_id && m.os == actor.os);
                let is_requester =
                    dispatch.source_project_ident == project && dispatch.requester == caller;
                let active = matches!(dispatch.state.as_str(), "claimed" | "running");
                if terminal(&dispatch.state) {
                    return Err(Conflict("dispatch is terminal").into());
                }
                if !(is_target
                    || machine_receive
                    || is_requester
                        && matches!(req.operation.as_str(), "cancel" | "request_cancel"))
                {
                    return Err(task_events::OwnershipConflict.into());
                }
                if active && req.claim_id.as_ref() != dispatch.claim_id.as_ref() {
                    return Err(Conflict("exact current claim_id required").into());
                }
                if req.result.as_ref().is_some_and(|s| s.len() > 16384) {
                    return Err(Conflict("result exceeds 16384 bytes").into());
                }
                let now = db::now_ms();
                match req.operation.as_str() {
                    "receive" if dispatch.state == "pending" || machine_receive => {
                        if machine_receive {
                            // Bind once, under the same transaction as receipt. This is not a claim.
                            eligible(
                                conn,
                                project,
                                &caller,
                                dispatch.required_os.as_deref(),
                                dispatch.required_provider.as_deref(),
                            )?;
                            dispatch.target = Some(caller.clone());
                        }
                        dispatch.state = "received".into();
                        dispatch.received_at = Some(now);
                    }
                    "claim" if dispatch.state == "received" => {
                        eligible(
                            conn,
                            project,
                            &caller,
                            dispatch.required_os.as_deref(),
                            dispatch.required_provider.as_deref(),
                        )?;
                        dispatch.state = "claimed".into();
                        dispatch.claim_id = Some(uuid::Uuid::now_v7().to_string());
                        dispatch.claimed_at = Some(now);
                        conn.execute("UPDATE tasks SET status='in_progress',owner_agent_id=?1,owner_origin_json=?2,started_at=NULL,done_at=NULL,updated_at=?3 WHERE id=?4",
                    params![actor.session_id,serde_json::to_string(actor)?,now,id])?;
                    }
                    "start"
                        if dispatch.state == "claimed"
                            && dispatch.cancel_requested_at.is_none() =>
                    {
                        let execution = req.execution_id.as_deref().ok_or(Conflict(
                            "execution_id required for explicit executor start",
                        ))?;
                        dispatch.execution_id = Some(
                            uuid(execution).map_err(|_| Conflict("execution_id must be a UUID"))?,
                        );
                        dispatch.state = "running".into();
                        dispatch.started_at = Some(now);
                        conn.execute(
                            "UPDATE tasks SET started_at=?1,updated_at=?1 WHERE id=?2",
                            params![now, id],
                        )?;
                    }
                    "succeeded" | "failed" if dispatch.state == "running" => {
                        if req.execution_id.as_ref() != dispatch.execution_id.as_ref() {
                            return Err(Conflict("exact execution_id required").into());
                        }
                        if req.result.as_deref().is_none_or(|r| r.trim().is_empty()) {
                            return Err(Conflict("explicit outcome result required").into());
                        }
                        dispatch.state = req.operation.clone();
                        dispatch.result = req.result.clone();
                        dispatch.finished_at = Some(now);
                        conn.execute(
                            "UPDATE tasks SET status='done',done_at=?1,updated_at=?1 WHERE id=?2",
                            params![now, id],
                        )?;
                    }
                    "request_cancel" if active => {
                        if req.result.as_deref().is_none_or(|r| r.trim().is_empty()) {
                            return Err(
                                Conflict("explicit cancellation request reason required").into()
                            );
                        }
                        dispatch.cancel_requested_at = Some(now);
                        dispatch.cancel_reason = req.result.clone();
                    }
                    "cancel" if !active || is_target => {
                        if dispatch.state == "running"
                            && req.execution_id.as_ref() != dispatch.execution_id.as_ref()
                        {
                            return Err(Conflict(
                                "exact execution_id required to confirm executor cancellation",
                            )
                            .into());
                        }
                        if req.result.as_deref().is_none_or(|r| r.trim().is_empty()) {
                            return Err(Conflict("explicit cancellation reason required").into());
                        }
                        dispatch.state = "cancelled".into();
                        dispatch.result = req.result.clone();
                        dispatch.finished_at = Some(now);
                        // Distinct terminal status; owner and claim provenance stay intact.
                        conn.execute(
                            "UPDATE tasks SET status='cancelled',updated_at=?1 WHERE id=?2",
                            params![now, id],
                        )?;
                    }
                    "handoff" if is_target && dispatch.cancel_requested_at.is_none() => {
                        let machine = match (&req.target, &req.machine_target) {
                            (Some(target), None) => {
                                if target == &caller {
                                    return Err(Conflict(
                                        "handoff requires a different session/generation",
                                    )
                                    .into());
                                }
                                eligible(
                                    conn,
                                    project,
                                    target,
                                    dispatch.required_os.as_deref(),
                                    dispatch.required_provider.as_deref(),
                                )?;
                                None
                            }
                            (None, Some(machine)) => Some(machine_target(
                                conn,
                                project,
                                machine,
                                dispatch.required_os.as_deref(),
                            )?),
                            _ => {
                                return Err(Conflict(
                                    "provide exactly one target or machine_target for handoff",
                                )
                                .into())
                            }
                        };
                        if req.result.as_deref().is_none_or(|r| r.trim().is_empty()) {
                            return Err(Conflict("handoff reason required").into());
                        }
                        // The event before reassignment preserves the previous claim and execution.
                        save(
                            conn,
                            &dispatch,
                            actor,
                            project,
                            &req.generation,
                            "handoff_prior_claim",
                        )?;
                        dispatch.target = req.target.clone();
                        dispatch.machine_target = machine;
                        dispatch.state = if req.machine_target.is_some() {
                            "pending_machine"
                        } else {
                            "pending"
                        }
                        .into();
                        dispatch.claim_id = None;
                        dispatch.received_at = None;
                        dispatch.claimed_at = None;
                        dispatch.started_at = None;
                        dispatch.execution_id = None;
                        dispatch.result = req.result.clone();
                        conn.execute("UPDATE tasks SET status='todo',owner_agent_id=NULL,owner_origin_json=NULL,started_at=NULL,done_at=NULL,updated_at=?1 WHERE id=?2",params![now,id])?;
                    }
                    _ => {
                        return Err(Conflict("invalid operation for current dispatch state").into())
                    }
                }
                save(
                    conn,
                    &dispatch,
                    actor,
                    project,
                    &req.generation,
                    &req.operation,
                )
            },
        )
    })
}

pub async fn action_handler(
    State(state): State<AppState>,
    Path((project, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<Action>,
) -> Result<Json<Value>, AppError> {
    let project = crate::projects::normalize_project_ident(&project);
    let database = state.db.clone();
    tokio::task::spawn_blocking(move || {
        let conn = database.lock().unwrap();
        let actor = authenticate(&conn, &project, &headers)?;
        authenticate_session(&conn, &project, &actor, &req.generation, &headers)?;
        Ok(Json(action(&conn, &project, &id, &actor, &req)?))
    })
    .await?
}

/// Recovery is read-only. It never transfers an old session's claim to a new one.
pub async fn recovery(
    State(state): State<AppState>,
    Path(project): Path<String>,
    Query(query): Query<Register>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let project = crate::projects::normalize_project_ident(&project);
    let database = state.db.clone();
    tokio::task::spawn_blocking(move || {
        let conn=database.lock().unwrap();
        let actor=authenticate(&conn,&project,&headers)?;
        authenticate_session(&conn,&project,&actor,&query.generation,&headers)?;
        let caller=Target{origin:actor.clone(),generation:query.generation};
        let mut stmt=conn.prepare("SELECT document_json FROM task_dispatches WHERE project_ident=?1 AND (json_extract(document_json,'$.target.origin.instance_id')=?2 OR json_extract(document_json,'$.machine_target.instance_id')=?2) ORDER BY task_id")?;
        let docs=stmt.query_map(params![project,actor.instance_id],|r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let docs=docs.iter().map(|s| serde_json::from_str::<Dispatch>(s)).collect::<serde_json::Result<Vec<_>>>()?;
        let machine_pending:Vec<_>=docs.iter().filter(|d|d.state=="pending_machine" && d.target.is_none() && d.machine_target.as_ref().is_some_and(|m|m.instance_id==actor.instance_id && m.os==actor.os)).collect();
        let pending:Vec<_>=docs.iter().filter(|d|d.target.as_ref()==Some(&caller) && matches!(d.state.as_str(),"pending"|"received")).collect();
        // Preserve own history and expose lineage only for tasks currently assigned
        // to this authenticated machine/OS in this project. This does not adopt a claim.
        let mut stmt=conn.prepare("SELECT e.id,e.operation,e.document_json,e.created_at,e.authentication_json
            FROM task_dispatch_events e WHERE e.project_ident=?1
            AND e.operation IN ('claim','start','handoff_prior_claim','succeeded','failed','cancel')
            AND (e.instance_id=?2 OR EXISTS (
                SELECT 1 FROM task_dispatches d WHERE d.project_ident=?1 AND d.task_id=e.task_id
                AND ((json_extract(d.document_json,'$.target.origin.instance_id')=?2
                      AND json_extract(d.document_json,'$.target.origin.os')=?3)
                  OR (json_extract(d.document_json,'$.state')='pending_machine'
                      AND json_extract(d.document_json,'$.target') IS NULL
                      AND json_extract(d.document_json,'$.machine_target.instance_id')=?2
                      AND json_extract(d.document_json,'$.machine_target.os')=?3))))
            ORDER BY e.id")?;
        let history=stmt.query_map(params![project,actor.instance_id,actor.os],|r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let history=history.into_iter().map(|(id,op,doc,at,auth)| -> Result<Value> {Ok(json!({"id":id,"operation":op,"dispatch":serde_json::from_str::<Value>(&doc)?,"created_at":at,"authentication":serde_json::from_str::<Value>(&auth)?}))}).collect::<Result<Vec<_>>>()?;
        let mut stmt=conn.prepare("SELECT document_json FROM task_dispatches WHERE json_extract(document_json,'$.source_project_ident')=?1 AND json_extract(document_json,'$.requester.origin.instance_id')=?2 AND json_extract(document_json,'$.requester.origin.session_id')=?3 AND json_extract(document_json,'$.requester.generation')=?4 ORDER BY task_id")?;
        let requested=stmt.query_map(params![project,actor.instance_id,actor.session_id,caller.generation],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let requested=requested.iter().map(|s|serde_json::from_str::<Value>(s)).collect::<serde_json::Result<Vec<_>>>()?;
        Ok(Json(json!({"history_complete":true,"pending":pending,"machine_pending":machine_pending,"machine_dispatches":docs,"prior_claims":history,"requested":requested})))
    }).await?
}

/// Kept separate so fixture HTTP tests exercise the production route wiring.
pub fn router() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route(
            "/v1/projects/{ident}/task-sessions",
            get(sessions).post(register),
        )
        .route(
            "/v1/projects/{ident}/task-dispatches",
            post(delegate_handler),
        )
        .route(
            "/v1/projects/{ident}/task-dispatches/recovery",
            get(recovery),
        )
        .route(
            "/v1/projects/{ident}/task-dispatches/{id}/actions",
            post(action_handler),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
        middleware,
        routing::{get, post},
        Router,
    };
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };
    use tower::ServiceExt;

    fn id() -> String {
        uuid::Uuid::now_v7().to_string()
    }
    fn fixture(database: db::Db) -> AppState {
        {
            let conn = database.lock().unwrap();
            for ident in ["example", "destination"] {
                if db::get_project(&conn, ident).unwrap().is_some() {
                    continue;
                }
                db::insert_project(
                    &conn,
                    &db::Project {
                        ident: ident.into(),
                        channel_name: "discord".into(),
                        room_id: ident.into(),
                        last_msg_id: None,
                        created_at: db::now_ms(),
                        repo_provider: None,
                        repo_namespace: None,
                        repo_name: None,
                        repo_full_name: None,
                        kind: db::PROJECT_KIND_REPO.into(),
                        archived_at: None,
                        canonical_remote: None,
                    },
                )
                .unwrap();
            }
        }
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

    #[derive(Clone)]
    struct Agent {
        target: Target,
        machine_key: String,
        secret: String,
    }
    impl Agent {
        fn new(instance: &str, key: &str) -> Self {
            let mut secret = [0u8; 32];
            OsRng.fill_bytes(&mut secret);
            Self {
                target: Target {
                    origin: AgentOrigin {
                        session_id: id(),
                        instance_id: instance.into(),
                        provider: "codex".into(),
                        os: "linux".into(),
                    },
                    generation: id(),
                },
                machine_key: key.into(),
                secret: hex::encode(secret),
            }
        }
        fn headers(&self) -> HeaderMap {
            let mut h = HeaderMap::new();
            for (name, value) in [
                ("authorization", "Bearer fixture-bearer"),
                ("x-agent-session-id", &self.target.origin.session_id),
                ("x-agent-instance-id", &self.target.origin.instance_id),
                ("x-agent-provider", &self.target.origin.provider),
                ("x-agent-os", &self.target.origin.os),
                ("x-gateway-machine-key", &self.machine_key),
                ("x-gateway-session-key", &self.secret),
            ] {
                h.insert(name, value.parse().unwrap());
            }
            h
        }
        fn action(&self, operation: &str, claim: Option<&str>) -> Value {
            json!({"request_id":id(),"generation":self.target.generation,"operation":operation,"claim_id":claim})
        }
        fn delegation(&self, target: &Agent, project: &str) -> Value {
            json!({"request_id":id(),"generation":self.target.generation,"target_project_ident":project,"target":target.target,"title":"fixture task","specification":"No executors or notifications run in this fixture.","required_os":target.target.origin.os,"required_provider":target.target.origin.provider})
        }
    }
    fn app(state: &AppState) -> Router {
        router()
            .route(
                "/v1/tasks/capabilities",
                get(crate::task_requester::capabilities),
            )
            .route(
                "/v1/projects/{ident}/tasks/{id}/requester",
                axum::routing::put(crate::task_requester::set),
            )
            .route(
                "/v1/projects/{ident}/tasks/delegate",
                post(crate::routes::delegate_task_handler),
            )
            .route(
                "/v1/projects/{ident}/task-targets",
                get(crate::task_targets::list).post(crate::task_targets::heartbeat),
            )
            .route(
                "/v1/projects/{ident}/tasks",
                get(crate::routes::list_tasks_handler).post(crate::routes::create_task_handler),
            )
            .route(
                "/v1/projects/{ident}/tasks/{id}/assign",
                post(crate::routes::assign_task_handler),
            )
            .route(
                "/v1/projects/{ident}/tasks/{id}/comments",
                post(crate::routes::add_comment_handler),
            )
            .route(
                "/v1/projects/{ident}/tasks/{id}/reassign",
                post(crate::routes::reassign_task_handler),
            )
            .route(
                "/v1/projects/{ident}/tasks/{id}",
                get(crate::routes::get_task_handler)
                    .patch(crate::routes::update_task_handler)
                    .delete(crate::routes::delete_task_handler),
            )
            .route(
                "/v1/projects/{ident}/tasks/reorder",
                post(crate::routes::reorder_tasks_handler),
            )
            .layer(middleware::from_fn_with_state(
                state.clone(),
                crate::bearer_auth,
            ))
            .with_state(state.clone())
    }
    async fn http(
        state: &AppState,
        agent: &Agent,
        method: &str,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        *request.headers_mut() = agent.headers();
        request
            .headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        let response = app(state).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 2_000_000).await.unwrap();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)}));
        (status, value)
    }
    async fn ok(state: &AppState, agent: &Agent, method: &str, path: &str, body: Value) -> Value {
        let (status, value) = http(state, agent, method, path, body).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        value
    }
    async fn register_agent(state: &AppState, agent: &Agent, project: &str) {
        let response = ok(
            state,
            agent,
            "POST",
            &format!("/v1/projects/{project}/task-sessions"),
            json!({"generation":agent.target.generation}),
        )
        .await;
        assert_eq!(response["target"], json!(agent.target));
    }
    async fn agents(state: &AppState) -> Vec<Agent> {
        let instance = id();
        let key = provision(&state.db.lock().unwrap(), "example", &instance, "linux").unwrap();
        let agents: Vec<_> = (0..3).map(|_| Agent::new(&instance, &key)).collect();
        for agent in &agents {
            register_agent(state, agent, "example").await;
        }
        agents
    }
    fn actions(task: &str) -> String {
        format!("/v1/projects/example/task-dispatches/{task}/actions")
    }
    async fn recovery_for(state: &AppState, agent: &Agent) -> Value {
        ok(
            state,
            agent,
            "GET",
            &format!(
                "/v1/projects/example/task-dispatches/recovery?generation={}",
                agent.target.generation
            ),
            Value::Null,
        )
        .await
    }

    #[tokio::test]
    async fn offline_machine_queue_survives_restart_and_binds_one_reconnecting_session() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/offline-machine-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.sqlite3", id()));
        let state = fixture(db::open(path.to_str().unwrap()).unwrap());
        let senders = agents(&state).await;
        let sender = &senders[0];
        let instance = id();
        let key = provision(
            &state.db.lock().unwrap(),
            "destination",
            &instance,
            "windows",
        )
        .unwrap();
        let body = json!({"request_id":id(),"generation":sender.target.generation,
            "target_project_ident":"destination","machine_target":{"instance_id":instance},
            "title":"Offline Windows fixture", "specification":"No executor is launched.",
            "required_os":"windows","required_provider":"codex"});
        let accepted = ok(
            &state,
            sender,
            "POST",
            "/v1/projects/example/task-dispatches",
            body.clone(),
        )
        .await;
        let task = accepted["task"]["id"].as_str().unwrap().to_string();
        assert_eq!(accepted["dispatch"]["state"], "pending_machine");
        assert_eq!(accepted["dispatch"]["target"], Value::Null);
        assert_eq!(accepted["dispatch"]["machine_target"]["os"], "windows");
        assert_eq!(accepted["task"]["status"], "todo");
        assert!(accepted["task"]["owner_origin"].is_null());
        assert!(accepted["task"]["started_at"].is_null());
        assert_eq!(
            accepted["authentication"]["machine"]["project_ident"],
            "example"
        );
        assert_eq!(
            recovery_for(&state, sender).await["requested"][0]["task_id"],
            task
        );
        assert_eq!(
            state
                .db
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(state);
        // Reopen the real SQLite file: neither accepted request nor unbound destination is lost.
        let state = fixture(db::open(path.to_str().unwrap()).unwrap());
        let replay = ok(
            &state,
            sender,
            "POST",
            "/v1/projects/example/task-dispatches",
            body,
        )
        .await;
        assert_eq!(accepted, replay);
        let peers: Vec<_> = (0..3)
            .map(|_| {
                let mut peer = Agent::new(&instance, &key);
                peer.target.origin.os = "windows".into();
                peer
            })
            .collect();
        let recovery_path = |peer: &Agent| {
            format!(
                "/v1/projects/destination/task-dispatches/recovery?generation={}",
                peer.target.generation
            )
        };
        let actions = format!("/v1/projects/destination/task-dispatches/{task}/actions");
        for peer in &peers {
            register_agent(&state, peer, "destination").await;
            let recovered = ok(&state, peer, "GET", &recovery_path(peer), Value::Null).await;
            assert_eq!(recovered["machine_pending"].as_array().unwrap().len(), 1);
            assert_eq!(recovered["pending"].as_array().unwrap().len(), 0);
            assert_eq!(recovered["machine_pending"][0]["state"], "pending_machine");
        }
        let responses = tokio::join!(
            http(
                &state,
                &peers[0],
                "POST",
                &actions,
                peers[0].action("receive", None)
            ),
            http(
                &state,
                &peers[1],
                "POST",
                &actions,
                peers[1].action("receive", None)
            ),
            http(
                &state,
                &peers[2],
                "POST",
                &actions,
                peers[2].action("receive", None)
            ),
        );
        let results = [responses.0, responses.1, responses.2];
        assert_eq!(results.iter().filter(|r| r.0 == StatusCode::OK).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|r| r.0 == StatusCode::CONFLICT)
                .count(),
            2
        );
        let winner_index = results.iter().position(|r| r.0 == StatusCode::OK).unwrap();
        let winner = &peers[winner_index];
        assert_eq!(
            results[winner_index].1["dispatch"]["target"],
            json!(winner.target)
        );
        assert_eq!(results[winner_index].1["task"]["status"], "todo");
        assert!(results[winner_index].1["task"]["owner_origin"].is_null());
        assert!(results[winner_index].1["dispatch"]["claim_id"].is_null());
        assert!(results[winner_index].1["task"]["started_at"].is_null());
        for (index, peer) in peers.iter().enumerate() {
            if index != winner_index {
                assert_eq!(
                    http(&state, peer, "POST", &actions, peer.action("claim", None))
                        .await
                        .0,
                    StatusCode::CONFLICT
                );
            }
        }
        let claimed = ok(
            &state,
            winner,
            "POST",
            &actions,
            winner.action("claim", None),
        )
        .await;
        let claim = claimed["dispatch"]["claim_id"].as_str().unwrap();
        assert!(claimed["task"]["started_at"].is_null());
        let execution = id();
        let mut start = winner.action("start", Some(claim));
        start["execution_id"] = json!(execution);
        let running = ok(&state, winner, "POST", &actions, start).await;
        assert_eq!(running["dispatch"]["state"], "running");
        let mut restarted = winner.clone();
        restarted.target.generation = id();
        restarted.secret = "ab".repeat(32);
        register_agent(&state, &restarted, "destination").await;
        let recovery = ok(
            &state,
            &restarted,
            "GET",
            &recovery_path(&restarted),
            Value::Null,
        )
        .await;
        assert_eq!(recovery["machine_pending"].as_array().unwrap().len(), 0);
        assert_eq!(recovery["pending"].as_array().unwrap().len(), 0);
        assert!(recovery["prior_claims"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["operation"] == "start" && e["dispatch"]["claim_id"] == claim));
        assert_eq!(
            http(
                &state,
                &restarted,
                "POST",
                &actions,
                restarted.action("claim", Some(claim))
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        // Only the original exact owner may deliberately return its claim to an offline machine queue.
        let next_instance = id();
        let next_key = provision(
            &state.db.lock().unwrap(),
            "destination",
            &next_instance,
            "windows",
        )
        .unwrap();
        let mut handoff = winner.action("handoff", Some(claim));
        handoff["machine_target"] = json!({"instance_id":next_instance});
        handoff["result"] = json!("Fixture executor stopped; explicit offline handoff");
        let queued = ok(&state, winner, "POST", &actions, handoff).await;
        assert_eq!(queued["dispatch"]["state"], "pending_machine");
        assert!(queued["dispatch"]["target"].is_null());
        assert!(queued["dispatch"]["claim_id"].is_null());
        assert_eq!(queued["task"]["status"], "todo");
        let prior = ok(
            &state,
            &restarted,
            "GET",
            &recovery_path(&restarted),
            Value::Null,
        )
        .await;
        assert!(prior["prior_claims"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["operation"] == "handoff_prior_claim"
                && e["dispatch"]["claim_id"] == claim
                && e["dispatch"]["execution_id"] == execution));
        let mut next = Agent::new(&next_instance, &next_key);
        next.target.origin.os = "windows".into();
        register_agent(&state, &next, "destination").await;
        ok(
            &state,
            &next,
            "POST",
            &actions,
            next.action("receive", None),
        )
        .await;
        let new_claim = ok(&state, &next, "POST", &actions, next.action("claim", None)).await;
        assert_ne!(new_claim["dispatch"]["claim_id"], claim);
        assert_eq!(
            http(
                &state,
                &next,
                "POST",
                &actions,
                next.action("start", Some(claim))
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        drop(state);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn new_machine_recovers_only_assigned_task_lineage_without_mutation() {
        let state = fixture(db::open(":memory:").unwrap());
        let senders = agents(&state).await;
        let sender = &senders[0];
        let old_instance = id();
        let old_key =
            provision(&state.db.lock().unwrap(), "example", &old_instance, "linux").unwrap();
        let old_owner = Agent::new(&old_instance, &old_key);
        register_agent(&state, &old_owner, "example").await;
        async fn started_task(
            state: &AppState,
            sender: &Agent,
            owner: &Agent,
        ) -> (String, String, String) {
            let accepted = ok(
                state,
                sender,
                "POST",
                "/v1/projects/example/task-dispatches",
                sender.delegation(owner, "example"),
            )
            .await;
            let task = accepted["task"]["id"].as_str().unwrap().to_string();
            ok(
                state,
                owner,
                "POST",
                &actions(&task),
                owner.action("receive", None),
            )
            .await;
            let claimed = ok(
                state,
                owner,
                "POST",
                &actions(&task),
                owner.action("claim", None),
            )
            .await;
            let claim = claimed["dispatch"]["claim_id"]
                .as_str()
                .unwrap()
                .to_string();
            let execution = id();
            let mut start = owner.action("start", Some(&claim));
            start["execution_id"] = json!(execution);
            ok(state, owner, "POST", &actions(&task), start).await;
            (task, claim, execution)
        }
        let (task, claim, execution) = started_task(&state, sender, &old_owner).await;
        let (unrelated, _, _) = started_task(&state, sender, &old_owner).await;
        let new_instance = id();
        let new_key =
            provision(&state.db.lock().unwrap(), "example", &new_instance, "linux").unwrap();
        let mut handoff = old_owner.action("handoff", Some(&claim));
        handoff["machine_target"] = json!({"instance_id":new_instance});
        handoff["result"] = json!("Synthetic executor stopped; deliberate machine handoff");
        ok(&state, &old_owner, "POST", &actions(&task), handoff).await;
        let receiver = Agent::new(&new_instance, &new_key);
        register_agent(&state, &receiver, "example").await;
        let snapshot = || {
            let conn = state.db.lock().unwrap();
            let document: String = conn
                .query_row(
                    "SELECT document_json FROM task_dispatches WHERE task_id=?1",
                    [&task],
                    |r| r.get(0),
                )
                .unwrap();
            let task = serde_json::to_value(
                db::get_task_detail(&conn, "example", &task)
                    .unwrap()
                    .unwrap()
                    .task,
            )
            .unwrap();
            let counts: (i64,i64) = conn.query_row("SELECT (SELECT count(*) FROM task_dispatch_events),(SELECT count(*) FROM task_dispatch_requests)", [], |r|Ok((r.get(0)?,r.get(1)?))).unwrap();
            (document, task, counts)
        };
        let before = snapshot();
        let recovery = recovery_for(&state, &receiver).await;
        assert_eq!(recovery["history_complete"], true);
        let mut bad_credentials = receiver.clone();
        bad_credentials.secret = "00".repeat(32);
        let (status, error) = http(
            &state,
            &bad_credentials,
            "GET",
            &format!(
                "/v1/projects/example/task-dispatches/recovery?generation={}",
                receiver.target.generation
            ),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            error.get("history_complete").is_none(),
            "an error response is no completeness proof"
        );
        assert_eq!(
            before,
            snapshot(),
            "recovery must not bind, claim, start, or add history/replay records"
        );
        assert_eq!(recovery["machine_pending"][0]["task_id"], task);
        assert!(recovery["pending"].as_array().unwrap().is_empty());
        let history = recovery["prior_claims"].as_array().unwrap();
        assert_eq!(
            history.len(),
            3,
            "new destination needs the prior claim, start, and pre-handoff snapshot"
        );
        for event in history {
            assert_eq!(event["dispatch"]["task_id"], task);
            assert_ne!(event["dispatch"]["task_id"], unrelated);
            assert_eq!(event["dispatch"]["claim_id"], claim);
            assert_eq!(
                event["authentication"]["machine"]["instance_id"],
                old_instance
            );
            assert_eq!(
                event["authentication"]["session"],
                json!(old_owner.target.origin)
            );
        }
        let prior = history
            .iter()
            .find(|e| e["operation"] == "handoff_prior_claim")
            .unwrap();
        assert_eq!(prior["dispatch"]["execution_id"], execution);
        assert_eq!(prior["dispatch"]["state"], "running");
        assert!(prior["dispatch"]["finished_at"].is_null());
        assert!(
            prior["dispatch"]["result"].is_null(),
            "historical unknown outcome is not rewritten as success"
        );
        // The requester machine has no prior claims or assigned tasks here.
        assert!(recovery_for(&state, sender).await["prior_claims"]
            .as_array()
            .unwrap()
            .is_empty());
        // The old machine retains its own history, including the unrelated task.
        assert!(recovery_for(&state, &old_owner).await["prior_claims"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["dispatch"]["task_id"] == unrelated));
        // Reusing the same machine UUID in another authorized project grants no cross-project history.
        let other_key = provision(
            &state.db.lock().unwrap(),
            "destination",
            &new_instance,
            "linux",
        )
        .unwrap();
        let other_project = Agent::new(&new_instance, &other_key);
        register_agent(&state, &other_project, "destination").await;
        let other = ok(
            &state,
            &other_project,
            "GET",
            &format!(
                "/v1/projects/destination/task-dispatches/recovery?generation={}",
                other_project.target.generation
            ),
            Value::Null,
        )
        .await;
        assert!(other["prior_claims"].as_array().unwrap().is_empty());
        assert!(other["machine_pending"].as_array().unwrap().is_empty());
        assert_eq!(before, snapshot());
    }

    #[tokio::test]
    async fn machine_eligibility_and_replay_keep_existing_session_contracts() {
        let state = fixture(db::open(":memory:").unwrap());
        let peers = agents(&state).await;
        let sender = &peers[0];
        let instance = id();
        let key = provision(&state.db.lock().unwrap(), "example", &instance, "windows").unwrap();
        let body = json!({"request_id":id(),"generation":sender.target.generation,"target_project_ident":"example",
            "machine_target":{"instance_id":instance},"title":"Offline","specification":"No execution", "required_os":"windows","required_provider":"codex"});
        for (field, value) in [
            ("required_os", json!("linux")),
            ("machine_target", json!({"instance_id":id()})),
            ("target", json!(peers[1].target)),
        ] {
            let mut invalid = body.clone();
            invalid[field] = value;
            assert_eq!(
                http(
                    &state,
                    sender,
                    "POST",
                    "/v1/projects/example/task-dispatches",
                    invalid
                )
                .await
                .0,
                StatusCode::CONFLICT
            );
        }
        let accepted = ok(
            &state,
            sender,
            "POST",
            "/v1/projects/example/task-dispatches",
            body.clone(),
        )
        .await;
        let task = accepted["task"]["id"].as_str().unwrap();
        let mut changed = body.clone();
        changed["title"] = json!("Changed");
        assert_eq!(
            http(
                &state,
                sender,
                "POST",
                "/v1/projects/example/task-dispatches",
                changed
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            http(
                &state,
                &peers[1],
                "POST",
                &actions(task),
                peers[1].action("receive", None)
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let mut wrong_provider = Agent::new(&instance, &key);
        wrong_provider.target.origin.os = "windows".into();
        wrong_provider.target.origin.provider = "claude".into();
        register_agent(&state, &wrong_provider, "example").await;
        assert_eq!(
            http(
                &state,
                &wrong_provider,
                "POST",
                &actions(task),
                wrong_provider.action("receive", None)
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let mut cancel = sender.action("cancel", None);
        cancel["result"] = json!("Withdraw before reconnect");
        let cancelled = ok(&state, sender, "POST", &actions(task), cancel).await;
        assert_eq!(cancelled["task"]["status"], "cancelled");
        assert!(cancelled["task"]["started_at"].is_null());
        assert!(cancelled["task"]["done_at"].is_null());
        assert_eq!(
            ok(
                &state,
                sender,
                "POST",
                "/v1/projects/example/task-dispatches",
                body
            )
            .await,
            accepted
        );
        // Absent machine_target must not change serialized legacy dedup fingerprints.
        let old = sender.delegation(&peers[1], "example");
        let parsed: Delegate = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), old);
        let old = sender.action("receive", None);
        let parsed: Action = serde_json::from_value(old.clone()).unwrap();
        let serialized = serde_json::to_value(parsed).unwrap();
        assert!(serialized.get("machine_target").is_none());
    }

    // Ordinary desktop requests deliberately carry no machine/session credentials.
    async fn desktop_http(
        state: &AppState,
        headers: HeaderMap,
        method: &str,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        *request.headers_mut() = headers;
        request
            .headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        let response = app(state).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 2_000_000).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})),
        )
    }
    fn human_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer fixture-bearer".parse().unwrap());
        h
    }
    fn ordinary_headers(agent: &Agent) -> HeaderMap {
        let mut h = agent.headers();
        h.remove("x-gateway-machine-key");
        h.remove("x-gateway-session-key");
        h
    }
    fn repository_fixture(state: &AppState) {
        state
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE projects SET canonical_remote='github.com/acme/' || ident",
                [],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn requester_survives_executor_changes_and_events_without_enrolling_peers() {
        let state = fixture(db::open(":memory:").unwrap());
        repository_fixture(&state);
        let a = Agent::new(&id(), "unused");
        let b = Agent::new(&id(), "unused");
        let c = Agent::new(&id(), "unused");
        let d = Agent::new(&id(), "unused");
        for peer in [&a, &b, &c, &d] {
            let (status,body) = desktop_http(&state, ordinary_headers(peer), "POST", "/v1/projects/example/task-targets",
                json!({"generation":peer.target.generation,"canonical_remote":"github.com/acme/example"})).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let (_, caps) = desktop_http(
            &state,
            human_headers(),
            "GET",
            "/v1/tasks/capabilities",
            Value::Null,
        )
        .await;
        assert_eq!(caps["requester_subscriptions"], 1);
        let (status,created) = desktop_http(&state,ordinary_headers(&a),"POST","/v1/projects/example/tasks",
            json!({"title":"Return results to A","requester":a.target,"canonical_remote":"github.com/acme/example",
                "routing":{"mode":"session","target":b.target},"labels":["os:linux","keep"]})).await;
        assert_eq!(status, StatusCode::OK, "{created}");
        assert_eq!(created["requester"], json!(a.target));
        assert_eq!(created["owner_origin"], json!(b.target.origin));
        let path = format!(
            "/v1/projects/example/tasks/{}",
            created["id"].as_str().unwrap()
        );
        let watcher = format!("{path}/requester");
        for body in [json!({"requester":d.target}), json!({"requester":null})] {
            assert_eq!(
                desktop_http(&state, ordinary_headers(&d), "PUT", &watcher, body)
                    .await
                    .0,
                StatusCode::CONFLICT
            );
        }
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&b),
                "POST",
                &format!("{path}/reassign"),
                json!({"target":c.target})
            )
            .await
            .0,
            StatusCode::OK
        );
        for (headers, status) in [
            (ordinary_headers(&c), "in_progress"),
            (ordinary_headers(&c), "user_review"),
            (human_headers(), "todo"),
        ] {
            let (code, body) =
                desktop_http(&state, headers, "PATCH", &path, json!({"status":status})).await;
            assert_eq!(code, StatusCode::OK, "{body}");
            assert_eq!(body["requester"], json!(a.target));
        }
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "POST",
                &format!("{path}/assign"),
                json!({"target":c.target})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&c),
                "POST",
                &format!("{path}/comments"),
                json!({"content":"C result"})
            )
            .await
            .0,
            StatusCode::OK
        );
        let (code, done) = desktop_http(
            &state,
            ordinary_headers(&c),
            "PATCH",
            &path,
            json!({"status":"done"}),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{done}");
        assert_eq!(done["requester"], json!(a.target));
        {
            let conn = state.db.lock().unwrap();
            let mut stmt = conn.prepare("SELECT kind,task_json,origin_json FROM task_events WHERE json_extract(task_json,'$.id')=?1 ORDER BY id").unwrap();
            let events = stmt
                .query_map([created["id"].as_str().unwrap()], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            for kind in ["task_created", "task_commented", "task_completed"] {
                let event = events.iter().rev().find(|e| e.0 == kind).unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&event.1).unwrap()["requester"],
                    json!(a.target)
                );
                if kind != "task_created" {
                    assert_eq!(
                        serde_json::from_str::<Value>(event.2.as_ref().unwrap()).unwrap(),
                        json!(c.target.origin)
                    );
                }
            }
        }
        // A can unsubscribe after C completes without owning execution.
        let (code, cleared) = desktop_http(
            &state,
            ordinary_headers(&a),
            "PUT",
            &watcher,
            json!({"requester":null}),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{cleared}");
        assert!(cleared["requester"].is_null());
        // Human workflow defaults to no subscriber, and can explicitly set/clear one.
        let (_, human) = desktop_http(
            &state,
            human_headers(),
            "POST",
            "/v1/projects/example/tasks",
            json!({"title":"Human"}),
        )
        .await;
        assert!(human["requester"].is_null());
        let hp = format!(
            "/v1/projects/example/tasks/{}/requester",
            human["id"].as_str().unwrap()
        );
        let (code, chosen) = desktop_http(
            &state,
            human_headers(),
            "PUT",
            &hp,
            json!({"requester":a.target}),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{chosen}");
        assert_eq!(chosen["requester"], json!(a.target));
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "PUT",
                &hp,
                json!({"requester":null})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            desktop_http(&state, human_headers(), "PUT", &hp, json!({}))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        // Cross-project delegation watches the SOURCE mirror, never a foreign-project target.
        let (code,delegated) = desktop_http(&state,ordinary_headers(&a),"POST","/v1/projects/example/tasks/delegate",
            json!({"title":"Delegated results","target_project_ident":"destination","requester":a.target})).await;
        assert_eq!(code, StatusCode::OK, "{delegated}");
        assert_eq!(delegated["source_task"]["requester"], json!(a.target));
        assert!(delegated["target_task"]["requester"].is_null());
    }

    #[tokio::test]
    async fn requester_rejects_stale_foreign_and_wrong_os_atomically() {
        let state = fixture(db::open(":memory:").unwrap());
        repository_fixture(&state);
        let a = Agent::new(&id(), "unused");
        let foreign = Agent::new(&id(), "unused");
        for (agent, project) in [(&a, "example"), (&foreign, "destination")] {
            assert_eq!(desktop_http(&state,ordinary_headers(agent),"POST",&format!("/v1/projects/{project}/task-targets"),
                json!({"generation":agent.target.generation,"canonical_remote":format!("github.com/acme/{project}")})).await.0,StatusCode::OK);
        }
        let mut stale = a.target.clone();
        stale.generation = id();
        for (headers, target, labels) in [
            (human_headers(), stale.clone(), json!([])),
            (human_headers(), foreign.target.clone(), json!([])),
            (human_headers(), a.target.clone(), json!(["os:windows"])),
            (ordinary_headers(&foreign), a.target.clone(), json!([])),
        ] {
            let before: i64 = state
                .db
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
                .unwrap();
            let (code, body) = desktop_http(
                &state,
                headers,
                "POST",
                "/v1/projects/example/tasks",
                json!({"title":"Rejected","requester":target,"labels":labels}),
            )
            .await;
            assert_eq!(code, StatusCode::CONFLICT, "{body}");
            assert_eq!(
                before,
                state
                    .db
                    .lock()
                    .unwrap()
                    .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get::<_, i64>(0))
                    .unwrap()
            );
        }
        // A source watcher rejection must roll back the already-inserted destination and its event.
        let counts = || {
            let conn = state.db.lock().unwrap();
            conn.query_row(
                "SELECT (SELECT COUNT(*) FROM tasks),(SELECT COUNT(*) FROM task_events)",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .unwrap()
        };
        let before = counts();
        let (code,body) = desktop_http(&state,ordinary_headers(&a),"POST","/v1/projects/example/tasks/delegate",
            json!({"title":"Rollback delegation","target_project_ident":"destination","requester":stale})).await;
        assert_eq!(code, StatusCode::CONFLICT, "{body}");
        assert_eq!(counts(), before);
        let (_, created) = desktop_http(
            &state,
            human_headers(),
            "POST",
            "/v1/projects/example/tasks",
            json!({"title":"Keep state","requester":a.target}),
        )
        .await;
        let path = format!(
            "/v1/projects/example/tasks/{}/requester",
            created["id"].as_str().unwrap()
        );
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "PUT",
                &path,
                json!({"requester":stale})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let conn = state.db.lock().unwrap();
        let task = db::get_task_detail(&conn, "example", created["id"].as_str().unwrap())
            .unwrap()
            .unwrap()
            .task;
        assert_eq!(task.requester, Some(a.target.clone()));
        assert_eq!(task.updated_at, created["updated_at"].as_i64().unwrap());
        conn.execute(
            "UPDATE task_target_presence SET generation=?1 WHERE project_ident='example'",
            [id()],
        )
        .unwrap();
        assert!(crate::task_requester::validate(&conn, "example", &[], Some(&a.target)).is_err());
    }

    #[tokio::test]
    async fn desktop_targets_need_only_existing_gateway_auth_and_keep_human_audit() {
        let state = fixture(db::open(":memory:").unwrap());
        repository_fixture(&state);
        let agent = Agent::new(&id(), "unused");
        let endpoint = "/v1/projects/example/task-targets";
        let beat = json!({"generation":agent.target.generation,"canonical_remote":"git@github.com:acme/example.git"});
        assert_eq!(
            desktop_http(&state, HeaderMap::new(), "GET", endpoint, Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            desktop_http(&state, human_headers(), "POST", endpoint, beat.clone())
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&agent),
                "POST",
                endpoint,
                json!({"generation":id(),"canonical_remote":"github.com/other/example"})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let (status, response) =
            desktop_http(&state, ordinary_headers(&agent), "POST", endpoint, beat).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["target"], json!(agent.target));
        let (_, directory) =
            desktop_http(&state, human_headers(), "GET", endpoint, Value::Null).await;
        assert_eq!(directory["targets"].as_array().unwrap().len(), 1);
        assert_eq!(directory["canonical_remote"], "github.com/acme/example");
        let body = json!({"title":"Human selects agent","canonical_remote":"https://github.com/acme/example.git","routing":{"mode":"session","target":directory["targets"][0]["target"]}});
        let (status, task) = desktop_http(
            &state,
            human_headers(),
            "POST",
            "/v1/projects/example/tasks",
            body.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{task}");
        assert_eq!(task["owner_origin"], json!(agent.target.origin));
        assert_eq!(task["status"], "todo");
        assert!(task["started_at"].is_null());
        assert_eq!(task["reporter"], "user");
        assert_eq!(task["routing"], body["routing"]);
        {
            let conn = state.db.lock().unwrap();
            let events = task_events::list(
                &conn,
                &task_events::EventQuery {
                    task_id: Some(task["id"].as_str().unwrap().into()),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].kind, "task_created");
            assert!(events[0].origin.is_none());
            assert_eq!(events[0].task["owner_origin"], task["owner_origin"]);
            assert_eq!(events[0].task["routing"], body["routing"]);
            assert_eq!(
                conn.query_row("SELECT count(*) FROM task_machines", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
            assert_eq!(
                conn.query_row("SELECT count(*) FROM task_sessions", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        // Same real calling agent may reassign without any extra credentials.
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&agent),
                "POST",
                &format!(
                    "/v1/projects/example/tasks/{}/reassign",
                    task["id"].as_str().unwrap()
                ),
                json!({"target":agent.target})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(desktop_http(&state,human_headers(),"POST","/v1/projects/destination/tasks",json!({"title":"Wrong project","canonical_remote":"github.com/acme/destination","routing":body["routing"]})).await.0,StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn desktop_routing_modes_and_stale_targets_fail_without_partial_tasks() {
        let state = fixture(db::open(":memory:").unwrap());
        repository_fixture(&state);
        let agent = Agent::new(&id(), "unused");
        let other = Agent::new(&agent.target.origin.instance_id, "unused");
        for mode in ["auto", "unassigned", "broadcast"] {
            let (status,task)=desktop_http(&state,ordinary_headers(&agent),"POST","/v1/projects/example/tasks",json!({"title":mode,"canonical_remote":"github.com/acme/example","routing":{"mode":mode}})).await;
            assert_eq!(status, StatusCode::OK, "{task}");
            assert_eq!(task["routing"]["mode"], mode);
            if mode == "auto" {
                assert_eq!(task["owner_origin"], json!(agent.target.origin));
            } else {
                assert!(task["owner_origin"].is_null());
                assert!(task["owner_agent_id"].is_null());
            }
            if mode == "broadcast" {
                let path = format!(
                    "/v1/projects/example/tasks/{}",
                    task["id"].as_str().unwrap()
                );
                assert_eq!(
                    desktop_http(
                        &state,
                        ordinary_headers(&agent),
                        "PATCH",
                        &path,
                        json!({"status":"in_progress"})
                    )
                    .await
                    .0,
                    StatusCode::OK
                );
                assert_eq!(
                    desktop_http(
                        &state,
                        ordinary_headers(&other),
                        "PATCH",
                        &path,
                        json!({"status":"in_progress"})
                    )
                    .await
                    .0,
                    StatusCode::CONFLICT
                );
            }
        }
        let endpoint = "/v1/projects/example/task-targets";
        let body = |generation: &str| json!({"generation":generation,"canonical_remote":"github.com/acme/example"});
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&agent),
                "POST",
                endpoint,
                body(&agent.target.generation)
            )
            .await
            .0,
            StatusCode::OK
        );
        let mut replacement = agent.target.clone();
        replacement.generation = id();
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&agent),
                "POST",
                endpoint,
                body(&replacement.generation)
            )
            .await
            .0,
            StatusCode::OK
        );
        let create = |target: &Target| json!({"title":"stale","canonical_remote":"github.com/acme/example","routing":{"mode":"session","target":target}});
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "POST",
                "/v1/projects/example/tasks",
                create(&agent.target)
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        state
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE task_target_presence SET seen_at=?1",
                [db::now_ms() - crate::task_targets::TTL_MS - 1],
            )
            .unwrap();
        let (_, directory) =
            desktop_http(&state, human_headers(), "GET", endpoint, Value::Null).await;
        assert!(directory["targets"].as_array().unwrap().is_empty());
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "POST",
                "/v1/projects/example/tasks",
                create(&replacement)
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(desktop_http(&state,human_headers(),"POST","/v1/projects/example/tasks",json!({"title":"mismatch","canonical_remote":"github.com/other/example","routing":{"mode":"broadcast"}})).await.0,StatusCode::CONFLICT);
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "POST",
                "/v1/projects/example/tasks",
                json!({"title":"bad mode","routing":{"mode":"invented"}})
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let conn = state.db.lock().unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM tasks", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM task_events WHERE kind='task_created'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            3
        );
    }

    #[tokio::test]
    async fn local_only_adhoc_projects_keep_presence_tasks_and_events_project_scoped() {
        let state = fixture(db::open(":memory:").unwrap());
        state
            .db
            .lock()
            .unwrap()
            .execute("UPDATE projects SET kind='adhoc' WHERE ident='example'", [])
            .unwrap();
        let agent = Agent::new(&id(), "unused");
        let beat = json!({"generation":agent.target.generation,"display_name":"Local actor","hostname":"Defcon1"});
        let (status, presence) = desktop_http(
            &state,
            ordinary_headers(&agent),
            "POST",
            "/v1/projects/example/task-targets",
            beat.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "local-only heartbeat: {presence}");
        assert!(presence["canonical_remote"].is_null());
        assert_eq!(presence["target"], json!(agent.target));
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&agent),
                "POST",
                "/v1/projects/example/task-targets",
                json!({"generation":agent.target.generation,"canonical_remote":null})
            )
            .await
            .0,
            StatusCode::OK
        );
        let (_, directory) = desktop_http(
            &state,
            human_headers(),
            "GET",
            "/v1/projects/example/task-targets",
            Value::Null,
        )
        .await;
        assert!(directory["canonical_remote"].is_null());
        assert_eq!(directory["targets"].as_array().unwrap().len(), 1);
        let (status, task) = desktop_http(
            &state,
            human_headers(),
            "POST",
            "/v1/projects/example/tasks",
            json!({"title":"Local only","routing":{"mode":"session","target":agent.target}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{task}");
        assert_eq!(task["project_ident"], "example");
        assert_eq!(task["owner_origin"], json!(agent.target.origin));
        let (_, listed) = desktop_http(
            &state,
            human_headers(),
            "GET",
            "/v1/projects/example/tasks",
            Value::Null,
        )
        .await;
        assert!(listed
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == task["id"]));
        let (_, other) = desktop_http(
            &state,
            human_headers(),
            "GET",
            "/v1/projects/destination/tasks",
            Value::Null,
        )
        .await;
        assert!(other.as_array().unwrap().is_empty());
        {
            let conn = state.db.lock().unwrap();
            let event = task_events::list(&conn, &Default::default())
                .unwrap()
                .remove(0);
            assert_eq!(event.project_ident, "example");
            assert!(event.canonical_remote.is_none());
            assert!(event.origin.is_none());
            assert_eq!(event.task["owner_origin"], task["owner_origin"]);
        }
        // Missing remote is not a wildcard for repo-kind projects or unknown projects.
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&agent),
                "POST",
                "/v1/projects/destination/task-targets",
                beat.clone()
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&agent),
                "POST",
                "/v1/projects/missing/task-targets",
                beat.clone()
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        // Another explicit ad-hoc project cannot consume this project's target.
        state
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE projects SET kind='adhoc' WHERE ident='destination'",
                [],
            )
            .unwrap();
        let (_, elsewhere) = desktop_http(
            &state,
            human_headers(),
            "POST",
            "/v1/projects/destination/tasks",
            json!({"title":"Other project"}),
        )
        .await;
        let other_path = format!(
            "/v1/projects/destination/tasks/{}/assign",
            elsewhere["id"].as_str().unwrap()
        );
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "POST",
                &other_path,
                json!({"target":agent.target})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        // Supplying a repository for a local-only binding is a mismatch.
        assert_eq!(desktop_http(&state,ordinary_headers(&agent),"POST","/v1/projects/example/task-targets",json!({"generation":agent.target.generation,"canonical_remote":"github.com/acme/example"})).await.0,StatusCode::CONFLICT);
        // If the project later gains a remote, old local presence stops being eligible.
        state.db.lock().unwrap().execute("UPDATE projects SET kind='repo',canonical_remote='github.com/acme/example' WHERE ident='example'",[]).unwrap();
        let (_, directory) = desktop_http(
            &state,
            human_headers(),
            "GET",
            "/v1/projects/example/task-targets",
            Value::Null,
        )
        .await;
        assert!(directory["targets"].as_array().unwrap().is_empty());
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&agent),
                "POST",
                "/v1/projects/example/task-targets",
                beat
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "POST",
                "/v1/projects/example/tasks",
                json!({"title":"No wildcard","routing":{"mode":"session","target":agent.target}})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let (status,_)=desktop_http(&state,ordinary_headers(&agent),"POST","/v1/projects/example/task-targets",json!({"generation":agent.target.generation,"canonical_remote":"https://github.com/acme/example.git"})).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn human_assignment_and_live_labels_preserve_user_task_controls() {
        let state = fixture(db::open(":memory:").unwrap());
        repository_fixture(&state);
        let owner = Agent::new(&id(), "unused");
        let target = Agent::new(&id(), "unused");
        let endpoint = "/v1/projects/example/task-targets";
        let beat = json!({"generation":target.target.generation,"canonical_remote":"github.com/acme/example","display_name":"  Grace Hopper - Codex  ","hostname":"  Defcon2  "});
        let (status, presence) =
            desktop_http(&state, ordinary_headers(&target), "POST", endpoint, beat).await;
        assert_eq!(status, StatusCode::OK, "{presence}");
        assert_eq!(presence["display_name"], "Grace Hopper - Codex");
        assert_eq!(presence["hostname"], "Defcon2");
        let (_, directory) =
            desktop_http(&state, human_headers(), "GET", endpoint, Value::Null).await;
        assert_eq!(
            directory["targets"][0]["display_name"],
            presence["display_name"]
        );
        assert_eq!(directory["targets"][0]["hostname"], presence["hostname"]);
        let (_, task) = desktop_http(
            &state,
            ordinary_headers(&owner),
            "POST",
            "/v1/projects/example/tasks",
            json!({"title":"Owner work"}),
        )
        .await;
        let path = format!(
            "/v1/projects/example/tasks/{}",
            task["id"].as_str().unwrap()
        );
        let assign = format!("{path}/assign");
        let body = json!({"target":target.target});
        assert_eq!(
            desktop_http(&state, HeaderMap::new(), "POST", &assign, body.clone())
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&owner),
                "POST",
                &assign,
                body.clone()
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let (status, moved) = desktop_http(&state, human_headers(), "POST", &assign, body).await;
        assert_eq!(status, StatusCode::OK, "{moved}");
        assert_eq!(moved["owner_origin"], json!(target.target.origin));
        assert_eq!(moved["status"], "todo");
        assert!(moved["started_at"].is_null());
        let (_, detail) = desktop_http(&state, human_headers(), "GET", &path, Value::Null).await;
        assert_eq!(detail["comments"].as_array().unwrap().len(), 1);
        assert_eq!(detail["comments"][0]["author_type"], "user");
        assert!(detail["comments"][0]["origin"].is_null());
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&owner),
                "PATCH",
                &path,
                json!({"status":"done"})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "PATCH",
                &path,
                json!({"title":"User edited","description":"User details"})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "POST",
                &format!("{path}/comments"),
                json!({"content":"User comment"})
            )
            .await
            .0,
            StatusCode::OK
        );
        let (status, done) = desktop_http(
            &state,
            human_headers(),
            "PATCH",
            &path,
            json!({"status":"done"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{done}");
        assert_eq!(done["status"], "done");
        assert_eq!(done["owner_origin"], json!(target.target.origin));
        {
            let conn = state.db.lock().unwrap();
            let events = task_events::list(
                &conn,
                &task_events::EventQuery {
                    task_id: Some(task["id"].as_str().unwrap().into()),
                    ..Default::default()
                },
            )
            .unwrap();
            let completed = events.iter().find(|e| e.kind == "task_completed").unwrap();
            assert!(completed.origin.is_none());
            let assigned = events
                .iter()
                .find(|e| {
                    e.kind == "task_commented"
                        && e.comment.as_ref().unwrap()["content"]
                            .as_str()
                            .unwrap()
                            .starts_with("Task reassigned")
                })
                .unwrap();
            assert!(assigned.origin.is_none());
            assert_eq!(assigned.task["owner_origin"], json!(target.target.origin));
        }
        // Old clients omit optional labels; nullable fields remain compatible.
        let (_,legacy)=desktop_http(&state,ordinary_headers(&target),"POST",endpoint,json!({"generation":target.target.generation,"canonical_remote":"github.com/acme/example"})).await;
        assert!(legacy["display_name"].is_null());
        assert!(legacy["hostname"].is_null());
        let (_, directory) =
            desktop_http(&state, human_headers(), "GET", endpoint, Value::Null).await;
        assert!(directory["targets"][0]["display_name"].is_null());
        assert_eq!(desktop_http(&state,ordinary_headers(&target),"POST",endpoint,json!({"generation":target.target.generation,"canonical_remote":"github.com/acme/example","display_name":"x".repeat(129)})).await.0,StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn user_assignment_rejects_invalid_expired_and_cross_project_targets_without_mutation() {
        let state = fixture(db::open(":memory:").unwrap());
        repository_fixture(&state);
        let owner = Agent::new(&id(), "unused");
        let target = Agent::new(&id(), "unused");
        let endpoint = "/v1/projects/example/task-targets";
        assert_eq!(desktop_http(&state,ordinary_headers(&target),"POST",endpoint,json!({"generation":target.target.generation,"canonical_remote":"github.com/acme/example"})).await.0,StatusCode::OK);
        let (_, task) = desktop_http(
            &state,
            ordinary_headers(&owner),
            "POST",
            "/v1/projects/example/tasks",
            json!({"title":"Reserved"}),
        )
        .await;
        let path = format!(
            "/v1/projects/example/tasks/{}",
            task["id"].as_str().unwrap()
        );
        let mut invalid = target.target.clone();
        invalid.generation = id();
        let mut another = target.target.clone();
        another.origin.instance_id = id();
        for bad in [&invalid, &another] {
            assert_eq!(
                desktop_http(
                    &state,
                    human_headers(),
                    "POST",
                    &format!("{path}/assign"),
                    json!({"target":bad})
                )
                .await
                .0,
                StatusCode::CONFLICT
            );
            assert_eq!(
                desktop_http(
                    &state,
                    ordinary_headers(&owner),
                    "POST",
                    &format!("{path}/reassign"),
                    json!({"target":bad})
                )
                .await
                .0,
                StatusCode::CONFLICT
            );
        }
        let (_, destination) = desktop_http(
            &state,
            human_headers(),
            "POST",
            "/v1/projects/destination/tasks",
            json!({"title":"Other project"}),
        )
        .await;
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "POST",
                &format!(
                    "/v1/projects/destination/tasks/{}/assign",
                    destination["id"].as_str().unwrap()
                ),
                json!({"target":target.target})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        state
            .db
            .lock()
            .unwrap()
            .execute("UPDATE task_target_presence SET seen_at=0", [])
            .unwrap();
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "POST",
                &format!("{path}/assign"),
                json!({"target":target.target})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        // Changed registry repository invalidates otherwise fresh presence.
        state
            .db
            .lock()
            .unwrap()
            .execute("UPDATE task_target_presence SET seen_at=?1", [db::now_ms()])
            .unwrap();
        state.db.lock().unwrap().execute("UPDATE projects SET canonical_remote='github.com/acme/replaced' WHERE ident='example'",[]).unwrap();
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "POST",
                &format!("{path}/assign"),
                json!({"target":target.target})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let (_, after) = desktop_http(&state, human_headers(), "GET", &path, Value::Null).await;
        assert_eq!(after["owner_origin"], task["owner_origin"]);
        assert_eq!(after["updated_at"], task["updated_at"]);
        assert!(after["comments"].as_array().unwrap().is_empty());
        let conn = state.db.lock().unwrap();
        assert_eq!(
            task_events::list(
                &conn,
                &task_events::EventQuery {
                    task_id: Some(task["id"].as_str().unwrap().into()),
                    ..Default::default()
                }
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[tokio::test]
    async fn presence_heartbeat_never_extends_user_or_agent_task_assignment() {
        let state = fixture(db::open(":memory:").unwrap());
        repository_fixture(&state);
        let owner = Agent::new(&id(), "unused");
        let beat = json!({"generation":owner.target.generation,"canonical_remote":"github.com/acme/example"});
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&owner),
                "POST",
                "/v1/projects/example/task-targets",
                beat.clone()
            )
            .await
            .0,
            StatusCode::OK
        );
        let (_, agent_task) = desktop_http(
            &state,
            ordinary_headers(&owner),
            "POST",
            "/v1/projects/example/tasks",
            json!({"title":"Agent assignment"}),
        )
        .await;
        let (_, human_task) = desktop_http(
            &state,
            human_headers(),
            "POST",
            "/v1/projects/example/tasks",
            json!({"title":"User assignment"}),
        )
        .await;
        let path = |task: &Value| {
            format!(
                "/v1/projects/example/tasks/{}",
                task["id"].as_str().unwrap()
            )
        };
        assert_eq!(
            desktop_http(
                &state,
                human_headers(),
                "POST",
                &format!("{}/assign", path(&human_task)),
                json!({"target":owner.target})
            )
            .await
            .0,
            StatusCode::OK
        );
        let stale = db::now_ms() - db::TASK_RECLAIM_MS - 1;
        state
            .db
            .lock()
            .unwrap()
            .execute("UPDATE tasks SET updated_at=?1", [stale])
            .unwrap();
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&owner),
                "POST",
                "/v1/projects/example/task-targets",
                beat
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            state
                .db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM tasks WHERE updated_at=?1",
                    [stale],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
        // Reassign does its normal stale sweep first; the stale owner cannot resurrect a claim.
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&owner),
                "POST",
                &format!("{}/reassign", path(&agent_task)),
                json!({"target":owner.target})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        for task in [&agent_task, &human_task] {
            let (_, detail) =
                desktop_http(&state, human_headers(), "GET", &path(task), Value::Null).await;
            assert_eq!(detail["status"], "todo");
            assert!(detail["owner_origin"].is_null());
            assert!(detail["owner_agent_id"].is_null());
        }
        {
            let conn = state.db.lock().unwrap();
            let events = task_events::list(&conn, &Default::default()).unwrap();
            assert_eq!(
                events.iter().filter(|e| e.kind == "task_unclaimed").count(),
                2
            );
        }
        // Normal claim is separate and remains available after time decay.
        assert_eq!(
            desktop_http(
                &state,
                ordinary_headers(&owner),
                "PATCH",
                &path(&agent_task),
                json!({"status":"in_progress"})
            )
            .await
            .0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn ordinary_creation_reserves_submitter_and_only_owner_can_reassign() {
        let state = fixture(db::open(":memory:").unwrap());
        let peers = agents(&state).await;
        let mut legacy = peers[0].headers();
        legacy.insert("x-agent-id", "legacy-machine-label".parse().unwrap());
        for header in [
            "x-agent-session-id",
            "x-agent-instance-id",
            "x-agent-provider",
            "x-agent-os",
        ] {
            legacy.remove(header);
        }
        let error = crate::routes::create_task_handler(
            State(state.clone()),
            legacy,
            Path("example".into()),
            Json(serde_json::from_value(json!({"title":"Missing native submitter"})).unwrap()),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        let created = ok(
            &state,
            &peers[0],
            "POST",
            "/v1/projects/example/tasks",
            json!({"title":"Reserved immediately","reporter":"misleading-display-name"}),
        )
        .await;
        let task_id = created["id"].as_str().unwrap();
        assert_eq!(created["owner_origin"], json!(peers[0].target.origin));
        assert_eq!(created["owner_agent_id"], peers[0].target.origin.session_id);
        assert_eq!(created["status"], "todo");
        assert!(created["started_at"].is_null());
        let event = task_events::list(&state.db.lock().unwrap(), &Default::default())
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            event.task["owner_origin"], created["owner_origin"],
            "the very first INSERT event must already have its owner"
        );
        let path = format!("/v1/projects/example/tasks/{task_id}");
        for peer in &peers[1..] {
            assert_eq!(
                http(
                    &state,
                    peer,
                    "PATCH",
                    &path,
                    json!({"status":"in_progress"})
                )
                .await
                .0,
                StatusCode::CONFLICT
            );
        }
        // A peer cannot take the owner's assignment; the exact current owner can hand off.
        assert_eq!(
            http(
                &state,
                &peers[2],
                "POST",
                &format!("{path}/reassign"),
                json!({"target":peers[1].target})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let moved = ok(
            &state,
            &peers[0],
            "POST",
            &format!("{path}/reassign"),
            json!({"target":peers[1].target}),
        )
        .await;
        assert_eq!(moved["owner_origin"], json!(peers[1].target.origin));
        assert_eq!(moved["status"], "todo");
        assert!(moved["started_at"].is_null());
        assert_eq!(
            http(
                &state,
                &peers[0],
                "PATCH",
                &path,
                json!({"status":"in_progress"})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let claimed = ok(
            &state,
            &peers[1],
            "PATCH",
            &path,
            json!({"status":"in_progress"}),
        )
        .await;
        assert_eq!(claimed["owner_origin"], json!(peers[1].target.origin));
        let mut disconnected = peers[0].target.clone();
        disconnected.generation = id();
        assert_eq!(
            http(
                &state,
                &peers[1],
                "POST",
                &format!("{path}/reassign"),
                json!({"target":disconnected})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let detail = ok(&state, &peers[1], "GET", &path, Value::Null).await;
        assert_eq!(detail["owner_origin"], claimed["owner_origin"]);
        assert_eq!(detail["comments"].as_array().unwrap().len(), 1);
        assert_eq!(
            detail["comments"][0]["origin"],
            json!(peers[0].target.origin)
        );
        state
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE task_sessions SET seen_at=0 WHERE session_id=?1",
                [&peers[0].target.origin.session_id],
            )
            .unwrap();
        assert_eq!(
            http(
                &state,
                &peers[1],
                "POST",
                &format!("{path}/reassign"),
                json!({"target":peers[0].target})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn ordinary_assignments_expire_once_after_fifteen_minutes_and_activity_resets_deadline() {
        let state = fixture(db::open(":memory:").unwrap());
        let peers = agents(&state).await;
        let mut tasks = Vec::new();
        for title in [
            "idle reserved todo",
            "comment activity",
            "task action",
            "idle in progress",
        ] {
            let t = ok(
                &state,
                &peers[0],
                "POST",
                "/v1/projects/example/tasks",
                json!({"title":title}),
            )
            .await;
            tasks.push(t["id"].as_str().unwrap().to_string());
        }
        ok(
            &state,
            &peers[0],
            "PATCH",
            &format!("/v1/projects/example/tasks/{}", tasks[3]),
            json!({"status":"in_progress"}),
        )
        .await;
        let action_before = db::now_ms() - 60_000;
        {
            let conn = state.db.lock().unwrap();
            conn.execute(
                "UPDATE tasks SET updated_at=?1 WHERE id=?2",
                params![db::now_ms() - db::TASK_RECLAIM_MS - 1, tasks[1]],
            )
            .unwrap();
            conn.execute(
                "UPDATE tasks SET updated_at=?1 WHERE id=?2",
                params![action_before, tasks[2]],
            )
            .unwrap();
            db::insert_comment(&conn, &tasks[1], "peer", "agent", "Still working").unwrap();
        }
        ok(
            &state,
            &peers[0],
            "PATCH",
            &format!("/v1/projects/example/tasks/{}", tasks[2]),
            json!({"title":"Action resets deadline"}),
        )
        .await;
        let conn = state.db.lock().unwrap();
        for index in [0, 3] {
            conn.execute(
                "UPDATE tasks SET updated_at=?1 WHERE id=?2",
                params![db::now_ms() - db::TASK_RECLAIM_MS - 1, tasks[index]],
            )
            .unwrap();
        }
        assert_eq!(db::reclaim_all_stale_tasks(&conn).unwrap(), 2);
        assert_eq!(db::reclaim_all_stale_tasks(&conn).unwrap(), 0);
        let expired = db::get_task_detail(&conn, "example", &tasks[0])
            .unwrap()
            .unwrap();
        assert_eq!(expired.task.status, "todo");
        assert!(expired.task.owner_origin.is_none());
        assert!(expired.task.owner_agent_id.is_none());
        assert!(expired.comments[0].content.contains("15 minutes"));
        for task in &tasks[1..3] {
            assert!(db::get_task_detail(&conn, "example", task)
                .unwrap()
                .unwrap()
                .task
                .owner_origin
                .is_some());
        }
        let events = task_events::list(&conn, &Default::default()).unwrap();
        let unclaimed: Vec<_> = events
            .iter()
            .filter(|e| e.kind == "task_unclaimed")
            .collect();
        assert_eq!(unclaimed.len(), 2);
        assert!(
            db::get_task_detail(&conn, "example", &tasks[2])
                .unwrap()
                .unwrap()
                .task
                .updated_at
                > action_before
        );
        let active_expired = db::get_task_detail(&conn, "example", &tasks[3])
            .unwrap()
            .unwrap()
            .task;
        assert_eq!(active_expired.status, "todo");
        assert!(active_expired.owner_origin.is_none());
        assert!(active_expired.started_at.is_none());
        assert!(unclaimed.iter().any(|e| e.task["id"] == tasks[0]));
        assert!(unclaimed[0].task["owner_origin"].is_null());
        assert!(unclaimed[0].comment.as_ref().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("became unclaimed"));
    }

    #[tokio::test]
    async fn three_same_machine_codex_sessions_delegate_and_claim_independently() {
        let state = fixture(db::open(":memory:").unwrap());
        let agents = agents(&state).await;
        let directory = ok(
            &state,
            &agents[0],
            "GET",
            "/v1/projects/example/task-sessions",
            Value::Null,
        )
        .await;
        assert_eq!(directory["sessions"].as_array().unwrap().len(), 3);
        let mut claimed = Vec::new();
        for n in 0..3 {
            let sender = &agents[n];
            let target = &agents[(n + 1) % 3];
            let other = &agents[(n + 2) % 3];
            let body = sender.delegation(target, "example");
            let accepted = ok(
                &state,
                sender,
                "POST",
                "/v1/projects/example/task-dispatches",
                body.clone(),
            )
            .await;
            assert_eq!(accepted["dispatch"]["state"], "pending");
            assert!(accepted["task"]["owner_origin"].is_null());
            assert!(accepted["task"]["started_at"].is_null());
            assert_eq!(
                ok(
                    &state,
                    sender,
                    "POST",
                    "/v1/projects/example/task-dispatches",
                    body.clone()
                )
                .await,
                accepted
            );
            let mut changed = body;
            changed["title"] = json!("changed");
            assert_eq!(
                http(
                    &state,
                    sender,
                    "POST",
                    "/v1/projects/example/task-dispatches",
                    changed
                )
                .await
                .0,
                StatusCode::CONFLICT
            );
            let task = accepted["task"]["id"].as_str().unwrap();
            let path = actions(task);
            assert_eq!(
                http(&state, other, "POST", &path, other.action("receive", None))
                    .await
                    .0,
                StatusCode::CONFLICT
            );
            assert_eq!(
                http(&state, target, "POST", &path, target.action("claim", None))
                    .await
                    .0,
                StatusCode::CONFLICT
            );
            assert!(recovery_for(&state, target).await["pending"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["task_id"] == task));
            let receipt = target.action("receive", None);
            let received = ok(&state, target, "POST", &path, receipt.clone()).await;
            assert_eq!(received["dispatch"]["state"], "received");
            assert!(received["task"]["owner_origin"].is_null());
            let claim = target.action("claim", None);
            let (a, b) = tokio::join!(
                http(&state, target, "POST", &path, claim.clone()),
                http(&state, target, "POST", &path, claim.clone())
            );
            assert_eq!(a.0, StatusCode::OK);
            assert_eq!(a, b);
            assert_eq!(a.1["task"]["owner_origin"], json!(target.target.origin));
            assert!(a.1["task"]["started_at"].is_null());
            assert_eq!(a.1["dispatch"]["state"], "claimed");
            // A second registration on the same machine cannot copy peer headers to act as that peer.
            let mut impersonator = other.clone();
            impersonator.target = target.target.clone();
            assert_eq!(
                http(
                    &state,
                    &impersonator,
                    "POST",
                    &path,
                    target.action("start", a.1["dispatch"]["claim_id"].as_str())
                )
                .await
                .0,
                StatusCode::FORBIDDEN
            );
            for status in ["todo", "in_progress", "done"] {
                assert_eq!(
                    http(
                        &state,
                        target,
                        "PATCH",
                        &format!("/v1/projects/example/tasks/{task}"),
                        json!({"status":status})
                    )
                    .await
                    .0,
                    StatusCode::CONFLICT
                );
            }
            assert_eq!(
                http(
                    &state,
                    target,
                    "DELETE",
                    &format!("/v1/projects/example/tasks/{task}"),
                    Value::Null
                )
                .await
                .0,
                StatusCode::CONFLICT
            );
            let claim_id = a.1["dispatch"]["claim_id"].as_str().unwrap().to_string();
            assert_eq!(
                http(
                    &state,
                    target,
                    "POST",
                    &path,
                    target.action("succeeded", Some(&claim_id))
                )
                .await
                .0,
                StatusCode::CONFLICT
            );
            let execution = id();
            let mut start = target.action("start", Some(&claim_id));
            start["execution_id"] = json!(execution);
            let started = ok(&state, target, "POST", &path, start.clone()).await;
            assert_eq!(started["dispatch"]["state"], "running");
            assert!(started["task"]["started_at"].is_i64());
            let mut finish = target.action("succeeded", Some(&claim_id));
            finish["execution_id"] = json!(execution);
            finish["result"] = json!("fixture outcome");
            let finished = ok(&state, target, "POST", &path, finish.clone()).await;
            assert_eq!(finished["task"]["status"], "done");
            assert_eq!(ok(&state, target, "POST", &path, finish).await, finished);
            // Lost acknowledgement retries return the original accepted result, never a new execution.
            assert_eq!(ok(&state, target, "POST", &path, start).await, started);
            assert_eq!(ok(&state, target, "POST", &path, receipt).await, received);
            claimed.push(claim_id);
        }
        claimed.sort();
        claimed.dedup();
        assert_eq!(claimed.len(), 3);
        let conn = state.db.lock().unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn restart_is_read_only_recovery_handoff_is_explicit_and_fenced() {
        let state = fixture(db::open(":memory:").unwrap());
        let agents = agents(&state).await;
        let (sender, target, next) = (&agents[0], &agents[1], &agents[2]);
        let accepted = ok(
            &state,
            sender,
            "POST",
            "/v1/projects/example/task-dispatches",
            sender.delegation(target, "example"),
        )
        .await;
        let task = accepted["task"]["id"].as_str().unwrap();
        let path = actions(task);
        ok(
            &state,
            target,
            "POST",
            &path,
            target.action("receive", None),
        )
        .await;
        let claim = ok(&state, target, "POST", &path, target.action("claim", None)).await;
        let claim_id = claim["dispatch"]["claim_id"].as_str().unwrap();
        let mut restarted = Agent::new(&target.target.origin.instance_id, &target.machine_key);
        restarted.target.origin = target.target.origin.clone(); // Same native conversation, new process generation and secret.
        register_agent(&state, &restarted, "example").await;
        let recovery = recovery_for(&state, &restarted).await;
        assert!(recovery["pending"].as_array().unwrap().is_empty());
        assert!(recovery["prior_claims"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["dispatch"]["claim_id"] == claim_id));
        let mut start = restarted.action("start", Some(claim_id));
        start["execution_id"] = json!(id());
        assert_eq!(
            http(&state, &restarted, "POST", &path, start).await.0,
            StatusCode::CONFLICT
        );
        // No time-based reclaim or legacy operator release may erase managed claims.
        {
            let conn = state.db.lock().unwrap();
            conn.execute(
                "UPDATE tasks SET started_at=0,updated_at=0 WHERE id=?1",
                [task],
            )
            .unwrap();
            assert_eq!(db::reclaim_stale_tasks(&conn, "example").unwrap(), 0);
            let t = db::get_task_detail(&conn, "example", task)
                .unwrap()
                .unwrap()
                .task;
            assert!(db::recover_task_claim(
                &conn,
                "example",
                task,
                &db::task_claim_token(&t).unwrap(),
                "fixture"
            )
            .is_err());
            assert!(
                db::reorder_tasks_in_column(&conn, "example", "todo", &[task.into()], None)
                    .is_err()
            );
        }
        let mut handoff = target.action("handoff", Some(claim_id));
        handoff["target"] = json!(next.target);
        handoff["result"] = json!("explicit owner handoff");
        let handed = ok(&state, target, "POST", &path, handoff.clone()).await;
        assert_eq!(handed["dispatch"]["target"], json!(next.target));
        assert!(handed["task"]["owner_origin"].is_null());
        assert_eq!(ok(&state, target, "POST", &path, handoff).await, handed);
        assert_eq!(
            http(
                &state,
                target,
                "POST",
                &path,
                target.action("receive", None)
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        ok(&state, next, "POST", &path, next.action("receive", None)).await;
        let new_claim = ok(&state, next, "POST", &path, next.action("claim", None)).await;
        assert_ne!(new_claim["dispatch"]["claim_id"], claim_id);
        let mut cancel = next.action("cancel", Some(claim_id));
        cancel["result"] = json!("stale claim");
        assert_eq!(
            http(&state, next, "POST", &path, cancel).await.0,
            StatusCode::CONFLICT
        );
        let mut cancel = next.action("cancel", new_claim["dispatch"]["claim_id"].as_str());
        cancel["result"] = json!("explicit cancellation");
        let cancelled = ok(&state, next, "POST", &path, cancel).await;
        assert_eq!(cancelled["dispatch"]["state"], "cancelled");
        assert!(cancelled["dispatch"]["started_at"].is_null());
        assert_eq!(cancelled["task"]["status"], "cancelled");
        assert!(cancelled["task"]["done_at"].is_null());
        assert_eq!(cancelled["task"]["owner_origin"], json!(next.target.origin));
    }

    #[tokio::test]
    async fn credentials_platform_eligibility_revocation_and_expiry_are_enforced() {
        let state = fixture(db::open(":memory:").unwrap());
        let agents = agents(&state).await;
        let (sender, target) = (&agents[0], &agents[1]);
        let mut fake = target.clone();
        fake.machine_key = "0".repeat(64);
        assert_eq!(
            http(
                &state,
                &fake,
                "GET",
                "/v1/projects/example/task-sessions",
                Value::Null
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        fake = target.clone();
        fake.target.origin.instance_id = id();
        assert_eq!(
            http(
                &state,
                &fake,
                "POST",
                "/v1/projects/example/task-sessions",
                json!({"generation":fake.target.generation})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        fake = target.clone();
        fake.target.origin.os = "windows".into();
        assert_eq!(
            http(
                &state,
                &fake,
                "POST",
                "/v1/projects/example/task-sessions",
                json!({"generation":fake.target.generation})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        fake = target.clone();
        fake.secret = "0".repeat(64);
        assert_eq!(
            http(
                &state,
                &fake,
                "POST",
                "/v1/projects/example/task-sessions",
                json!({"generation":fake.target.generation})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let mut body = sender.delegation(target, "example");
        body["required_os"] = json!("windows");
        assert_eq!(
            http(
                &state,
                sender,
                "POST",
                "/v1/projects/example/task-dispatches",
                body
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let instance = id();
        let key = provision(
            &state.db.lock().unwrap(),
            "destination",
            &instance,
            "windows",
        )
        .unwrap();
        let mut remote = Agent::new(&instance, &key);
        remote.target.origin.os = "windows".into();
        remote.target.origin.provider = "claude".into();
        register_agent(&state, &remote, "destination").await;
        let accepted = ok(
            &state,
            sender,
            "POST",
            "/v1/projects/example/task-dispatches",
            sender.delegation(&remote, "destination"),
        )
        .await;
        assert_eq!(accepted["dispatch"]["target"], json!(remote.target));
        assert_eq!(accepted["task"]["project_ident"], "destination");
        let task = accepted["task"]["id"].as_str().unwrap();
        // Requester cancels through its authorized source project, without impersonating destination.
        let mut cancel = sender.action("cancel", None);
        cancel["result"] = json!("withdraw delegated request");
        let withdrawn = ok(&state, sender, "POST", &actions(task), cancel).await;
        assert_eq!(withdrawn["dispatch"]["state"], "cancelled");
        assert_eq!(withdrawn["task"]["status"], "cancelled");
        let before: i64 = state
            .db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        state
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE task_sessions SET seen_at=0 WHERE session_id=?1",
                [&target.target.origin.session_id],
            )
            .unwrap();
        assert_eq!(
            http(
                &state,
                sender,
                "POST",
                "/v1/projects/example/task-dispatches",
                sender.delegation(target, "example")
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            state
                .db
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            before
        );
        revoke(
            &state.db.lock().unwrap(),
            "example",
            &sender.target.origin.instance_id,
        )
        .unwrap();
        assert_eq!(
            http(
                &state,
                sender,
                "GET",
                "/v1/projects/example/task-sessions",
                Value::Null
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn accepted_request_and_claim_history_survive_database_restart() {
        let directory = std::env::current_dir()
            .unwrap()
            .join("target/m2-fixtures")
            .join(id());
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("gateway.db");
        let state = fixture(db::open(file.to_str().unwrap()).unwrap());
        let agents = agents(&state).await;
        let body = agents[0].delegation(&agents[1], "example");
        let accepted = ok(
            &state,
            &agents[0],
            "POST",
            "/v1/projects/example/task-dispatches",
            body.clone(),
        )
        .await;
        let task = accepted["task"]["id"].as_str().unwrap();
        ok(
            &state,
            &agents[1],
            "POST",
            &actions(task),
            agents[1].action("receive", None),
        )
        .await;
        let claimed = ok(
            &state,
            &agents[1],
            "POST",
            &actions(task),
            agents[1].action("claim", None),
        )
        .await;
        drop(state);
        let state = fixture(db::open(file.to_str().unwrap()).unwrap());
        assert_eq!(
            ok(
                &state,
                &agents[0],
                "POST",
                "/v1/projects/example/task-dispatches",
                body
            )
            .await,
            accepted
        );
        let recovered = recovery_for(&state, &agents[1]).await;
        assert_eq!(
            recovered["machine_dispatches"][0]["claim_id"],
            claimed["dispatch"]["claim_id"]
        );
        assert_eq!(recovered["prior_claims"][0]["operation"], "claim");
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn rejected_transactions_rollback_and_distinct_claim_requests_race_atomically() {
        let state = fixture(db::open(":memory:").unwrap());
        let agents = agents(&state).await;
        let (sender, target) = (&agents[0], &agents[1]);
        let body = sender.delegation(target, "example");
        state.db.lock().unwrap().execute_batch("CREATE TRIGGER fixture_failure BEFORE INSERT ON task_dispatch_events BEGIN SELECT RAISE(ABORT,'fixture rollback'); END;").unwrap();
        assert_eq!(
            http(
                &state,
                sender,
                "POST",
                "/v1/projects/example/task-dispatches",
                body.clone()
            )
            .await
            .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        {
            let conn = state.db.lock().unwrap();
            for table in [
                "tasks",
                "task_events",
                "task_dispatches",
                "task_dispatch_events",
                "task_dispatch_requests",
            ] {
                assert_eq!(
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))
                        .unwrap(),
                    0
                );
            }
            assert_eq!(task_events::current_origin(&conn).unwrap(), None);
            conn.execute_batch("DROP TRIGGER fixture_failure").unwrap();
        }
        let accepted = ok(
            &state,
            sender,
            "POST",
            "/v1/projects/example/task-dispatches",
            body,
        )
        .await;
        let task = accepted["task"]["id"].as_str().unwrap();
        let path = actions(task);
        ok(
            &state,
            target,
            "POST",
            &path,
            target.action("receive", None),
        )
        .await;
        let (a, b) = tokio::join!(
            http(&state, target, "POST", &path, target.action("claim", None)),
            http(&state, target, "POST", &path, target.action("claim", None))
        );
        let mut statuses = [a.0, b.0];
        statuses.sort();
        assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
        let conn = state.db.lock().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM task_dispatch_events WHERE operation='claim'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn cancellation_request_is_not_executor_cancellation_or_an_outcome() {
        let state = fixture(db::open(":memory:").unwrap());
        let agents = agents(&state).await;
        let (sender, target) = (&agents[0], &agents[1]);
        let accepted = ok(
            &state,
            sender,
            "POST",
            "/v1/projects/example/task-dispatches",
            sender.delegation(target, "example"),
        )
        .await;
        let task = accepted["task"]["id"].as_str().unwrap();
        let path = actions(task);
        ok(
            &state,
            target,
            "POST",
            &path,
            target.action("receive", None),
        )
        .await;
        let claimed = ok(&state, target, "POST", &path, target.action("claim", None)).await;
        let claim = claimed["dispatch"]["claim_id"].as_str().unwrap();
        let execution = id();
        let mut start = target.action("start", Some(claim));
        start["execution_id"] = json!(execution);
        ok(&state, target, "POST", &path, start).await;
        let mut cancel = sender.action("cancel", Some(claim));
        cancel["result"] = json!("stop requested");
        assert_eq!(
            http(&state, sender, "POST", &path, cancel.clone()).await.0,
            StatusCode::CONFLICT
        );
        cancel["operation"] = json!("request_cancel");
        let requested = ok(&state, sender, "POST", &path, cancel).await;
        assert_eq!(requested["dispatch"]["state"], "running");
        assert!(requested["dispatch"]["finished_at"].is_null());
        assert_eq!(requested["task"]["status"], "in_progress");
        let mut confirmation = target.action("cancel", Some(claim));
        confirmation["result"] = json!("executor explicitly confirmed stopped");
        assert_eq!(
            http(&state, target, "POST", &path, confirmation.clone())
                .await
                .0,
            StatusCode::CONFLICT
        );
        confirmation["execution_id"] = json!(execution);
        let confirmed = ok(&state, target, "POST", &path, confirmation).await;
        assert_eq!(confirmed["dispatch"]["state"], "cancelled");
        assert!(confirmed["dispatch"]["finished_at"].is_i64());
        assert_eq!(confirmed["dispatch"]["cancel_reason"], "stop requested");
        assert_eq!(confirmed["task"]["status"], "cancelled");
        assert!(confirmed["task"]["done_at"].is_null());
        assert_eq!(
            confirmed["task"]["owner_origin"],
            json!(target.target.origin)
        );
        assert_eq!(
            confirmed["task"]["started_at"],
            requested["task"]["started_at"]
        );
        {
            let conn = state.db.lock().unwrap();
            let kinds = |kind: &str| -> i64 {
                conn.query_row(
                    "SELECT COUNT(*) FROM task_events WHERE kind=?1 AND task_json->>'id'=?2",
                    params![kind, task],
                    |r| r.get(0),
                )
                .unwrap()
            };
            assert_eq!(kinds("task_cancelled"), 1);
            assert_eq!(kinds("task_completed"), 0);
        }
        let auth = &confirmed["authentication"];
        let expected_hash = hash(&target.machine_key);
        assert_eq!(auth["machine"]["credential_id"], expected_hash);
        assert_eq!(
            auth["machine"]["instance_id"],
            target.target.origin.instance_id
        );
        assert_eq!(auth["session"], json!(target.target.origin));
        assert_eq!(auth["generation"], target.target.generation);
    }

    async fn windows_agent(state: &AppState) -> Agent {
        let instance = id();
        let key = provision(&state.db.lock().unwrap(), "example", &instance, "windows").unwrap();
        let mut agent = Agent::new(&instance, &key);
        agent.target.origin.os = "windows".into();
        register_agent(state, &agent, "example").await;
        agent
    }

    #[tokio::test]
    async fn ordinary_task_os_labels_gate_creation_claims_assignment_and_set_os() {
        let state = fixture(db::open(":memory:").unwrap());
        let linux = agents(&state).await;
        let windows = windows_agent(&state).await;
        let tasks = "/v1/projects/example/tasks";
        for labels in [
            json!(["os:freebsd"]),
            json!(["os:linux", "os:windows"]),
            json!(["os:linux", "os:linux"]),
            json!(["OS:linux"]),
        ] {
            let (status, body) = http(
                &state,
                &linux[0],
                "POST",
                tasks,
                json!({"title":"bad","labels":labels}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{labels} {body}");
        }

        // A submitter on another OS leaves the task unassigned TODO.
        let wrong = ok(
            &state,
            &linux[0],
            "POST",
            tasks,
            json!({"title":"Windows only","labels":["autoreview-finding","os:windows"]}),
        )
        .await;
        assert!(wrong["owner_origin"].is_null() && wrong["owner_agent_id"].is_null());
        assert_eq!(wrong["status"], "todo");
        assert_eq!(wrong["labels"], json!(["autoreview-finding", "os:windows"]));
        let wrong_path = format!("{tasks}/{}", wrong["id"].as_str().unwrap());
        assert_eq!(
            http(
                &state,
                &linux[1],
                "PATCH",
                &wrong_path,
                json!({"status":"in_progress"})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let claimed = ok(
            &state,
            &windows,
            "PATCH",
            &wrong_path,
            json!({"status":"in_progress"}),
        )
        .await;
        assert_eq!(claimed["owner_origin"], json!(windows.target.origin));

        // Explicit routing and reassignment refuse an incompatible target.
        assert_eq!(http(&state, &linux[0], "POST", tasks,
            json!({"title":"routed","labels":["os:windows"],"routing":{"mode":"session","target":linux[1].target}})).await.0,
            StatusCode::CONFLICT);
        let owned = ok(
            &state,
            &linux[0],
            "POST",
            tasks,
            json!({"title":"Linux","labels":["os:linux"]}),
        )
        .await;
        assert_eq!(owned["owner_origin"], json!(linux[0].target.origin));
        let path = format!("{tasks}/{}", owned["id"].as_str().unwrap());
        assert_eq!(
            http(
                &state,
                &linux[0],
                "POST",
                &format!("{path}/reassign"),
                json!({"target":windows.target})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let moved = ok(
            &state,
            &linux[0],
            "POST",
            &format!("{path}/reassign"),
            json!({"target":linux[1].target}),
        )
        .await;
        assert_eq!(moved["owner_origin"], json!(linux[1].target.origin));
        assert_eq!(
            moved["routing"],
            json!({"mode":"session","target":linux[1].target}),
            "reassign replaces the stale route"
        );
        ok(
            &state,
            &linux[1],
            "PATCH",
            &path,
            json!({"status":"in_progress"}),
        )
        .await;

        // Only the exact owner may change OS, never combined with status/owner changes.
        assert_eq!(
            http(
                &state,
                &linux[2],
                "PATCH",
                &path,
                json!({"labels":["os:windows"]})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            http(
                &state,
                &linux[1],
                "PATCH",
                &path,
                json!({"labels":["os:windows"],"status":"todo"})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            http(
                &state,
                &linux[1],
                "PATCH",
                &path,
                json!({"labels":["os:mac"]})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        // Same-OS label edits keep the claim.
        let kept = ok(
            &state,
            &linux[1],
            "PATCH",
            &path,
            json!({"labels":["os:linux","keep"]}),
        )
        .await;
        assert_eq!(kept["status"], "in_progress");
        let released = ok(
            &state,
            &linux[1],
            "PATCH",
            &path,
            json!({"labels":["keep","os:windows"]}),
        )
        .await;
        assert_eq!(released["status"], "todo");
        assert!(
            released["owner_origin"].is_null()
                && released["owner_agent_id"].is_null()
                && released["started_at"].is_null()
        );
        assert_eq!(released["routing"], json!({"mode":"auto"}));
        assert_eq!(released["labels"], json!(["keep", "os:windows"]));
        assert_eq!(
            http(
                &state,
                &linux[1],
                "PATCH",
                &path,
                json!({"status":"in_progress"})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        ok(
            &state,
            &windows,
            "PATCH",
            &path,
            json!({"status":"in_progress"}),
        )
        .await;
        // Removing the restriction never releases a compatible owner.
        assert_eq!(
            ok(&state, &windows, "PATCH", &path, json!({"labels":["keep"]})).await["status"],
            "in_progress"
        );

        // User review and closed tasks stay protected from agent OS changes.
        ok(
            &state,
            &windows,
            "PATCH",
            &wrong_path,
            json!({"status":"user_review"}),
        )
        .await;
        assert_eq!(
            http(
                &state,
                &windows,
                "PATCH",
                &wrong_path,
                json!({"labels":["os:linux"]})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        ok(&state, &windows, "PATCH", &path, json!({"status":"done"})).await;
        assert_eq!(
            http(
                &state,
                &windows,
                "PATCH",
                &path,
                json!({"labels":["os:linux"]})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
    }
}
