use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::task::spawn_blocking;
use tracing::error;

use crate::{
    channel::OutboundMessage,
    db::{self, now_ms, Message, Project},
    projects::sanitize_ident,
    AppState,
};

// ── Error helper ─────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct AppError(pub StatusCode, pub String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        error!("handler error: {} — {}", self.0, self.1);
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        let err = e.into();
        AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
    }
}

type Result<T> = std::result::Result<T, AppError>;

const EVENTIC_SERVERS_SETTING: &str = "eventic.servers";

// ── Artifact operations envelope accessor (T016 plumbing) ───────────────────
//
// T016 owns the typed runtime surface for the T004 operations envelope. The
// envelope is parsed from `std::env` exactly once per process via a
// `OnceLock`; route handlers MUST call `operations_envelope()` instead of
// reading env vars or hardcoding T004 constants. T007 will migrate this
// accessor's caller-shape into `AppState` once the envelope acquires
// per-project overrides; until then, a process-wide cache is the right
// scope (the env values are immutable across a single binary lifetime).
//
// Failures during load short-circuit to production defaults AND log a
// warning. We deliberately do NOT panic on misconfigured env values at
// load time here — that decision belongs to a future startup gate in
// `main.rs` (out of touch surface for T016). T007 SHOULD wire
// `ArtifactOperationsEnvelope::from_env()?` into the explicit startup
// path so SRE catches misconfiguration before traffic lands.

static ARTIFACT_OPERATIONS_ENVELOPE: std::sync::OnceLock<db::ArtifactOperationsEnvelope> =
    std::sync::OnceLock::new();

/// Returns the process-wide artifact operations envelope. First call loads
/// from environment variables; subsequent calls return the cached value.
/// On parse failure the call logs a warning and returns
/// `ArtifactOperationsEnvelope::production_defaults()` so the gateway
/// continues to serve with documented T004 defaults. Startup code (future
/// T007 work) SHOULD call `db::ArtifactOperationsEnvelope::from_env()`
/// directly to surface configuration errors explicitly.
#[allow(dead_code)]
pub(crate) fn operations_envelope() -> &'static db::ArtifactOperationsEnvelope {
    ARTIFACT_OPERATIONS_ENVELOPE.get_or_init(|| {
        match db::ArtifactOperationsEnvelope::from_env() {
            Ok(envelope) => envelope,
            Err(err) => {
                tracing::warn!(
                    "artifact operations envelope failed to load from env, falling back to T004 defaults: {err}"
                );
                db::ArtifactOperationsEnvelope::production_defaults()
            }
        }
    })
}

/// Extract agent identity from X-Agent-Id header, defaulting to "_default".
fn extract_agent_id(headers: &HeaderMap) -> String {
    headers
        .get("x-agent-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("_default")
        .to_string()
}

/// Maximum length of an auto-derived subject when the agent does not supply one.
const AUTO_SUBJECT_MAX: usize = 80;

/// Derive a subject from the body when one is not supplied: first non-empty
/// line, trimmed, capped at `AUTO_SUBJECT_MAX` characters with an ellipsis if
/// truncated. Falls back to a generic placeholder for empty bodies.
fn derive_subject(body: &str) -> String {
    let first_line = body.lines().map(str::trim).find(|l| !l.is_empty());
    match first_line {
        None => "(no content)".to_string(),
        Some(line) => {
            let count = line.chars().count();
            if count <= AUTO_SUBJECT_MAX {
                line.to_string()
            } else {
                let mut out: String = line.chars().take(AUTO_SUBJECT_MAX - 1).collect();
                out.push('…');
                out
            }
        }
    }
}

/// Apply default values for any missing structured fields and return a
/// fully-populated `OutboundMessage`. The body argument is the resolved
/// payload (caller picks between the structured `body` field and any
/// route-specific alias such as `content` or `message`).
fn build_outbound(
    agent_id: &str,
    body: String,
    subject: Option<String>,
    hostname: Option<String>,
    event_at: Option<i64>,
) -> OutboundMessage {
    let subject = subject
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| derive_subject(&body));
    let hostname = hostname
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| agent_id.to_string());
    let event_at = event_at.unwrap_or_else(now_ms);
    OutboundMessage {
        agent_id: agent_id.to_string(),
        hostname,
        subject,
        body,
        event_at,
    }
}

// -- Generic artifacts (/v1/projects/:ident/artifacts) -----------------------

#[derive(Debug, Clone)]
struct ArtifactActorHeaders {
    actor_type: String,
    agent_system: Option<String>,
    agent_id: String,
    host: Option<String>,
}

#[derive(Debug, Clone)]
struct ArtifactMutationContext {
    actor: ArtifactActorHeaders,
    idempotency_key: String,
    workflow_run_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ArtifactActorResponse {
    actor_id: String,
    actor_type: String,
    agent_system: Option<String>,
    agent_id: Option<String>,
    host: Option<String>,
    display_name: String,
}

#[derive(Debug, Serialize)]
struct ArtifactAuthorization {
    boundary: &'static str,
    required_scopes: Vec<&'static str>,
}

#[derive(Debug, Clone)]
struct ArtifactAuthorizationDecision {
    boundary: &'static str,
    required_scopes: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct ArtifactProvenance {
    actor: ArtifactActorResponse,
    workflow_run_id: Option<String>,
    idempotency_key: String,
    request_id: String,
    created_at: i64,
    authorization: ArtifactAuthorization,
    generated_resources: Value,
    replay: bool,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ArtifactMutationResponse<T: Serialize> {
    data: T,
    provenance: ArtifactProvenance,
}

#[derive(Debug, Serialize)]
pub struct ArtifactReadResponse<T: Serialize> {
    data: T,
    chunking_status: ChunkingStatus,
}

#[derive(Debug, Serialize)]
pub struct ArtifactDetailResponse {
    artifact: db::ArtifactSummary,
    current_version: Option<VersionReadModel>,
    accepted_version: Option<VersionReadModel>,
}

#[derive(Debug, Serialize)]
pub struct VersionReadModel {
    #[serde(flatten)]
    version: db::ArtifactVersion,
    chunking_status: ChunkingStatus,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChunkingStatus {
    status: &'static str,
    current_chunk_count: usize,
    stale_chunk_count: usize,
    superseded_chunk_count: usize,
    failed_addresses: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ArtifactDiffResponse {
    from_version_id: Option<String>,
    to_version_id: String,
    format: &'static str,
    byte_delta: isize,
    diff: String,
    chunking_status: ChunkingStatus,
}

#[derive(Debug, Deserialize)]
pub struct ListArtifactsQuery {
    kind: Option<String>,
    subkind: Option<String>,
    lifecycle_state: Option<String>,
    label: Option<String>,
    actor_id: Option<String>,
    q: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateArtifactRequest {
    kind: String,
    subkind: Option<String>,
    title: String,
    labels: Option<Vec<String>>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateArtifactVersionRequest {
    version_label: Option<String>,
    parent_version_id: Option<String>,
    body_format: Option<String>,
    body: Option<String>,
    structured_payload: Option<Value>,
    source_format: Option<String>,
    version_state: Option<String>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DiffQuery {
    base_version_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateContributionRequest {
    target_kind: String,
    target_id: String,
    contribution_kind: String,
    phase: Option<String>,
    role: String,
    read_set: Option<Value>,
    body_format: Option<String>,
    body: String,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateCommentRequest {
    target_kind: String,
    target_id: String,
    child_address: Option<String>,
    parent_comment_id: Option<String>,
    body: String,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResolveCommentRequest {
    resolution_note: Option<String>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReopenCommentRequest {
    note_body: Option<String>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListLinksQuery {
    link_type: Option<String>,
    source_kind: Option<String>,
    source_id: Option<String>,
    target_kind: Option<String>,
    target_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateLinkRequest {
    link_type: String,
    source_kind: String,
    source_id: String,
    source_version_id: Option<String>,
    source_child_address: Option<String>,
    target_kind: String,
    target_id: String,
    target_version_id: Option<String>,
    target_child_address: Option<String>,
    supersedes_link_id: Option<String>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StartWorkflowRunRequest {
    workflow_kind: String,
    phase: Option<String>,
    round_id: Option<String>,
    participant_actor_ids: Option<Vec<String>>,
    source_artifact_version_id: Option<String>,
    read_set: Option<Value>,
    is_resumable: Option<bool>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CompleteWorkflowRunRequest {
    state: String,
    failure_reason: Option<String>,
    generated_contribution_ids: Option<Vec<String>>,
    generated_version_ids: Option<Vec<String>>,
    generated_task_ids: Option<Vec<String>>,
    generated_link_ids: Option<Vec<String>>,
    generated_chunk_ids: Option<Vec<String>>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SpecImportRequest {
    title: String,
    labels: Option<Vec<String>>,
    body: Option<String>,
    manifest: Value,
    file_bodies: Option<std::collections::HashMap<String, String>>,
    source_doc: Option<String>,
    source_artifact_id: Option<String>,
    source_artifact_version_id: Option<String>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SpecVersionRequest {
    version_label: Option<String>,
    parent_version_id: Option<String>,
    body: Option<String>,
    manifest: Value,
    file_bodies: Option<std::collections::HashMap<String, String>>,
    source_doc: Option<String>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SpecAcceptRequest {
    version_id: String,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SpecManifestQuery {
    version_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GenerateSpecTasksRequest {
    confirmed: bool,
    manifest_item_ids: Option<Vec<String>>,
    reporter: Option<String>,
    hostname: Option<String>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LinkSpecTaskRequest {
    version_id: Option<String>,
    manifest_item_id: String,
    task_id: String,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DesignReviewCreateRequest {
    title: String,
    labels: Option<Vec<String>>,
    body: Option<String>,
    body_format: Option<String>,
    source_artifact_id: Option<String>,
    source_artifact_version_id: Option<String>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DesignReviewRoundRequest {
    round_id: Option<String>,
    participant_actor_ids: Option<Vec<String>>,
    source_artifact_version_id: String,
    read_set: Option<Value>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DesignReviewContributionRequest {
    phase: String,
    role: Option<String>,
    reviewed_version_id: Option<String>,
    read_set: Option<Value>,
    body_format: Option<String>,
    body: String,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DesignReviewSynthesisRequest {
    reviewed_version_id: Option<String>,
    read_set: Value,
    body_format: Option<String>,
    body: String,
    create_version: Option<bool>,
    version_label: Option<String>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DesignReviewStateRequest {
    lifecycle_state: Option<String>,
    review_state: Option<String>,
    note: Option<String>,
    actor_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListDesignReviewContributionsQuery {
    round_id: Option<String>,
    phase: Option<String>,
    role: Option<String>,
    reviewed_version_id: Option<String>,
    read_set_contains: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ArtifactWorkspaceQuery {
    kind: Option<String>,
    status: Option<String>,
    label: Option<String>,
    actor: Option<String>,
    q: Option<String>,
    chunk_q: Option<String>,
    include_history: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SpecManifestItem {
    manifest_item_id: String,
    phase: Option<String>,
    task_code: Option<String>,
    team: Option<String>,
    title: String,
    status: Option<String>,
    dependencies: Vec<String>,
    labels: Vec<String>,
    touch_surface: Vec<String>,
    acceptance_criteria: Vec<String>,
    validation_plan: Vec<String>,
    gateway_task_id: Option<String>,
    spec_file: Option<String>,
    spec_body: Option<String>,
    metadata: Value,
}

#[derive(Debug, Serialize)]
pub struct SpecManifestResponse {
    artifact_id: String,
    artifact_version_id: String,
    manifest: Value,
    items: Vec<SpecManifestItem>,
    stability_policy: Value,
}

#[derive(Debug, Serialize)]
pub struct GenerateSpecTasksResponse {
    artifact_id: String,
    artifact_version_id: String,
    workflow_run_id: String,
    generated_task_ids: Vec<String>,
    generated_link_ids: Vec<String>,
    items: Vec<Value>,
    replayed: bool,
}

fn parse_scope_header(headers: &HeaderMap) -> std::collections::BTreeSet<String> {
    headers
        .get("x-agent-scopes")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect()
}

fn scope_grants(held: &std::collections::BTreeSet<String>, required: &str) -> bool {
    held.contains("*")
        || held.contains(required)
        || required
            .split_once('.')
            .is_some_and(|(prefix, _)| held.contains(&format!("{prefix}.*")))
}

fn require_artifact_authorization(
    state: &AppState,
    headers: &HeaderMap,
    project_ident: &str,
    required_scopes: Vec<&'static str>,
) -> Result<ArtifactAuthorizationDecision> {
    if !state.artifact_auth_enforced {
        return Ok(ArtifactAuthorizationDecision {
            boundary: "trusted-single-tenant",
            required_scopes,
        });
    }

    let authorized_project = headers
        .get("x-agent-project")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    if authorized_project != Some(project_ident) {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            "artifact_authorization_forbidden: missing or mismatched x-agent-project".to_string(),
        ));
    }

    let held_scopes = parse_scope_header(headers);
    let missing_scopes: Vec<&'static str> = required_scopes
        .iter()
        .copied()
        .filter(|scope| !scope_grants(&held_scopes, scope))
        .collect();
    if !missing_scopes.is_empty() {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            format!(
                "artifact_authorization_forbidden: missing scopes {}",
                missing_scopes.join(",")
            ),
        ));
    }

    Ok(ArtifactAuthorizationDecision {
        boundary: "project-scoped",
        required_scopes,
    })
}

fn require_artifact_read(
    state: &AppState,
    headers: &HeaderMap,
    project_ident: &str,
) -> Result<ArtifactAuthorizationDecision> {
    require_artifact_authorization(state, headers, project_ident, vec!["artifact.read"])
}

fn require_quota_override_authorization(
    state: &AppState,
    headers: &HeaderMap,
    project_ident: &str,
) -> Result<()> {
    let requested = headers
        .get("x-artifact-quota-override")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"));
    if requested {
        require_artifact_authorization(state, headers, project_ident, vec!["project.administer"])?;
    }
    Ok(())
}

fn artifact_body_schema_allowed(
    state: &AppState,
    body_format: &str,
    payload: Option<&Value>,
) -> Result<()> {
    if state.artifact_body_schema_enabled || (body_format == "markdown" && payload.is_none()) {
        Ok(())
    } else {
        Err(AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "artifact_body_schema_disabled".to_string(),
        ))
    }
}

fn parse_artifact_actor_headers(
    headers: &HeaderMap,
    mutation: bool,
) -> Result<ArtifactActorHeaders> {
    let agent_id = headers
        .get("x-agent-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    if mutation && agent_id.as_deref().unwrap_or("_default") == "_default" {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "x_agent_id_required".to_string(),
        ));
    }

    let actor_type = headers
        .get("x-actor-type")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("agent")
        .to_ascii_lowercase();
    if !matches!(actor_type.as_str(), "user" | "agent" | "system") {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "invalid_x_actor_type".to_string(),
        ));
    }

    let agent_system = headers
        .get("x-agent-system")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_ascii_lowercase());
    if actor_type == "agent" {
        match agent_system.as_deref() {
            Some("claude" | "codex" | "gemini" | "other") => {}
            Some(_) => {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    "invalid_x_agent_system".to_string(),
                ));
            }
            None if mutation => {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    "x_agent_system_required".to_string(),
                ));
            }
            None => {}
        }
    }

    Ok(ArtifactActorHeaders {
        actor_type,
        agent_system,
        agent_id: agent_id.unwrap_or_else(|| "_default".to_string()),
        host: headers
            .get("x-host")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string),
    })
}

fn parse_mutation_context(headers: &HeaderMap) -> Result<ArtifactMutationContext> {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            AppError(
                StatusCode::BAD_REQUEST,
                "idempotency_key_required".to_string(),
            )
        })?
        .to_string();
    if key.len() > 255 {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "idempotency_key_too_long".to_string(),
        ));
    }

    Ok(ArtifactMutationContext {
        actor: parse_artifact_actor_headers(headers, true)?,
        idempotency_key: key,
        workflow_run_id: headers
            .get("x-workflow-run-id")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string),
    })
}

fn trim_required(value: &str, field: &'static str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("{field}_required"),
        ))
    } else {
        Ok(trimmed.to_string())
    }
}

fn validate_artifact_kind(kind: &str) -> Result<()> {
    if matches!(kind, "design_review" | "spec" | "documentation") {
        Ok(())
    } else {
        Err(AppError(
            StatusCode::BAD_REQUEST,
            "invalid_artifact_kind".to_string(),
        ))
    }
}

fn validate_body_format(format: &str) -> Result<()> {
    if matches!(
        format,
        "markdown" | "application/agent-context+json" | "openapi" | "swagger"
    ) {
        Ok(())
    } else {
        Err(AppError(
            StatusCode::BAD_REQUEST,
            "invalid_body_format".to_string(),
        ))
    }
}

fn validate_read_set_required(
    phase: Option<&str>,
    kind: &str,
    read_set: Option<&Value>,
) -> Result<()> {
    if read_set.is_none() && (matches!(phase, Some("pass_2" | "synthesis")) || kind == "synthesis")
    {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "read_set_required".to_string(),
        ));
    }
    Ok(())
}

fn resolve_artifact_actor(
    conn: &rusqlite::Connection,
    headers: &ArtifactActorHeaders,
    display_name: Option<&str>,
) -> anyhow::Result<db::ArtifactActor> {
    let display = display_name
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(&headers.agent_id);
    db::resolve_artifact_actor(
        conn,
        &db::ArtifactActorIdentity {
            actor_type: &headers.actor_type,
            agent_system: headers.agent_system.as_deref(),
            agent_system_label: None,
            agent_id: Some(&headers.agent_id),
            host: headers.host.as_deref(),
            display_name: display,
            runtime_metadata: None,
        },
    )
}

fn map_db_error(err: anyhow::Error) -> AppError {
    if let Some(ops) = err.downcast_ref::<db::OperationsError>() {
        return match ops {
            db::OperationsError::SizeLimit { kind, .. } => {
                let token = kind.token();
                let code = match *kind {
                    db::SizeLimitKind::ArtifactLabelsCount
                    | db::SizeLimitKind::ArtifactLabelBytes
                    | db::SizeLimitKind::ReadSetRefs => token.to_string(),
                    _ => format!("{token}_too_large"),
                };
                let status = match *kind {
                    db::SizeLimitKind::ArtifactLabelsCount
                    | db::SizeLimitKind::ArtifactLabelBytes
                    | db::SizeLimitKind::ReadSetRefs => StatusCode::BAD_REQUEST,
                    _ => StatusCode::PAYLOAD_TOO_LARGE,
                };
                AppError(status, code)
            }
            db::OperationsError::QuotaHardReject { counter, .. } => AppError(
                StatusCode::TOO_MANY_REQUESTS,
                format!("quota_{}_exceeded", counter.token()),
            ),
            db::OperationsError::InvalidEnvValue { .. }
            | db::OperationsError::InvalidQuotaThresholds { .. } => {
                AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
            }
        };
    }

    let text = format!("{err:#}");
    let lower = text.to_ascii_lowercase();
    if lower.contains("not found") {
        AppError(StatusCode::NOT_FOUND, text)
    } else if lower.contains("required")
        || lower.contains("invalid")
        || lower.contains("missing")
        || lower.contains("does not belong")
        || lower.contains("cannot")
        || lower.contains("unsupported")
        || lower.contains("not retryable")
        || lower.contains("check constraint failed")
    {
        AppError(StatusCode::BAD_REQUEST, text)
    } else {
        AppError(StatusCode::INTERNAL_SERVER_ERROR, text)
    }
}

fn generated_resource(key: &str, value: impl Into<Value>) -> Value {
    serde_json::json!({ key: value.into() })
}

fn generated_workflow_resources(run: &db::WorkflowRun) -> Value {
    serde_json::json!({
        "workflow_run_id": run.workflow_run_id,
        "state": run.state,
        "generated_contribution_ids": run.generated_contribution_ids,
        "generated_version_ids": run.generated_version_ids,
        "generated_task_ids": run.generated_task_ids,
        "generated_link_ids": run.generated_link_ids,
        "generated_chunk_ids": run.generated_chunk_ids,
    })
}

fn build_artifact_provenance(
    actor: db::ArtifactActor,
    context: &ArtifactMutationContext,
    authorization: ArtifactAuthorizationDecision,
    generated_resources: Value,
    warnings: Vec<db::QuotaWarning>,
    replay: bool,
) -> ArtifactProvenance {
    ArtifactProvenance {
        actor: ArtifactActorResponse {
            actor_id: actor.actor_id,
            actor_type: actor.actor_type,
            agent_system: actor.agent_system,
            agent_id: actor.agent_id,
            host: actor.host,
            display_name: actor.display_name,
        },
        workflow_run_id: context.workflow_run_id.clone(),
        idempotency_key: context.idempotency_key.clone(),
        request_id: uuid::Uuid::now_v7().to_string(),
        created_at: db::now_ms(),
        authorization: ArtifactAuthorization {
            boundary: authorization.boundary,
            required_scopes: authorization.required_scopes,
        },
        generated_resources,
        replay,
        warnings: warnings.into_iter().map(|w| w.token()).collect(),
    }
}

fn mutation_response<T: Serialize>(
    status: StatusCode,
    data: T,
    provenance: ArtifactProvenance,
) -> Response {
    (status, Json(ArtifactMutationResponse { data, provenance })).into_response()
}

fn status_for_replay(replayed: bool) -> StatusCode {
    if replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    }
}

/// Canonical metric `result` label for create-style artifact mutations.
/// Replayed inserts (idempotency-key hits) report `"replayed"`; first-time
/// inserts report `"created"`. Centralized so every handler reports the same
/// label values to the operations pipeline.
fn result_label(replayed: bool) -> &'static str {
    if replayed {
        "replayed"
    } else {
        "created"
    }
}

fn emit_artifact_metric(name: &str, labels: &[(&str, &str)]) {
    tracing::info!(target: "gateway_metrics", metric = name, labels = ?labels, "artifact metric");
}

/// Common entry validation for every artifact mutation route handler:
/// parse the mandatory mutation envelope (actor headers + idempotency-key,
/// plus optional workflow-run id).
/// Returns the parsed [`ArtifactMutationContext`] on success.
///
/// Endpoint-specific request validation (`trim_required`, body-format checks,
/// read-set requirements, etc.) intentionally stays in the handler so this
/// helper remains a stable mutation envelope rather than a generic abstraction.
fn begin_artifact_mutation(
    state: &AppState,
    headers: &HeaderMap,
    project_ident: &str,
    scopes: Vec<&'static str>,
) -> Result<(ArtifactMutationContext, ArtifactAuthorizationDecision)> {
    let authorization = require_artifact_authorization(state, headers, project_ident, scopes)?;
    require_quota_override_authorization(state, headers, project_ident)?;
    Ok((parse_mutation_context(headers)?, authorization))
}

/// Build the standard mutation response envelope: status (replay-aware),
/// provenance (actor + idempotency-key + generated_resources + warnings +
/// authorization scopes), and JSON payload. Every artifact mutation handler
/// funnels its happy-path response through this helper so provenance fields,
/// replay status semantics, and the response shape stay consistent.
fn finalize_mutation<T: Serialize>(
    actor: db::ArtifactActor,
    context: &ArtifactMutationContext,
    authorization: ArtifactAuthorizationDecision,
    generated_resources: Value,
    warnings: Vec<db::QuotaWarning>,
    replayed: bool,
    data: T,
) -> Response {
    let provenance = build_artifact_provenance(
        actor,
        context,
        authorization,
        generated_resources,
        warnings,
        replayed,
    );
    mutation_response(status_for_replay(replayed), data, provenance)
}

/// Build the response envelope for mutations whose status is always `200 OK`
/// and which do not surface idempotent replay (e.g. comment resolve/reopen,
/// workflow-run complete). Keeps the provenance construction signature uniform
/// with [`finalize_mutation`] for handlers that branch on replay state.
fn finalize_ok_mutation<T: Serialize>(
    actor: db::ArtifactActor,
    context: &ArtifactMutationContext,
    authorization: ArtifactAuthorizationDecision,
    generated_resources: Value,
    warnings: Vec<db::QuotaWarning>,
    data: T,
) -> Response {
    let provenance = build_artifact_provenance(
        actor,
        context,
        authorization,
        generated_resources,
        warnings,
        false,
    );
    mutation_response(StatusCode::OK, data, provenance)
}

fn chunking_status_for(
    artifact: &db::ArtifactSummary,
    chunks: &[db::ArtifactChunk],
) -> ChunkingStatus {
    let freshness_version = artifact
        .accepted_version_id
        .as_deref()
        .or(artifact.current_version_id.as_deref());
    let mut current = 0usize;
    let mut stale = 0usize;
    let mut superseded = 0usize;
    let mut failed_addresses = Vec::new();
    for chunk in chunks {
        if chunk.superseded_by_chunk_id.is_some() {
            superseded += 1;
        } else if Some(chunk.artifact_version_id.as_str()) == freshness_version {
            current += 1;
        } else {
            stale += 1;
        }
        if let Some(Value::Object(metadata)) = &chunk.metadata {
            let failed = metadata
                .get("status")
                .or_else(|| metadata.get("chunking_status"))
                .and_then(Value::as_str)
                == Some("failed");
            if failed {
                failed_addresses.push(chunk.child_address.clone());
            }
        }
    }
    let status = if !failed_addresses.is_empty() {
        "partial"
    } else if stale > 0 {
        "stale"
    } else if current > 0 {
        "current"
    } else {
        "none"
    };
    ChunkingStatus {
        status,
        current_chunk_count: current,
        stale_chunk_count: stale,
        superseded_chunk_count: superseded,
        failed_addresses,
    }
}

fn version_status(
    artifact: &db::ArtifactSummary,
    version: db::ArtifactVersion,
    chunks: &[db::ArtifactChunk],
) -> VersionReadModel {
    let selected: Vec<_> = chunks
        .iter()
        .filter(|chunk| chunk.artifact_version_id == version.artifact_version_id)
        .cloned()
        .collect();
    VersionReadModel {
        version,
        chunking_status: chunking_status_for(artifact, &selected),
    }
}

/// Shared retryability gate for any artifact mutation that references an existing
/// `workflow_run`. Cancelled runs and non-resumable failed runs cannot be reused;
/// every other state (running, succeeded, resumable-failed) is allowed to proceed
/// — replay/idempotency-key handling at the DB layer still owns terminal-state
/// dedup semantics. Centralizing this here keeps artifact-scoped and project-
/// scoped (link) handlers from drifting on these error messages, which
/// `map_db_error` matches against to produce `BAD_REQUEST`.
fn ensure_workflow_run_retryable(run: &db::WorkflowRun) -> anyhow::Result<()> {
    match run.state.as_str() {
        "cancelled" => anyhow::bail!("cancelled workflow_run is not retryable"),
        "failed" if !run.is_resumable => {
            anyhow::bail!("non-resumable failed workflow_run is not retryable")
        }
        _ => Ok(()),
    }
}

/// Load and validate a workflow run that must belong to a specific artifact.
/// Returns `Ok(None)` when no run id is supplied, `Ok(Some(run))` for a
/// retryable run, and `Err` for not-found, cross-artifact, or non-retryable
/// state.
fn validate_workflow_run_for_mutation(
    conn: &rusqlite::Connection,
    artifact_id: &str,
    workflow_run_id: Option<&str>,
) -> anyhow::Result<Option<db::WorkflowRun>> {
    let Some(run_id) = workflow_run_id else {
        return Ok(None);
    };
    let run = db::get_workflow_run(conn, run_id)?
        .ok_or_else(|| anyhow::anyhow!("workflow_run not found"))?;
    if run.artifact_id != artifact_id {
        anyhow::bail!("workflow_run does not belong to artifact");
    }
    ensure_workflow_run_retryable(&run)?;
    Ok(Some(run))
}

/// Project-scoped workflow run lookup for link creation. Unlike
/// [`validate_workflow_run_for_mutation`], this does not bind the run to a
/// specific artifact — links are project-scoped and may reference resources
/// across artifacts — but it still applies the canonical retryability gate so
/// link creation cannot resurrect cancelled or non-resumable-failed runs.
fn load_workflow_run_for_link(
    conn: &rusqlite::Connection,
    workflow_run_id: Option<&str>,
) -> anyhow::Result<Option<db::WorkflowRun>> {
    let Some(run_id) = workflow_run_id else {
        return Ok(None);
    };
    let run = db::get_workflow_run(conn, run_id)?
        .ok_or_else(|| anyhow::anyhow!("workflow_run not found"))?;
    ensure_workflow_run_retryable(&run)?;
    Ok(Some(run))
}

fn complete_resumed_run(
    conn: &rusqlite::Connection,
    run: Option<&db::WorkflowRun>,
    contribution_id: Option<String>,
    version_id: Option<String>,
    link_id: Option<String>,
) -> anyhow::Result<()> {
    let Some(run) = run else {
        return Ok(());
    };
    if !(run.state == "failed" && run.is_resumable) {
        return Ok(());
    }

    let mut contribution_ids = run.generated_contribution_ids.clone();
    let mut version_ids = run.generated_version_ids.clone();
    let mut link_ids = run.generated_link_ids.clone();
    if let Some(id) = contribution_id {
        if !contribution_ids.contains(&id) {
            contribution_ids.push(id);
        }
    }
    if let Some(id) = version_id {
        if !version_ids.contains(&id) {
            version_ids.push(id);
        }
    }
    if let Some(id) = link_id {
        if !link_ids.contains(&id) {
            link_ids.push(id);
        }
    }
    db::update_workflow_run(
        conn,
        &run.workflow_run_id,
        &db::WorkflowRunUpdate {
            state: Some("succeeded"),
            failure_reason: Some(None),
            generated_contribution_ids: Some(&contribution_ids),
            generated_version_ids: Some(&version_ids),
            generated_task_ids: Some(&run.generated_task_ids),
            generated_link_ids: Some(&link_ids),
            generated_chunk_ids: Some(&run.generated_chunk_ids),
            ended_at: None,
        },
    )?;
    Ok(())
}

fn spec_item_stability_policy() -> Value {
    serde_json::json!({
        "stable_id_field": "manifest_item_id",
        "fallback_heuristic": "phase_id plus task id/code from source-adjacent manifests",
        "unchanged": "preserve manifest_item_id",
        "renamed": "preserve manifest_item_id when the task id/code is unchanged",
        "split": "keep the original manifest_item_id for the continuing item and issue new ids for split-out work",
        "merged": "issue a new manifest_item_id unless the manifest explicitly declares the surviving id",
        "deleted": "do not delete existing gateway task or artifact links; future generations simply omit the item",
        "collision": "explicit duplicate manifest_item_id values are rejected"
    })
}

fn string_array_field(value: &Value, keys: &[&str]) -> Vec<String> {
    for key in keys {
        if let Some(values) = value.get(*key).and_then(Value::as_array) {
            return values
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .collect();
        }
        if let Some(value) = value.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return vec![trimmed.to_string()];
            }
        }
    }
    Vec::new()
}

fn optional_string_field_value(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    })
}

fn manifest_item_id(phase_id: Option<&str>, item: &Value) -> Option<String> {
    optional_string_field_value(item, &["manifest_item_id", "stable_id"]).or_else(|| {
        optional_string_field_value(item, &["id", "task_code", "code"])
            .map(|id| phase_id.map(|phase| format!("{phase}:{id}")).unwrap_or(id))
    })
}

fn normalize_spec_manifest(
    manifest: &Value,
    file_bodies: Option<&std::collections::HashMap<String, String>>,
) -> Result<Vec<SpecManifestItem>> {
    let mut items = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    if let Some(raw_items) = manifest.get("items").and_then(Value::as_array) {
        for item in raw_items {
            let manifest_item_id = manifest_item_id(None, item).ok_or_else(|| {
                AppError(
                    StatusCode::BAD_REQUEST,
                    "manifest_item_id_required".to_string(),
                )
            })?;
            if !seen.insert(manifest_item_id.clone()) {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    "duplicate_manifest_item_id".to_string(),
                ));
            }
            let title = optional_string_field_value(item, &["title"])
                .ok_or_else(|| AppError(StatusCode::BAD_REQUEST, "task_title_required".into()))?;
            let spec_file = optional_string_field_value(item, &["spec_file", "file_path"]);
            let spec_body =
                optional_string_field_value(item, &["spec_body", "body"]).or_else(|| {
                    spec_file
                        .as_deref()
                        .and_then(|path| file_bodies.and_then(|bodies| bodies.get(path)).cloned())
                });
            items.push(SpecManifestItem {
                manifest_item_id,
                phase: optional_string_field_value(item, &["phase", "phase_id"]),
                task_code: optional_string_field_value(item, &["task_code", "id", "code"]),
                team: optional_string_field_value(item, &["team"]),
                title,
                status: optional_string_field_value(item, &["status"]),
                dependencies: string_array_field(item, &["dependencies", "depends_on"]),
                labels: string_array_field(item, &["labels"]),
                touch_surface: string_array_field(item, &["touch_surface"]),
                acceptance_criteria: string_array_field(item, &["acceptance_criteria"]),
                validation_plan: string_array_field(item, &["validation_plan"]),
                gateway_task_id: optional_string_field_value(item, &["gateway_task_id"]),
                spec_file,
                spec_body,
                metadata: item.clone(),
            });
        }
    } else if let Some(phases) = manifest.get("phases").and_then(Value::as_array) {
        for phase in phases {
            let phase_id = optional_string_field_value(phase, &["id"]);
            let phase_name = optional_string_field_value(phase, &["name"]).or(phase_id.clone());
            let Some(tasks) = phase.get("tasks").and_then(Value::as_array) else {
                continue;
            };
            for task in tasks {
                let manifest_item_id =
                    manifest_item_id(phase_id.as_deref(), task).ok_or_else(|| {
                        AppError(
                            StatusCode::BAD_REQUEST,
                            "manifest_item_id_required".to_string(),
                        )
                    })?;
                if !seen.insert(manifest_item_id.clone()) {
                    return Err(AppError(
                        StatusCode::BAD_REQUEST,
                        "duplicate_manifest_item_id".to_string(),
                    ));
                }
                let title = optional_string_field_value(task, &["title"]).ok_or_else(|| {
                    AppError(StatusCode::BAD_REQUEST, "task_title_required".into())
                })?;
                let spec_file = optional_string_field_value(task, &["spec_file", "file_path"]);
                let spec_body =
                    optional_string_field_value(task, &["spec_body", "body"]).or_else(|| {
                        spec_file.as_deref().and_then(|path| {
                            file_bodies.and_then(|bodies| bodies.get(path)).cloned()
                        })
                    });
                items.push(SpecManifestItem {
                    manifest_item_id,
                    phase: phase_name.clone(),
                    task_code: optional_string_field_value(task, &["task_code", "id", "code"]),
                    team: optional_string_field_value(task, &["team"]),
                    title,
                    status: optional_string_field_value(task, &["status"]),
                    dependencies: string_array_field(task, &["dependencies", "depends_on"]),
                    labels: string_array_field(task, &["labels"]),
                    touch_surface: string_array_field(task, &["touch_surface"]),
                    acceptance_criteria: string_array_field(task, &["acceptance_criteria"]),
                    validation_plan: string_array_field(task, &["validation_plan"]),
                    gateway_task_id: optional_string_field_value(task, &["gateway_task_id"]),
                    spec_file,
                    spec_body,
                    metadata: task.clone(),
                });
            }
        }
    }

    if items.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "manifest_items_required".to_string(),
        ));
    }
    Ok(items)
}

fn spec_structured_payload(
    manifest: Value,
    file_bodies: Option<std::collections::HashMap<String, String>>,
    source_doc: Option<String>,
    source_artifact_id: Option<String>,
    source_artifact_version_id: Option<String>,
) -> Result<Value> {
    let items = normalize_spec_manifest(&manifest, file_bodies.as_ref())?;
    Ok(serde_json::json!({
        "schema": "gateway.spec_manifest.v1",
        "source_doc": source_doc,
        "source_artifact_id": source_artifact_id,
        "source_artifact_version_id": source_artifact_version_id,
        "manifest": manifest,
        "items": items,
        "stability_policy": spec_item_stability_policy()
    }))
}

fn spec_manifest_from_version(
    artifact_id: &str,
    version: &db::ArtifactVersion,
) -> Result<SpecManifestResponse> {
    let payload = version
        .structured_payload
        .as_ref()
        .ok_or_else(|| AppError(StatusCode::BAD_REQUEST, "spec_manifest_missing".to_string()))?;
    let manifest = payload
        .get("manifest")
        .cloned()
        .unwrap_or_else(|| payload.clone());
    let items_value = payload.get("items").cloned().unwrap_or(Value::Null);
    let items: Vec<SpecManifestItem> = serde_json::from_value(items_value).map_err(|err| {
        AppError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("invalid stored spec manifest items: {err}"),
        )
    })?;
    Ok(SpecManifestResponse {
        artifact_id: artifact_id.to_string(),
        artifact_version_id: version.artifact_version_id.clone(),
        manifest,
        items,
        stability_policy: payload
            .get("stability_policy")
            .cloned()
            .unwrap_or_else(spec_item_stability_policy),
    })
}

fn spec_task_source_block(artifact_id: &str, version_id: &str, item: &SpecManifestItem) -> String {
    format!(
        "Source:\nsource_spec_artifact_id: {artifact_id}\nsource_spec_version_id: {version_id}\nmanifest_item_id: {}\nspec_file: {}\n",
        item.manifest_item_id,
        item.spec_file.as_deref().unwrap_or("")
    )
}

fn generated_task_specification(
    artifact_id: &str,
    version_id: &str,
    item: &SpecManifestItem,
) -> String {
    let mut out = spec_task_source_block(artifact_id, version_id, item);
    out.push('\n');
    if let Some(team) = &item.team {
        out.push_str(&format!("Team: {team}\n"));
    }
    if !item.dependencies.is_empty() {
        out.push_str(&format!("Depends on: {}\n", item.dependencies.join(", ")));
    }
    if !item.touch_surface.is_empty() {
        out.push_str("\nTouch surface:\n");
        for path in &item.touch_surface {
            out.push_str(&format!("- {path}\n"));
        }
    }
    if !item.acceptance_criteria.is_empty() {
        out.push_str("\nAcceptance criteria:\n");
        for criterion in &item.acceptance_criteria {
            out.push_str(&format!("- {criterion}\n"));
        }
    }
    if !item.validation_plan.is_empty() {
        out.push_str("\nValidation plan:\n");
        for step in &item.validation_plan {
            out.push_str(&format!("- {step}\n"));
        }
    }
    if let Some(body) = &item.spec_body {
        out.push_str("\nFocused spec:\n");
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn generated_spec_task_title(item: &SpecManifestItem) -> String {
    item.task_code
        .as_deref()
        .map(|code| format!("{code} {}", item.title))
        .unwrap_or_else(|| item.title.clone())
}

fn design_review_payload(
    source_artifact_id: Option<String>,
    source_artifact_version_id: Option<String>,
) -> Value {
    serde_json::json!({
        "workflow": "design_review",
        "source": {
            "artifact_id": source_artifact_id,
            "artifact_version_id": source_artifact_version_id,
        }
    })
}

fn read_set_has_ids(read_set: &Value, keys: &[&str]) -> bool {
    let Value::Object(map) = read_set else {
        return false;
    };
    keys.iter().any(|key| {
        map.get(*key)
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().any(|value| value.as_str().is_some()))
    })
}

fn validate_design_review_phase(phase: &str, read_set: Option<&Value>) -> Result<()> {
    if !matches!(phase, "pass_1" | "pass_2") {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "invalid_review_phase".to_string(),
        ));
    }
    validate_read_set_required(Some(phase), "review", read_set)?;
    if phase == "pass_2"
        && !read_set
            .is_some_and(|value| read_set_has_ids(value, &["contribution", "contributions"]))
    {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "pass_2_read_set_contributions_required".to_string(),
        ));
    }
    Ok(())
}

fn validate_synthesis_read_set(read_set: &Value) -> Result<()> {
    if !read_set_has_ids(read_set, &["contribution", "contributions"]) {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "synthesis_read_set_contributions_required".to_string(),
        ));
    }
    Ok(())
}

fn read_set_for_reviewed_version(reviewed_version_id: &str) -> Value {
    serde_json::json!({ "versions": [reviewed_version_id] })
}

fn item_generation_key(version_id: &str, manifest_item_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(version_id.as_bytes());
    hasher.update(b":");
    hasher.update(manifest_item_id.as_bytes());
    format!("spec-task-{}", hex::encode(hasher.finalize()))
}

fn body_text(version: &db::ArtifactVersion) -> String {
    if let Some(body) = &version.body {
        body.clone()
    } else if let Some(payload) = &version.structured_payload {
        serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string())
    } else {
        String::new()
    }
}

fn simple_diff(from: &str, to: &str) -> String {
    let from_lines: Vec<_> = from.lines().collect();
    let to_lines: Vec<_> = to.lines().collect();
    let mut out = String::new();
    out.push_str("--- base\n+++ target\n");
    for line in &from_lines {
        if !to_lines.contains(line) {
            out.push('-');
            out.push_str(line);
            out.push('\n');
        }
    }
    for line in &to_lines {
        if !from_lines.contains(line) {
            out.push('+');
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

pub async fn list_artifacts_handler(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    Query(query): Query<ListArtifactsQuery>,
    headers: HeaderMap,
) -> Result<Json<ArtifactReadResponse<Vec<db::ArtifactSummary>>>> {
    require_artifact_read(&state, &headers, &ident)?;
    emit_artifact_metric(
        "gateway_artifact_search_requests_total",
        &[
            ("project", ident.as_str()),
            ("by", if query.q.is_some() { "query" } else { "id" }),
        ],
    );
    let db = state.db.clone();
    let artifacts = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::list_artifacts(
            &conn,
            &ident,
            &db::ArtifactFilters {
                kind: query.kind.as_deref(),
                subkind: query.subkind.as_deref(),
                lifecycle_state: query.lifecycle_state.as_deref(),
                label: query.label.as_deref(),
                actor_id: query.actor_id.as_deref(),
                query: query.q.as_deref(),
            },
        )
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(Json(ArtifactReadResponse {
        data: artifacts,
        chunking_status: ChunkingStatus {
            status: "none",
            current_chunk_count: 0,
            stale_chunk_count: 0,
            superseded_chunk_count: 0,
            failed_addresses: Vec::new(),
        },
    }))
}

pub async fn create_artifact_handler(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateArtifactRequest>,
) -> Result<Response> {
    let (context, authorization) =
        begin_artifact_mutation(&state, &headers, &ident, vec!["artifact.write"])?;
    let kind = trim_required(&body.kind, "kind")?;
    validate_artifact_kind(&kind)?;
    let title = trim_required(&body.title, "title")?;
    let labels = body.labels.unwrap_or_default();
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let context_for_db = context.clone();
    let (actor, write) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_project(&conn, &ident)?.ok_or_else(|| anyhow::anyhow!("project not found"))?;
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let result = db::create_artifact(
            &conn,
            &envelope,
            &db::ArtifactInsert {
                project_ident: &ident,
                kind: &kind,
                subkind: body.subkind.as_deref(),
                title: &title,
                labels: &labels,
                created_by_actor_id: &actor.actor_id,
            },
        )?;
        Ok::<_, anyhow::Error>((actor, result))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    emit_artifact_metric(
        "gateway_artifact_writes_total",
        &[
            ("project", write.record.project_ident.as_str()),
            ("kind", write.record.kind.as_str()),
            ("result", "created"),
        ],
    );
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        generated_resource("artifact_id", write.record.artifact_id.clone()),
        write.warnings,
        write.replayed,
        write.record,
    ))
}

pub async fn get_artifact_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<ArtifactReadResponse<ArtifactDetailResponse>>> {
    require_artifact_read(&state, &headers, &ident)?;
    let db = state.db.clone();
    let data = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let detail = db::get_artifact(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        let chunks = db::list_artifact_chunks(
            &conn,
            &ident,
            &artifact_id,
            &db::ArtifactChunkFilters {
                include_superseded: true,
                ..Default::default()
            },
        )?;
        let status = chunking_status_for(&detail.artifact, &chunks);
        let current_version = detail
            .current_version
            .map(|version| version_status(&detail.artifact, version, &chunks));
        let accepted_version = detail
            .accepted_version
            .map(|version| version_status(&detail.artifact, version, &chunks));
        Ok::<_, anyhow::Error>(ArtifactReadResponse {
            data: ArtifactDetailResponse {
                artifact: detail.artifact,
                current_version,
                accepted_version,
            },
            chunking_status: status,
        })
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(Json(data))
}

pub async fn list_artifact_versions_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<ArtifactReadResponse<Vec<VersionReadModel>>>> {
    require_artifact_read(&state, &headers, &ident)?;
    let db = state.db.clone();
    let data = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let artifact = db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        let versions = db::list_artifact_versions(&conn, &ident, &artifact_id)?;
        let chunks = db::list_artifact_chunks(
            &conn,
            &ident,
            &artifact_id,
            &db::ArtifactChunkFilters {
                include_superseded: true,
                ..Default::default()
            },
        )?;
        let status = chunking_status_for(&artifact, &chunks);
        Ok::<_, anyhow::Error>(ArtifactReadResponse {
            data: versions
                .into_iter()
                .map(|v| version_status(&artifact, v, &chunks))
                .collect(),
            chunking_status: status,
        })
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(Json(data))
}

pub async fn create_artifact_version_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<CreateArtifactVersionRequest>,
) -> Result<Response> {
    let (context, authorization) =
        begin_artifact_mutation(&state, &headers, &ident, vec!["artifact_version.create"])?;
    let body_format = body
        .body_format
        .as_deref()
        .unwrap_or("markdown")
        .to_string();
    validate_body_format(&body_format)?;
    artifact_body_schema_allowed(&state, &body_format, body.structured_payload.as_ref())?;
    let version_state = body.version_state.as_deref().unwrap_or("draft").to_string();
    if !matches!(
        version_state.as_str(),
        "draft" | "under_review" | "accepted" | "superseded" | "rejected"
    ) {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "invalid_version_state".to_string(),
        ));
    }
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let ident_for_metric = ident.clone();
    let body_format_for_db = body_format.clone();
    let body_format_for_metric = body_format.clone();
    let context_for_db = context.clone();
    let workflow_run_id_for_response = context.workflow_run_id.clone();
    let (actor, write) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        let run = validate_workflow_run_for_mutation(
            &conn,
            &artifact_id,
            context_for_db.workflow_run_id.as_deref(),
        )?;
        let result = db::create_artifact_version(
            &conn,
            &envelope,
            &db::ArtifactVersionInsert {
                artifact_id: &artifact_id,
                version_label: body.version_label.as_deref(),
                parent_version_id: body.parent_version_id.as_deref(),
                body_format: &body_format_for_db,
                body: body.body.as_deref(),
                structured_payload: body.structured_payload.as_ref(),
                source_format: body.source_format.as_deref(),
                created_by_actor_id: &actor.actor_id,
                created_via_workflow_run_id: context_for_db.workflow_run_id.as_deref(),
                version_state: &version_state,
                idempotency_key: Some(&context_for_db.idempotency_key),
            },
        )?;
        complete_resumed_run(
            &conn,
            run.as_ref(),
            None,
            Some(result.record.artifact_version_id.clone()),
            None,
        )?;
        Ok::<_, anyhow::Error>((actor, result))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    emit_artifact_metric(
        "gateway_artifact_version_writes_total",
        &[
            ("project", ident_for_metric.as_str()),
            ("kind", "artifact_version"),
            ("result", result_label(write.replayed)),
        ],
    );
    emit_artifact_metric(
        "gateway_artifact_version_body_bytes",
        &[
            ("project", ident_for_metric.as_str()),
            ("kind", body_format_for_metric.as_str()),
        ],
    );
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        serde_json::json!({
            "artifact_version_id": write.record.artifact_version_id,
            "workflow_run_id": workflow_run_id_for_response,
        }),
        write.warnings,
        write.replayed,
        write.record,
    ))
}

pub async fn get_artifact_version_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id, version_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<ArtifactReadResponse<VersionReadModel>>> {
    require_artifact_read(&state, &headers, &ident)?;
    let db = state.db.clone();
    let data = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let artifact = db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        let version = db::get_artifact_version(&conn, &ident, &artifact_id, &version_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact_version not found"))?;
        let chunks = db::list_artifact_chunks(
            &conn,
            &ident,
            &artifact_id,
            &db::ArtifactChunkFilters {
                include_superseded: true,
                artifact_version_id: Some(&version_id),
                ..Default::default()
            },
        )?;
        Ok::<_, anyhow::Error>(ArtifactReadResponse {
            data: version_status(&artifact, version, &chunks),
            chunking_status: chunking_status_for(&artifact, &chunks),
        })
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(Json(data))
}

pub async fn diff_artifact_version_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id, version_id)): Path<(String, String, String)>,
    Query(query): Query<DiffQuery>,
    headers: HeaderMap,
) -> Result<Json<ArtifactReadResponse<ArtifactDiffResponse>>> {
    require_artifact_read(&state, &headers, &ident)?;
    let db = state.db.clone();
    let data = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let artifact = db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        let target = db::get_artifact_version(&conn, &ident, &artifact_id, &version_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact_version not found"))?;
        let base_id = query
            .base_version_id
            .as_deref()
            .or(target.parent_version_id.as_deref());
        let base = base_id
            .map(|id| db::get_artifact_version(&conn, &ident, &artifact_id, id))
            .transpose()?
            .flatten();
        if base_id.is_some() && base.is_none() {
            anyhow::bail!("base artifact_version not found");
        }
        let from_text = base.as_ref().map(body_text).unwrap_or_default();
        let to_text = body_text(&target);
        let chunks = db::list_artifact_chunks(
            &conn,
            &ident,
            &artifact_id,
            &db::ArtifactChunkFilters {
                include_superseded: true,
                artifact_version_id: Some(&version_id),
                ..Default::default()
            },
        )?;
        emit_artifact_metric(
            "gateway_artifact_version_diff_bytes",
            &[
                ("project", ident.as_str()),
                ("kind", target.body_format.as_str()),
            ],
        );
        Ok::<_, anyhow::Error>(ArtifactReadResponse {
            data: ArtifactDiffResponse {
                from_version_id: base.map(|v| v.artifact_version_id),
                to_version_id: target.artifact_version_id,
                format: "unified",
                byte_delta: to_text.len() as isize - from_text.len() as isize,
                diff: simple_diff(&from_text, &to_text),
                chunking_status: chunking_status_for(&artifact, &chunks),
            },
            chunking_status: chunking_status_for(&artifact, &chunks),
        })
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(Json(data))
}

pub async fn list_artifact_comments_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<ArtifactReadResponse<Vec<db::ArtifactComment>>>> {
    require_artifact_read(&state, &headers, &ident)?;
    let db = state.db.clone();
    let comments = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::list_artifact_comments(&conn, &ident, &artifact_id)
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(Json(ArtifactReadResponse {
        data: comments,
        chunking_status: ChunkingStatus {
            status: "none",
            current_chunk_count: 0,
            stale_chunk_count: 0,
            superseded_chunk_count: 0,
            failed_addresses: Vec::new(),
        },
    }))
}

pub async fn create_artifact_comment_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<CreateCommentRequest>,
) -> Result<Response> {
    let (context, authorization) =
        begin_artifact_mutation(&state, &headers, &ident, vec!["comment.write"])?;
    let target_kind = trim_required(&body.target_kind, "target_kind")?;
    let target_id = trim_required(&body.target_id, "target_id")?;
    let comment_body = trim_required(&body.body, "body")?;
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let ident_for_metric = ident.clone();
    let target_kind_for_db = target_kind.clone();
    let target_kind_for_metric = target_kind.clone();
    let context_for_db = context.clone();
    let (actor, write) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let _ = validate_workflow_run_for_mutation(
            &conn,
            &artifact_id,
            context_for_db.workflow_run_id.as_deref(),
        )?;
        let result = db::add_artifact_comment(
            &conn,
            &envelope,
            &db::ArtifactCommentInsert {
                artifact_id: &artifact_id,
                target_kind: &target_kind_for_db,
                target_id: &target_id,
                child_address: body.child_address.as_deref(),
                parent_comment_id: body.parent_comment_id.as_deref(),
                actor_id: &actor.actor_id,
                body: &comment_body,
                idempotency_key: Some(&context_for_db.idempotency_key),
            },
        )?;
        Ok::<_, anyhow::Error>((actor, result))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    emit_artifact_metric(
        "gateway_comment_writes_total",
        &[
            ("project", ident_for_metric.as_str()),
            ("target_kind", target_kind_for_metric.as_str()),
            ("result", result_label(write.replayed)),
        ],
    );
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        generated_resource("comment_id", write.record.comment_id.clone()),
        write.warnings,
        write.replayed,
        write.record,
    ))
}

pub async fn resolve_artifact_comment_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id, comment_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<ResolveCommentRequest>,
) -> Result<Response> {
    let (context, authorization) =
        begin_artifact_mutation(&state, &headers, &ident, vec!["comment.resolve"])?;
    let db = state.db.clone();
    let context_for_db = context.clone();
    let (actor, comment) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        validate_workflow_run_for_mutation(
            &conn,
            &artifact_id,
            context_for_db.workflow_run_id.as_deref(),
        )?;
        let comment = db::resolve_artifact_comment(
            &conn,
            &ident,
            &artifact_id,
            &comment_id,
            &actor.actor_id,
            context_for_db.workflow_run_id.as_deref(),
            body.resolution_note.as_deref(),
        )?
        .ok_or_else(|| anyhow::anyhow!("comment not found"))?;
        Ok::<_, anyhow::Error>((actor, comment))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(finalize_ok_mutation(
        actor,
        &context,
        authorization,
        generated_resource("comment_id", comment.comment_id.clone()),
        Vec::new(),
        comment,
    ))
}

pub async fn reopen_artifact_comment_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id, comment_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<ReopenCommentRequest>,
) -> Result<Response> {
    let (context, authorization) =
        begin_artifact_mutation(&state, &headers, &ident, vec!["comment.write"])?;
    let note_body = body
        .note_body
        .unwrap_or_else(|| "reopened comment".to_string());
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let context_for_db = context.clone();
    let (actor, comment) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        validate_workflow_run_for_mutation(
            &conn,
            &artifact_id,
            context_for_db.workflow_run_id.as_deref(),
        )?;
        let comment = db::reopen_artifact_comment(
            &conn,
            &envelope,
            &ident,
            &artifact_id,
            &comment_id,
            &actor.actor_id,
            &note_body,
            Some(&context_for_db.idempotency_key),
        )?
        .ok_or_else(|| anyhow::anyhow!("comment not found"))?;
        Ok::<_, anyhow::Error>((actor, comment))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(finalize_ok_mutation(
        actor,
        &context,
        authorization,
        generated_resource("comment_id", comment.comment_id.clone()),
        Vec::new(),
        comment,
    ))
}

pub async fn list_artifact_contributions_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<ArtifactReadResponse<Vec<db::ArtifactContribution>>>> {
    require_artifact_read(&state, &headers, &ident)?;
    let db = state.db.clone();
    let contributions = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::list_artifact_contributions(&conn, &ident, &artifact_id)
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(Json(ArtifactReadResponse {
        data: contributions,
        chunking_status: ChunkingStatus {
            status: "none",
            current_chunk_count: 0,
            stale_chunk_count: 0,
            superseded_chunk_count: 0,
            failed_addresses: Vec::new(),
        },
    }))
}

pub async fn create_artifact_contribution_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<CreateContributionRequest>,
) -> Result<Response> {
    let (context, authorization) =
        begin_artifact_mutation(&state, &headers, &ident, vec!["contribution.write"])?;
    let target_kind = trim_required(&body.target_kind, "target_kind")?;
    let target_id = trim_required(&body.target_id, "target_id")?;
    let contribution_kind = trim_required(&body.contribution_kind, "contribution_kind")?;
    validate_read_set_required(
        body.phase.as_deref(),
        &contribution_kind,
        body.read_set.as_ref(),
    )?;
    let role = trim_required(&body.role, "role")?;
    let body_format = body
        .body_format
        .as_deref()
        .unwrap_or("markdown")
        .to_string();
    validate_body_format(&body_format)?;
    let contribution_body = trim_required(&body.body, "body")?;
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let ident_for_metric = ident.clone();
    let contribution_kind_for_db = contribution_kind.clone();
    let contribution_kind_for_metric = contribution_kind.clone();
    let phase_for_metric = body.phase.clone();
    let context_for_db = context.clone();
    let workflow_run_id_for_response = context.workflow_run_id.clone();
    let (actor, write) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let run = validate_workflow_run_for_mutation(
            &conn,
            &artifact_id,
            context_for_db.workflow_run_id.as_deref(),
        )?;
        let result = db::add_artifact_contribution(
            &conn,
            &envelope,
            &db::ArtifactContributionInsert {
                artifact_id: &artifact_id,
                target_kind: &target_kind,
                target_id: &target_id,
                contribution_kind: &contribution_kind_for_db,
                phase: body.phase.as_deref(),
                role: &role,
                actor_id: &actor.actor_id,
                workflow_run_id: context_for_db.workflow_run_id.as_deref(),
                read_set: body.read_set.as_ref(),
                body_format: &body_format,
                body: &contribution_body,
                idempotency_key: Some(&context_for_db.idempotency_key),
            },
        )?;
        complete_resumed_run(
            &conn,
            run.as_ref(),
            Some(result.record.contribution_id.clone()),
            None,
            None,
        )?;
        Ok::<_, anyhow::Error>((actor, result))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    emit_artifact_metric(
        "gateway_contribution_writes_total",
        &[
            ("project", ident_for_metric.as_str()),
            ("kind", contribution_kind_for_metric.as_str()),
            ("phase", phase_for_metric.as_deref().unwrap_or("none")),
            ("result", result_label(write.replayed)),
        ],
    );
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        serde_json::json!({
            "contribution_id": write.record.contribution_id,
            "workflow_run_id": workflow_run_id_for_response,
        }),
        write.warnings,
        write.replayed,
        write.record,
    ))
}

pub async fn list_artifact_links_handler(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    Query(query): Query<ListLinksQuery>,
    headers: HeaderMap,
) -> Result<Json<ArtifactReadResponse<Vec<db::ArtifactLink>>>> {
    require_artifact_read(&state, &headers, &ident)?;
    let db = state.db.clone();
    let links = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::list_artifact_links(
            &conn,
            &ident,
            &db::ArtifactLinkFilters {
                link_type: query.link_type.as_deref(),
                source_kind: query.source_kind.as_deref(),
                source_id: query.source_id.as_deref(),
                target_kind: query.target_kind.as_deref(),
                target_id: query.target_id.as_deref(),
            },
        )
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(Json(ArtifactReadResponse {
        data: links,
        chunking_status: ChunkingStatus {
            status: "none",
            current_chunk_count: 0,
            stale_chunk_count: 0,
            superseded_chunk_count: 0,
            failed_addresses: Vec::new(),
        },
    }))
}

pub async fn create_artifact_link_handler(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateLinkRequest>,
) -> Result<Response> {
    let (context, authorization) =
        begin_artifact_mutation(&state, &headers, &ident, vec!["link.write"])?;
    let link_type = trim_required(&body.link_type, "link_type")?;
    let source_kind = trim_required(&body.source_kind, "source_kind")?;
    let source_id = trim_required(&body.source_id, "source_id")?;
    let target_kind = trim_required(&body.target_kind, "target_kind")?;
    let target_id = trim_required(&body.target_id, "target_id")?;
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let ident_for_metric = ident.clone();
    let link_type_for_db = link_type.clone();
    let link_type_for_metric = link_type.clone();
    let context_for_db = context.clone();
    let (actor, write) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_project(&conn, &ident)?.ok_or_else(|| anyhow::anyhow!("project not found"))?;
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        // Links are project-scoped: the workflow run (if supplied) does NOT
        // need to belong to a specific artifact, but it must still be
        // retryable per the shared `ensure_workflow_run_retryable` semantics.
        let run_artifact =
            load_workflow_run_for_link(&conn, context_for_db.workflow_run_id.as_deref())?;
        let result = db::create_artifact_link(
            &conn,
            &envelope,
            &db::ArtifactLinkInsert {
                link_type: &link_type_for_db,
                source_kind: &source_kind,
                source_id: &source_id,
                source_version_id: body.source_version_id.as_deref(),
                source_child_address: body.source_child_address.as_deref(),
                target_kind: &target_kind,
                target_id: &target_id,
                target_version_id: body.target_version_id.as_deref(),
                target_child_address: body.target_child_address.as_deref(),
                created_by_actor_id: &actor.actor_id,
                created_via_workflow_run_id: context_for_db.workflow_run_id.as_deref(),
                idempotency_key: Some(&context_for_db.idempotency_key),
                supersedes_link_id: body.supersedes_link_id.as_deref(),
            },
        )?;
        complete_resumed_run(
            &conn,
            run_artifact.as_ref(),
            None,
            None,
            Some(result.record.link_id.clone()),
        )?;
        Ok::<_, anyhow::Error>((actor, result))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    emit_artifact_metric(
        "gateway_link_writes_total",
        &[
            ("project", ident_for_metric.as_str()),
            ("link_type", link_type_for_metric.as_str()),
            ("result", result_label(write.replayed)),
        ],
    );
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        generated_resource("link_id", write.record.link_id.clone()),
        write.warnings,
        write.replayed,
        write.record,
    ))
}

pub async fn list_artifact_workflow_runs_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<ArtifactReadResponse<Vec<db::WorkflowRun>>>> {
    require_artifact_read(&state, &headers, &ident)?;
    let db = state.db.clone();
    let runs = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::list_workflow_runs(&conn, &ident, &artifact_id)
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(Json(ArtifactReadResponse {
        data: runs,
        chunking_status: ChunkingStatus {
            status: "none",
            current_chunk_count: 0,
            stale_chunk_count: 0,
            superseded_chunk_count: 0,
            failed_addresses: Vec::new(),
        },
    }))
}

pub async fn start_artifact_workflow_run_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<StartWorkflowRunRequest>,
) -> Result<Response> {
    let (context, authorization) =
        begin_artifact_mutation(&state, &headers, &ident, vec!["workflow_run.start"])?;
    let workflow_kind = trim_required(&body.workflow_kind, "workflow_kind")?;
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let ident_for_metric = ident.clone();
    let workflow_kind_for_db = workflow_kind.clone();
    let workflow_kind_for_metric = workflow_kind.clone();
    let context_for_db = context.clone();
    let (actor, write) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let participants = body.participant_actor_ids.unwrap_or_default();
        let result = db::start_workflow_run(
            &conn,
            &envelope,
            &db::WorkflowRunInsert {
                artifact_id: &artifact_id,
                workflow_kind: &workflow_kind_for_db,
                phase: body.phase.as_deref(),
                round_id: body.round_id.as_deref(),
                coordinator_actor_id: &actor.actor_id,
                participant_actor_ids: &participants,
                source_artifact_version_id: body.source_artifact_version_id.as_deref(),
                read_set: body.read_set.as_ref(),
                idempotency_key: Some(&context_for_db.idempotency_key),
                is_resumable: body.is_resumable.unwrap_or(matches!(
                    workflow_kind_for_db.as_str(),
                    "spec_task_generation" | "doc_publish"
                )),
            },
        )?;
        Ok::<_, anyhow::Error>((actor, result))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    emit_artifact_metric(
        "gateway_workflow_runs_total",
        &[
            ("project", ident_for_metric.as_str()),
            ("kind", workflow_kind_for_metric.as_str()),
            ("state", write.record.state.as_str()),
        ],
    );
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        generated_resource("workflow_run_id", write.record.workflow_run_id.clone()),
        write.warnings,
        write.replayed,
        write.record,
    ))
}

pub async fn get_artifact_workflow_run_handler(
    State(state): State<AppState>,
    Path((ident, _artifact_id, workflow_run_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<ArtifactReadResponse<db::WorkflowRun>>> {
    require_artifact_read(&state, &headers, &ident)?;
    let db = state.db.clone();
    let run = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_workflow_run(&conn, &workflow_run_id)?
            .ok_or_else(|| anyhow::anyhow!("workflow_run not found"))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(Json(ArtifactReadResponse {
        data: run,
        chunking_status: ChunkingStatus {
            status: "none",
            current_chunk_count: 0,
            stale_chunk_count: 0,
            superseded_chunk_count: 0,
            failed_addresses: Vec::new(),
        },
    }))
}

pub async fn complete_artifact_workflow_run_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id, workflow_run_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<CompleteWorkflowRunRequest>,
) -> Result<Response> {
    let (context, authorization) =
        begin_artifact_mutation(&state, &headers, &ident, vec!["workflow_run.complete"])?;
    let state_value = trim_required(&body.state, "state")?;
    if !matches!(state_value.as_str(), "succeeded" | "failed" | "cancelled") {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "invalid_workflow_run_state".to_string(),
        ));
    }
    let db = state.db.clone();
    let ident_for_metric = ident.clone();
    let context_for_db = context.clone();
    let (actor, updated) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let current = db::get_workflow_run(&conn, &workflow_run_id)?
            .ok_or_else(|| anyhow::anyhow!("workflow_run not found"))?;
        if current.artifact_id != artifact_id {
            anyhow::bail!("workflow_run does not belong to artifact");
        }
        if current.coordinator_actor_id != actor.actor_id {
            anyhow::bail!("workflow_run coordinator mismatch");
        }
        let updated = db::update_workflow_run(
            &conn,
            &workflow_run_id,
            &db::WorkflowRunUpdate {
                state: Some(&state_value),
                failure_reason: if state_value == "failed" {
                    Some(body.failure_reason.as_deref())
                } else {
                    Some(None)
                },
                generated_contribution_ids: body.generated_contribution_ids.as_deref(),
                generated_version_ids: body.generated_version_ids.as_deref(),
                generated_task_ids: body.generated_task_ids.as_deref(),
                generated_link_ids: body.generated_link_ids.as_deref(),
                generated_chunk_ids: body.generated_chunk_ids.as_deref(),
                ended_at: None,
            },
        )?
        .ok_or_else(|| anyhow::anyhow!("workflow_run not found"))?;
        Ok::<_, anyhow::Error>((actor, updated))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    emit_artifact_metric(
        "gateway_workflow_runs_total",
        &[
            ("project", ident_for_metric.as_str()),
            ("kind", updated.workflow_kind.as_str()),
            ("state", updated.state.as_str()),
        ],
    );
    let generated = generated_workflow_resources(&updated);
    Ok(finalize_ok_mutation(
        actor,
        &context,
        authorization,
        generated,
        Vec::new(),
        updated,
    ))
}

pub async fn create_design_review_handler(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    headers: HeaderMap,
    Json(body): Json<DesignReviewCreateRequest>,
) -> Result<Response> {
    let (context, authorization) = begin_artifact_mutation(
        &state,
        &headers,
        &ident,
        vec!["artifact.write", "artifact_version.create"],
    )?;
    let title = trim_required(&body.title, "title")?;
    let labels = body.labels.unwrap_or_default();
    let body_format = body
        .body_format
        .as_deref()
        .unwrap_or("markdown")
        .to_string();
    validate_body_format(&body_format)?;
    let payload = design_review_payload(
        body.source_artifact_id.clone(),
        body.source_artifact_version_id.clone(),
    );
    artifact_body_schema_allowed(&state, &body_format, Some(&payload))?;
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let context_for_db = context.clone();
    let (actor, artifact_write, version_write) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_project(&conn, &ident)?.ok_or_else(|| anyhow::anyhow!("project not found"))?;
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let artifact_write = db::create_artifact(
            &conn,
            &envelope,
            &db::ArtifactInsert {
                project_ident: &ident,
                kind: "design_review",
                subkind: None,
                title: &title,
                labels: &labels,
                created_by_actor_id: &actor.actor_id,
            },
        )?;
        let version_write = if body.body.is_some() {
            let version_key = format!("{}:initial-version", context_for_db.idempotency_key);
            Some(db::create_artifact_version(
                &conn,
                &envelope,
                &db::ArtifactVersionInsert {
                    artifact_id: &artifact_write.record.artifact_id,
                    version_label: Some("source"),
                    parent_version_id: None,
                    body_format: &body_format,
                    body: body.body.as_deref(),
                    structured_payload: Some(&payload),
                    source_format: None,
                    created_by_actor_id: &actor.actor_id,
                    created_via_workflow_run_id: None,
                    version_state: "under_review",
                    idempotency_key: Some(&version_key),
                },
            )?)
        } else {
            None
        };
        Ok::<_, anyhow::Error>((actor, artifact_write, version_write))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    let mut warnings = artifact_write.warnings;
    if let Some(version) = &version_write {
        warnings.extend(version.warnings.clone());
    }
    let generated = serde_json::json!({
        "artifact_id": artifact_write.record.artifact_id,
        "artifact_version_id": version_write.as_ref().map(|write| write.record.artifact_version_id.clone()),
    });
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        generated,
        warnings,
        artifact_write.replayed && version_write.as_ref().is_some_and(|write| write.replayed),
        serde_json::json!({
            "artifact": artifact_write.record,
            "version": version_write.map(|write| write.record),
        }),
    ))
}

pub async fn create_design_review_round_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<DesignReviewRoundRequest>,
) -> Result<Response> {
    let (context, authorization) = begin_artifact_mutation(
        &state,
        &headers,
        &ident,
        vec!["workflow_run.start", "artifact.write"],
    )?;
    let source_version_id = trim_required(
        &body.source_artifact_version_id,
        "source_artifact_version_id",
    )?;
    let round_id = body
        .round_id
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("review-round:{}", context.idempotency_key));
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let context_for_db = context.clone();
    let (actor, write, artifact) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let artifact = db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        if artifact.kind != "design_review" {
            anyhow::bail!("artifact is not a design_review");
        }
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let participants = body.participant_actor_ids.unwrap_or_default();
        let write = db::start_workflow_run(
            &conn,
            &envelope,
            &db::WorkflowRunInsert {
                artifact_id: &artifact_id,
                workflow_kind: "design_review_round",
                phase: Some("pass_1"),
                round_id: Some(&round_id),
                coordinator_actor_id: &actor.actor_id,
                participant_actor_ids: &participants,
                source_artifact_version_id: Some(&source_version_id),
                read_set: body.read_set.as_ref(),
                idempotency_key: Some(&context_for_db.idempotency_key),
                is_resumable: false,
            },
        )?;
        let artifact = db::update_artifact(
            &conn,
            &ident,
            &artifact_id,
            &db::ArtifactUpdate {
                lifecycle_state: Some("active"),
                review_state: Some("collecting_reviews"),
                ..Default::default()
            },
            &envelope,
        )?
        .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        Ok::<_, anyhow::Error>((actor, write, artifact))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        generated_resource("workflow_run_id", write.record.workflow_run_id.clone()),
        write.warnings,
        write.replayed,
        serde_json::json!({
            "workflow_run": write.record,
            "artifact": artifact,
        }),
    ))
}

pub async fn create_design_review_contribution_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id, workflow_run_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<DesignReviewContributionRequest>,
) -> Result<Response> {
    let (mut context, authorization) =
        begin_artifact_mutation(&state, &headers, &ident, vec!["contribution.write"])?;
    context.workflow_run_id = Some(workflow_run_id.clone());
    let phase = trim_required(&body.phase, "phase")?;
    let role = body.role.unwrap_or_else(|| "reviewer".to_string());
    let role = trim_required(&role, "role")?;
    let body_format = body
        .body_format
        .as_deref()
        .unwrap_or("markdown")
        .to_string();
    validate_body_format(&body_format)?;
    let contribution_body = trim_required(&body.body, "body")?;
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let context_for_db = context.clone();
    let workflow_run_id_for_response = workflow_run_id.clone();
    let (actor, write) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let artifact = db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        if artifact.kind != "design_review" {
            anyhow::bail!("artifact is not a design_review");
        }
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let run = validate_workflow_run_for_mutation(&conn, &artifact_id, Some(&workflow_run_id))?
            .ok_or_else(|| anyhow::anyhow!("workflow run not found"))?;
        if run.workflow_kind != "design_review_round" {
            anyhow::bail!("workflow run is not a design_review_round");
        }
        let reviewed_version_id = body
            .reviewed_version_id
            .as_deref()
            .or(run.source_artifact_version_id.as_deref())
            .ok_or_else(|| anyhow::anyhow!("reviewed version required"))?
            .to_string();
        let read_set = body
            .read_set
            .unwrap_or_else(|| read_set_for_reviewed_version(&reviewed_version_id));
        validate_design_review_phase(&phase, Some(&read_set)).map_err(|e| anyhow::anyhow!(e.1))?;
        let write = db::add_artifact_contribution(
            &conn,
            &envelope,
            &db::ArtifactContributionInsert {
                artifact_id: &artifact_id,
                target_kind: "artifact_version",
                target_id: &reviewed_version_id,
                contribution_kind: "review",
                phase: Some(&phase),
                role: &role,
                actor_id: &actor.actor_id,
                workflow_run_id: Some(&workflow_run_id),
                read_set: Some(&read_set),
                body_format: &body_format,
                body: &contribution_body,
                idempotency_key: Some(&context_for_db.idempotency_key),
            },
        )?;
        db::append_workflow_run_outputs(
            &conn,
            &workflow_run_id,
            Some(&write.record.contribution_id),
            None,
        )?;
        Ok::<_, anyhow::Error>((actor, write))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        serde_json::json!({
            "contribution_id": write.record.contribution_id,
            "workflow_run_id": workflow_run_id_for_response,
        }),
        write.warnings,
        write.replayed,
        write.record,
    ))
}

pub async fn create_design_review_synthesis_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id, workflow_run_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<DesignReviewSynthesisRequest>,
) -> Result<Response> {
    let (mut context, authorization) = begin_artifact_mutation(
        &state,
        &headers,
        &ident,
        vec![
            "contribution.write",
            "artifact_version.create",
            "artifact.write",
        ],
    )?;
    context.workflow_run_id = Some(workflow_run_id.clone());
    validate_synthesis_read_set(&body.read_set)?;
    let body_format = body
        .body_format
        .as_deref()
        .unwrap_or("markdown")
        .to_string();
    validate_body_format(&body_format)?;
    let synthesis_body = trim_required(&body.body, "body")?;
    let should_create_version = body.create_version.unwrap_or(true);
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let context_for_db = context.clone();
    let workflow_run_id_for_response = workflow_run_id.clone();
    let (actor, contribution_write, version_write, artifact) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let artifact = db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        if artifact.kind != "design_review" {
            anyhow::bail!("artifact is not a design_review");
        }
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let run = validate_workflow_run_for_mutation(&conn, &artifact_id, Some(&workflow_run_id))?
            .ok_or_else(|| anyhow::anyhow!("workflow run not found"))?;
        if run.workflow_kind != "design_review_round" {
            anyhow::bail!("workflow run is not a design_review_round");
        }
        let reviewed_version_id = body
            .reviewed_version_id
            .as_deref()
            .or(run.source_artifact_version_id.as_deref())
            .ok_or_else(|| anyhow::anyhow!("reviewed version required"))?
            .to_string();
        let contribution_write = db::add_artifact_contribution(
            &conn,
            &envelope,
            &db::ArtifactContributionInsert {
                artifact_id: &artifact_id,
                target_kind: "artifact_version",
                target_id: &reviewed_version_id,
                contribution_kind: "synthesis",
                phase: Some("synthesis"),
                role: "analyst",
                actor_id: &actor.actor_id,
                workflow_run_id: Some(&workflow_run_id),
                read_set: Some(&body.read_set),
                body_format: &body_format,
                body: &synthesis_body,
                idempotency_key: Some(&context_for_db.idempotency_key),
            },
        )?;
        let version_write = if should_create_version {
            let version_key = format!("{}:synthesis-version", context_for_db.idempotency_key);
            let payload = serde_json::json!({
                "workflow": "design_review_synthesis",
                "round_id": run.round_id,
                "source_artifact_version_id": reviewed_version_id,
                "synthesis_contribution_id": contribution_write.record.contribution_id,
                "read_set": body.read_set,
            });
            Some(db::create_artifact_version(
                &conn,
                &envelope,
                &db::ArtifactVersionInsert {
                    artifact_id: &artifact_id,
                    version_label: body.version_label.as_deref(),
                    parent_version_id: Some(&reviewed_version_id),
                    body_format: &body_format,
                    body: Some(&synthesis_body),
                    structured_payload: Some(&payload),
                    source_format: None,
                    created_by_actor_id: &actor.actor_id,
                    created_via_workflow_run_id: Some(&workflow_run_id),
                    version_state: "draft",
                    idempotency_key: Some(&version_key),
                },
            )?)
        } else {
            None
        };
        db::append_workflow_run_outputs(
            &conn,
            &workflow_run_id,
            Some(&contribution_write.record.contribution_id),
            version_write
                .as_ref()
                .map(|write| write.record.artifact_version_id.as_str()),
        )?;
        let artifact = db::update_artifact(
            &conn,
            &ident,
            &artifact_id,
            &db::ArtifactUpdate {
                review_state: Some("needs_user_decision"),
                ..Default::default()
            },
            &envelope,
        )?
        .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        Ok::<_, anyhow::Error>((actor, contribution_write, version_write, artifact))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    let mut warnings = contribution_write.warnings;
    if let Some(version) = &version_write {
        warnings.extend(version.warnings.clone());
    }
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        serde_json::json!({
            "contribution_id": contribution_write.record.contribution_id,
            "artifact_version_id": version_write.as_ref().map(|write| write.record.artifact_version_id.clone()),
            "workflow_run_id": workflow_run_id_for_response,
        }),
        warnings,
        contribution_write.replayed && version_write.as_ref().is_some_and(|write| write.replayed),
        serde_json::json!({
            "contribution": contribution_write.record,
            "version": version_write.map(|write| write.record),
            "artifact": artifact,
        }),
    ))
}

pub async fn update_design_review_state_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<DesignReviewStateRequest>,
) -> Result<Response> {
    let (context, authorization) = begin_artifact_mutation(
        &state,
        &headers,
        &ident,
        vec!["artifact.write", "contribution.write"],
    )?;
    if body.lifecycle_state.is_none() && body.review_state.is_none() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "state_required".to_string(),
        ));
    }
    let note = body
        .note
        .unwrap_or_else(|| "design review state transition".to_string());
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let context_for_db = context.clone();
    let workflow_run_id_for_response = context.workflow_run_id.clone();
    let (actor, artifact, contribution) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let artifact = db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        if artifact.kind != "design_review" {
            anyhow::bail!("artifact is not a design_review");
        }
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let run = validate_workflow_run_for_mutation(
            &conn,
            &artifact_id,
            context_for_db.workflow_run_id.as_deref(),
        )?;
        let contribution = db::add_artifact_contribution(
            &conn,
            &envelope,
            &db::ArtifactContributionInsert {
                artifact_id: &artifact_id,
                target_kind: "artifact",
                target_id: &artifact_id,
                contribution_kind: "state_transition",
                phase: Some("state_transition"),
                role: "coordinator",
                actor_id: &actor.actor_id,
                workflow_run_id: context_for_db.workflow_run_id.as_deref(),
                read_set: None,
                body_format: "markdown",
                body: &note,
                idempotency_key: Some(&context_for_db.idempotency_key),
            },
        )?;
        if let Some(run) = run.as_ref() {
            db::append_workflow_run_outputs(
                &conn,
                &run.workflow_run_id,
                Some(&contribution.record.contribution_id),
                None,
            )?;
        }
        let artifact = db::update_artifact(
            &conn,
            &ident,
            &artifact_id,
            &db::ArtifactUpdate {
                lifecycle_state: body.lifecycle_state.as_deref(),
                review_state: body.review_state.as_deref(),
                ..Default::default()
            },
            &envelope,
        )?
        .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        Ok::<_, anyhow::Error>((actor, artifact, contribution))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        serde_json::json!({
            "artifact_id": artifact.artifact_id,
            "contribution_id": contribution.record.contribution_id,
            "workflow_run_id": workflow_run_id_for_response,
        }),
        contribution.warnings,
        contribution.replayed,
        serde_json::json!({
            "artifact": artifact,
            "contribution": contribution.record,
        }),
    ))
}

pub async fn list_design_review_contributions_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    Query(query): Query<ListDesignReviewContributionsQuery>,
    headers: HeaderMap,
) -> Result<Json<ArtifactReadResponse<Vec<db::ArtifactContribution>>>> {
    require_artifact_read(&state, &headers, &ident)?;
    let db = state.db.clone();
    let contributions = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::list_design_review_contributions(
            &conn,
            &ident,
            &artifact_id,
            &db::DesignReviewContributionFilters {
                round_id: query.round_id.as_deref(),
                phase: query.phase.as_deref(),
                role: query.role.as_deref(),
                reviewed_version_id: query.reviewed_version_id.as_deref(),
                read_set_contains: query.read_set_contains.as_deref(),
            },
        )
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(Json(ArtifactReadResponse {
        data: contributions,
        chunking_status: ChunkingStatus {
            status: "none",
            current_chunk_count: 0,
            stale_chunk_count: 0,
            superseded_chunk_count: 0,
            failed_addresses: Vec::new(),
        },
    }))
}

pub async fn create_spec_handler(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SpecImportRequest>,
) -> Result<Response> {
    let (context, authorization) = begin_artifact_mutation(
        &state,
        &headers,
        &ident,
        vec!["artifact.write", "artifact_version.create"],
    )?;
    let title = trim_required(&body.title, "title")?;
    let labels = body.labels.unwrap_or_default();
    let payload = spec_structured_payload(
        body.manifest,
        body.file_bodies,
        body.source_doc,
        body.source_artifact_id,
        body.source_artifact_version_id,
    )?;
    artifact_body_schema_allowed(&state, "markdown", Some(&payload))?;
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let context_for_db = context.clone();
    let (actor, artifact_write, version_write) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_project(&conn, &ident)?.ok_or_else(|| anyhow::anyhow!("project not found"))?;
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let artifact_write = db::create_artifact(
            &conn,
            &envelope,
            &db::ArtifactInsert {
                project_ident: &ident,
                kind: "spec",
                subkind: Some("implementation"),
                title: &title,
                labels: &labels,
                created_by_actor_id: &actor.actor_id,
            },
        )?;
        let version_key = format!("{}:initial-version", context_for_db.idempotency_key);
        let version_write = db::create_artifact_version(
            &conn,
            &envelope,
            &db::ArtifactVersionInsert {
                artifact_id: &artifact_write.record.artifact_id,
                version_label: Some("imported"),
                parent_version_id: None,
                body_format: "markdown",
                body: body.body.as_deref(),
                structured_payload: Some(&payload),
                source_format: Some("gateway-spec-directory"),
                created_by_actor_id: &actor.actor_id,
                created_via_workflow_run_id: None,
                version_state: "draft",
                idempotency_key: Some(&version_key),
            },
        )?;
        Ok::<_, anyhow::Error>((actor, artifact_write, version_write))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    let generated = serde_json::json!({
        "artifact_id": artifact_write.record.artifact_id,
        "artifact_version_id": version_write.record.artifact_version_id
    });
    let mut warnings = artifact_write.warnings;
    warnings.extend(version_write.warnings);
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        generated,
        warnings,
        artifact_write.replayed && version_write.replayed,
        serde_json::json!({
            "artifact": artifact_write.record,
            "version": version_write.record,
        }),
    ))
}

pub async fn create_spec_version_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SpecVersionRequest>,
) -> Result<Response> {
    let (context, authorization) =
        begin_artifact_mutation(&state, &headers, &ident, vec!["artifact_version.create"])?;
    let payload =
        spec_structured_payload(body.manifest, body.file_bodies, body.source_doc, None, None)?;
    artifact_body_schema_allowed(&state, "markdown", Some(&payload))?;
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let context_for_db = context.clone();
    let workflow_run_id_for_response = context.workflow_run_id.clone();
    let (actor, write) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let artifact = db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        if artifact.kind != "spec" {
            anyhow::bail!("artifact is not a spec");
        }
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let run = validate_workflow_run_for_mutation(
            &conn,
            &artifact_id,
            context_for_db.workflow_run_id.as_deref(),
        )?;
        let result = db::create_artifact_version(
            &conn,
            &envelope,
            &db::ArtifactVersionInsert {
                artifact_id: &artifact_id,
                version_label: body.version_label.as_deref(),
                parent_version_id: body.parent_version_id.as_deref(),
                body_format: "markdown",
                body: body.body.as_deref(),
                structured_payload: Some(&payload),
                source_format: Some("gateway-spec-directory"),
                created_by_actor_id: &actor.actor_id,
                created_via_workflow_run_id: context_for_db.workflow_run_id.as_deref(),
                version_state: "draft",
                idempotency_key: Some(&context_for_db.idempotency_key),
            },
        )?;
        complete_resumed_run(
            &conn,
            run.as_ref(),
            None,
            Some(result.record.artifact_version_id.clone()),
            None,
        )?;
        Ok::<_, anyhow::Error>((actor, result))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        serde_json::json!({
            "artifact_version_id": write.record.artifact_version_id,
            "workflow_run_id": workflow_run_id_for_response,
        }),
        write.warnings,
        write.replayed,
        write.record,
    ))
}

pub async fn accept_spec_version_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SpecAcceptRequest>,
) -> Result<Response> {
    let (context, authorization) = begin_artifact_mutation(
        &state,
        &headers,
        &ident,
        vec!["artifact_version.accept", "artifact.write"],
    )?;
    let version_id = trim_required(&body.version_id, "version_id")?;
    let db = state.db.clone();
    let context_for_db = context.clone();
    let version_id_for_response = version_id.clone();
    let (actor, contribution) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let artifact = db::get_artifact_summary(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        if artifact.kind != "spec" {
            anyhow::bail!("artifact is not a spec");
        }
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        validate_workflow_run_for_mutation(
            &conn,
            &artifact_id,
            context_for_db.workflow_run_id.as_deref(),
        )?;
        let contribution = db::accept_artifact_version(
            &conn,
            &ident,
            &artifact_id,
            &version_id,
            &actor.actor_id,
            context_for_db.workflow_run_id.as_deref(),
            Some(&context_for_db.idempotency_key),
        )?;
        Ok::<_, anyhow::Error>((actor, contribution))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(finalize_ok_mutation(
        actor,
        &context,
        authorization,
        serde_json::json!({
            "artifact_version_id": version_id_for_response,
            "contribution_id": contribution.contribution_id,
        }),
        Vec::new(),
        contribution,
    ))
}

pub async fn get_spec_manifest_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    Query(query): Query<SpecManifestQuery>,
    headers: HeaderMap,
) -> Result<Json<ArtifactReadResponse<SpecManifestResponse>>> {
    require_artifact_read(&state, &headers, &ident)?;
    let db = state.db.clone();
    let response = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let detail = db::get_artifact(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        if detail.artifact.kind != "spec" {
            anyhow::bail!("artifact is not a spec");
        }
        let version = if let Some(version_id) = query.version_id {
            db::get_artifact_version(&conn, &ident, &artifact_id, &version_id)?
                .ok_or_else(|| anyhow::anyhow!("version not found"))?
        } else {
            detail
                .accepted_version
                .or(detail.current_version)
                .ok_or_else(|| anyhow::anyhow!("spec version not found"))?
        };
        spec_manifest_from_version(&artifact_id, &version).map_err(|err| anyhow::anyhow!(err.1))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(Json(ArtifactReadResponse {
        data: response,
        chunking_status: ChunkingStatus {
            status: "none",
            current_chunk_count: 0,
            stale_chunk_count: 0,
            superseded_chunk_count: 0,
            failed_addresses: Vec::new(),
        },
    }))
}

pub async fn get_spec_manifest_item_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id, manifest_item_id)): Path<(String, String, String)>,
    Query(query): Query<SpecManifestQuery>,
    headers: HeaderMap,
) -> Result<Json<ArtifactReadResponse<SpecManifestItem>>> {
    let manifest = get_spec_manifest_handler(
        State(state),
        Path((ident, artifact_id)),
        Query(query),
        headers,
    )
    .await?
    .0
    .data;
    let item = manifest
        .items
        .into_iter()
        .find(|item| item.manifest_item_id == manifest_item_id)
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "manifest_item not found".to_string()))?;
    Ok(Json(ArtifactReadResponse {
        data: item,
        chunking_status: ChunkingStatus {
            status: "none",
            current_chunk_count: 0,
            stale_chunk_count: 0,
            superseded_chunk_count: 0,
            failed_addresses: Vec::new(),
        },
    }))
}

#[allow(clippy::too_many_arguments)]
fn link_task_to_spec_item(
    conn: &rusqlite::Connection,
    envelope: &db::ArtifactOperationsEnvelope,
    actor_id: &str,
    workflow_run_id: &str,
    source_version_id: &str,
    manifest_item_id: &str,
    task_id: &str,
) -> anyhow::Result<db::ArtifactWriteResult<db::ArtifactLink>> {
    let child_address = format!("manifest.items[{manifest_item_id}]");
    db::create_artifact_link(
        conn,
        envelope,
        &db::ArtifactLinkInsert {
            link_type: "task_generated_from_spec",
            source_kind: "artifact_version",
            source_id: source_version_id,
            source_version_id: Some(source_version_id),
            source_child_address: Some(&child_address),
            target_kind: "task",
            target_id: task_id,
            target_version_id: Some(source_version_id),
            target_child_address: None,
            created_by_actor_id: actor_id,
            created_via_workflow_run_id: Some(workflow_run_id),
            idempotency_key: Some(&item_generation_key(source_version_id, manifest_item_id)),
            supersedes_link_id: None,
        },
    )
}

pub async fn generate_spec_tasks_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<GenerateSpecTasksRequest>,
) -> Result<Response> {
    let (context, authorization) = begin_artifact_mutation(
        &state,
        &headers,
        &ident,
        vec![
            "task.generate_from_spec",
            "workflow_run.start",
            "link.write",
        ],
    )?;
    if !body.confirmed {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "task_generation_confirmation_required".to_string(),
        ));
    }
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let context_for_db = context.clone();
    let (actor, output) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let detail = db::get_artifact(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        if detail.artifact.kind != "spec" {
            anyhow::bail!("artifact is not a spec");
        }
        let accepted = detail
            .accepted_version
            .ok_or_else(|| anyhow::anyhow!("accepted spec version required"))?;
        let manifest = spec_manifest_from_version(&artifact_id, &accepted)
            .map_err(|err| anyhow::anyhow!(err.1))?;
        let selected: std::collections::BTreeSet<String> = body
            .manifest_item_ids
            .unwrap_or_default()
            .into_iter()
            .collect();
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let read_set = serde_json::json!({
            "artifact_version_ids": [accepted.artifact_version_id.clone()]
        });
        let run_write = db::start_workflow_run(
            &conn,
            &envelope,
            &db::WorkflowRunInsert {
                artifact_id: &artifact_id,
                workflow_kind: "spec_task_generation",
                phase: Some("generate_tasks"),
                round_id: None,
                coordinator_actor_id: &actor.actor_id,
                participant_actor_ids: &[],
                source_artifact_version_id: Some(&accepted.artifact_version_id),
                read_set: Some(&read_set),
                idempotency_key: Some(&context_for_db.idempotency_key),
                is_resumable: true,
            },
        )?;
        ensure_workflow_run_retryable(&run_write.record)?;

        let mut task_ids = run_write.record.generated_task_ids.clone();
        let mut link_ids = run_write.record.generated_link_ids.clone();
        let mut item_outputs = Vec::new();
        let reporter = body
            .reporter
            .as_deref()
            .unwrap_or(&context_for_db.actor.agent_id)
            .to_string();
        for item in manifest.items {
            if !selected.is_empty() && !selected.contains(&item.manifest_item_id) {
                continue;
            }
            let child_address = format!("manifest.items[{}]", item.manifest_item_id);
            let existing_link = db::list_artifact_links(
                &conn,
                &ident,
                &db::ArtifactLinkFilters {
                    link_type: Some("task_generated_from_spec"),
                    source_kind: Some("artifact_version"),
                    source_id: Some(&accepted.artifact_version_id),
                    target_kind: Some("task"),
                    target_id: None,
                },
            )?
            .into_iter()
            .find(|link| link.source_child_address.as_deref() == Some(child_address.as_str()));
            if let Some(link) = existing_link {
                if !task_ids.contains(&link.target_id) {
                    task_ids.push(link.target_id.clone());
                }
                if !link_ids.contains(&link.link_id) {
                    link_ids.push(link.link_id.clone());
                }
                item_outputs.push(serde_json::json!({
                    "manifest_item_id": item.manifest_item_id,
                    "task_id": link.target_id,
                    "link_id": link.link_id,
                    "reused": true
                }));
                continue;
            }

            let task = if let Some(task_id) = item.gateway_task_id.as_deref() {
                db::get_task_detail(&conn, &ident, task_id)?.map(|detail| detail.task)
            } else {
                db::find_task_by_spec_source(
                    &conn,
                    &ident,
                    &artifact_id,
                    &accepted.artifact_version_id,
                    &item.manifest_item_id,
                )?
            };
            let task = match task {
                Some(task) => task,
                None => {
                    let mut labels = item.labels.clone();
                    if !labels.iter().any(|label| label == "generated-from-spec") {
                        labels.push("generated-from-spec".to_string());
                    }
                    if let Some(team) = &item.team {
                        labels.push(team.clone());
                    }
                    db::insert_task(
                        &conn,
                        &ident,
                        &generated_spec_task_title(&item),
                        Some(&format!(
                            "Generated from spec artifact {} version {} manifest item {}.",
                            artifact_id, accepted.artifact_version_id, item.manifest_item_id
                        )),
                        Some(&generated_task_specification(
                            &artifact_id,
                            &accepted.artifact_version_id,
                            &item,
                        )),
                        &labels,
                        body.hostname.as_deref(),
                        &reporter,
                    )?
                }
            };
            let link = link_task_to_spec_item(
                &conn,
                &envelope,
                &actor.actor_id,
                &run_write.record.workflow_run_id,
                &accepted.artifact_version_id,
                &item.manifest_item_id,
                &task.id,
            )?;
            db::insert_comment(
                &conn,
                &task.id,
                "system",
                "system",
                &format!(
                    "Generated from spec artifact `{}` accepted version `{}` manifest item `{}`. Compatibility fallback: the current task schema has no dedicated source fields, so the source tuple is embedded in this task's specification/details and mirrored by artifact link `{}`.",
                    artifact_id,
                    accepted.artifact_version_id,
                    item.manifest_item_id,
                    link.record.link_id
                ),
            )?;
            if !task_ids.contains(&task.id) {
                task_ids.push(task.id.clone());
            }
            if !link_ids.contains(&link.record.link_id) {
                link_ids.push(link.record.link_id.clone());
            }
            item_outputs.push(serde_json::json!({
                "manifest_item_id": item.manifest_item_id,
                "task_id": task.id,
                "link_id": link.record.link_id,
                "reused": false
            }));
        }
        let updated_run = db::update_workflow_run(
            &conn,
            &run_write.record.workflow_run_id,
            &db::WorkflowRunUpdate {
                state: Some("succeeded"),
                failure_reason: Some(None),
                generated_contribution_ids: Some(&run_write.record.generated_contribution_ids),
                generated_version_ids: Some(&run_write.record.generated_version_ids),
                generated_task_ids: Some(&task_ids),
                generated_link_ids: Some(&link_ids),
                generated_chunk_ids: Some(&run_write.record.generated_chunk_ids),
                ended_at: None,
            },
        )?
        .ok_or_else(|| anyhow::anyhow!("workflow_run not found"))?;
        Ok::<_, anyhow::Error>((
            actor,
            GenerateSpecTasksResponse {
                artifact_id,
                artifact_version_id: accepted.artifact_version_id,
                workflow_run_id: updated_run.workflow_run_id,
                generated_task_ids: task_ids,
                generated_link_ids: link_ids,
                items: item_outputs,
                replayed: run_write.replayed,
            },
        ))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        serde_json::json!({
            "workflow_run_id": output.workflow_run_id,
            "generated_task_ids": output.generated_task_ids.clone(),
            "generated_link_ids": output.generated_link_ids.clone(),
        }),
        Vec::new(),
        output.replayed,
        output,
    ))
}

pub async fn link_existing_spec_task_handler(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<LinkSpecTaskRequest>,
) -> Result<Response> {
    let (context, authorization) = begin_artifact_mutation(
        &state,
        &headers,
        &ident,
        vec![
            "task.generate_from_spec",
            "workflow_run.start",
            "link.write",
        ],
    )?;
    let manifest_item_id = trim_required(&body.manifest_item_id, "manifest_item_id")?;
    let task_id = trim_required(&body.task_id, "task_id")?;
    let envelope = state.artifact_operations;
    let db = state.db.clone();
    let context_for_db = context.clone();
    let (actor, link) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let detail = db::get_artifact(&conn, &ident, &artifact_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        if detail.artifact.kind != "spec" {
            anyhow::bail!("artifact is not a spec");
        }
        db::get_task_detail(&conn, &ident, &task_id)?
            .ok_or_else(|| anyhow::anyhow!("task not found"))?;
        let version = if let Some(version_id) = body.version_id {
            db::get_artifact_version(&conn, &ident, &artifact_id, &version_id)?
                .ok_or_else(|| anyhow::anyhow!("version not found"))?
        } else {
            detail
                .accepted_version
                .ok_or_else(|| anyhow::anyhow!("accepted spec version required"))?
        };
        let manifest = spec_manifest_from_version(&artifact_id, &version)
            .map_err(|err| anyhow::anyhow!(err.1))?;
        if !manifest
            .items
            .iter()
            .any(|item| item.manifest_item_id == manifest_item_id)
        {
            anyhow::bail!("manifest_item not found");
        }
        let actor = resolve_artifact_actor(
            &conn,
            &context_for_db.actor,
            body.actor_display_name.as_deref(),
        )?;
        let read_set = serde_json::json!({
            "artifact_version_ids": [version.artifact_version_id.clone()]
        });
        let run_write = db::start_workflow_run(
            &conn,
            &envelope,
            &db::WorkflowRunInsert {
                artifact_id: &artifact_id,
                workflow_kind: "spec_task_generation",
                phase: Some("link_existing_task"),
                round_id: None,
                coordinator_actor_id: &actor.actor_id,
                participant_actor_ids: &[],
                source_artifact_version_id: Some(&version.artifact_version_id),
                read_set: Some(&read_set),
                idempotency_key: Some(&context_for_db.idempotency_key),
                is_resumable: true,
            },
        )?;
        ensure_workflow_run_retryable(&run_write.record)?;
        let link = link_task_to_spec_item(
            &conn,
            &envelope,
            &actor.actor_id,
            &run_write.record.workflow_run_id,
            &version.artifact_version_id,
            &manifest_item_id,
            &task_id,
        )?;
        let mut task_ids = run_write.record.generated_task_ids;
        let mut link_ids = run_write.record.generated_link_ids;
        if !task_ids.contains(&task_id) {
            task_ids.push(task_id.clone());
        }
        if !link_ids.contains(&link.record.link_id) {
            link_ids.push(link.record.link_id.clone());
        }
        db::update_workflow_run(
            &conn,
            &run_write.record.workflow_run_id,
            &db::WorkflowRunUpdate {
                state: Some("succeeded"),
                failure_reason: Some(None),
                generated_contribution_ids: Some(&run_write.record.generated_contribution_ids),
                generated_version_ids: Some(&run_write.record.generated_version_ids),
                generated_task_ids: Some(&task_ids),
                generated_link_ids: Some(&link_ids),
                generated_chunk_ids: Some(&run_write.record.generated_chunk_ids),
                ended_at: None,
            },
        )?;
        Ok::<_, anyhow::Error>((actor, link))
    })
    .await
    .map_err(AppError::from)?
    .map_err(map_db_error)?;
    Ok(finalize_mutation(
        actor,
        &context,
        authorization,
        generated_resource("link_id", link.record.link_id.clone()),
        link.warnings,
        link.replayed,
        link.record,
    ))
}

// ── Theme (GET/POST /theme) ──────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ThemeResponse {
    pub theme: String,
}

#[derive(Deserialize)]
pub struct ThemeRequest {
    pub theme: String,
}

pub async fn get_theme(State(state): State<AppState>) -> Result<Json<ThemeResponse>> {
    let db = state.db.clone();
    let theme = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_theme(&conn)
    })
    .await??;
    Ok(Json(ThemeResponse { theme }))
}

pub async fn set_theme(
    State(state): State<AppState>,
    Json(body): Json<ThemeRequest>,
) -> Result<Json<ThemeResponse>> {
    let theme = body.theme.trim().to_lowercase();
    if theme != "light" && theme != "dark" {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("unsupported theme '{}': must be 'light' or 'dark'", theme),
        ));
    }
    let db = state.db.clone();
    let t = theme.clone();
    spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::set_theme(&conn, &t)
    })
    .await??;
    Ok(Json(ThemeResponse { theme }))
}

// ── Eventic configuration + build status ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventicServer {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct EventicServerInput {
    pub id: Option<String>,
    pub name: String,
    pub base_url: String,
    pub enabled: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct RepoMappingRequest {
    pub provider: Option<String>,
    pub namespace: Option<String>,
    pub repo_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BulkRepoMappingRequest {
    pub provider: String,
    pub namespace: String,
}

#[derive(Debug, Serialize)]
pub struct BulkRepoMappingResponse {
    pub updated: usize,
}

#[derive(Debug, Serialize)]
pub struct EventicProjectSource {
    pub server_id: String,
    pub server_name: String,
    pub base_url: String,
    pub projects: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ProjectBuildStatus {
    pub project_ident: String,
    pub repo_provider: Option<String>,
    pub repo_full_name: Option<String>,
    pub server_id: Option<String>,
    pub server_name: Option<String>,
    pub base_url: Option<String>,
    pub status: Option<Value>,
    pub hint: Option<String>,
}

fn normalize_eventic_server(input: EventicServerInput, existing_id: Option<&str>) -> EventicServer {
    let base_url = input.base_url.trim().trim_end_matches('/').to_string();
    let raw_id = input
        .id
        .as_deref()
        .or(existing_id)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let seed = if input.name.trim().is_empty() {
                &base_url
            } else {
                input.name.trim()
            };
            sanitize_ident(seed)
        });
    EventicServer {
        id: sanitize_ident(&raw_id),
        name: input.name.trim().to_string(),
        base_url,
        enabled: input
            .enabled
            .as_ref()
            .and_then(|v| match v {
                Value::Bool(b) => Some(*b),
                Value::String(s) => Some(s == "true" || s == "on" || s == "1"),
                Value::Number(n) => Some(n.as_i64().unwrap_or_default() != 0),
                _ => None,
            })
            .unwrap_or(true),
    }
}

fn load_eventic_servers(conn: &rusqlite::Connection) -> anyhow::Result<Vec<EventicServer>> {
    match db::get_setting(conn, EVENTIC_SERVERS_SETTING)? {
        Some(raw) => Ok(serde_json::from_str(&raw)?),
        None => Ok(Vec::new()),
    }
}

fn save_eventic_servers(
    conn: &rusqlite::Connection,
    servers: &[EventicServer],
) -> anyhow::Result<()> {
    db::set_setting(
        conn,
        EVENTIC_SERVERS_SETTING,
        &serde_json::to_string(servers)?,
    )?;
    Ok(())
}

async fn fetch_eventic_projects(server: &EventicServer) -> anyhow::Result<Vec<String>> {
    let url = format!("{}/projects", server.base_url.trim_end_matches('/'));
    let projects = reqwest::Client::new()
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<String>>()
        .await?;
    Ok(projects)
}

async fn fetch_eventic_project_status(
    server: &EventicServer,
    repo_full_name: &str,
) -> anyhow::Result<Value> {
    let url = format!(
        "{}/projects/{}",
        server.base_url.trim_end_matches('/'),
        repo_full_name.trim_start_matches('/')
    );
    let status = reqwest::Client::new()
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    Ok(status)
}

pub async fn get_eventic_servers(
    State(state): State<AppState>,
) -> Result<Json<Vec<EventicServer>>> {
    let db = state.db.clone();
    let servers = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        load_eventic_servers(&conn)
    })
    .await??;
    Ok(Json(servers))
}

pub async fn replace_eventic_servers(
    State(state): State<AppState>,
    Json(servers): Json<Vec<EventicServer>>,
) -> Result<Json<Vec<EventicServer>>> {
    let db = state.db.clone();
    let saved = spawn_blocking(move || -> anyhow::Result<Vec<EventicServer>> {
        let conn = db.lock().unwrap();
        save_eventic_servers(&conn, &servers)?;
        Ok(servers)
    })
    .await??;
    Ok(Json(saved))
}

pub async fn add_eventic_server(
    State(state): State<AppState>,
    Json(input): Json<EventicServerInput>,
) -> Result<Json<Vec<EventicServer>>> {
    let db = state.db.clone();
    let servers = spawn_blocking(move || -> anyhow::Result<Vec<EventicServer>> {
        let conn = db.lock().unwrap();
        let mut servers = load_eventic_servers(&conn)?;
        let mut server = normalize_eventic_server(input, None);
        if server.id.is_empty() {
            server.id = format!("eventic-{}", now_ms());
        }
        let original_id = server.id.clone();
        let mut suffix = 2;
        while servers.iter().any(|s| s.id == server.id) {
            server.id = format!("{original_id}-{suffix}");
            suffix += 1;
        }
        servers.push(server);
        save_eventic_servers(&conn, &servers)?;
        Ok(servers)
    })
    .await??;
    Ok(Json(servers))
}

pub async fn update_eventic_server(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<EventicServerInput>,
) -> Result<Json<Vec<EventicServer>>> {
    let db = state.db.clone();
    let servers = spawn_blocking(move || -> anyhow::Result<Vec<EventicServer>> {
        let conn = db.lock().unwrap();
        let mut servers = load_eventic_servers(&conn)?;
        let idx = servers
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| anyhow::anyhow!("eventic server '{id}' not found"))?;
        servers[idx] = normalize_eventic_server(input, Some(&id));
        save_eventic_servers(&conn, &servers)?;
        Ok(servers)
    })
    .await??;
    Ok(Json(servers))
}

pub async fn delete_eventic_server(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<EventicServer>>> {
    let db = state.db.clone();
    let servers = spawn_blocking(move || -> anyhow::Result<Vec<EventicServer>> {
        let conn = db.lock().unwrap();
        let mut servers = load_eventic_servers(&conn)?;
        servers.retain(|s| s.id != id);
        save_eventic_servers(&conn, &servers)?;
        Ok(servers)
    })
    .await??;
    Ok(Json(servers))
}

pub async fn list_eventic_projects(
    State(state): State<AppState>,
) -> Result<Json<Vec<EventicProjectSource>>> {
    let db = state.db.clone();
    let servers = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        load_eventic_servers(&conn)
    })
    .await??;

    let mut out = Vec::new();
    for server in servers.iter().filter(|s| s.enabled) {
        if let Ok(projects) = fetch_eventic_projects(server).await {
            out.push(EventicProjectSource {
                server_id: server.id.clone(),
                server_name: server.name.clone(),
                base_url: server.base_url.clone(),
                projects,
            });
        }
    }
    Ok(Json(out))
}

pub async fn update_project_repo_mapping(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    Json(req): Json<RepoMappingRequest>,
) -> Result<Json<Project>> {
    let db = state.db.clone();
    let project = spawn_blocking(move || -> anyhow::Result<Project> {
        let conn = db.lock().unwrap();
        db::update_project_repo_mapping(
            &conn,
            &ident,
            req.provider.as_deref(),
            req.namespace.as_deref(),
            req.repo_name.as_deref(),
        )?
        .ok_or_else(|| anyhow::anyhow!("project '{ident}' not found"))
    })
    .await??;
    Ok(Json(project))
}

pub async fn bulk_update_project_repo_mappings(
    State(state): State<AppState>,
    Json(req): Json<BulkRepoMappingRequest>,
) -> Result<Json<BulkRepoMappingResponse>> {
    let db = state.db.clone();
    let updated = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::bulk_fill_missing_repo_mappings(&conn, &req.provider, &req.namespace)
    })
    .await??;
    Ok(Json(BulkRepoMappingResponse { updated }))
}

pub async fn get_project_eventic_status(
    State(state): State<AppState>,
    Path(ident): Path<String>,
) -> Result<Json<ProjectBuildStatus>> {
    let db = state.db.clone();
    let (project, servers) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        let project = db::get_project(&conn, &ident)?
            .ok_or_else(|| anyhow::anyhow!("project '{ident}' not found"))?;
        Ok((project, load_eventic_servers(&conn)?))
    })
    .await??;

    let Some(repo_full_name) = project.repo_full_name.clone() else {
        return Ok(Json(ProjectBuildStatus {
            project_ident: project.ident,
            repo_provider: project.repo_provider,
            repo_full_name: None,
            server_id: None,
            server_name: None,
            base_url: None,
            status: None,
            hint: Some("No repository mapping is configured for this gateway project. Add a provider, namespace, and repository in Settings, or tell the user Eventic is not configured so build information is unavailable.".into()),
        }));
    };

    let enabled: Vec<_> = servers.into_iter().filter(|s| s.enabled).collect();
    if enabled.is_empty() {
        return Ok(Json(ProjectBuildStatus {
            project_ident: project.ident,
            repo_provider: project.repo_provider,
            repo_full_name: Some(repo_full_name),
            server_id: None,
            server_name: None,
            base_url: None,
            status: None,
            hint: Some("No enabled Eventic servers are configured. Add an Eventic server in Settings to expose build information.".into()),
        }));
    }

    for server in &enabled {
        if let Ok(status) = fetch_eventic_project_status(server, &repo_full_name).await {
            return Ok(Json(ProjectBuildStatus {
                project_ident: project.ident,
                repo_provider: project.repo_provider,
                repo_full_name: Some(repo_full_name),
                server_id: Some(server.id.clone()),
                server_name: Some(server.name.clone()),
                base_url: Some(server.base_url.clone()),
                status: Some(status),
                hint: None,
            }));
        }
    }

    Ok(Json(ProjectBuildStatus {
        project_ident: project.ident,
        repo_provider: project.repo_provider,
        repo_full_name: Some(repo_full_name),
        server_id: None,
        server_name: None,
        base_url: None,
        status: None,
        hint: Some("The mapped repository was not found on any enabled Eventic server, or the servers were unreachable.".into()),
    }))
}

// ── Agent API docs ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ListApiDocsQuery {
    pub q: Option<String>,
    pub app: Option<String>,
    pub label: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub include_history: bool,
    #[serde(default)]
    pub envelope: bool,
}

#[derive(Deserialize)]
pub struct CreateApiDocRequest {
    pub app: String,
    pub title: String,
    pub summary: Option<String>,
    #[serde(default = "default_api_doc_kind")]
    pub kind: String,
    #[serde(default = "default_api_doc_source_format")]
    pub source_format: String,
    pub source_ref: Option<String>,
    pub version: Option<String>,
    #[serde(default)]
    pub labels: Value,
    pub content: Value,
    pub author: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateApiDocRequest {
    pub app: Option<String>,
    pub title: Option<String>,
    pub summary: Option<Value>,
    pub kind: Option<String>,
    pub source_format: Option<String>,
    pub source_ref: Option<Value>,
    pub version: Option<Value>,
    pub labels: Option<Value>,
    pub content: Option<Value>,
}

#[derive(Serialize)]
pub struct ApiDocChunk {
    pub doc_id: String,
    pub project_ident: String,
    pub app: String,
    pub title: String,
    pub chunk_type: String,
    pub labels: Vec<String>,
    pub text: String,
    pub updated_at: i64,
}

fn default_api_doc_kind() -> String {
    "agent_context".to_string()
}

fn default_api_doc_source_format() -> String {
    "agent_context".to_string()
}

fn optional_string_field(field: &str, value: Value) -> Result<Option<String>> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s)),
        _ => Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("{field} must be a string or null"),
        )),
    }
}

fn validate_api_doc_input(
    app: Option<&str>,
    title: Option<&str>,
    kind: Option<&str>,
    source_format: Option<&str>,
) -> Result<()> {
    if app.is_some_and(|v| v.trim().is_empty()) {
        return Err(AppError(StatusCode::BAD_REQUEST, "app is required".into()));
    }
    if title.is_some_and(|v| v.trim().is_empty()) {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "title is required".into(),
        ));
    }
    if kind.is_some_and(|v| v.trim().is_empty()) {
        return Err(AppError(StatusCode::BAD_REQUEST, "kind is required".into()));
    }
    if source_format.is_some_and(|v| v.trim().is_empty()) {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "source_format is required".into(),
        ));
    }
    Ok(())
}

fn api_doc_chunks(doc: &db::ApiDoc) -> Vec<ApiDocChunk> {
    let mut chunks = Vec::new();
    let mut overview = format!("{} ({})", doc.title, doc.app);
    if let Some(summary) = doc.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        overview.push_str("\n\n");
        overview.push_str(summary.trim());
    }
    overview.push_str("\n\nkind: ");
    overview.push_str(&doc.kind);
    overview.push_str("\nsource_format: ");
    overview.push_str(&doc.source_format);
    if let Some(version) = doc.version.as_deref().filter(|s| !s.trim().is_empty()) {
        overview.push_str("\nversion: ");
        overview.push_str(version.trim());
    }
    chunks.push(ApiDocChunk {
        doc_id: doc.id.clone(),
        project_ident: doc.project_ident.clone(),
        app: doc.app.clone(),
        title: doc.title.clone(),
        chunk_type: "overview".to_string(),
        labels: doc.labels.clone(),
        text: overview,
        updated_at: doc.updated_at,
    });

    if let Value::Object(map) = &doc.content {
        for key in [
            "purpose",
            "workflows",
            "endpoints",
            "auth",
            "safety",
            "relationships",
            "examples",
            "operations",
            "schemas",
        ] {
            if let Some(value) = map.get(key) {
                let rendered = match value {
                    Value::String(s) => s.clone(),
                    _ => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
                };
                if rendered.trim().is_empty() {
                    continue;
                }
                chunks.push(ApiDocChunk {
                    doc_id: doc.id.clone(),
                    project_ident: doc.project_ident.clone(),
                    app: doc.app.clone(),
                    title: doc.title.clone(),
                    chunk_type: key.to_string(),
                    labels: doc.labels.clone(),
                    text: rendered,
                    updated_at: doc.updated_at,
                });
            }
        }
    } else {
        chunks.push(ApiDocChunk {
            doc_id: doc.id.clone(),
            project_ident: doc.project_ident.clone(),
            app: doc.app.clone(),
            title: doc.title.clone(),
            chunk_type: "content".to_string(),
            labels: doc.labels.clone(),
            text: doc.content.to_string(),
            updated_at: doc.updated_at,
        });
    }

    chunks
}

pub async fn list_api_docs_handler(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    Query(q): Query<ListApiDocsQuery>,
) -> Result<Json<Vec<db::ApiDocSummary>>> {
    let db = state.db.clone();
    let (project_exists, docs) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Ok::<_, anyhow::Error>((false, Vec::new()));
        }
        let filters = db::ApiDocFilters {
            query: q.q.as_deref(),
            app: q.app.as_deref(),
            label: q.label.as_deref(),
            kind: q.kind.as_deref(),
        };
        let docs = db::list_api_docs(&conn, &ident, &filters)?;
        Ok((true, docs))
    })
    .await??;
    if !project_exists {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            "project not found".to_string(),
        ));
    }
    Ok(Json(docs))
}

pub async fn create_api_doc_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ident): Path<String>,
    Json(req): Json<CreateApiDocRequest>,
) -> Result<Json<db::ApiDoc>> {
    validate_api_doc_input(
        Some(req.app.as_str()),
        Some(req.title.as_str()),
        Some(req.kind.as_str()),
        Some(req.source_format.as_str()),
    )?;
    if req.content.is_null() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "content is required".into(),
        ));
    }
    let labels = decode_labels_field("labels", Some(req.labels))?;
    let author = resolve_identity(req.author, &headers);
    let db = state.db.clone();
    let doc = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Ok::<_, anyhow::Error>(None);
        }
        let doc = db::insert_api_doc(
            &conn,
            &ident,
            &db::ApiDocInsert {
                app: req.app.trim(),
                title: req.title.trim(),
                summary: req
                    .summary
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                kind: req.kind.trim(),
                source_format: req.source_format.trim(),
                source_ref: req
                    .source_ref
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                version: req
                    .version
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                labels: &labels,
                content: &req.content,
                author: &author,
            },
        )?;
        Ok(Some(doc))
    })
    .await??;
    match doc {
        Some(doc) => Ok(Json(doc)),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            "project not found".to_string(),
        )),
    }
}

pub async fn get_api_doc_handler(
    State(state): State<AppState>,
    Path((ident, id)): Path<(String, String)>,
) -> Result<Json<db::ApiDoc>> {
    let db = state.db.clone();
    let doc = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_api_doc(&conn, &ident, &id)
    })
    .await??;
    match doc {
        Some(doc) => Ok(Json(doc)),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            "api doc not found".to_string(),
        )),
    }
}

pub async fn update_api_doc_handler(
    State(state): State<AppState>,
    Path((ident, id)): Path<(String, String)>,
    Json(req): Json<UpdateApiDocRequest>,
) -> Result<Json<db::ApiDoc>> {
    validate_api_doc_input(
        req.app.as_deref(),
        req.title.as_deref(),
        req.kind.as_deref(),
        req.source_format.as_deref(),
    )?;
    let labels = match req.labels {
        Some(labels) => Some(decode_labels_field("labels", Some(labels))?),
        None => None,
    };
    let summary = match req.summary {
        Some(value) => Some(optional_string_field("summary", value)?),
        None => None,
    };
    let source_ref = match req.source_ref {
        Some(value) => Some(optional_string_field("source_ref", value)?),
        None => None,
    };
    let version = match req.version {
        Some(value) => Some(optional_string_field("version", value)?),
        None => None,
    };
    let db = state.db.clone();
    let doc = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::update_api_doc(
            &conn,
            &ident,
            &id,
            &db::ApiDocUpdate {
                app: req.app.as_deref().map(str::trim),
                title: req.title.as_deref().map(str::trim),
                summary: summary
                    .as_ref()
                    .map(|value| value.as_deref().map(str::trim).filter(|s| !s.is_empty())),
                kind: req.kind.as_deref().map(str::trim),
                source_format: req.source_format.as_deref().map(str::trim),
                source_ref: source_ref
                    .as_ref()
                    .map(|value| value.as_deref().map(str::trim).filter(|s| !s.is_empty())),
                version: version
                    .as_ref()
                    .map(|value| value.as_deref().map(str::trim).filter(|s| !s.is_empty())),
                labels: labels.as_deref(),
                content: req.content.as_ref(),
            },
        )
    })
    .await??;
    match doc {
        Some(doc) => Ok(Json(doc)),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            "api doc not found".to_string(),
        )),
    }
}

pub async fn delete_api_doc_handler(
    State(state): State<AppState>,
    Path((ident, id)): Path<(String, String)>,
) -> Result<Json<DeleteResponse>> {
    let db = state.db.clone();
    let deleted = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::delete_api_doc(&conn, &ident, &id)
    })
    .await??;
    if deleted {
        Ok(Json(DeleteResponse { deleted: true }))
    } else {
        Err(AppError(
            StatusCode::NOT_FOUND,
            "api doc not found".to_string(),
        ))
    }
}

pub async fn list_api_doc_chunks_handler(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    Query(q): Query<ListApiDocsQuery>,
) -> Result<Response> {
    let db = state.db.clone();
    let envelope = q.envelope;
    let (project_exists, chunk_list) = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Ok::<_, anyhow::Error>((
                false,
                db::ApiDocChunkList {
                    chunks: Vec::new(),
                    chunking_status: db::ApiDocChunkingStatus {
                        status: "none".to_string(),
                        current_chunk_count: 0,
                        stale_chunk_count: 0,
                        superseded_chunk_count: 0,
                        failed_addresses: Vec::new(),
                    },
                    retrieval_scope: "current".to_string(),
                    include_history: false,
                },
            ));
        }
        let filters = db::ApiDocFilters {
            query: q.q.as_deref(),
            app: q.app.as_deref(),
            label: q.label.as_deref(),
            kind: q.kind.as_deref(),
        };
        let chunks = db::list_api_doc_chunks(&conn, &ident, &filters, q.include_history)?;
        Ok::<_, anyhow::Error>((true, chunks))
    })
    .await??;
    if !project_exists {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            "project not found".to_string(),
        ));
    }
    if envelope {
        Ok(Json(chunk_list).into_response())
    } else {
        Ok(Json(chunk_list.chunks).into_response())
    }
}

// ── POST /v1/projects ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RegisterProjectRequest {
    /// Raw project identity (git remote URL or directory name).
    pub ident: String,
    /// Which channel plugin to use. Defaults to gateway's DEFAULT_CHANNEL.
    pub channel: Option<String>,
}

#[derive(Serialize)]
pub struct RegisterProjectResponse {
    pub ident: String,
    pub channel_name: String,
    pub room_id: String,
}

pub async fn register_project(
    State(state): State<AppState>,
    Json(body): Json<RegisterProjectRequest>,
) -> Result<Json<RegisterProjectResponse>> {
    let project_ident = sanitize_ident(&body.ident);
    let channel_name = body
        .channel
        .unwrap_or_else(|| state.default_channel.clone());

    // Return existing project immediately (idempotent).
    {
        let conn = state.db.lock().unwrap();
        if let Some(existing) = db::get_project(&conn, &project_ident)? {
            return Ok(Json(RegisterProjectResponse {
                ident: existing.ident,
                channel_name: existing.channel_name,
                room_id: existing.room_id,
            }));
        }
    }

    // Look up the requested plugin.
    let plugin = state
        .plugins
        .get(&channel_name)
        .ok_or_else(|| {
            AppError(
                StatusCode::BAD_REQUEST,
                format!("unknown channel plugin: '{channel_name}'"),
            )
        })?
        .clone();

    // Plugin creates/finds the room.
    let room_id = plugin.ensure_room(&project_ident).await?;

    // Persist.
    let project = Project {
        ident: project_ident.clone(),
        channel_name: channel_name.clone(),
        room_id: room_id.clone(),
        last_msg_id: None,
        created_at: now_ms(),
        repo_provider: None,
        repo_namespace: None,
        repo_name: None,
        repo_full_name: None,
    };

    let db = state.db.clone();
    let project_clone = project.clone();
    spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::insert_project(&conn, &project_clone)
    })
    .await??;

    Ok(Json(RegisterProjectResponse {
        ident: project_ident,
        channel_name,
        room_id,
    }))
}

// ── POST /v1/projects/:ident/messages ─────────────────────────────────────────

#[derive(Deserialize)]
pub struct SendMessageRequest {
    /// Back-compat alias for `body`. If both are set, `body` wins.
    pub content: Option<String>,
    pub body: Option<String>,
    pub subject: Option<String>,
    pub hostname: Option<String>,
    /// Event time in epoch milliseconds. Defaults to now() when omitted.
    pub event_at: Option<i64>,
}

#[derive(Serialize)]
pub struct SendMessageResponse {
    pub message_id: i64,
    pub external_message_id: String,
}

pub async fn send_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ident): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>> {
    let (channel_name, room_id) = {
        let conn = state.db.lock().unwrap();
        match db::get_project(&conn, &ident)? {
            Some(p) => (p.channel_name, p.room_id),
            None => {
                return Err(AppError(
                    StatusCode::NOT_FOUND,
                    format!("project '{}' not found", ident),
                ))
            }
        }
    };

    let plugin = state
        .plugins
        .get(&channel_name)
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("channel plugin '{channel_name}' is not configured"),
            )
        })?
        .clone();

    let agent_id = extract_agent_id(&headers);
    let body_text = req.body.or(req.content).unwrap_or_default();
    if body_text.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "request must include non-empty 'body' (or 'content')".into(),
        ));
    }
    let outbound = build_outbound(
        &agent_id,
        body_text,
        req.subject,
        req.hostname,
        req.event_at,
    );
    let external_id = plugin.send_structured(&room_id, &outbound).await?;

    let msg = Message {
        id: 0,
        project_ident: ident.clone(),
        source: "agent".into(),
        external_message_id: Some(external_id.clone()),
        content: outbound.body.clone(),
        sent_at: now_ms(),
        confirmed_at: None,
        parent_message_id: None,
        agent_id: Some(agent_id.clone()),
        message_type: "message".into(),
        subject: Some(outbound.subject.clone()),
        hostname: Some(outbound.hostname.clone()),
        event_at: Some(outbound.event_at),
        deliver_to_agents: false,
    };

    let db = state.db.clone();
    let ident_clone = ident.clone();
    let aid = agent_id;
    let row_id = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_agent(&conn, &ident_clone, &aid)?;
        db::insert_message(&conn, &msg)
    })
    .await??;

    Ok(Json(SendMessageResponse {
        message_id: row_id,
        external_message_id: external_id,
    }))
}

// ── Skills API ────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct SkillUploadResponse {
    pub name: String,
    pub kind: String,
    pub size: i64,
    pub checksum: String,
}

pub async fn upload_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<SkillUploadResponse>> {
    use sha2::{Digest, Sha256};

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/zip");

    // X-Kind header takes precedence; fall back to Content-Type detection.
    let kind = match headers
        .get("x-kind")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_lowercase())
        .as_deref()
    {
        Some("skill") => "skill".to_string(),
        Some("command") => "command".to_string(),
        Some("agent") => "agent".to_string(),
        Some(other) => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                format!("invalid X-Kind: '{other}'"),
            ))
        }
        None => {
            if content_type.starts_with("text/markdown") {
                "command".to_string()
            } else {
                "skill".to_string()
            }
        }
    };

    let is_text = kind == "command" || kind == "agent";

    let (zip_data, content, size) = if is_text {
        let text = String::from_utf8(body.to_vec())
            .map_err(|_| AppError(StatusCode::BAD_REQUEST, "body is not valid UTF-8".into()))?;
        if text.is_empty() {
            return Err(AppError(StatusCode::BAD_REQUEST, "body is empty".into()));
        }
        let size = text.len() as i64;
        (vec![], Some(text), size)
    } else {
        let zip = body.to_vec();
        let size = zip.len() as i64;
        (zip, None, size)
    };

    let mut hasher = Sha256::new();
    match &content {
        Some(text) => hasher.update(text.as_bytes()),
        None => hasher.update(&zip_data),
    }
    let checksum = hex::encode(hasher.finalize());

    let record = db::SkillRecord {
        name: name.clone(),
        kind: kind.clone(),
        zip_data,
        content,
        size,
        checksum: checksum.clone(),
        uploaded_at: db::now_ms(),
    };
    let db = state.db.clone();
    spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_skill(&conn, &record)
    })
    .await??;

    Ok(Json(SkillUploadResponse {
        name,
        kind,
        size,
        checksum,
    }))
}

/// Multipart variant of the skill-upload endpoint. Accepts a form with:
///   - `kind` (text): `"skill" | "command" | "agent"` — required
///   - `content` (text): markdown body — required when kind is `command|agent`
///   - `file` (binary): zip bytes — required when kind is `skill`
///
/// Designed for ndesign's `data-nd-action` form serializer, which posts
/// `multipart/form-data` when any field is a file. Persists into the same
/// `skills` table as the raw-body PUT variant.
pub async fn upload_skill_multipart(
    State(state): State<AppState>,
    Path(name): Path<String>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<SkillUploadResponse>> {
    use sha2::{Digest, Sha256};

    let mut kind_field: Option<String> = None;
    let mut content_field: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        AppError(
            StatusCode::BAD_REQUEST,
            format!("failed to parse multipart body: {e}"),
        )
    })? {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "kind" => {
                let text = field.text().await.map_err(|e| {
                    AppError(
                        StatusCode::BAD_REQUEST,
                        format!("failed to read 'kind' field: {e}"),
                    )
                })?;
                kind_field = Some(text.trim().to_lowercase());
            }
            "content" => {
                let text = field.text().await.map_err(|e| {
                    AppError(
                        StatusCode::BAD_REQUEST,
                        format!("failed to read 'content' field: {e}"),
                    )
                })?;
                content_field = Some(text);
            }
            "file" => {
                let bytes = field.bytes().await.map_err(|e| {
                    AppError(
                        StatusCode::BAD_REQUEST,
                        format!("failed to read 'file' field: {e}"),
                    )
                })?;
                file_bytes = Some(bytes.to_vec());
            }
            _ => {
                // Silently ignore unknown fields — ndesign's serializer may
                // add incidental metadata fields that are not part of the
                // upload contract.
            }
        }
    }

    let kind = match kind_field.as_deref() {
        Some("skill") => "skill".to_string(),
        Some("command") => "command".to_string(),
        Some("agent") => "agent".to_string(),
        Some(other) => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                format!("invalid 'kind': '{other}' (must be skill|command|agent)"),
            ))
        }
        None => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "'kind' field is required".into(),
            ))
        }
    };

    let (zip_data, content, size) = match kind.as_str() {
        "skill" => {
            let bytes = file_bytes.ok_or_else(|| {
                AppError(
                    StatusCode::BAD_REQUEST,
                    "'file' field is required when kind is 'skill'".into(),
                )
            })?;
            if bytes.is_empty() {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    "'file' must be non-empty".into(),
                ));
            }
            let size = bytes.len() as i64;
            (bytes, None, size)
        }
        _ => {
            // command | agent
            let text = content_field.ok_or_else(|| {
                AppError(
                    StatusCode::BAD_REQUEST,
                    format!("'content' field is required when kind is '{kind}'"),
                )
            })?;
            if text.trim().is_empty() {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    "'content' must be non-empty".into(),
                ));
            }
            let size = text.len() as i64;
            (vec![], Some(text), size)
        }
    };

    let mut hasher = Sha256::new();
    match &content {
        Some(text) => hasher.update(text.as_bytes()),
        None => hasher.update(&zip_data),
    }
    let checksum = hex::encode(hasher.finalize());

    let record = db::SkillRecord {
        name: name.clone(),
        kind: kind.clone(),
        zip_data,
        content,
        size,
        checksum: checksum.clone(),
        uploaded_at: db::now_ms(),
    };
    let db = state.db.clone();
    spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_skill(&conn, &record)
    })
    .await??;

    Ok(Json(SkillUploadResponse {
        name,
        kind,
        size,
        checksum,
    }))
}

// ── POST /v1/skills/:name (JSON upsert for command/agent) ────────────────────

/// Request body for the JSON upsert endpoint.
///
/// `kind` must be `"command"` or `"agent"` — zip-backed skills cannot be
/// upserted via this endpoint and must use the existing `PUT` (raw body) or
/// `POST .../multipart` variants, which accept binary data.
///
/// `content` is the markdown body and must be non-empty after trimming.
#[derive(Deserialize)]
pub struct JsonSkillRequest {
    pub kind: String,
    pub content: String,
}

#[derive(Deserialize)]
pub struct NamedJsonSkillRequest {
    pub name: String,
    pub kind: String,
    pub content: String,
}

async fn persist_text_skill(
    state: AppState,
    name: String,
    kind: String,
    content: String,
) -> Result<Json<SkillUploadResponse>> {
    use sha2::{Digest, Sha256};

    if name.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "'name' must be non-empty".into(),
        ));
    }

    let kind = match kind.as_str() {
        "command" => "command".to_string(),
        "agent" => "agent".to_string(),
        other => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                format!(
                    "invalid 'kind': '{other}' (must be 'command' or 'agent'; \
                     zip skills use PUT /v1/skills/:name)"
                ),
            ))
        }
    };

    if content.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "'content' must be non-empty".into(),
        ));
    }

    let name = name.trim().to_string();
    let size = content.len() as i64;
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let checksum = hex::encode(hasher.finalize());

    let record = db::SkillRecord {
        name: name.clone(),
        kind: kind.clone(),
        zip_data: vec![],
        content: Some(content),
        size,
        checksum: checksum.clone(),
        uploaded_at: db::now_ms(),
    };
    let db = state.db.clone();
    spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_skill(&conn, &record)
    })
    .await??;

    Ok(Json(SkillUploadResponse {
        name,
        kind,
        size,
        checksum,
    }))
}

/// Create-or-update a text-kind skill (`command` or `agent`) via JSON.
///
/// ndesign's `data-nd-action` on a `<form>` serializes named inputs into a
/// JSON body (not multipart). This endpoint is the JSON-native create/edit
/// path used by the Commands and Agents control-panel pages. Zip skills are
/// not supported here — they are managed exclusively from the agent-tools
/// CLI via the raw-body `PUT` endpoint.
///
/// Validation:
/// * `name` — must be non-empty (400).
/// * `kind` — must be exactly `"command"` or `"agent"` (400 on anything else,
///   including `"skill"`).
/// * `content` — must be non-empty after `trim()` (400).
///
/// On success the same `SkillUploadResponse` shape as the raw-body PUT is
/// returned so clients can treat the two endpoints uniformly.
pub async fn upsert_skill_json(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<JsonSkillRequest>,
) -> Result<Json<SkillUploadResponse>> {
    persist_text_skill(state, name, req.kind, req.content).await
}

pub async fn upsert_named_skill_json(
    State(state): State<AppState>,
    Json(req): Json<NamedJsonSkillRequest>,
) -> Result<Json<SkillUploadResponse>> {
    persist_text_skill(state, req.name, req.kind, req.content).await
}

#[derive(Deserialize)]
pub struct ListSkillsQuery {
    /// Optional filter — when set, restrict the response to a single kind.
    pub kind: Option<String>,
}

pub async fn list_skills_handler(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListSkillsQuery>,
) -> Result<Json<Vec<db::SkillMeta>>> {
    let kind = match q.kind.as_deref() {
        None | Some("") => None,
        Some(k) => {
            if k != "skill" && k != "command" && k != "agent" {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    format!("invalid kind '{k}': must be skill|command|agent"),
                ));
            }
            Some(k.to_string())
        }
    };

    let db = state.db.clone();
    let skills = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::list_skills(&conn, kind.as_deref())
    })
    .await??;
    Ok(Json(skills))
}

pub async fn download_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse> {
    let db = state.db.clone();
    let record = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_skill(&conn, &name)
    })
    .await??;

    match record {
        None => Err(AppError(StatusCode::NOT_FOUND, "not found".into())),
        Some(r) if r.kind == "command" || r.kind == "agent" => {
            let text = r.content.unwrap_or_default();
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/markdown; charset=utf-8"),
            );
            if let Ok(v) = HeaderValue::from_str(&r.kind) {
                headers.insert("x-kind", v);
            }
            let cd = format!("attachment; filename=\"{}.md\"", r.name);
            headers.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&cd).unwrap_or(HeaderValue::from_static("attachment")),
            );
            Ok((headers, text.into_bytes()))
        }
        Some(r) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            );
            if let Ok(v) = HeaderValue::from_str(&r.kind) {
                headers.insert("x-kind", v);
            }
            let cd = format!("attachment; filename=\"{}.zip\"", r.name);
            headers.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&cd).unwrap_or(HeaderValue::from_static("attachment")),
            );
            Ok((headers, r.zip_data))
        }
    }
}

pub async fn delete_skill_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode> {
    let db = state.db.clone();
    let existed = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::delete_skill(&conn, &name)
    })
    .await??;

    if existed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError(StatusCode::NOT_FOUND, "skill not found".into()))
    }
}

// ── GET /v1/skills/:name/content ──────────────────────────────────────────────

#[derive(Serialize)]
pub struct SkillContentResponse {
    pub name: String,
    pub kind: String,
    pub content: Option<String>,
}

pub async fn get_skill_content(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SkillContentResponse>> {
    let db = state.db.clone();
    let record = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_skill(&conn, &name)
    })
    .await??;

    match record {
        None => Err(AppError(StatusCode::NOT_FOUND, "not found".into())),
        Some(r) => Ok(Json(SkillContentResponse {
            name: r.name,
            kind: r.kind,
            content: r.content,
        })),
    }
}

// ── GET /skills, /commands, /agents (control-panel list pages) ──────────────

/// Render one of the three admin list pages: skills, commands, or agents.
///
/// `kind` must be `"skill"`, `"command"`, or `"agent"` and drives the API
/// URLs, page labels, whether to show the create affordance, and each row's
/// full-page view/edit link.
///
/// This is the shared body for `skills_page`, `commands_page`, and
/// `agents_page`; each public handler is a thin wrapper that passes its kind
/// through. See the per-kind table in the control-panel docs for the exact
/// label mapping.
async fn render_kind_page(state: &AppState, kind: &str) -> Result<Html<String>> {
    debug_assert!(
        kind == "skill" || kind == "command" || kind == "agent",
        "render_kind_page called with unsupported kind '{kind}'"
    );

    let db = state.db.clone();
    let theme = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_theme(&conn)
    })
    .await??;

    let (singular, plural, active, page_title) = match kind {
        "skill" => ("Skill", "Skills", "skills", "Skills"),
        "command" => ("Command", "Commands", "commands", "Commands"),
        "agent" => ("Agent", "Agents", "agents", "Agents"),
        // Defensive default — debug_assert above catches unexpected kinds in
        // debug builds; in release we fall through to the skills shape.
        _ => ("Skill", "Skills", "skills", "Skills"),
    };

    let is_text_kind = kind == "command" || kind == "agent";

    let create_row = if is_text_kind {
        format!(
            r##"  <div class="nd-flex nd-gap-md nd-mb-md">
    <a class="nd-btn-primary nd-btn-sm" href="/{active}/new">
      + New {singular}
    </a>
  </div>
"##,
        )
    } else {
        String::new()
    };

    let row_action = if is_text_kind {
        format!(
            r##"<a class="nd-btn-secondary nd-btn-sm"
                 href="/{active}/{{{{name}}}}">Edit</a>"##
        )
    } else {
        format!(
            r##"<a class="nd-btn-secondary nd-btn-sm"
                 href="/{active}/{{{{name}}}}">View</a>"##
        )
    };

    let content = format!(
        r##"{create_row}  <section class="nd-card">
    <div class="nd-card-header"><strong>{plural}</strong></div>
    <div class="nd-card-body nd-p-0">
      <table class="nd-table nd-table-hover">
        <thead>
          <tr><th>Name</th><th>Size</th><th>Updated</th><th>Actions</th></tr>
        </thead>
        <tbody id="list-body"
               data-nd-bind="/v1/skills?kind={kind}"
               data-nd-template="row-template">
          <template id="row-template">
            <tr>
              <td>{{{{name}}}}</td>
              <td class="nd-text-muted">{{{{size}}}}</td>
              <td class="nd-text-muted">{{{{uploaded_at}}}}</td>
              <td>
                {row_action}
                <button class="nd-btn-danger nd-btn-sm"
                        data-nd-action="DELETE /v1/skills/{{{{name}}}}"
                        data-nd-confirm="Delete {{{{name}}}}?"
                        data-nd-success="refresh:#list-body">Delete</button>
              </td>
            </tr>
          </template>
          <template data-nd-empty>
            <tr><td colspan="4" class="nd-text-muted">No {plural_lower} yet.</td></tr>
          </template>
        </tbody>
      </table>
    </div>
  </section>
"##,
        plural_lower = plural.to_lowercase(),
    );

    let full_title = format!("agent-gateway — {page_title}");
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head(&full_title, &theme, ""),
        open = control_panel_open(page_title, active),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

/// `GET /skills` — read-only list of zip-backed skills. Upload/edit flows
/// live in the agent-tools CLI; the UI here only supports download + delete.
pub async fn skills_page(State(state): State<AppState>) -> Result<Html<String>> {
    render_kind_page(&state, "skill").await
}

/// `GET /commands` — list + create/edit/delete for markdown command skills.
pub async fn commands_page(State(state): State<AppState>) -> Result<Html<String>> {
    render_kind_page(&state, "command").await
}

/// `GET /agents` — list + create/edit/delete for markdown agent skills.
pub async fn agents_page(State(state): State<AppState>) -> Result<Html<String>> {
    render_kind_page(&state, "agent").await
}

async fn render_skill_detail_page(
    state: &AppState,
    expected_kind: &str,
    name: Option<String>,
) -> Result<Html<String>> {
    debug_assert!(
        expected_kind == "skill" || expected_kind == "command" || expected_kind == "agent",
        "render_skill_detail_page called with unsupported kind '{expected_kind}'"
    );

    let db = state.db.clone();
    let lookup_name = name.clone();
    let (theme, record) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        let record = match lookup_name {
            Some(name) => db::get_skill(&conn, &name)?,
            None => None,
        };
        Ok((db::get_theme(&conn)?, record))
    })
    .await??;

    let (singular, plural, active) = match expected_kind {
        "skill" => ("Skill", "Skills", "skills"),
        "command" => ("Command", "Commands", "commands"),
        "agent" => ("Agent", "Agents", "agents"),
        _ => ("Skill", "Skills", "skills"),
    };

    let is_new = name.is_none();
    if expected_kind == "skill" && is_new {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            "skills are uploaded from the CLI".into(),
        ));
    }

    let record = match (is_new, record) {
        (true, _) => None,
        (false, Some(record)) if record.kind == expected_kind => Some(record),
        (false, Some(_)) | (false, None) => {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("{} not found", singular.to_lowercase()),
            ))
        }
    };

    let title_name = name.as_deref().unwrap_or("New");
    let page_title = if is_new {
        format!("New {singular}")
    } else {
        format!("{singular}: {title_name}")
    };

    let content = if expected_kind == "skill" {
        let record = record.expect("skill detail requires an existing record");
        let download_path = path_segment(&record.name);
        format!(
            r#"  <div class="nd-flex nd-gap-md nd-mb-md">
    <a class="nd-btn-secondary nd-btn-sm" href="/{active}">Back to {plural}</a>
  </div>

  <section class="nd-card">
    <div class="nd-card-header"><strong>{name}</strong></div>
    <div class="nd-card-body">
      <dl>
        <dt>Kind</dt><dd>{kind}</dd>
        <dt>Size</dt><dd>{size}</dd>
        <dt>Checksum</dt><dd><code>{checksum}</code></dd>
        <dt>Uploaded</dt><dd>{uploaded_at}</dd>
      </dl>
      <a class="nd-btn-primary" href="/v1/skills/{download_path}">Download</a>
    </div>
  </section>"#,
            active = active,
            plural = plural,
            name = he(&record.name),
            kind = he(&record.kind),
            size = record.size,
            checksum = he(&record.checksum),
            uploaded_at = record.uploaded_at,
            download_path = download_path,
        )
    } else {
        let (form_action, name_input, content, submit_label) = match record {
            Some(record) => (
                format!("POST /v1/skills/{}", path_segment(&record.name)),
                format!(
                    r#"<input id="skill-edit-name" name="name" value="{}" disabled>"#,
                    he(&record.name)
                ),
                record.content.unwrap_or_default(),
                "Save",
            ),
            None => (
                "POST /v1/skills".to_string(),
                r#"<input id="skill-edit-name" name="name" required>"#.to_string(),
                String::new(),
                "Create",
            ),
        };

        format!(
            r#"  <div class="nd-flex nd-gap-md nd-mb-md">
    <a class="nd-btn-secondary nd-btn-sm" href="/{active}">Back to {plural}</a>
  </div>

  <section class="nd-card">
    <div class="nd-card-header"><strong>{page_title}</strong></div>
    <div class="nd-card-body">
      <form data-nd-action="{form_action}">
        <div class="nd-form-group">
          <label for="skill-edit-name">Name</label>
          {name_input}
        </div>
        <div class="nd-form-group">
          <label for="skill-edit-content">Markdown</label>
          <textarea id="skill-edit-content" name="content" rows="28" required>{content}</textarea>
        </div>
        <input type="hidden" name="kind" value="{kind}">
        <div class="nd-flex nd-gap-sm">
          <button type="submit" class="nd-btn-primary">{submit_label}</button>
          <a class="nd-btn-secondary" href="/{active}">Done</a>
        </div>
      </form>
    </div>
  </section>"#,
            active = active,
            plural = plural,
            page_title = he(&page_title),
            form_action = he(&form_action),
            name_input = name_input,
            content = he(&content),
            kind = expected_kind,
            submit_label = submit_label,
        )
    };

    let full_title = format!("agent-gateway — {page_title}");
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head(&full_title, &theme, ""),
        open = control_panel_open(&page_title, active),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

pub async fn skill_detail_page(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Html<String>> {
    render_skill_detail_page(&state, "skill", Some(name)).await
}

pub async fn new_command_page(State(state): State<AppState>) -> Result<Html<String>> {
    render_skill_detail_page(&state, "command", None).await
}

pub async fn command_detail_page(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Html<String>> {
    render_skill_detail_page(&state, "command", Some(name)).await
}

pub async fn new_agent_page(State(state): State<AppState>) -> Result<Html<String>> {
    render_skill_detail_page(&state, "agent", None).await
}

pub async fn agent_detail_page(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Html<String>> {
    render_skill_detail_page(&state, "agent", Some(name)).await
}

// ── GET / (dashboard) ─────────────────────────────────────────────────────────

fn he(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn truncate_text(s: &str, max_chars: usize) -> String {
    let mut out = s.chars().take(max_chars).collect::<String>();
    if s.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn empty_table_rows(rows: String, colspan: usize) -> String {
    if rows.is_empty() {
        format!(
            r#"<tr><td colspan="{colspan}" class="nd-text-center nd-text-muted">No records.</td></tr>"#
        )
    } else {
        rows
    }
}

fn artifact_auth_signal(state: &AppState) -> &'static str {
    if state.artifact_auth_enforced {
        "Authorization: project-scoped"
    } else {
        "Authorization: trusted-single-tenant"
    }
}

fn path_segment(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub async fn dashboard(State(state): State<AppState>) -> Result<Html<String>> {
    let db = state.db.clone();
    let (data, theme) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        Ok((db::get_dashboard_data(&conn)?, db::get_theme(&conn)?))
    })
    .await??;

    let current_version = env!("AGENT_GATEWAY_VERSION");
    let update_banner = {
        let guard = state.update_available.lock().unwrap();
        match guard.as_deref() {
            Some(version) => format!(
                r#"<div class="nd-alert nd-alert-warning nd-mb-lg">
  <strong>Update available:</strong> {} (current: v{}) — run <code>gateway update</code>
</div>"#,
                he(version),
                he(current_version),
            ),
            None => String::new(),
        }
    };

    let rows = if data.project_count == 0 {
        r#"<tr><td colspan="8" class="nd-text-muted nd-text-center">No projects registered yet</td></tr>"#.to_string()
    } else {
        data.projects
            .iter()
            .map(|p| {
                let unread_cell = if p.unread_count > 0 {
                    format!(
                        r#"<span class="nd-badge nd-badge-sm nd-text-danger">{}</span>"#,
                        p.unread_count
                    )
                } else {
                    "0".into()
                };
                let build_cell = match &p.repo_full_name {
                    Some(repo) => format!(
                        r#"<a class="nd-btn-secondary nd-btn-sm" href="/projects/{}/build">{}</a>"#,
                        he(&p.ident),
                        he(repo)
                    ),
                    None => r#"<a class="nd-btn-ghost nd-btn-sm" href="/settings">Map repo</a>"#.into(),
                };
                let docs_cell = if p.api_doc_count > 0 {
                    format!(
                        r#"<a class="nd-btn-secondary nd-btn-sm" href="/projects/{}/api-docs">{} docs</a>"#,
                        he(&p.ident),
                        p.api_doc_count
                    )
                } else {
                    format!(
                        r#"<a class="nd-btn-ghost nd-btn-sm" href="/projects/{}/api-docs">No docs</a>"#,
                        he(&p.ident)
                    )
                };
                let tasks_cell = format!(
                    r#"<a class="nd-btn-secondary nd-btn-sm" href="/projects/{}/tasks">Tasks</a>"#,
                    he(&p.ident)
                );
                format!(
                    "<tr><td>{}</td><td>{}</td><td class=\"nd-text-muted\">{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    he(&p.ident),
                    he(&p.channel_name),
                    he(&p.room_id),
                    p.total_messages,
                    unread_cell,
                    build_cell,
                    docs_cell,
                    tasks_cell,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let content = format!(
        r#"  {banner}
  <p class="nd-text-muted nd-text-sm">Channel plugin dashboard · v{version}</p>

  <section class="nd-row nd-gap-md nd-mb-lg">
    <div class="nd-col-2"><div class="nd-card"><div class="nd-card-body"><div class="nd-text-2xl nd-font-bold">{projects}</div><div class="nd-text-xs nd-text-muted">Projects</div></div></div></div>
    <div class="nd-col-2"><div class="nd-card"><div class="nd-card-body"><div class="nd-text-2xl nd-font-bold">{total}</div><div class="nd-text-xs nd-text-muted">Total messages</div></div></div></div>
    <div class="nd-col-2"><div class="nd-card"><div class="nd-card-body"><div class="nd-text-2xl nd-font-bold">{agent}</div><div class="nd-text-xs nd-text-muted">Agent</div></div></div></div>
    <div class="nd-col-2"><div class="nd-card"><div class="nd-card-body"><div class="nd-text-2xl nd-font-bold">{user}</div><div class="nd-text-xs nd-text-muted">User</div></div></div></div>
    <div class="nd-col-2"><div class="nd-card"><div class="nd-card-body"><div class="nd-text-2xl nd-font-bold">{skills}</div><div class="nd-text-xs nd-text-muted">Skills</div></div></div></div>
    <div class="nd-col-2"><div class="nd-card"><div class="nd-card-body"><div class="nd-text-2xl nd-font-bold">{api_docs}</div><div class="nd-text-xs nd-text-muted">API docs</div></div></div></div>
  </section>

  <section class="nd-card">
    <div class="nd-card-header"><strong>Projects</strong></div>
    <div class="nd-card-body nd-p-0">
      <table class="nd-table nd-table-hover">
        <thead><tr><th>Project</th><th>Channel</th><th>Room ID</th><th>Messages</th><th>Unread</th><th>Build</th><th>Docs</th><th>Tasks</th></tr></thead>
        <tbody>{rows}</tbody>
      </table>
    </div>
  </section>"#,
        banner = update_banner,
        version = he(current_version),
        projects = data.project_count,
        total = data.total_messages,
        agent = data.agent_messages,
        user = data.user_messages,
        skills = data.skill_count,
        api_docs = data.api_doc_count,
        rows = rows,
    );

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway — Dashboard", &theme, ""),
        open = control_panel_open("Dashboard", "dashboard"),
        content = content,
        close = control_panel_close(&state.api_key),
    );

    Ok(Html(html))
}

pub async fn api_docs_index_page(State(state): State<AppState>) -> Result<Html<String>> {
    let db = state.db.clone();
    let (projects, theme) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        Ok((db::list_project_stats(&conn)?, db::get_theme(&conn)?))
    })
    .await??;

    let rows = if projects.is_empty() {
        r#"<tr><td colspan="5" class="nd-text-muted nd-text-center">No projects registered yet.</td></tr>"#.to_string()
    } else {
        projects
            .iter()
            .map(|project| {
                let docs_cell = if project.api_doc_count > 0 {
                    format!(
                        r#"<a class="nd-btn-secondary nd-btn-sm" href="/projects/{ident}/api-docs">{count} docs</a>"#,
                        ident = he(&project.ident),
                        count = project.api_doc_count,
                    )
                } else {
                    format!(
                        r#"<a class="nd-btn-ghost nd-btn-sm" href="/projects/{ident}/api-docs">Start docs</a>"#,
                        ident = he(&project.ident),
                    )
                };
                let repo = project.repo_full_name.as_deref().unwrap_or("");
                format!(
                    r#"<tr>
  <td><strong>{ident}</strong><div class="nd-text-xs nd-text-muted">{repo}</div></td>
  <td>{docs}</td>
  <td>{messages}</td>
  <td>{unread}</td>
  <td><a class="nd-btn-secondary nd-btn-sm" href="/projects/{ident}/tasks">Tasks</a></td>
</tr>"#,
                    ident = he(&project.ident),
                    repo = he(repo),
                    docs = docs_cell,
                    messages = project.total_messages,
                    unread = project.unread_count,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let content = format!(
        r#"  <section class="nd-card">
    <div class="nd-card-header"><strong>API Docs by Project</strong></div>
    <div class="nd-card-body nd-p-0">
      <table class="nd-table nd-table-hover">
        <thead><tr><th>Project</th><th>Docs</th><th>Messages</th><th>Unread</th><th>Tasks</th></tr></thead>
        <tbody>{rows}</tbody>
      </table>
    </div>
  </section>"#,
        rows = rows,
    );

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway - API docs", &theme, ""),
        open = control_panel_open("API Docs", "api-docs"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

pub async fn api_docs_page(
    State(state): State<AppState>,
    Path(ident): Path<String>,
) -> Result<Html<String>> {
    let db = state.db.clone();
    let ident_for_lookup = ident.clone();
    let (project, docs, theme) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        let project = db::get_project(&conn, &ident_for_lookup)?;
        let docs = db::list_api_docs(&conn, &ident_for_lookup, &db::ApiDocFilters::default())?;
        let theme = db::get_theme(&conn)?;
        Ok((project, docs, theme))
    })
    .await??;

    let project = project.ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("project '{}' not found", ident),
        )
    })?;

    let rows = if docs.is_empty() {
        r#"<tr><td colspan="6" class="nd-text-muted nd-text-center">No agent API context has been published for this project.</td></tr>"#.to_string()
    } else {
        docs.iter()
            .map(|doc| {
                let labels = if doc.labels.is_empty() {
                    String::new()
                } else {
                    doc.labels
                        .iter()
                        .map(|label| {
                            format!(r#"<span class="nd-badge nd-badge-sm">{}</span>"#, he(label))
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                let summary = doc.summary.as_deref().unwrap_or("");
                format!(
                    r#"<tr>
  <td><a class="nd-btn-ghost nd-text-left" href="/projects/{ident}/api-docs/{id}"><strong>{app}</strong><div class="nd-text-xs nd-text-muted">{title}</div></a></td>
  <td>{kind}</td>
  <td>{source}</td>
  <td>{version}</td>
  <td>{labels}</td>
  <td class="nd-text-muted">{summary}</td>
</tr>"#,
                    ident = he(&project.ident),
                    id = he(&doc.id),
                    app = he(&doc.app),
                    title = he(&doc.title),
                    kind = he(&doc.kind),
                    source = he(&doc.source_format),
                    version = he(doc.version.as_deref().unwrap_or("")),
                    labels = labels,
                    summary = he(summary),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let ident_attr = he(&project.ident);
    let content = format!(
        r#"  <div class="nd-flex nd-gap-md nd-mb-md">
    <a class="nd-btn-secondary nd-btn-sm" href="/api-docs">All API docs</a>
    <a class="nd-btn-secondary nd-btn-sm" href="/">Dashboard</a>
    <a class="nd-btn-secondary nd-btn-sm" href="/projects/{ident}/tasks">Tasks</a>
    <a class="nd-btn-secondary nd-btn-sm" href="/projects/{ident}/build">Build</a>
  </div>

  <section class="nd-card">
    <div class="nd-card-header"><strong>Agent API Context</strong></div>
    <div class="nd-card-body nd-p-0">
      <table class="nd-table nd-table-hover">
        <thead><tr><th>App</th><th>Kind</th><th>Source</th><th>Version</th><th>Labels</th><th>Summary</th></tr></thead>
        <tbody>{rows}</tbody>
      </table>
    </div>
  </section>

  <section class="nd-card nd-mt-lg">
    <div class="nd-card-header"><strong>Agent endpoints</strong></div>
    <div class="nd-card-body">
      <p class="nd-text-muted nd-text-sm">Publish docs-first context with <code>POST /v1/projects/{ident}/api-docs</code>. Retrieve RAG-ready chunks with <code>GET /v1/projects/{ident}/api-docs/chunks</code>.</p>
    </div>
  </section>"#,
        ident = ident_attr,
        rows = rows,
    );

    let page_title = format!("API docs - {}", project.ident);
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway - API docs", &theme, ""),
        open = control_panel_open(&page_title, "dashboard"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

pub async fn api_doc_detail_page(
    State(state): State<AppState>,
    Path((ident, id)): Path<(String, String)>,
) -> Result<Html<String>> {
    let db = state.db.clone();
    let ident_for_lookup = ident.clone();
    let id_for_lookup = id.clone();
    let (project, doc, theme) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        let project = db::get_project(&conn, &ident_for_lookup)?;
        let doc = db::get_api_doc(&conn, &ident_for_lookup, &id_for_lookup)?;
        let theme = db::get_theme(&conn)?;
        Ok((project, doc, theme))
    })
    .await??;

    let project = project.ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("project '{}' not found", ident),
        )
    })?;
    let doc =
        doc.ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("api doc '{}' not found", id)))?;

    let labels = if doc.labels.is_empty() {
        String::new()
    } else {
        doc.labels
            .iter()
            .map(|label| format!(r#"<span class="nd-badge nd-badge-sm">{}</span>"#, he(label)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let chunks = api_doc_chunks(&doc)
        .into_iter()
        .map(|chunk| {
            format!(
                r#"<section class="nd-card nd-mt-md">
  <div class="nd-card-header"><strong>{chunk_type}</strong></div>
  <div class="nd-card-body"><pre><code>{text}</code></pre></div>
</section>"#,
                chunk_type = he(&chunk.chunk_type),
                text = he(&chunk.text),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let content_json =
        serde_json::to_string_pretty(&doc.content).unwrap_or_else(|_| doc.content.to_string());
    let source_ref = doc.source_ref.as_deref().unwrap_or("");
    let version = doc.version.as_deref().unwrap_or("");
    let summary = doc.summary.as_deref().unwrap_or("");
    let ident_attr = he(&project.ident);

    let content = format!(
        r#"  <div class="nd-flex nd-gap-md nd-mb-md">
    <a class="nd-btn-secondary nd-btn-sm" href="/projects/{ident}/api-docs">Back to project docs</a>
    <a class="nd-btn-secondary nd-btn-sm" href="/api-docs">All API docs</a>
  </div>

  <section class="nd-card">
    <div class="nd-card-header"><strong>{title}</strong></div>
    <div class="nd-card-body">
      <div class="nd-row nd-gap-md">
        <div class="nd-col-3"><div class="nd-text-xs nd-text-muted">App</div><strong>{app}</strong></div>
        <div class="nd-col-3"><div class="nd-text-xs nd-text-muted">Kind</div><strong>{kind}</strong></div>
        <div class="nd-col-3"><div class="nd-text-xs nd-text-muted">Source</div><strong>{source}</strong></div>
        <div class="nd-col-3"><div class="nd-text-xs nd-text-muted">Version</div><strong>{version}</strong></div>
      </div>
      <p class="nd-text-muted nd-mt-md">{summary}</p>
      <div class="nd-mt-md">{labels}</div>
      <div class="nd-text-xs nd-text-muted nd-mt-md">Source ref: {source_ref}</div>
    </div>
  </section>

  <section class="nd-card nd-mt-lg">
    <div class="nd-card-header"><strong>Stored agent context</strong></div>
    <div class="nd-card-body"><pre><code>{content_json}</code></pre></div>
  </section>

  <section class="nd-mt-lg">
    <h2 class="nd-text-lg nd-mb-sm">RAG chunks</h2>
    {chunks}
  </section>"#,
        ident = ident_attr,
        title = he(&doc.title),
        app = he(&doc.app),
        kind = he(&doc.kind),
        source = he(&doc.source_format),
        version = he(version),
        summary = he(summary),
        labels = labels,
        source_ref = he(source_ref),
        content_json = he(&content_json),
        chunks = chunks,
    );

    let page_title = format!("{} - {}", doc.app, project.ident);
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway - API doc", &theme, ""),
        open = control_panel_open(&page_title, "api-docs"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

pub async fn artifacts_index_page(State(state): State<AppState>) -> Result<Html<String>> {
    let db = state.db.clone();
    let (theme, projects) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        Ok((db::get_theme(&conn)?, db::list_project_stats(&conn)?))
    })
    .await??;

    let rows = if projects.is_empty() {
        r#"<tr><td colspan="5" class="nd-text-center nd-text-muted">No projects registered.</td></tr>"#
            .to_string()
    } else {
        projects
            .iter()
            .map(|project| {
                format!(
                    r#"<tr>
  <td><a class="nd-btn-ghost nd-text-left" href="/projects/{ident}/artifacts"><strong>{ident}</strong></a></td>
  <td>{api_docs}</td>
  <td>{tasks}</td>
  <td>{messages}</td>
  <td><a class="nd-btn-secondary nd-btn-sm" href="/projects/{ident}/artifacts">Open workspace</a></td>
</tr>"#,
                    ident = he(&project.ident),
                    api_docs = project.api_doc_count,
                    tasks = "—",
                    messages = project.total_messages,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let content = format!(
        r#"  <div class="nd-alert nd-mb-md">{auth_signal}</div>

  <section class="nd-card">
    <div class="nd-card-header"><strong>Artifact Workspaces</strong></div>
    <div class="nd-card-body nd-p-0">
      <table class="nd-table nd-table-hover">
        <thead><tr><th>Project</th><th>API docs</th><th>Tasks</th><th>Messages</th><th></th></tr></thead>
        <tbody>{rows}</tbody>
      </table>
    </div>
  </section>"#,
        auth_signal = he(artifact_auth_signal(&state)),
        rows = rows,
    );

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway - Artifacts", &theme, ""),
        open = control_panel_open("Artifacts", "artifacts"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

pub async fn artifact_workspace_page(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    Query(query): Query<ArtifactWorkspaceQuery>,
) -> Result<Html<String>> {
    let db = state.db.clone();
    let ident_for_lookup = ident.clone();
    let kind = query.kind.clone();
    let status = query.status.clone();
    let label = query.label.clone();
    let actor = query.actor.clone();
    let q = query.q.clone();
    let (project, artifacts, docs, theme) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        let project = db::get_project(&conn, &ident_for_lookup)?;
        let artifacts = db::list_artifacts(
            &conn,
            &ident_for_lookup,
            &db::ArtifactFilters {
                kind: kind.as_deref(),
                subkind: None,
                lifecycle_state: status.as_deref(),
                label: label.as_deref(),
                actor_id: actor.as_deref(),
                query: q.as_deref(),
            },
        )?;
        let docs = db::list_api_docs(
            &conn,
            &ident_for_lookup,
            &db::ApiDocFilters {
                query: q.as_deref(),
                app: None,
                label: label.as_deref(),
                kind: None,
            },
        )?;
        let theme = db::get_theme(&conn)?;
        Ok((project, artifacts, docs, theme))
    })
    .await??;

    let project = project.ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("project '{}' not found", ident),
        )
    })?;
    let ident_attr = he(&project.ident);
    let artifacts_rows = if artifacts.is_empty() {
        r#"<tr><td colspan="8" class="nd-text-center nd-text-muted">No artifacts match these filters.</td></tr>"#.to_string()
    } else {
        artifacts
            .iter()
            .map(|artifact| {
                let labels = artifact
                    .labels
                    .iter()
                    .map(|label| format!(r#"<span class="nd-badge nd-badge-sm">{}</span>"#, he(label)))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!(
                    r#"<tr>
  <td><a class="nd-btn-ghost nd-text-left" href="/projects/{ident}/artifacts/{artifact_id}"><strong>{title}</strong><div class="nd-text-xs nd-text-muted">{artifact_id}</div></a></td>
  <td>{kind}<div class="nd-text-xs nd-text-muted">{subkind}</div></td>
  <td>{lifecycle}</td>
  <td>{review}</td>
  <td>{implementation}</td>
  <td>{labels}</td>
  <td><code>{current}</code></td>
  <td><code>{accepted}</code></td>
</tr>"#,
                    ident = ident_attr,
                    artifact_id = he(&artifact.artifact_id),
                    title = he(&artifact.title),
                    kind = he(&artifact.kind),
                    subkind = he(artifact.subkind.as_deref().unwrap_or("")),
                    lifecycle = he(&artifact.lifecycle_state),
                    review = he(&artifact.review_state),
                    implementation = he(&artifact.implementation_state),
                    labels = labels,
                    current = he(artifact.current_version_id.as_deref().unwrap_or("")),
                    accepted = he(artifact.accepted_version_id.as_deref().unwrap_or("")),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let docs_rows = if docs.is_empty() {
        r#"<tr><td colspan="5" class="nd-text-center nd-text-muted">No documentation contexts match these filters.</td></tr>"#.to_string()
    } else {
        docs.iter()
            .map(|doc| {
                format!(
                    r#"<tr><td><a href="/projects/{ident}/api-docs/{id}">{app}</a></td><td>{kind}</td><td>{subkind}</td><td>{version}</td><td>{summary}</td></tr>"#,
                    ident = ident_attr,
                    id = he(&doc.id),
                    app = he(&doc.app),
                    kind = he(&doc.kind),
                    subkind = he(&doc.subkind),
                    version = he(doc.version.as_deref().unwrap_or("")),
                    summary = he(doc.summary.as_deref().unwrap_or("")),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let filter_value = |value: &Option<String>| he(value.as_deref().unwrap_or(""));
    let content = format!(
        r#"  <div class="nd-flex nd-gap-md nd-mb-md">
    <a class="nd-btn-secondary nd-btn-sm" href="/artifacts">All projects</a>
    <a class="nd-btn-secondary nd-btn-sm" href="/projects/{ident}/api-docs">API docs</a>
    <a class="nd-btn-secondary nd-btn-sm" href="/projects/{ident}/tasks">Tasks</a>
  </div>

  <div class="nd-alert nd-mb-md">{auth_signal}</div>

  <section class="nd-card">
    <div class="nd-card-header"><strong>Artifact filters</strong></div>
    <div class="nd-card-body">
      <form class="nd-grid nd-gap-sm" method="get" action="/projects/{ident}/artifacts">
        <input class="nd-input" name="q" placeholder="Search artifacts, bodies, contributions, links, docs" value="{q}">
        <input class="nd-input" name="kind" placeholder="kind: spec, design_review, documentation" value="{kind}">
        <input class="nd-input" name="status" placeholder="lifecycle status" value="{status}">
        <input class="nd-input" name="label" placeholder="label" value="{label}">
        <input class="nd-input" name="actor" placeholder="actor id" value="{actor}">
        <button class="nd-btn-primary" type="submit">Filter</button>
      </form>
    </div>
  </section>

  <section class="nd-card nd-mt-lg">
    <div class="nd-card-header"><strong>Artifacts</strong></div>
    <div class="nd-card-body nd-p-0">
      <table class="nd-table nd-table-hover">
        <thead><tr><th>Artifact</th><th>Kind</th><th>Lifecycle</th><th>Review</th><th>Implementation</th><th>Labels</th><th>Current</th><th>Accepted</th></tr></thead>
        <tbody>{artifacts_rows}</tbody>
      </table>
    </div>
  </section>

  <section class="nd-card nd-mt-lg">
    <div class="nd-card-header"><strong>Documentation browser</strong></div>
    <div class="nd-card-body nd-p-0">
      <table class="nd-table nd-table-hover">
        <thead><tr><th>App</th><th>Legacy kind</th><th>Subkind</th><th>Version</th><th>Summary</th></tr></thead>
        <tbody>{docs_rows}</tbody>
      </table>
    </div>
  </section>"#,
        ident = ident_attr,
        q = filter_value(&query.q),
        kind = filter_value(&query.kind),
        status = filter_value(&query.status),
        label = filter_value(&query.label),
        actor = filter_value(&query.actor),
        auth_signal = he(artifact_auth_signal(&state)),
        artifacts_rows = artifacts_rows,
        docs_rows = docs_rows,
    );

    let page_title = format!("Artifacts - {}", project.ident);
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway - Artifacts", &theme, ""),
        open = control_panel_open(&page_title, "artifacts"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

pub async fn artifact_detail_page(
    State(state): State<AppState>,
    Path((ident, artifact_id)): Path<(String, String)>,
    Query(query): Query<ArtifactWorkspaceQuery>,
) -> Result<Html<String>> {
    let db = state.db.clone();
    let ident_for_lookup = ident.clone();
    let artifact_for_lookup = artifact_id.clone();
    let chunk_q = query.chunk_q.clone();
    let include_history = query.include_history.unwrap_or(false);
    let (
        project,
        detail,
        versions,
        contributions,
        comments,
        links,
        chunks,
        review_contributions,
        theme,
    ) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        let project = db::get_project(&conn, &ident_for_lookup)?;
        let detail = db::get_artifact(&conn, &ident_for_lookup, &artifact_for_lookup)?;
        let versions = db::list_artifact_versions(&conn, &ident_for_lookup, &artifact_for_lookup)?;
        let contributions =
            db::list_artifact_contributions(&conn, &ident_for_lookup, &artifact_for_lookup)?;
        let comments = db::list_artifact_comments(&conn, &ident_for_lookup, &artifact_for_lookup)?;
        let mut links = db::list_artifact_links(
            &conn,
            &ident_for_lookup,
            &db::ArtifactLinkFilters::default(),
        )?;
        links.retain(|link| {
            link.source_id == artifact_for_lookup
                || link.target_id == artifact_for_lookup
                || detail.as_ref().is_some_and(|d| {
                    let current = d.artifact.current_version_id.as_deref();
                    let accepted = d.artifact.accepted_version_id.as_deref();
                    [
                        link.source_version_id.as_deref(),
                        link.target_version_id.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    .any(|id| Some(id) == current || Some(id) == accepted)
                })
        });
        let chunks = db::list_artifact_chunks(
            &conn,
            &ident_for_lookup,
            &artifact_for_lookup,
            &db::ArtifactChunkFilters {
                artifact_version_id: None,
                app: None,
                label: None,
                kind: None,
                include_superseded: include_history,
                query: chunk_q.as_deref(),
            },
        )?;
        let review_contributions = if detail
            .as_ref()
            .is_some_and(|d| d.artifact.kind == "design_review")
        {
            db::list_design_review_contributions(
                &conn,
                &ident_for_lookup,
                &artifact_for_lookup,
                &db::DesignReviewContributionFilters::default(),
            )?
        } else {
            Vec::new()
        };
        let theme = db::get_theme(&conn)?;
        Ok((
            project,
            detail,
            versions,
            contributions,
            comments,
            links,
            chunks,
            review_contributions,
            theme,
        ))
    })
    .await??;

    let project = project.ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("project '{}' not found", ident),
        )
    })?;
    let detail = detail.ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("artifact '{}' not found", artifact_id),
        )
    })?;
    let artifact = &detail.artifact;
    let ident_attr = he(&project.ident);
    let artifact_attr = he(&artifact.artifact_id);
    let version_rows = versions
        .iter()
        .map(|version| {
            format!(
                r#"<tr><td><code>{id}</code></td><td>{label}</td><td>{state}</td><td>{format}</td><td><code>{parent}</code></td><td>{bytes}</td></tr>"#,
                id = he(&version.artifact_version_id),
                label = he(version.version_label.as_deref().unwrap_or("")),
                state = he(&version.version_state),
                format = he(&version.body_format),
                parent = he(version.parent_version_id.as_deref().unwrap_or("")),
                bytes = version.body.as_ref().map_or(0, |body| body.len()),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let contribution_rows = contributions
        .iter()
        .map(|c| {
            format!(
                r#"<tr><td>{kind}</td><td>{phase}</td><td>{role}</td><td><code>{target}</code></td><td>{body}</td></tr>"#,
                kind = he(&c.contribution_kind),
                phase = he(c.phase.as_deref().unwrap_or("")),
                role = he(&c.role),
                target = he(&c.target_id),
                body = he(&truncate_text(&c.body, 160)),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let comment_rows = comments
        .iter()
        .map(|c| {
            format!(
                r#"<tr><td>{state}</td><td>{target}</td><td>{child}</td><td>{body}</td></tr>"#,
                state = he(&c.state),
                target = he(&c.target_id),
                child = he(c.child_address.as_deref().unwrap_or("")),
                body = he(&truncate_text(&c.body, 160)),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let link_rows = links
        .iter()
        .map(|link| {
            format!(
                r#"<tr><td>{typ}</td><td>{source}</td><td>{target}</td><td><code>{source_version}</code></td><td><code>{target_version}</code></td></tr>"#,
                typ = he(&link.link_type),
                source = he(&format!("{}:{}", link.source_kind, link.source_id)),
                target = he(&format!("{}:{}", link.target_kind, link.target_id)),
                source_version = he(link.source_version_id.as_deref().unwrap_or("")),
                target_version = he(link.target_version_id.as_deref().unwrap_or("")),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let chunk_rows = chunks
        .iter()
        .map(|chunk| {
            let status = chunk
                .metadata
                .as_ref()
                .and_then(|m| m.get("status").or_else(|| m.get("chunking_status")))
                .and_then(Value::as_str)
                .unwrap_or("ok");
            format!(
                r#"<tr><td><code>{address}</code></td><td>{kind}</td><td>{label}</td><td>{status}</td><td>{text}</td></tr>"#,
                address = he(&chunk.child_address),
                kind = he(chunk.kind.as_deref().unwrap_or("")),
                label = he(chunk.label.as_deref().unwrap_or("")),
                status = he(status),
                text = he(&truncate_text(&chunk.text, 180)),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let diff_section = if let (Some(current), Some(accepted)) =
        (&detail.current_version, &detail.accepted_version)
    {
        if current.artifact_version_id != accepted.artifact_version_id {
            let diff = simple_diff(&body_text(accepted), &body_text(current));
            format!(
                r#"<section class="nd-card nd-mt-lg"><div class="nd-card-header"><strong>Accepted to current diff</strong></div><div class="nd-card-body"><pre><code>{}</code></pre></div></section>"#,
                he(&diff)
            )
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let spec_section = if artifact.kind == "spec" {
        detail
            .current_version
            .as_ref()
            .and_then(|version| spec_manifest_from_version(&artifact.artifact_id, version).ok())
            .map(|manifest| {
                let rows = manifest
                    .items
                    .iter()
                    .map(|item| {
                        format!(
                            r#"<tr><td><code>{id}</code></td><td>{phase}</td><td>{team}</td><td>{status}</td><td>{title}</td><td>{deps}</td><td><code>{task}</code></td></tr>"#,
                            id = he(&item.manifest_item_id),
                            phase = he(item.phase.as_deref().unwrap_or("")),
                            team = he(item.team.as_deref().unwrap_or("")),
                            status = he(item.status.as_deref().unwrap_or("")),
                            title = he(&item.title),
                            deps = he(&item.dependencies.join(", ")),
                            task = he(item.gateway_task_id.as_deref().unwrap_or("")),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    r#"<section class="nd-card nd-mt-lg"><div class="nd-card-header"><strong>Spec manifest</strong></div><div class="nd-card-body nd-p-0"><table class="nd-table nd-table-hover"><thead><tr><th>Stable item</th><th>Phase</th><th>Team</th><th>Status</th><th>Title</th><th>Deps</th><th>Gateway task</th></tr></thead><tbody>{rows}</tbody></table></div></section>"#,
                    rows = rows,
                )
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    let review_section = if artifact.kind == "design_review" {
        let rows = review_contributions
            .iter()
            .map(|c| {
                let read_set = c
                    .read_set
                    .as_ref()
                    .map(Value::to_string)
                    .unwrap_or_default();
                format!(
                    r#"<tr><td>{phase}</td><td>{role}</td><td><code>{run}</code></td><td>{read_set}</td><td>{body}</td></tr>"#,
                    phase = he(c.phase.as_deref().unwrap_or("")),
                    role = he(&c.role),
                    run = he(c.workflow_run_id.as_deref().unwrap_or("")),
                    read_set = he(&truncate_text(&read_set, 160)),
                    body = he(&truncate_text(&c.body, 180)),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            r#"<section class="nd-card nd-mt-lg"><div class="nd-card-header"><strong>Review rounds</strong></div><div class="nd-card-body nd-p-0"><table class="nd-table nd-table-hover"><thead><tr><th>Phase</th><th>Role</th><th>Workflow run</th><th>Read set</th><th>Contribution</th></tr></thead><tbody>{rows}</tbody></table></div></section>"#
        )
    } else {
        String::new()
    };
    let docs_section = if artifact.kind == "documentation" {
        format!(
            r#"<section class="nd-card nd-mt-lg">
  <div class="nd-card-header"><strong>Documentation chunks</strong></div>
  <div class="nd-card-body">
    <form class="nd-grid nd-gap-sm" method="get" action="/projects/{ident}/artifacts/{artifact_id}">
      <input class="nd-input" name="chunk_q" placeholder="Search chunks" value="{chunk_q}">
      <label><input type="checkbox" name="include_history" value="true" {checked}> Include superseded/history chunks</label>
      <button class="nd-btn-secondary" type="submit">Search chunks</button>
    </form>
  </div>
  <div class="nd-card-body nd-p-0"><table class="nd-table nd-table-hover"><thead><tr><th>Address</th><th>Kind</th><th>Label</th><th>Status</th><th>Text</th></tr></thead><tbody>{chunk_rows}</tbody></table></div>
</section>"#,
            ident = ident_attr,
            artifact_id = artifact_attr,
            chunk_q = he(query.chunk_q.as_deref().unwrap_or("")),
            checked = if include_history { "checked" } else { "" },
            chunk_rows = if chunk_rows.is_empty() {
                r#"<tr><td colspan="5" class="nd-text-center nd-text-muted">No chunks match.</td></tr>"#.to_string()
            } else {
                chunk_rows.clone()
            },
        )
    } else {
        String::new()
    };

    let content = format!(
        r#"  <div class="nd-flex nd-gap-md nd-mb-md">
    <a class="nd-btn-secondary nd-btn-sm" href="/projects/{ident}/artifacts">Back to artifacts</a>
    <a class="nd-btn-secondary nd-btn-sm" href="/projects/{ident}/api-docs">API docs</a>
    <a class="nd-btn-secondary nd-btn-sm" href="/projects/{ident}/tasks">Tasks</a>
  </div>

  <div class="nd-alert nd-mb-md">{auth_signal}</div>

  <section class="nd-card">
    <div class="nd-card-header"><strong>{title}</strong></div>
    <div class="nd-card-body">
      <div class="nd-row nd-gap-md">
        <div class="nd-col-3"><div class="nd-text-xs nd-text-muted">Kind</div><strong>{kind}</strong></div>
        <div class="nd-col-3"><div class="nd-text-xs nd-text-muted">Lifecycle</div><strong>{lifecycle}</strong></div>
        <div class="nd-col-3"><div class="nd-text-xs nd-text-muted">Review</div><strong>{review}</strong></div>
        <div class="nd-col-3"><div class="nd-text-xs nd-text-muted">Implementation</div><strong>{implementation}</strong></div>
      </div>
      <div class="nd-grid nd-gap-xs nd-mt-md">
        <div>Current version: <code>{current}</code></div>
        <div>Accepted version: <code>{accepted}</code></div>
        <div>Labels: {labels}</div>
      </div>
    </div>
  </section>

  {diff_section}
  {spec_section}
  {review_section}
  {docs_section}

  <section class="nd-card nd-mt-lg"><div class="nd-card-header"><strong>Version history</strong></div><div class="nd-card-body nd-p-0"><table class="nd-table nd-table-hover"><thead><tr><th>Version</th><th>Label</th><th>State</th><th>Format</th><th>Parent</th><th>Body bytes</th></tr></thead><tbody>{version_rows}</tbody></table></div></section>
  <section class="nd-card nd-mt-lg"><div class="nd-card-header"><strong>Contributions</strong></div><div class="nd-card-body nd-p-0"><table class="nd-table nd-table-hover"><thead><tr><th>Kind</th><th>Phase</th><th>Role</th><th>Target</th><th>Body</th></tr></thead><tbody>{contribution_rows}</tbody></table></div></section>
  <section class="nd-card nd-mt-lg"><div class="nd-card-header"><strong>Comments</strong></div><div class="nd-card-body nd-p-0"><table class="nd-table nd-table-hover"><thead><tr><th>State</th><th>Target</th><th>Child</th><th>Body</th></tr></thead><tbody>{comment_rows}</tbody></table></div></section>
  <section class="nd-card nd-mt-lg"><div class="nd-card-header"><strong>Links</strong></div><div class="nd-card-body nd-p-0"><table class="nd-table nd-table-hover"><thead><tr><th>Type</th><th>Source</th><th>Target</th><th>Source version</th><th>Target version</th></tr></thead><tbody>{link_rows}</tbody></table></div></section>"#,
        ident = ident_attr,
        title = he(&artifact.title),
        kind = he(&artifact.kind),
        lifecycle = he(&artifact.lifecycle_state),
        review = he(&artifact.review_state),
        implementation = he(&artifact.implementation_state),
        current = he(artifact.current_version_id.as_deref().unwrap_or("")),
        accepted = he(artifact.accepted_version_id.as_deref().unwrap_or("")),
        labels = artifact
            .labels
            .iter()
            .map(|label| format!(r#"<span class="nd-badge nd-badge-sm">{}</span>"#, he(label)))
            .collect::<Vec<_>>()
            .join(" "),
        auth_signal = he(artifact_auth_signal(&state)),
        diff_section = diff_section,
        spec_section = spec_section,
        review_section = review_section,
        docs_section = docs_section,
        version_rows = empty_table_rows(version_rows, 6),
        contribution_rows = empty_table_rows(contribution_rows, 5),
        comment_rows = empty_table_rows(comment_rows, 4),
        link_rows = empty_table_rows(link_rows, 5),
    );

    let page_title = format!("Artifact - {}", artifact.title);
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway - Artifact", &theme, ""),
        open = control_panel_open(&page_title, "artifacts"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

pub async fn settings_page(State(state): State<AppState>) -> Result<Html<String>> {
    let db = state.db.clone();
    let (theme, servers, projects) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        Ok((
            db::get_theme(&conn)?,
            load_eventic_servers(&conn)?,
            db::list_project_stats(&conn)?,
        ))
    })
    .await??;

    let mut eventic_projects = Vec::new();
    for server in servers.iter().filter(|s| s.enabled) {
        if let Ok(projects) = fetch_eventic_projects(server).await {
            eventic_projects.extend(projects);
        }
    }
    eventic_projects.sort();
    eventic_projects.dedup();

    let server_rows = if servers.is_empty() {
        r#"<tr><td colspan="5" class="nd-text-muted nd-text-center">No Eventic servers configured.</td></tr>"#.to_string()
    } else {
        servers
            .iter()
            .map(|s| {
                let enabled_true = if s.enabled { " selected" } else { "" };
                let enabled_false = if s.enabled { "" } else { " selected" };
                format!(
                    r#"<tr>
  <td class="nd-text-muted">{id}</td>
  <td colspan="3">
    <form class="settings-inline-form" data-nd-action="PATCH /v1/eventic/servers/{id}" data-nd-success="reload">
      <input type="hidden" name="id" value="{id}">
      <input name="name" value="{name}" aria-label="Server name" required>
      <input name="base_url" value="{base_url}" aria-label="Base URL" required>
      <select name="enabled" aria-label="Enabled">
        <option value="true"{enabled_true}>enabled</option>
        <option value="false"{enabled_false}>disabled</option>
      </select>
      <button type="submit" class="nd-btn-primary nd-btn-sm">Save</button>
    </form>
  </td>
  <td><button type="button" class="nd-btn-danger nd-btn-sm" data-nd-action="DELETE /v1/eventic/servers/{id}" data-nd-success="reload">Delete</button></td>
</tr>"#,
                    id = he(&s.id),
                    name = he(&s.name),
                    base_url = he(&s.base_url),
                    enabled_true = enabled_true,
                    enabled_false = enabled_false,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let project_rows = if projects.is_empty() {
        r#"<tr><td colspan="6" class="nd-text-muted nd-text-center">No projects registered yet.</td></tr>"#.to_string()
    } else {
        projects
            .iter()
            .map(|p| {
                let provider = p.repo_provider.as_deref().unwrap_or("github");
                let namespace = p.repo_namespace.as_deref().unwrap_or("");
                let repo_name = p.repo_name.as_deref().unwrap_or(&p.ident);
                let mapped = p
                    .repo_full_name
                    .as_ref()
                    .map(|r| {
                        if eventic_projects.iter().any(|candidate| candidate == r) {
                            format!(r#"<span class="nd-badge nd-badge-sm">{}</span>"#, he(r))
                        } else {
                            format!(r#"<span class="nd-text-muted">{}</span>"#, he(r))
                        }
                    })
                    .unwrap_or_else(|| r#"<span class="nd-text-muted">unmapped</span>"#.into());
                format!(
                    r#"<tr>
  <td><strong>{ident}</strong></td>
  <td>{mapped}</td>
  <td colspan="4">
    <form class="settings-inline-form" data-nd-action="PATCH /v1/projects/{ident}/repo" data-nd-success="reload">
      <select name="provider" aria-label="Provider">
        <option value="github"{github_selected}>github</option>
        <option value="gitlab"{gitlab_selected}>gitlab</option>
        <option value="bitbucket"{bitbucket_selected}>bitbucket</option>
      </select>
      <input name="namespace" value="{namespace}" placeholder="namespace" aria-label="Namespace">
      <input name="repo_name" value="{repo_name}" placeholder="repo" aria-label="Repository">
      <button type="submit" class="nd-btn-primary nd-btn-sm">Save</button>
      <a class="nd-btn-secondary nd-btn-sm" href="/projects/{ident}/build">Build</a>
    </form>
  </td>
</tr>"#,
                    ident = he(&p.ident),
                    mapped = mapped,
                    namespace = he(namespace),
                    repo_name = he(repo_name),
                    github_selected = if provider == "github" { " selected" } else { "" },
                    gitlab_selected = if provider == "gitlab" { " selected" } else { "" },
                    bitbucket_selected = if provider == "bitbucket" { " selected" } else { "" },
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let content = format!(
        r#"  <section class="nd-card nd-mb-lg">
    <div class="nd-card-header"><strong>Eventic Servers</strong></div>
    <div class="nd-card-body">
      <form class="settings-inline-form nd-mb-md" data-nd-action="POST /v1/eventic/servers" data-nd-success="reload">
        <input name="name" value="Local Eventic" aria-label="Server name" required>
        <input name="base_url" value="http://127.0.0.1:16384" aria-label="Base URL" required>
        <select name="enabled" aria-label="Enabled">
          <option value="true" selected>enabled</option>
          <option value="false">disabled</option>
        </select>
        <button type="submit" class="nd-btn-primary nd-btn-sm">Add server</button>
      </form>
      <table class="nd-table nd-table-hover">
        <thead><tr><th>ID</th><th colspan="3">Server</th><th></th></tr></thead>
        <tbody>{server_rows}</tbody>
      </table>
    </div>
  </section>

  <section class="nd-card">
    <div class="nd-card-header"><strong>Repository Mapping</strong></div>
    <div class="nd-card-body">
      <form class="settings-inline-form nd-mb-md" data-nd-action="POST /v1/projects/repo-mappings/bulk" data-nd-success="reload">
        <select name="provider" aria-label="Provider">
          <option value="github" selected>github</option>
          <option value="gitlab">gitlab</option>
          <option value="bitbucket">bitbucket</option>
        </select>
        <input name="namespace" placeholder="namespace" aria-label="Namespace" required>
        <button type="submit" class="nd-btn-secondary nd-btn-sm">Fill unmapped legacy projects</button>
      </form>
      <table class="nd-table nd-table-hover">
        <thead><tr><th>Project</th><th>Current mapping</th><th colspan="4">Repository</th></tr></thead>
        <tbody>{project_rows}</tbody>
      </table>
    </div>
  </section>"#,
        server_rows = server_rows,
        project_rows = project_rows,
    );

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head(
            "agent-gateway - Settings",
            &theme,
            r#"<style>
.settings-inline-form {
  display: flex;
  gap: 0.5rem;
  align-items: center;
  flex-wrap: wrap;
}
.settings-inline-form input,
.settings-inline-form select {
  min-width: 10rem;
  max-width: 18rem;
}
</style>"#,
        ),
        open = control_panel_open("Settings", "settings"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

pub async fn project_build_page(
    State(state): State<AppState>,
    Path(ident): Path<String>,
) -> Result<Html<String>> {
    let db = state.db.clone();
    let (theme, project, servers) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        let project = db::get_project(&conn, &ident)?
            .ok_or_else(|| anyhow::anyhow!("project '{ident}' not found"))?;
        Ok((db::get_theme(&conn)?, project, load_eventic_servers(&conn)?))
    })
    .await??;

    let mut hint = None;
    let mut server_name = String::new();
    let mut status = None;
    if let Some(repo) = project.repo_full_name.as_deref() {
        for server in servers.iter().filter(|s| s.enabled) {
            match fetch_eventic_project_status(server, repo).await {
                Ok(value) => {
                    server_name = server.name.clone();
                    status = Some(value);
                    break;
                }
                Err(err) => {
                    hint = Some(format!("{}: {err:#}", server.name));
                }
            }
        }
        if status.is_none() && hint.is_none() {
            hint = Some("No enabled Eventic server returned status for this repository.".into());
        }
    } else {
        hint = Some(
            "No repository mapping is configured for this project. Add one in Settings.".into(),
        );
    }

    let summary = status
        .as_ref()
        .map(|value: &Value| {
            let state = value
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let hash = value.get("hash").and_then(Value::as_str).unwrap_or("");
            let event = value.get("event").and_then(Value::as_str).unwrap_or("");
            let action = value.get("action").and_then(Value::as_str).unwrap_or("");
            format!(
                r#"<div class="nd-alert nd-alert-info">
  <strong>{state}</strong> {event}.{action} <span class="nd-text-muted">{hash}</span>
</div>"#,
                state = he(state),
                event = he(event),
                action = he(action),
                hash = he(hash),
            )
        })
        .unwrap_or_default();
    let latest_output = status
        .as_ref()
        .and_then(|value: &Value| value.get("latest_output"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let raw_json = status
        .as_ref()
        .map(|value| serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into()))
        .unwrap_or_else(|| "{}".into());
    let hint_html = hint
        .map(|h| format!(r#"<div class="nd-alert nd-alert-warning">{}</div>"#, he(&h)))
        .unwrap_or_default();

    let content = format!(
        r#"  <div class="nd-flex nd-gap-md nd-mb-md">
    <a class="nd-btn-ghost nd-btn-sm" href="/">Back to dashboard</a>
    <a class="nd-btn-secondary nd-btn-sm" href="/settings">Settings</a>
  </div>
  <p class="nd-text-muted nd-text-sm">Repository: {repo} {server}</p>
  {hint}
  {summary}
  <section class="nd-card nd-mb-lg">
    <div class="nd-card-header"><strong>Latest output</strong></div>
    <div class="nd-card-body"><pre class="nd-text-sm build-output">{latest_output}</pre></div>
  </section>
  <section class="nd-card">
    <div class="nd-card-header"><strong>Raw Eventic status</strong></div>
    <div class="nd-card-body"><pre class="nd-text-sm build-output">{raw_json}</pre></div>
  </section>"#,
        repo = project
            .repo_full_name
            .as_deref()
            .map(he)
            .unwrap_or_else(|| "unmapped".into()),
        server = if server_name.is_empty() {
            String::new()
        } else {
            format!("via {}", he(&server_name))
        },
        hint = hint_html,
        summary = summary,
        latest_output = he(latest_output),
        raw_json = he(&raw_json),
    );

    let page_title = format!("Build - {}", project.ident);
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head(
            &page_title,
            &theme,
            r#"<style>
.build-output {
  overflow: auto;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
</style>"#,
        ),
        open = control_panel_open(&page_title, "dashboard"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

// ── Patterns API ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ListPatternsQuery {
    pub q: Option<String>,
    pub label: Option<String>,
    pub version: Option<String>,
    pub state: Option<String>,
    pub superseded_by: Option<String>,
}

#[derive(Deserialize)]
pub struct CreatePatternRequest {
    pub title: String,
    pub slug: Option<String>,
    pub summary: Option<String>,
    pub body: String,
    pub labels: Option<serde_json::Value>,
    pub version: String,
    pub state: String,
    pub superseded_by: Option<String>,
    /// Defaults to X-Agent-Id header, or "user" when the header is absent.
    pub author: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdatePatternRequest {
    pub title: Option<String>,
    pub slug: Option<String>,
    /// `Some(null)` clears the summary; absent leaves it untouched.
    pub summary: Option<serde_json::Value>,
    pub body: Option<String>,
    pub labels: Option<serde_json::Value>,
    pub version: Option<String>,
    pub state: Option<String>,
    pub superseded_by: Option<serde_json::Value>,
}

fn validate_pattern_version_field(version: &str) -> Result<()> {
    if version == "draft" || version == "latest" || version == "superseded" {
        Ok(())
    } else {
        Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("invalid version '{version}': must be draft|latest|superseded"),
        ))
    }
}

fn validate_pattern_state_field(state: &str) -> Result<()> {
    if state == "active" || state == "archived" {
        Ok(())
    } else {
        Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("invalid state '{state}': must be active|archived"),
        ))
    }
}

fn decode_labels_field(field: &str, value: Option<serde_json::Value>) -> Result<Vec<String>> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .map(|v| match v {
                serde_json::Value::String(s) => Ok(s.trim().to_string()),
                _ => Err(AppError(
                    StatusCode::BAD_REQUEST,
                    format!("'{field}' must be an array of strings or a comma-separated string"),
                )),
            })
            .filter_map(|r| match r {
                Ok(s) if s.is_empty() => None,
                other => Some(other),
            })
            .collect(),
        Some(serde_json::Value::String(s)) => Ok(s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()),
        Some(_) => Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("'{field}' must be an array of strings or a comma-separated string"),
        )),
    }
}

fn decode_optional_labels_field(
    field: &str,
    value: Option<serde_json::Value>,
) -> Result<Option<Vec<String>>> {
    match value {
        None => Ok(None),
        Some(v) => decode_labels_field(field, Some(v)).map(Some),
    }
}

pub async fn list_patterns_handler(
    State(state): State<AppState>,
    Query(q): Query<ListPatternsQuery>,
) -> Result<Json<Vec<db::PatternSummary>>> {
    if let Some(version) = q.version.as_deref() {
        validate_pattern_version_field(version.trim())?;
    }
    if let Some(state) = q.state.as_deref() {
        validate_pattern_state_field(state.trim())?;
    }
    let db = state.db.clone();
    let query = q.q;
    let label = q.label;
    let version = q.version;
    let state_value = q.state;
    let superseded_by = q.superseded_by;
    let patterns = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let filters = db::PatternFilters {
            query: query.as_deref(),
            label: label.as_deref(),
            version: version.as_deref(),
            state: state_value.as_deref(),
            superseded_by: superseded_by.as_deref(),
        };
        db::list_patterns(&conn, &filters)
    })
    .await??;
    Ok(Json(patterns))
}

pub async fn create_pattern_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreatePatternRequest>,
) -> Result<Json<db::Pattern>> {
    if req.title.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "title is required".into(),
        ));
    }
    if req.body.trim().is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "body is required".into()));
    }
    validate_pattern_version_field(req.version.trim())?;
    validate_pattern_state_field(req.state.trim())?;
    let superseded_by = req
        .superseded_by
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if req.version.trim() == "superseded" && superseded_by.is_none() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "superseded_by is required when version is superseded".into(),
        ));
    }
    if req.version.trim() != "superseded" && superseded_by.is_some() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "superseded_by can only be set when version is superseded".into(),
        ));
    }

    let labels = decode_labels_field("labels", req.labels)?;
    let author = resolve_identity(req.author, &headers);
    let db = state.db.clone();
    let title = req.title;
    let slug = req.slug;
    let summary = req.summary;
    let body = req.body;
    let version = req.version;
    let state_value = req.state;
    let superseded_by = req.superseded_by;

    let pattern = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::insert_pattern(
            &conn,
            title.trim(),
            slug.as_deref().map(str::trim),
            summary.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            &body,
            &labels,
            version.trim(),
            state_value.trim(),
            superseded_by.as_deref().map(str::trim),
            &author,
        )
    })
    .await??;
    Ok(Json(pattern))
}

pub async fn get_pattern_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<db::Pattern>> {
    let db = state.db.clone();
    let id_for_lookup = id.clone();
    let pattern = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_pattern(&conn, &id_for_lookup)
    })
    .await??;

    match pattern {
        Some(pattern) => Ok(Json(pattern)),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("pattern '{}' not found", id),
        )),
    }
}

pub async fn update_pattern_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdatePatternRequest>,
) -> Result<Json<db::Pattern>> {
    let summary = decode_nullable_string("summary", req.summary)?;
    let labels = decode_optional_labels_field("labels", req.labels)?;
    let superseded_by = decode_nullable_string("superseded_by", req.superseded_by)?;
    if let Some(version) = req.version.as_deref() {
        validate_pattern_version_field(version.trim())?;
    }
    if let Some(state) = req.state.as_deref() {
        validate_pattern_state_field(state.trim())?;
    }
    let db = state.db.clone();
    let id_for_update = id.clone();
    let title = req.title;
    let slug = req.slug;
    let body = req.body;
    let version = req.version;
    let state_value = req.state;

    let pattern = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let upd = db::PatternUpdate {
            title: title.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            slug: slug.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            summary: summary.as_ref().map(|inner| inner.as_deref()),
            body: body.as_deref(),
            labels: labels.as_deref(),
            version: version.as_deref().map(str::trim),
            state: state_value.as_deref().map(str::trim),
            superseded_by: superseded_by.as_ref().map(|inner| inner.as_deref()),
        };
        db::update_pattern(&conn, &id_for_update, &upd)
    })
    .await??;

    match pattern {
        Some(pattern) => Ok(Json(pattern)),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("pattern '{}' not found", id),
        )),
    }
}

pub async fn delete_pattern_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<DeleteResponse>> {
    let db = state.db.clone();
    let id_for_delete = id.clone();
    let deleted = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::delete_pattern(&conn, &id_for_delete)
    })
    .await??;

    if deleted {
        Ok(Json(DeleteResponse { deleted }))
    } else {
        Err(AppError(
            StatusCode::NOT_FOUND,
            format!("pattern '{}' not found", id),
        ))
    }
}

pub async fn list_pattern_comments_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<db::PatternComment>>> {
    let db = state.db.clone();
    let id_for_lookup = id.clone();
    let comments = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::list_pattern_comments(&conn, &id_for_lookup)
    })
    .await??;

    match comments {
        Some(comments) => Ok(Json(comments)),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("pattern '{}' not found", id),
        )),
    }
}

pub async fn add_pattern_comment_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<AddCommentRequest>,
) -> Result<Json<db::PatternComment>> {
    if req.content.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "content is required".into(),
        ));
    }
    let author = resolve_identity(req.author, &headers);
    let author_type = req.author_type.unwrap_or_else(|| {
        if actor_agent_id(&headers).is_some() {
            "agent".to_string()
        } else {
            "user".to_string()
        }
    });
    if author_type != "agent" && author_type != "user" && author_type != "system" {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("invalid author_type '{author_type}': must be agent|user|system"),
        ));
    }

    let db = state.db.clone();
    let id_for_insert = id.clone();
    let content = req.content;
    let comment = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::insert_pattern_comment(&conn, &id_for_insert, &author, &author_type, &content)
    })
    .await??;

    match comment {
        Some(comment) => Ok(Json(comment)),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("pattern '{}' not found", id),
        )),
    }
}

fn pattern_version_options(selected: &str) -> String {
    ["draft", "latest", "superseded"]
        .iter()
        .map(|value| {
            if *value == selected {
                format!(r#"<option value="{value}" selected>{value}</option>"#)
            } else {
                format!(r#"<option value="{value}">{value}</option>"#)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn pattern_state_options(selected: &str) -> String {
    ["active", "archived"]
        .iter()
        .map(|value| {
            if *value == selected {
                format!(r#"<option value="{value}" selected>{value}</option>"#)
            } else {
                format!(r#"<option value="{value}">{value}</option>"#)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn pattern_superseded_by_options(
    patterns: &[db::PatternSummary],
    selected: Option<&str>,
    exclude_id: Option<&str>,
) -> String {
    let mut options = vec![r#"<option value="">Select replacement pattern</option>"#.to_string()];
    options.extend(
        patterns
            .iter()
            .filter(|p| exclude_id != Some(p.id.as_str()))
            .map(|p| {
                let selected_attr = if selected.map(|s| s == p.id || s == p.slug).unwrap_or(false) {
                    " selected"
                } else {
                    ""
                };
                let label = format!("{} ({})", p.title, p.slug);
                format!(
                    r#"<option value="{}"{}>{}</option>"#,
                    he(&p.id),
                    selected_attr,
                    he(&label)
                )
            }),
    );
    options.join("\n")
}

fn pattern_superseded_select_script() -> &'static str {
    r#"<script>
(() => {
  const syncPatternSupersededControls = () => {
    document.querySelectorAll('[data-pattern-version-select]').forEach((version) => {
      const form = version.closest('form');
      const select = form && form.querySelector('[data-pattern-superseded-select]');
      if (!select) return;
      const enabled = version.value === 'superseded';
      select.disabled = !enabled;
      select.required = enabled;
      if (!enabled) select.value = '';
    });
  };
  document.addEventListener('change', (event) => {
    if (event.target && event.target.matches('[data-pattern-version-select]')) {
      syncPatternSupersededControls();
    }
  });
  syncPatternSupersededControls();
})();
</script>"#
}

// ── Tasks API ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ListTasksQuery {
    /// Comma-separated list of statuses (e.g. "todo,in_progress").
    /// Defaults to `todo,in_progress` when absent.
    pub status: Option<String>,
    /// When true, include `done` tasks older than 7 days. Default false.
    pub include_stale: Option<bool>,
}

#[derive(Deserialize)]
pub struct CreateTaskRequest {
    pub title: String,
    pub description: Option<String>,
    pub specification: Option<String>,
    pub details: Option<String>,
    pub labels: Option<Vec<String>>,
    pub hostname: Option<String>,
    /// Optional override of reporter. Defaults to X-Agent-Id header, or "user"
    /// when the header is absent or "_default".
    pub reporter: Option<String>,
}

#[derive(Deserialize)]
pub struct DelegateTaskRequest {
    pub target_project_ident: String,
    pub title: String,
    pub description: Option<String>,
    pub details: Option<String>,
    pub specification: Option<String>,
    pub labels: Option<Vec<String>>,
    pub hostname: Option<String>,
    pub reporter: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateTaskRequest {
    pub status: Option<String>,
    /// `Some(null)` in JSON clears the owner; `Some("xyz")` assigns it;
    /// absent leaves the current owner alone.
    pub owner_agent_id: Option<serde_json::Value>,
    pub rank: Option<i64>,
    pub title: Option<String>,
    pub description: Option<serde_json::Value>,
    pub specification: Option<serde_json::Value>,
    pub details: Option<serde_json::Value>,
    pub labels: Option<Vec<String>>,
    pub hostname: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct UpdateDelegationRequest {
    pub title: Option<String>,
    pub description: Option<Option<String>>,
    pub details: Option<Option<String>>,
    pub specification: Option<Option<String>>,
    pub labels: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct AddCommentRequest {
    pub content: String,
    /// `"agent"` | `"user"`. Defaults based on whether X-Agent-Id is present.
    pub author_type: Option<String>,
    /// Defaults to X-Agent-Id header, or `"user"` when the header is absent or
    /// `"_default"`.
    pub author: Option<String>,
}

/// Flat detail shape: all `Task` fields at the top level (via `#[serde(flatten)]`)
/// plus a sibling `comments` array and a derived `actions` array. Designed so
/// that ndesign's `data-nd-bind` can render the detail view — including
/// status-transition buttons — without template conditionals.
#[derive(Serialize)]
pub struct TaskWithComments {
    #[serde(flatten)]
    pub task: db::Task,
    pub specification: Option<String>,
    pub comments: Vec<db::TaskComment>,
    pub actions: Vec<TaskAction>,
}

impl TaskWithComments {
    fn new(task: db::Task, comments: Vec<db::TaskComment>) -> Self {
        let specification = task.details.clone();
        let actions = actions_for_status(&task.status);
        Self {
            task,
            specification,
            comments,
            actions,
        }
    }
}

const TASK_SPECIFICATION_HINT: &str = "If this is a complex task, make sure it has a proper specification so another agent can pick up the work if you drop it.";

#[derive(Serialize)]
pub struct TaskCreateResponse {
    #[serde(flatten)]
    pub task: db::Task,
    pub specification: Option<String>,
    pub hint: &'static str,
}

#[derive(Serialize)]
pub struct DelegationResponse {
    pub delegation: db::TaskDelegation,
    pub source_task: db::Task,
    pub target_task: db::Task,
    pub message_id: i64,
}

impl TaskCreateResponse {
    fn new(task: db::Task) -> Self {
        let specification = task.details.clone();
        Self {
            task,
            specification,
            hint: TASK_SPECIFICATION_HINT,
        }
    }
}

fn system_nudge(
    conn: &rusqlite::Connection,
    project_ident: &str,
    subject: &str,
    content: String,
) -> anyhow::Result<i64> {
    db::insert_message(
        conn,
        &Message {
            id: 0,
            project_ident: project_ident.to_string(),
            source: "system".into(),
            external_message_id: None,
            content,
            sent_at: now_ms(),
            confirmed_at: None,
            parent_message_id: None,
            agent_id: None,
            message_type: "message".into(),
            subject: Some(subject.to_string()),
            hostname: Some("agent-gateway".into()),
            event_at: Some(now_ms()),
            deliver_to_agents: true,
        },
    )
}

/// One status-transition button derived from the task's current status.
///
/// The UI iterates this array inside the modal; each entry is rendered as a
/// `<button data-nd-action="PATCH …" data-nd-body=…>` so ndesign fires the
/// PATCH when the user clicks. `style` is the `nd-btn-*` suffix
/// (`primary` | `secondary`) so the template can build the class name.
#[derive(Serialize)]
pub struct TaskAction {
    pub verb: String,
    pub style: String,
    pub target_status: String,
}

/// Compute the list of allowed status transitions for a given current status.
/// Kept in one place so the UI and any future API consumers agree.
fn actions_for_status(status: &str) -> Vec<TaskAction> {
    let mk = |verb: &str, style: &str, target: &str| TaskAction {
        verb: verb.into(),
        style: style.into(),
        target_status: target.into(),
    };
    match status {
        "todo" => vec![
            mk("Claim", "primary", "in_progress"),
            mk("Done", "primary", "done"),
        ],
        "in_progress" => vec![
            mk("Release", "secondary", "todo"),
            mk("Done", "primary", "done"),
        ],
        "done" => vec![mk("Reopen", "secondary", "todo")],
        _ => vec![],
    }
}

#[derive(Serialize)]
pub struct DeleteResponse {
    pub deleted: bool,
}

#[derive(Deserialize)]
pub struct ReorderTasksQuery {
    /// Target column (`todo` | `in_progress` | `done`). Required.
    pub status: String,
}

#[derive(Deserialize)]
pub struct ReorderTasksRequest {
    pub order: Vec<String>,
}

/// Parse a JSON nullable-string update field.
///
/// - `None`                          → `None`        (field not touched)
/// - `Some(Value::Null)`             → `Some(None)`  (clear column)
/// - `Some(Value::String(s))`        → `Some(Some(s))` (set column)
/// - anything else                   → 400
fn decode_nullable_string(
    field: &str,
    value: Option<serde_json::Value>,
) -> Result<Option<Option<String>>> {
    match value {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(Some(None)),
        Some(serde_json::Value::String(s)) => Ok(Some(Some(s))),
        Some(_) => Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("'{field}' must be a string or null"),
        )),
    }
}

/// Resolve the reporter/author identity from an explicit body field, the
/// X-Agent-Id header, or fall back to `"user"`.
fn resolve_identity(explicit: Option<String>, headers: &HeaderMap) -> String {
    if let Some(s) = explicit.and_then(|s| {
        let t = s.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    }) {
        return s;
    }
    let hdr = extract_agent_id(headers);
    if hdr == "_default" {
        "user".to_string()
    } else {
        hdr
    }
}

/// Optional agent id from header for actor-aware operations (None when the
/// header is absent or is the sentinel "_default").
fn actor_agent_id(headers: &HeaderMap) -> Option<String> {
    let hdr = extract_agent_id(headers);
    if hdr == "_default" {
        None
    } else {
        Some(hdr)
    }
}

pub async fn list_tasks_handler(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    Query(q): Query<ListTasksQuery>,
) -> Result<Json<Vec<db::TaskSummary>>> {
    // Verify project exists (consistent with other handlers).
    {
        let conn = state.db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            ));
        }
    }

    let statuses: Vec<String> = match q.status.as_deref() {
        None | Some("") => vec!["todo".into(), "in_progress".into()],
        Some(s) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
    };
    for s in &statuses {
        if s != "todo" && s != "in_progress" && s != "done" {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                format!("invalid status '{s}': must be todo|in_progress|done"),
            ));
        }
    }
    let include_stale = q.include_stale.unwrap_or(false);

    let db = state.db.clone();
    let ident_for_reclaim = ident.clone();
    let ident_for_list = ident;
    let tasks = spawn_blocking(move || -> anyhow::Result<Vec<db::TaskSummary>> {
        let conn = db.lock().unwrap();
        // Reclaim stale in-progress tasks before listing so clients see a
        // consistent view.
        db::reclaim_stale_tasks(&conn, &ident_for_reclaim)?;
        db::list_tasks(&conn, &ident_for_list, &statuses, include_stale)
    })
    .await??;

    Ok(Json(tasks))
}

pub async fn create_task_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ident): Path<String>,
    Json(req): Json<CreateTaskRequest>,
) -> Result<Json<TaskCreateResponse>> {
    // Verify project exists.
    {
        let conn = state.db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            ));
        }
    }

    let title = req.title.trim().to_string();
    if title.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "'title' must be non-empty".into(),
        ));
    }

    let reporter = resolve_identity(req.reporter, &headers);
    let description = req.description;
    let specification = req.specification.or(req.details);
    let labels = req.labels.unwrap_or_default();
    let hostname = req.hostname;

    let db = state.db.clone();
    let ident_clone = ident;
    let task = spawn_blocking(move || -> anyhow::Result<db::Task> {
        let conn = db.lock().unwrap();
        let task = db::insert_task(
            &conn,
            &ident_clone,
            &title,
            description.as_deref(),
            specification.as_deref(),
            &labels,
            hostname.as_deref(),
            &reporter,
        )?;
        if task.owner_agent_id.is_none() && task.status == "todo" {
            system_nudge(
                &conn,
                &ident_clone,
                "Task created",
                format!(
                    "New task `{}` was created in project `{}`: {}\n\nFetch task `{}` to claim and execute it.",
                    task.id, ident_clone, task.title, task.id
                ),
            )?;
        }
        Ok(task)
    })
    .await??;

    Ok(Json(TaskCreateResponse::new(task)))
}

pub async fn delegate_task_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(source_ident): Path<String>,
    Json(req): Json<DelegateTaskRequest>,
) -> Result<Json<DelegationResponse>> {
    let target_ident = req.target_project_ident.trim().to_string();
    let title = req.title.trim().to_string();
    if target_ident.is_empty() || title.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "'target_project_ident' and 'title' must be non-empty".into(),
        ));
    }
    if target_ident == source_ident {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "delegated task target must be a different project".into(),
        ));
    }

    let reporter = resolve_identity(req.reporter, &headers);
    let requester = actor_agent_id(&headers);
    let description = req.description;
    let specification = req.specification.or(req.details);
    let labels = req.labels.unwrap_or_default();
    let hostname = req.hostname;

    let db = state.db.clone();
    let response = spawn_blocking(move || -> anyhow::Result<DelegationResponse> {
        let conn = db.lock().unwrap();
        if db::get_project(&conn, &source_ident)?.is_none() {
            anyhow::bail!("project '{source_ident}' not found");
        }
        if db::get_project(&conn, &target_ident)?.is_none() {
            anyhow::bail!("project '{target_ident}' not found");
        }

        let target_task = db::insert_task(
            &conn,
            &target_ident,
            &title,
            description.as_deref(),
            specification.as_deref(),
            &labels,
            hostname.as_deref(),
            &reporter,
        )?;
        let source_title = format!("{title} (DELEGATED)");
        let source_task = db::insert_delegated_task(
            &conn,
            &db::DelegatedTaskInsert {
                project_ident: &source_ident,
                title: &source_title,
                description: description.as_deref(),
                details: specification.as_deref(),
                labels: &labels,
                hostname: hostname.as_deref(),
                reporter: &reporter,
                target_project_ident: &target_ident,
                target_task_id: &target_task.id,
            },
        )?;
        let delegation = db::insert_task_delegation(
            &conn,
            &source_ident,
            &source_task.id,
            &target_ident,
            &target_task.id,
            requester.as_deref(),
            hostname.as_deref(),
        )?;
        db::insert_comment(
            &conn,
            &source_task.id,
            "agent-gateway",
            "system",
            &format!(
                "Delegated to project `{}` as task `{}`.",
                target_ident, target_task.id
            ),
        )?;
        db::insert_comment(
            &conn,
            &target_task.id,
            "agent-gateway",
            "system",
            &format!(
                "Delegated from project `{}`; source tracking task `{}`.",
                source_ident, source_task.id
            ),
        )?;
        let message_id = system_nudge(
            &conn,
            &target_ident,
            "Delegated task created",
            format!(
                "Project `{}` delegated task `{}`: {}\n\nFetch task `{}` in project `{}` to claim and execute it.",
                source_ident, target_task.id, title, target_task.id, target_ident
            ),
        )?;

        Ok(DelegationResponse {
            delegation,
            source_task,
            target_task,
            message_id,
        })
    })
    .await??;

    Ok(Json(response))
}

pub async fn get_task_handler(
    State(state): State<AppState>,
    Path((ident, task_id)): Path<(String, String)>,
) -> Result<Json<TaskWithComments>> {
    let db = state.db.clone();
    let ident_for_reclaim = ident.clone();
    let ident_for_fetch = ident.clone();
    let task_id_clone = task_id;
    let detail = spawn_blocking(move || -> anyhow::Result<Option<db::TaskDetail>> {
        let conn = db.lock().unwrap();
        db::reclaim_stale_tasks(&conn, &ident_for_reclaim)?;
        db::get_task_detail(&conn, &ident_for_fetch, &task_id_clone)
    })
    .await??;

    match detail {
        Some(d) => Ok(Json(TaskWithComments::new(d.task, d.comments))),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("task not found in project '{}'", ident),
        )),
    }
}

pub async fn update_task_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((ident, task_id)): Path<(String, String)>,
    Json(req): Json<UpdateTaskRequest>,
) -> Result<Json<db::Task>> {
    // Validate & decode the nullable-string fields up-front so we can return
    // 400 on a client-side shape mistake rather than bubbling a 500.
    let owner_opt = decode_nullable_string("owner_agent_id", req.owner_agent_id)?;
    let description_opt = decode_nullable_string("description", req.description)?;
    let details_opt = if req.specification.is_some() {
        decode_nullable_string("specification", req.specification)?
    } else {
        decode_nullable_string("details", req.details)?
    };
    let hostname_opt = decode_nullable_string("hostname", req.hostname)?;

    let actor = actor_agent_id(&headers);
    let db = state.db.clone();
    let ident_for_reclaim = ident.clone();
    let ident_for_update = ident.clone();
    let task_id_clone = task_id;
    let status = req.status;
    let rank = req.rank;
    let title = req.title;
    let labels = req.labels;

    // Invalid status transitions and bad status values are currently reported
    // by `db::update_task` as anyhow errors; they bubble through `AppError`'s
    // blanket From impl as 500. The CHECK constraint on `status` catches the
    // truly invalid values at the SQL layer. Refine to 400 when we add a
    // dedicated error enum.
    let task = spawn_blocking(move || -> anyhow::Result<Option<db::Task>> {
        let conn = db.lock().unwrap();
        db::reclaim_stale_tasks(&conn, &ident_for_reclaim)?;
        let before = db::get_task_detail(&conn, &ident_for_update, &task_id_clone)?.map(|d| d.task);

        let upd = db::TaskUpdate {
            status: status.as_deref(),
            owner_agent_id: owner_opt.as_ref().map(|inner| inner.as_deref()),
            rank,
            title: title.as_deref(),
            description: description_opt.as_ref().map(|inner| inner.as_deref()),
            details: details_opt.as_ref().map(|inner| inner.as_deref()),
            labels: labels.as_deref(),
            hostname: hostname_opt.as_ref().map(|inner| inner.as_deref()),
        };
        let updated = db::update_task(
            &conn,
            &ident_for_update,
            &task_id_clone,
            &upd,
            actor.as_deref(),
        )?;

        if let (Some(before), Some(after)) = (&before, &updated) {
            if before.status != "done" && after.status == "done" {
                if let Some(delegation) =
                    db::get_delegation_by_target(&conn, &ident_for_update, &task_id_clone)?
                {
                    let target_detail =
                        db::get_task_detail(&conn, &delegation.target_project_ident, &delegation.target_task_id)?
                            .ok_or_else(|| anyhow::anyhow!("target delegated task missing"))?;
                    let comments = target_detail
                        .comments
                        .iter()
                        .filter(|c| c.author_type != "system")
                        .map(|c| format!("- {}: {}", c.author, c.content))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let summary = format!(
                        "Delegated task completed in project `{}`.\n\nTitle: {}\nDescription: {}\nSpecification: {}\nComments:\n{}",
                        delegation.target_project_ident,
                        target_detail.task.title,
                        target_detail.task.description.as_deref().unwrap_or(""),
                        target_detail.task.details.as_deref().unwrap_or(""),
                        if comments.is_empty() { "(none)" } else { &comments }
                    );
                    db::insert_comment(
                        &conn,
                        &delegation.source_task_id,
                        "agent-gateway",
                        "system",
                        &summary,
                    )?;
                    let done_upd = db::TaskUpdate {
                        status: Some("done"),
                        owner_agent_id: None,
                        rank: None,
                        title: None,
                        description: None,
                        details: None,
                        labels: None,
                        hostname: None,
                    };
                    let _ = db::update_task(
                        &conn,
                        &delegation.source_project_ident,
                        &delegation.source_task_id,
                        &done_upd,
                        None,
                    )?;
                    let message_id = system_nudge(
                        &conn,
                        &delegation.source_project_ident,
                        "Delegated task completed",
                        summary,
                    )?;
                    db::mark_delegation_complete(&conn, &delegation.id, message_id)?;
                }
            }
        }

        Ok(updated)
    })
    .await??;

    match task {
        Some(t) => Ok(Json(t)),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("task not found in project '{}'", ident),
        )),
    }
}

pub async fn update_delegation_handler(
    State(state): State<AppState>,
    Path((ident, task_id)): Path<(String, String)>,
    Json(req): Json<UpdateDelegationRequest>,
) -> Result<Json<DelegationResponse>> {
    let details_opt = if req.specification.is_some() {
        req.specification
    } else {
        req.details
    };
    let db = state.db.clone();
    let response = spawn_blocking(move || -> anyhow::Result<DelegationResponse> {
        let conn = db.lock().unwrap();
        let delegation = db::get_delegation_by_source(&conn, &ident, &task_id)?
            .ok_or_else(|| anyhow::anyhow!("task '{task_id}' is not a delegated source task"))?;
        let source_title = req.title.as_ref().map(|title| {
            if title.ends_with(" (DELEGATED)") {
                title.clone()
            } else {
                format!("{title} (DELEGATED)")
            }
        });

        let source_upd = db::TaskUpdate {
            status: None,
            owner_agent_id: None,
            rank: None,
            title: source_title.as_deref(),
            description: req.description.as_ref().map(|inner| inner.as_deref()),
            details: details_opt.as_ref().map(|inner| inner.as_deref()),
            labels: req.labels.as_deref(),
            hostname: None,
        };
        let source_task = db::update_task(
            &conn,
            &delegation.source_project_ident,
            &delegation.source_task_id,
            &source_upd,
            None,
        )?
        .ok_or_else(|| anyhow::anyhow!("source delegated task missing"))?;

        let target_title = req.title.as_deref().map(|t| t.trim_end_matches(" (DELEGATED)"));
        let target_upd = db::TaskUpdate {
            status: None,
            owner_agent_id: None,
            rank: None,
            title: target_title,
            description: req.description.as_ref().map(|inner| inner.as_deref()),
            details: details_opt.as_ref().map(|inner| inner.as_deref()),
            labels: req.labels.as_deref(),
            hostname: None,
        };
        let target_task = db::update_task(
            &conn,
            &delegation.target_project_ident,
            &delegation.target_task_id,
            &target_upd,
            None,
        )?
        .ok_or_else(|| anyhow::anyhow!("target delegated task missing"))?;

        let note = "Delegated task contract was updated by the source project.";
        db::insert_comment(
            &conn,
            &delegation.source_task_id,
            "agent-gateway",
            "system",
            note,
        )?;
        db::insert_comment(
            &conn,
            &delegation.target_task_id,
            "agent-gateway",
            "system",
            note,
        )?;
        let message_id = system_nudge(
            &conn,
            &delegation.target_project_ident,
            "Delegated task updated",
            format!(
                "Project `{}` updated delegated task `{}`. Review the latest title, description, specification, and labels before continuing.",
                delegation.source_project_ident, delegation.target_task_id
            ),
        )?;

        Ok(DelegationResponse {
            delegation,
            source_task,
            target_task,
            message_id,
        })
    })
    .await??;

    Ok(Json(response))
}

pub async fn add_comment_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((ident, task_id)): Path<(String, String)>,
    Json(req): Json<AddCommentRequest>,
) -> Result<Json<db::TaskComment>> {
    let content = req.content;
    if content.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "'content' must be non-empty".into(),
        ));
    }

    // Confirm the task exists in the project first.
    {
        let conn = state.db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            ));
        }
        if db::get_task_detail(&conn, &ident, &task_id)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("task not found in project '{}'", ident),
            ));
        }
    }

    let header_agent = actor_agent_id(&headers);
    let author = resolve_identity(req.author, &headers);
    let author_type = match req.author_type.as_deref() {
        Some("agent") => "agent".to_string(),
        Some("user") => "user".to_string(),
        Some(other) => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                format!("invalid author_type '{other}': must be agent|user"),
            ));
        }
        None => {
            if header_agent.is_some() {
                "agent".into()
            } else {
                "user".into()
            }
        }
    };

    let db = state.db.clone();
    let ident_clone = ident;
    let task_id_clone = task_id;
    let comment = spawn_blocking(move || -> anyhow::Result<db::TaskComment> {
        let conn = db.lock().unwrap();
        let comment = db::insert_comment(&conn, &task_id_clone, &author, &author_type, &content)?;
        if author_type != "system" {
            if let Some(delegation) =
                db::get_delegation_by_target(&conn, &ident_clone, &task_id_clone)?
            {
                let body = format!(
                    "New comment on delegated task `{}` from project `{}`:\n\n{}: {}",
                    delegation.source_task_id, delegation.target_project_ident, author, content
                );
                system_nudge(
                    &conn,
                    &delegation.source_project_ident,
                    "Delegated task comment added",
                    body,
                )?;
            }
        }
        Ok(comment)
    })
    .await??;

    Ok(Json(comment))
}

pub async fn delete_task_handler(
    State(state): State<AppState>,
    Path((ident, task_id)): Path<(String, String)>,
) -> Result<Json<DeleteResponse>> {
    let db = state.db.clone();
    let ident_clone = ident;
    let task_id_clone = task_id;
    let deleted = spawn_blocking(move || -> anyhow::Result<bool> {
        let conn = db.lock().unwrap();
        db::delete_task(&conn, &ident_clone, &task_id_clone)
    })
    .await??;
    Ok(Json(DeleteResponse { deleted }))
}

/// Apply a client-driven reorder within a single status column.
///
/// Designed to receive `data-nd-sortable` POSTs: the body is
/// `{"order": ["id1", "id2", ...]}` and `?status=` selects the column the
/// order applies to. Returns the fresh list for that column so callers can
/// re-render without a follow-up GET.
pub async fn reorder_tasks_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ident): Path<String>,
    axum::extract::Query(q): axum::extract::Query<ReorderTasksQuery>,
    Json(req): Json<ReorderTasksRequest>,
) -> Result<Json<Vec<db::TaskSummary>>> {
    // Verify project exists (consistent with other project-scoped handlers).
    {
        let conn = state.db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            ));
        }
    }

    let status = q.status.clone();
    if status != "todo" && status != "in_progress" && status != "done" {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("invalid status '{status}': must be todo|in_progress|done"),
        ));
    }

    let actor = actor_agent_id(&headers);
    let db = state.db.clone();
    let ident_clone = ident;
    let status_clone = status.clone();
    let order = req.order;

    let tasks = spawn_blocking(move || -> anyhow::Result<Vec<db::TaskSummary>> {
        let conn = db.lock().unwrap();
        db::reorder_tasks_in_column(&conn, &ident_clone, &status_clone, &order, actor.as_deref())?;
        db::list_tasks(
            &conn,
            &ident_clone,
            std::slice::from_ref(&status_clone),
            false,
        )
    })
    .await??;

    Ok(Json(tasks))
}

// ── GET /v1/projects (JSON — used by the Tasks picker binding) ───────────────

/// List all registered projects with the same per-project stats shape the
/// dashboard uses. Returned as a bare array (no envelope) so ndesign's
/// `data-nd-bind` can render rows directly.
pub async fn list_projects_handler(
    State(state): State<AppState>,
) -> Result<Json<Vec<db::ProjectStats>>> {
    let db = state.db.clone();
    let projects = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::list_project_stats(&conn)
    })
    .await??;
    Ok(Json(projects))
}

// ── GET /patterns (global pattern library) ───────────────────────────────────

pub async fn patterns_page(State(state): State<AppState>) -> Result<Html<String>> {
    let db = state.db.clone();
    let (theme, patterns) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        Ok((
            db::get_theme(&conn)?,
            db::list_patterns(&conn, &db::PatternFilters::default())?,
        ))
    })
    .await??;

    let rows = if patterns.is_empty() {
        r#"<tr><td colspan="5" class="nd-text-muted">No patterns yet.</td></tr>"#.to_string()
    } else {
        patterns
            .iter()
            .map(|p| {
                let summary = p.summary.as_deref().unwrap_or("");
                format!(
                    r#"<tr>
  <td>
    <a class="nd-btn-ghost nd-text-left" href="/patterns/{id}">
      <strong>{title}</strong>
    </a>
    <div class="nd-text-muted nd-text-sm">{summary}</div>
  </td>
  <td class="nd-text-muted">{slug}</td>
  <td>{version}</td>
  <td class="nd-text-muted">{state}</td>
  <td>{comment_count}</td>
</tr>"#,
                    id = path_segment(&p.id),
                    title = he(&p.title),
                    summary = he(summary),
                    slug = he(&p.slug),
                    version = he(&p.version),
                    state = he(&p.state),
                    comment_count = p.comment_count,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let content = format!(
        r##"  <div class="nd-flex nd-gap-md nd-mb-md">
    <button class="nd-btn-primary nd-btn-sm" data-nd-modal="#new-pattern-modal">+ New pattern</button>
  </div>

  <section class="nd-card">
    <div class="nd-card-header"><strong>Patterns</strong></div>
    <div class="nd-card-body nd-p-0">
      <table class="nd-table nd-table-hover">
        <thead>
          <tr><th>Pattern</th><th>Slug</th><th>Version</th><th>State</th><th>Comments</th></tr>
        </thead>
        <tbody id="patterns-list">
          {rows}
        </tbody>
      </table>
    </div>
  </section>

  <dialog id="new-pattern-modal" class="nd-modal nd-modal-lg">
    <form data-nd-action="POST /v1/patterns"
          data-nd-success="close-modal,refresh:#patterns-list,reset">
      <header><h3>New pattern</h3></header>
      <div>
        <div class="nd-form-group">
          <label for="pattern-title">Title</label>
          <input id="pattern-title" name="title" required>
        </div>
        <div class="nd-form-group">
          <label for="pattern-slug">Slug</label>
          <input id="pattern-slug" name="slug">
        </div>
        <div class="nd-form-group">
          <label for="pattern-summary">Summary</label>
          <textarea id="pattern-summary" name="summary" rows="2"></textarea>
        </div>
        <div class="nd-form-group">
          <label for="pattern-labels">Labels</label>
          <input id="pattern-labels" name="labels">
        </div>
        <div class="nd-form-group">
          <label for="pattern-version">Version</label>
          <select id="pattern-version" name="version" data-pattern-version-select required>
            {version_options}
          </select>
        </div>
        <div class="nd-form-group">
          <label for="pattern-state">State</label>
          <select id="pattern-state" name="state" required>
            {state_options}
          </select>
        </div>
        <div class="nd-form-group">
          <label for="pattern-superseded-by">Superseded by</label>
          <select id="pattern-superseded-by" name="superseded_by" data-pattern-superseded-select disabled>
            {superseded_options}
          </select>
        </div>
        <div class="nd-form-group">
          <label for="pattern-body">Markdown</label>
          <textarea id="pattern-body" name="body" rows="16" required></textarea>
        </div>
      </div>
      <footer>
        <button type="button" data-nd-dismiss class="nd-btn-ghost">Cancel</button>
        <button type="submit" class="nd-btn-primary">Create</button>
      </footer>
    </form>
  </dialog>

  {script}"##,
        rows = rows,
        version_options = pattern_version_options("draft"),
        state_options = pattern_state_options("active"),
        superseded_options = pattern_superseded_by_options(&patterns, None, None),
        script = pattern_superseded_select_script(),
    );

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway — Patterns", &theme, "",),
        open = control_panel_open("Patterns", "patterns"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

// ── GET /patterns/:id (global pattern detail/editor) ─────────────────────────

pub async fn pattern_detail_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Html<String>> {
    let db = state.db.clone();
    let id_for_lookup = id.clone();
    let (pattern, theme, patterns) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        Ok((
            db::get_pattern(&conn, &id_for_lookup)?,
            db::get_theme(&conn)?,
            db::list_patterns(&conn, &db::PatternFilters::default())?,
        ))
    })
    .await??;

    let pattern = pattern
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("pattern '{}' not found", id)))?;

    let labels = pattern.labels.join(", ");
    let summary = pattern.summary.as_deref().unwrap_or("");
    let detail_title = format!("Pattern: {}", pattern.title);
    let api_id = he(&pattern.id);
    let superseded_meta = pattern
        .superseded_by
        .as_deref()
        .map(|target| format!(" · superseded by {}", he(target)))
        .unwrap_or_default();

    let content = format!(
        r#"  <div class="nd-flex nd-gap-md nd-mb-md">
    <a class="nd-btn-secondary nd-btn-sm" href="/patterns">Back to patterns</a>
  </div>

  <section class="nd-card">
    <div class="nd-card-header">
      <div>
        <strong>{title}</strong>
        <div id="pattern-detail-meta" class="nd-text-muted nd-text-sm">
          slug: {slug} · version {version} · state {state}{superseded_meta} · author {author}
        </div>
      </div>
    </div>
    <div class="nd-card-body">
      <form data-nd-action="PATCH /v1/patterns/{api_id}">
        <div class="nd-row">
          <div class="nd-col-6">
            <div class="nd-form-group">
              <label for="pattern-edit-title">Title</label>
              <input id="pattern-edit-title" name="title" value="{title}" required>
            </div>
          </div>
          <div class="nd-col-6">
            <div class="nd-form-group">
              <label for="pattern-edit-slug">Slug</label>
              <input id="pattern-edit-slug" name="slug" value="{slug}" required>
            </div>
          </div>
        </div>
        <div class="nd-form-group">
          <label for="pattern-edit-summary">Summary</label>
          <textarea id="pattern-edit-summary" name="summary" rows="2">{summary}</textarea>
        </div>
        <div class="nd-form-group">
          <label for="pattern-edit-labels">Labels</label>
          <input id="pattern-edit-labels" name="labels" value="{labels}">
        </div>
        <div class="nd-row">
          <div class="nd-col-6">
            <div class="nd-form-group">
              <label for="pattern-edit-version">Version</label>
              <select id="pattern-edit-version" name="version" data-pattern-version-select required>
                {version_options}
              </select>
            </div>
          </div>
          <div class="nd-col-6">
            <div class="nd-form-group">
              <label for="pattern-edit-state">State</label>
              <select id="pattern-edit-state" name="state" required>
                {state_options}
              </select>
            </div>
          </div>
        </div>
        <div class="nd-form-group">
          <label for="pattern-edit-superseded-by">Superseded by</label>
          <select id="pattern-edit-superseded-by" name="superseded_by" data-pattern-superseded-select>
            {superseded_options}
          </select>
        </div>
        <div class="nd-form-group">
          <label for="pattern-edit-body">Markdown</label>
          <textarea id="pattern-edit-body" name="body" rows="28" required>{body}</textarea>
        </div>
        <div class="nd-flex nd-gap-sm">
          <button type="submit" class="nd-btn-primary">Save pattern</button>
          <a class="nd-btn-secondary" href="/patterns">Done</a>
        </div>
      </form>
    </div>
  </section>

  <section class="nd-card nd-mt-lg">
    <div class="nd-card-header"><strong>Comments</strong></div>
    <div class="nd-card-body">
      <div id="pattern-comments"
           data-nd-bind="/v1/patterns/{api_id}/comments"
           data-nd-template="pattern-comment-tmpl">
        <template id="pattern-comment-tmpl">
          <div class="nd-mb-md">
            <div class="nd-text-muted nd-text-sm">{{{{author}}}} ({{{{author_type}}}})</div>
            <div>{{{{content}}}}</div>
          </div>
        </template>
        <template data-nd-empty>
          <p class="nd-text-muted nd-text-sm">No comments yet.</p>
        </template>
      </div>

      <form class="nd-mt-lg"
            data-nd-action="POST /v1/patterns/{api_id}/comments"
            data-nd-success="refresh:#pattern-comments,reset">
        <div class="nd-form-group">
          <label for="pattern-comment">Add a comment</label>
          <textarea id="pattern-comment" name="content" rows="3" required></textarea>
        </div>
        <button type="submit" class="nd-btn-primary nd-btn-sm">Comment</button>
      </form>
    </div>
  </section>

  {script}"#,
        api_id = api_id,
        title = he(&pattern.title),
        slug = he(&pattern.slug),
        version = he(&pattern.version),
        state = he(&pattern.state),
        superseded_meta = superseded_meta,
        author = he(&pattern.author),
        summary = he(summary),
        labels = he(&labels),
        body = he(&pattern.body),
        version_options = pattern_version_options(&pattern.version),
        state_options = pattern_state_options(&pattern.state),
        superseded_options = pattern_superseded_by_options(
            &patterns,
            pattern.superseded_by.as_deref(),
            Some(&pattern.id),
        ),
        script = pattern_superseded_select_script(),
    );

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway — Pattern", &theme, ""),
        open = control_panel_open(&detail_title, "patterns"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

// ── GET /tasks (project picker) ──────────────────────────────────────────────

/// Render the project picker with task counts so `/tasks` stays focused on
/// task-board navigation rather than dashboard message metadata.
pub async fn tasks_picker(State(state): State<AppState>) -> Result<Html<String>> {
    let db = state.db.clone();
    let (theme, projects) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        Ok((db::get_theme(&conn)?, db::list_project_task_stats(&conn)?))
    })
    .await??;

    let rows = if projects.is_empty() {
        r#"<tr><td colspan="4" class="nd-text-muted">No projects registered yet.</td></tr>"#
            .to_string()
    } else {
        projects
            .iter()
            .map(|p| {
                format!(
                    r#"<tr>
  <td><a class="nd-btn-ghost nd-text-left" href="/projects/{ident}/tasks"><strong>{ident}</strong></a></td>
  <td>{todo}</td>
  <td>{in_progress}</td>
  <td>{done}</td>
</tr>"#,
                    ident = he(&p.ident),
                    todo = p.todo_count,
                    in_progress = p.in_progress_count,
                    done = p.done_count,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let content = format!(
        r##"  <section class="nd-card">
    <div class="nd-card-header"><strong>Projects</strong></div>
    <div class="nd-card-body nd-p-0">
      <table class="nd-table nd-table-hover">
        <thead>
          <tr><th>Project</th><th>Todo</th><th>In progress</th><th>Complete</th></tr>
        </thead>
        <tbody>
          {rows}
        </tbody>
      </table>
    </div>
  </section>"##,
        rows = rows,
    );

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway — Tasks", &theme, ""),
        open = control_panel_open("Tasks", "tasks"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

pub async fn new_task_page(
    State(state): State<AppState>,
    Path(ident): Path<String>,
) -> Result<Html<String>> {
    let db_handle = state.db.clone();
    let ident_for_lookup = ident.clone();
    let (project, theme) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        Ok((
            db::get_project(&conn, &ident_for_lookup)?,
            db::get_theme(&conn)?,
        ))
    })
    .await??;

    let project = project.ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("project '{}' not found", ident),
        )
    })?;

    let ident_attr = he(&project.ident);
    let page_title = format!("New task - {}", project.ident);
    let content = format!(
        r#"  <div class="nd-flex nd-gap-md nd-mb-md">
    <a class="nd-btn-secondary nd-btn-sm" href="/projects/{ident}/tasks">Back to board</a>
  </div>

  <section class="nd-card">
    <div class="nd-card-header"><strong>New task</strong></div>
    <div class="nd-card-body">
      <form data-nd-action="POST /v1/projects/{ident}/tasks">
        <div class="nd-form-group">
          <label for="new-task-title">Title</label>
          <input id="new-task-title" name="title" required>
        </div>
        <div class="nd-form-group">
          <label for="new-task-description">Description</label>
          <textarea id="new-task-description" name="description" rows="5"></textarea>
        </div>
        <div class="nd-form-group">
          <label for="new-task-specification">Specification</label>
          <textarea id="new-task-specification" name="specification" rows="24"></textarea>
        </div>
        <div class="nd-flex nd-gap-sm">
          <button type="submit" class="nd-btn-primary">Create task</button>
          <a class="nd-btn-secondary" href="/projects/{ident}/tasks">Done</a>
        </div>
      </form>
    </div>
  </section>"#,
        ident = ident_attr,
    );

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway - New task", &theme, ""),
        open = control_panel_open(&page_title, "tasks"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

// ── GET /projects/:ident/tasks (board) ───────────────────────────────────────

/// Render the three-column task board for a single project.
///
/// The columns bind to `GET /v1/projects/:ident/tasks?status=…` and the
/// drag-and-drop reorder posts to `POST /v1/projects/:ident/tasks/reorder?status=…`.
/// Returns 404 when the project is not registered.
pub async fn tasks_board(
    State(state): State<AppState>,
    Path(ident): Path<String>,
) -> Result<Html<String>> {
    let db_handle = state.db.clone();
    let ident_for_lookup = ident.clone();
    let (project, theme) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        let project = db::get_project(&conn, &ident_for_lookup)?;
        let theme = db::get_theme(&conn)?;
        Ok((project, theme))
    })
    .await??;

    let project = project.ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("project '{}' not found", ident),
        )
    })?;

    // `ident` is produced by `sanitize_ident` (enforced at registration
    // time, see `register_project`) so it is already safe for URLs. We still
    // HTML-escape before emitting into attribute values and text nodes as
    // defense in depth.
    let ident_attr = he(&project.ident);
    let page_title = format!("Tasks — {}", project.ident);

    // Layout: `.nd-row` gives each `.nd-col-*` 0.5rem of inner padding on
    // both sides (Bootstrap-style gutter). For the cards to have visible
    // space BETWEEN them, the card must live *inside* the col wrapper — if
    // `.nd-card` and `.nd-col-*` are applied to the same element, the col
    // padding lands inside the card's background and the three cards appear
    // flush against each other.
    //
    // Cross-column drag: each column carries `data-nd-sortable-group="tasks"`
    // so the ndesign sortable runtime allows drops between them. On a
    // cross-column drop the runtime POSTs to the destination column's reorder
    // URL, which mutates the task's status server-side (see
    // `reorder_tasks_in_column`). A follow-up `nd:refresh` on every column
    // keeps the board in sync without a reload.
    //
    // Modal pattern (ndesign SPEC §5.8, §20.12): the card button writes the
    // task id into the `selectedTaskId` store var, opens the dialog, and
    // dispatches `nd:refresh` on every bound panel inside the dialog. The
    // bound panels share the same URL so the runtime dedupes them into a
    // single HTTP fetch. `#task-modal-meta` MUST be in the refresh list —
    // it is `data-nd-defer` and holds the description + specification, so
    // without the explicit refresh those fields stay blank on first open.
    let content = format!(
        r##"  <div class="nd-flex nd-gap-md nd-mb-md">
    <a class="nd-btn-ghost nd-btn-sm" href="/tasks">← All projects</a>
    <a class="nd-btn-primary nd-btn-sm" href="/projects/{ident}/tasks/new">+ New task</a>
  </div>

  <!-- Shared card template used by all three columns. -->
  <template id="task-card">
    <li class="nd-card nd-mb-sm task-kind-{{{{kind}}}}" data-id="{{{{id}}}}">
      <button type="button"
              class="nd-card-body nd-btn-ghost nd-text-left nd-w-full task-card-button"
              data-nd-set="selectedTaskId='{{{{id}}}}'"
              data-nd-modal="#task-modal"
              data-nd-success="refresh:#task-modal-header,refresh:#task-modal-meta,refresh:#task-modal-comments">
        <div class="nd-font-semibold task-card-title">{{{{title}}}}</div>
        <ul class="task-card-meta nd-text-muted nd-text-sm">
          <li>{{{{comment_count}}}} comments</li>
        </ul>
      </button>
    </li>
  </template>

  <div class="nd-row">
    <div class="nd-col-4">
      <section class="nd-card">
        <div class="nd-card-header"><strong>TODO</strong></div>
        <ul class="nd-card-body"
            id="col-todo"
            data-nd-bind="/v1/projects/{ident}/tasks?status=todo"
            data-nd-template="task-card"
            data-nd-sortable="POST /v1/projects/{ident}/tasks/reorder?status=todo"
            data-nd-sortable-group="tasks"
            data-nd-sortable-refresh="#col-todo,#col-in_progress,#col-done">
          <template data-nd-empty>
            <li class="nd-text-muted nd-text-sm">No tasks.</li>
          </template>
        </ul>
      </section>
    </div>

    <div class="nd-col-4">
      <section class="nd-card">
        <div class="nd-card-header"><strong>IN PROGRESS</strong></div>
        <ul class="nd-card-body"
            id="col-in_progress"
            data-nd-bind="/v1/projects/{ident}/tasks?status=in_progress"
            data-nd-template="task-card"
            data-nd-sortable="POST /v1/projects/{ident}/tasks/reorder?status=in_progress"
            data-nd-sortable-group="tasks"
            data-nd-sortable-refresh="#col-todo,#col-in_progress,#col-done">
          <template data-nd-empty>
            <li class="nd-text-muted nd-text-sm">No tasks.</li>
          </template>
        </ul>
      </section>
    </div>

    <div class="nd-col-4">
      <section class="nd-card">
        <div class="nd-card-header"><strong>DONE</strong></div>
        <ul class="nd-card-body"
            id="col-done"
            data-nd-bind="/v1/projects/{ident}/tasks?status=done"
            data-nd-template="task-card"
            data-nd-sortable="POST /v1/projects/{ident}/tasks/reorder?status=done"
            data-nd-sortable-group="tasks"
            data-nd-sortable-refresh="#col-todo,#col-in_progress,#col-done">
          <template data-nd-empty>
            <li class="nd-text-muted nd-text-sm">No tasks.</li>
          </template>
        </ul>
      </section>
    </div>
  </div>

  <!--
    Task detail modal. The bound panels share the same URL so ndesign's
    in-flight dedup issues exactly one GET per open/switch. The action buttons
    are static DOM nodes rather than template-rendered nodes because ndesign's
    click action binding is installed during page init. Every write (PATCH,
    POST comment) refreshes the panels and every column, so the board and the
    modal stay in lockstep without a page reload.
  -->
  <dialog id="task-modal" class="nd-modal nd-modal-lg">
    <header>
      <h3 id="task-modal-header"
          data-nd-bind="/v1/projects/{ident}/tasks/${{selectedTaskId}}"
          data-nd-field="title"
          data-nd-defer></h3>
      <button type="button" class="nd-modal-close" data-nd-dismiss aria-label="Close">&times;</button>
    </header>
    <div>
      <div id="task-modal-meta"
           data-nd-bind="/v1/projects/{ident}/tasks/${{selectedTaskId}}"
           data-nd-template="task-modal-meta-tmpl"
           data-nd-defer>
        <template id="task-modal-meta-tmpl">
          <div class="nd-text-muted nd-text-sm nd-mb-md">
            status: {{{{status}}}} · rank {{{{rank}}}} · reporter {{{{reporter}}}}
          </div>
          <p class="nd-text-muted nd-text-sm">Description</p>
          <p>{{{{description}}}}</p>
          <p class="nd-text-muted nd-text-sm nd-mt-md">Specification</p>
          <pre class="nd-text-sm">{{{{specification}}}}</pre>
        </template>
      </div>

      <div id="task-modal-actions" class="nd-flex nd-gap-sm nd-mt-md nd-mb-lg">
        <button type="button"
                class="nd-btn-primary nd-btn-sm"
                data-nd-action="PATCH /v1/projects/{ident}/tasks/${{selectedTaskId}}"
                data-nd-body='{{"status":"in_progress"}}'
                data-nd-success="refresh:#col-todo,refresh:#col-in_progress,refresh:#col-done,refresh:#task-modal-header,refresh:#task-modal-meta">
          Claim
        </button>
        <button type="button"
                class="nd-btn-secondary nd-btn-sm"
                data-nd-action="PATCH /v1/projects/{ident}/tasks/${{selectedTaskId}}"
                data-nd-body='{{"status":"todo"}}'
                data-nd-success="refresh:#col-todo,refresh:#col-in_progress,refresh:#col-done,refresh:#task-modal-header,refresh:#task-modal-meta">
          Release
        </button>
        <button type="button"
                class="nd-btn-primary nd-btn-sm"
                data-nd-action="PATCH /v1/projects/{ident}/tasks/${{selectedTaskId}}"
                data-nd-body='{{"status":"done"}}'
                data-nd-success="refresh:#col-todo,refresh:#col-in_progress,refresh:#col-done,refresh:#task-modal-header,refresh:#task-modal-meta">
          Done
        </button>
        <button type="button"
                class="nd-btn-secondary nd-btn-sm"
                data-nd-action="PATCH /v1/projects/{ident}/tasks/${{selectedTaskId}}"
                data-nd-body='{{"status":"todo"}}'
                data-nd-success="refresh:#col-todo,refresh:#col-in_progress,refresh:#col-done,refresh:#task-modal-header,refresh:#task-modal-meta">
          Reopen
        </button>
      </div>

      <section class="nd-card">
        <div class="nd-card-header"><strong>Comments</strong></div>
        <div class="nd-card-body">
          <div id="task-modal-comments"
               data-nd-bind="/v1/projects/{ident}/tasks/${{selectedTaskId}}"
               data-nd-select="comments"
               data-nd-template="task-modal-comment-tmpl"
               data-nd-defer>
            <template id="task-modal-comment-tmpl">
              <div class="nd-mb-md">
                <div class="nd-text-muted nd-text-sm">{{{{author}}}} ({{{{author_type}}}})</div>
                <div>{{{{content}}}}</div>
              </div>
            </template>
            <template data-nd-empty>
              <p class="nd-text-muted nd-text-sm">No comments yet.</p>
            </template>
          </div>

          <form class="nd-mt-lg"
                data-nd-action="POST /v1/projects/{ident}/tasks/${{selectedTaskId}}/comments"
                data-nd-success="refresh:#task-modal-comments,reset">
            <div class="nd-form-group">
              <label for="task-modal-comment">Add a comment</label>
              <textarea id="task-modal-comment" name="content" rows="3" required></textarea>
            </div>
            <button type="submit" class="nd-btn-primary nd-btn-sm">Comment</button>
          </form>
        </div>
      </section>
    </div>
  </dialog>"##,
        ident = ident_attr,
    );

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head(
            &page_title,
            &theme,
            r#"<meta name="var:selectedTaskId" content="">
<style>
.task-card-button {
  display: block;
  max-width: 100%;
  min-width: 0;
  white-space: normal;
}
.task-card-title {
  overflow-wrap: anywhere;
  white-space: normal;
}
.task-card-meta {
  margin: 0.375rem 0 0 1.25rem;
  padding: 0;
  white-space: normal;
}
.task-kind-delegated {
  border-left: 4px solid #b45309;
}
.task-kind-delegated .task-card-title {
  color: #92400e;
}
</style>"#,
        ),
        open = control_panel_open(&page_title, "tasks"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

// ── ndesign partials (shared by control-panel pages) ─────────────────────────

/// CDN base for the ndesign runtime and theme stylesheets. Shared by
/// `control_panel_head` / `control_panel_close` and every page that uses
/// them. Kept as a constant so the version is bumped in one place.
const NDESIGN_BASE: &str = "https://storage.googleapis.com/ndesign-cdn/ndesign/latest";

fn theme_toggle_button() -> &'static str {
    r#"<button class="nd-btn-secondary" data-nd-theme-toggle title="Toggle theme">Theme</button>"#
}

// ── Control-panel layout helpers (shared by dashboard + future admin pages) ───

/// Render the `<head>` contents for a control-panel page.
///
/// Emits charset + viewport meta, the page `<title>`, ndesign base CSS, the
/// active theme stylesheet (class `theme` so the runtime switcher can swap it),
/// the two theme-registration meta tags, plus the `endpoint:api` and
/// `csrf-token` meta tags the ndesign runtime expects. `extra` is appended
/// verbatim — pages that declare ndesign store vars (`<meta name="var:…">`)
/// pass them in here so the runtime finds them during init.
///
/// `theme` must be `"light"` or `"dark"`; any other value falls back to
/// `"dark"`.
fn control_panel_head(title: &str, theme: &str, extra: &str) -> String {
    let theme = if theme == "light" { "light" } else { "dark" };
    format!(
        r#"<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<link rel="stylesheet" href="{base}/ndesign.min.css">
<link rel="stylesheet" class="theme" data-theme="{theme}" href="{base}/themes/{theme}.min.css">
<meta name="nd-theme" content="light" data-href="{base}/themes/light.min.css">
<meta name="nd-theme" content="dark" data-href="{base}/themes/dark.min.css">
<meta name="endpoint:api" content="">
<meta name="csrf-token" content="">
{extra}"#,
        title = he(title),
        base = NDESIGN_BASE,
        theme = theme,
        extra = extra,
    )
}

/// Open the control-panel body up to the start of `<main class="app-content">`.
///
/// Emits `<body class="app-page">`, the app layout wrapper, the sidebar (brand
/// plus the Main section with Dashboard, Tasks, Patterns, Skills, Commands,
/// Agents, and Settings links), and the header (hamburger toggle, page title,
/// theme toggle).
///
/// * `page_title` — rendered inside the header's `<h1>`.
/// * `active` — which sidebar link receives `class="nd-active"`. Accepts
///   `"dashboard"`, `"tasks"`, `"patterns"`, `"skills"`, `"commands"`,
///   `"agents"`, or `"settings"`. Any other value leaves all links inactive.
fn control_panel_open(page_title: &str, active: &str) -> String {
    let cls = |key: &str| -> &'static str {
        if key == active {
            r#" class="nd-active""#
        } else {
            ""
        }
    };
    format!(
        r#"<body class="app-page">
<div class="app-layout nd-h-screen nd-overflow-hidden">
  <nav class="sidebar" id="app-sidebar">
    <span class="nd-nav-brand">agent-gateway</span>
    <p class="nd-nav-section">Main</p>
    <ul class="nd-nav-menu">
      <li><a href="/"{dashboard}>Dashboard</a></li>
      <li><a href="/api-docs"{api_docs}>API Docs</a></li>
      <li><a href="/artifacts"{artifacts}>Artifacts</a></li>
      <li><a href="/tasks"{tasks}>Tasks</a></li>
      <li><a href="/patterns"{patterns}>Patterns</a></li>
      <li><a href="/skills"{skills}>Skills</a></li>
      <li><a href="/commands"{commands}>Commands</a></li>
      <li><a href="/agents"{agents}>Agents</a></li>
      <li><a href="/settings"{settings}>Settings</a></li>
    </ul>
  </nav>
  <div class="app-body">
    <header>
      <div class="app-header-left">
        <button class="hamburger" data-nd-toggle="sidebar">&#9776;</button>
        <h1 class="app-header-title">{title}</h1>
      </div>
      <div class="app-header-right">
        {theme_toggle}
      </div>
    </header>
    <main class="app-content">"#,
        dashboard = cls("dashboard"),
        api_docs = cls("api-docs"),
        artifacts = cls("artifacts"),
        tasks = cls("tasks"),
        patterns = cls("patterns"),
        skills = cls("skills"),
        commands = cls("commands"),
        agents = cls("agents"),
        settings = cls("settings"),
        title = he(page_title),
        theme_toggle = theme_toggle_button(),
    )
}

/// Close the control-panel body: close `<main>`, `<div class="app-body">`,
/// and `<div class="app-layout">`, then emit the ndesign runtime script and
/// an inline config block that (a) wires bearer-auth for XHR and (b)
/// persists `nd:theme-change` events back to the server.
///
/// The theme-change listener was historically emitted by `ndesign_scripts`
/// for the old `/manage` page. When the dashboard was refactored onto this
/// shared shell (commit `538d374`), the listener was dropped and theme
/// toggles stopped surviving reloads. Re-registering it here fixes that
/// regression for every page built on the control-panel shell.
///
/// Output is deliberately limited to two `<script>` tags (the ndesign
/// runtime + this inline config block) to keep the per-page script budget
/// predictable.
///
/// The bearer token is JSON-escaped via `serde_json::to_string` so it is safe
/// to interpolate inside the inline script literal.
fn control_panel_close(api_key: &str) -> String {
    let api_key_json = serde_json::to_string(api_key).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"    </main>
  </div>
</div>
<script src="{base}/ndesign.min.js"></script>
<script>
NDesign.configure({{ headers: {{ 'Authorization': 'Bearer ' + {api_key_json} }} }});
document.addEventListener('nd:theme-change', (e) => {{
  const theme = e.detail && e.detail.theme;
  if (!theme) return;
  fetch('/theme', {{
    method: 'POST',
    headers: {{ 'Content-Type': 'application/json' }},
    body: JSON.stringify({{ theme }})
  }}).catch(() => {{}});
}});
</script>
</body>
</html>"#,
        base = NDESIGN_BASE,
        api_key_json = api_key_json,
    )
}

// ── GET /v1/projects/:ident/messages/unread ───────────────────────────────────

#[derive(Serialize)]
pub struct GetUnreadResponse {
    pub messages: Vec<Message>,
    pub status: String,
}

pub async fn get_unread_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ident): Path<String>,
) -> Result<Json<GetUnreadResponse>> {
    {
        let conn = state.db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            ));
        }
    }

    let agent_id = extract_agent_id(&headers);
    let db = state.db.clone();
    let ident_clone = ident.clone();
    let aid = agent_id;
    let messages = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_agent(&conn, &ident_clone, &aid)?;
        db::get_unconfirmed_for_agent(&conn, &ident_clone, &aid)
    })
    .await??;

    let status = if messages.is_empty() {
        "no messages".to_string()
    } else {
        format!("{} unread user message(s)", messages.len())
    };

    Ok(Json(GetUnreadResponse { messages, status }))
}

// ── POST /v1/projects/:ident/messages/:id/confirm ────────────────────────────

#[derive(Serialize)]
pub struct ConfirmResponse {
    pub confirmed: bool,
}

pub async fn confirm_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((ident, msg_id)): Path<(String, i64)>,
) -> Result<Json<ConfirmResponse>> {
    {
        let conn = state.db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            ));
        }
    }

    let agent_id = extract_agent_id(&headers);
    let db = state.db.clone();
    let ident_clone = ident.clone();
    let aid = agent_id;
    let confirmed = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::confirm_message_for_agent(&conn, &ident_clone, &aid, msg_id)
    })
    .await??;

    Ok(Json(ConfirmResponse { confirmed }))
}

// ── POST /v1/projects/:ident/messages/:id/reply ─────────────────────────────

#[derive(Deserialize)]
pub struct ReplyRequest {
    /// Back-compat alias for `body`. If both are set, `body` wins.
    pub content: Option<String>,
    pub body: Option<String>,
    pub subject: Option<String>,
    pub hostname: Option<String>,
    pub event_at: Option<i64>,
}

#[derive(Serialize)]
pub struct ReplyResponse {
    pub message_id: i64,
    pub external_message_id: String,
    pub parent_message_id: i64,
}

pub async fn reply_to_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((ident, parent_id)): Path<(String, i64)>,
    Json(req): Json<ReplyRequest>,
) -> Result<Json<ReplyResponse>> {
    let agent_id = extract_agent_id(&headers);

    let (channel_name, room_id, parent_external_id) = {
        let conn = state.db.lock().unwrap();
        let project = db::get_project(&conn, &ident)?.ok_or_else(|| {
            AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            )
        })?;
        let parent = db::get_message_by_id(&conn, &ident, parent_id)?.ok_or_else(|| {
            AppError(
                StatusCode::NOT_FOUND,
                format!("message {} not found", parent_id),
            )
        })?;
        (
            project.channel_name,
            project.room_id,
            parent.external_message_id,
        )
    };

    let plugin = state
        .plugins
        .get(&channel_name)
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("channel plugin '{channel_name}' is not configured"),
            )
        })?
        .clone();

    let body_text = req.body.or(req.content).unwrap_or_default();
    if body_text.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "request must include non-empty 'body' (or 'content')".into(),
        ));
    }
    let outbound = build_outbound(
        &agent_id,
        body_text,
        req.subject,
        req.hostname,
        req.event_at,
    );

    let external_id = match &parent_external_id {
        Some(ext_id) => plugin.reply_structured(&room_id, ext_id, &outbound).await?,
        None => plugin.send_structured(&room_id, &outbound).await?,
    };

    let msg = Message {
        id: 0,
        project_ident: ident.clone(),
        source: "agent".into(),
        external_message_id: Some(external_id.clone()),
        content: outbound.body.clone(),
        sent_at: now_ms(),
        confirmed_at: None,
        parent_message_id: Some(parent_id),
        agent_id: Some(agent_id.clone()),
        message_type: "reply".into(),
        subject: Some(outbound.subject.clone()),
        hostname: Some(outbound.hostname.clone()),
        event_at: Some(outbound.event_at),
        deliver_to_agents: false,
    };

    let db = state.db.clone();
    let ident_clone = ident.clone();
    let aid = agent_id;
    let row_id = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_agent(&conn, &ident_clone, &aid)?;
        db::insert_message(&conn, &msg)
    })
    .await??;

    Ok(Json(ReplyResponse {
        message_id: row_id,
        external_message_id: external_id,
        parent_message_id: parent_id,
    }))
}

// ── POST /v1/projects/:ident/messages/:id/action ────────────────────────────

#[derive(Deserialize)]
pub struct ActionRequest {
    /// Back-compat alias for `body`. If both are set, `body` wins.
    pub message: Option<String>,
    pub body: Option<String>,
    pub subject: Option<String>,
    pub hostname: Option<String>,
    pub event_at: Option<i64>,
}

#[derive(Serialize)]
pub struct ActionResponse {
    pub message_id: i64,
    pub external_message_id: String,
    pub parent_message_id: i64,
}

pub async fn taking_action_on(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((ident, parent_id)): Path<(String, i64)>,
    Json(req): Json<ActionRequest>,
) -> Result<Json<ActionResponse>> {
    let agent_id = extract_agent_id(&headers);

    let (channel_name, room_id, parent_external_id) = {
        let conn = state.db.lock().unwrap();
        let project = db::get_project(&conn, &ident)?.ok_or_else(|| {
            AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            )
        })?;
        let parent = db::get_message_by_id(&conn, &ident, parent_id)?.ok_or_else(|| {
            AppError(
                StatusCode::NOT_FOUND,
                format!("message {} not found", parent_id),
            )
        })?;
        (
            project.channel_name,
            project.room_id,
            parent.external_message_id,
        )
    };

    let plugin = state
        .plugins
        .get(&channel_name)
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("channel plugin '{channel_name}' is not configured"),
            )
        })?
        .clone();

    let body_text = req.body.or(req.message).unwrap_or_default();
    if body_text.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "request must include non-empty 'body' (or 'message')".into(),
        ));
    }
    // Action posts get an `[ACTION]` subject prefix when the agent doesn't
    // supply one, so they remain visually distinct from regular replies.
    let subject = req.subject.or_else(|| {
        let derived = derive_subject(&body_text);
        Some(format!("[ACTION] {}", derived))
    });
    let outbound = build_outbound(&agent_id, body_text, subject, req.hostname, req.event_at);

    let external_id = match &parent_external_id {
        Some(ext_id) => plugin.reply_structured(&room_id, ext_id, &outbound).await?,
        None => plugin.send_structured(&room_id, &outbound).await?,
    };

    let msg = Message {
        id: 0,
        project_ident: ident.clone(),
        source: "agent".into(),
        external_message_id: Some(external_id.clone()),
        content: outbound.body.clone(),
        sent_at: now_ms(),
        confirmed_at: None,
        parent_message_id: Some(parent_id),
        agent_id: Some(agent_id.clone()),
        message_type: "action".into(),
        subject: Some(outbound.subject.clone()),
        hostname: Some(outbound.hostname.clone()),
        event_at: Some(outbound.event_at),
        deliver_to_agents: false,
    };

    let db = state.db.clone();
    let ident_clone = ident.clone();
    let aid = agent_id;
    let row_id = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_agent(&conn, &ident_clone, &aid)?;
        db::insert_message(&conn, &msg)
    })
    .await??;

    Ok(Json(ActionResponse {
        message_id: row_id,
        external_message_id: external_id,
        parent_message_id: parent_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_for_status_covers_transitions() {
        let todo: Vec<_> = actions_for_status("todo")
            .into_iter()
            .map(|a| (a.verb, a.target_status))
            .collect();
        assert_eq!(
            todo,
            vec![
                ("Claim".into(), "in_progress".into()),
                ("Done".into(), "done".into())
            ],
        );

        let in_progress: Vec<_> = actions_for_status("in_progress")
            .into_iter()
            .map(|a| (a.verb, a.target_status))
            .collect();
        assert_eq!(
            in_progress,
            vec![
                ("Release".into(), "todo".into()),
                ("Done".into(), "done".into())
            ],
        );

        let done: Vec<_> = actions_for_status("done")
            .into_iter()
            .map(|a| (a.verb, a.target_status))
            .collect();
        assert_eq!(done, vec![("Reopen".into(), "todo".into())]);

        assert!(actions_for_status("nonsense").is_empty());
    }

    /// Render a literal that mirrors the shape of the string produced by
    /// `tasks_board` and assert the attributes the ndesign runtime needs
    /// survive `format!` escaping. Specifically:
    ///   * template-level `{{id}}` placeholders are emitted verbatim,
    ///   * store-var references render as `${selectedTaskId}`,
    ///   * the static PATCH action body is valid JSON,
    ///   * the card-click success refresh list includes `#task-modal-meta`
    ///     (deferred panel — without this refresh description + specification
    ///     stays blank on first open),
    ///   * the kanban row does NOT carry `nd-gap-md` (stacks on the Bootstrap-
    ///     style gutter and wraps the third column),
    ///   * each column is a `<div class="nd-col-4">` wrapper with the card
    ///     inside (so col padding creates gutter BETWEEN cards, not inside),
    ///   * each sortable list carries `data-nd-sortable-group="tasks"` so the
    ///     ndesign runtime allows drops between columns.
    #[test]
    fn tasks_board_html_shape() {
        let ident_attr = "demo-project";
        let content = format!(
            r##"<li data-id="{{{{id}}}}">
<button class="task-card-button"
        data-nd-set="selectedTaskId='{{{{id}}}}'"
        data-nd-modal="#task-modal"
        data-nd-success="refresh:#task-modal-header,refresh:#task-modal-meta,refresh:#task-modal-comments"
        data-nd-bind="/v1/projects/{ident}/tasks/${{selectedTaskId}}"
        data-nd-body='{{"status":"in_progress"}}'>
  <div class="task-card-title">{{{{title}}}}</div>
  <ul class="task-card-meta"><li>{{{{comment_count}}}} comments</li></ul>
</button>
<label for="new-specification">Specification</label>
<textarea id="new-specification" name="specification"></textarea>
<a href="/projects/{ident}/tasks/new">+ New task</a>
<pre>{{{{specification}}}}</pre>
<div class="nd-row">
  <div class="nd-col-4">
    <section class="nd-card">
      <ul id="col-todo"
          data-nd-sortable="POST /v1/projects/{ident}/tasks/reorder?status=todo"
          data-nd-sortable-group="tasks"></ul>
    </section>
  </div>
</div>"##,
            ident = ident_attr,
        );

        assert!(
            content.contains(r#"data-id="{{id}}""#),
            "card id placeholder must survive format! as `{{{{id}}}}`: {content}"
        );
        assert!(
            content.contains(r#"selectedTaskId='{{id}}'"#),
            "data-nd-set must embed the template id placeholder: {content}"
        );
        assert!(
            content.contains("/v1/projects/demo-project/tasks/${selectedTaskId}"),
            "bind URL must resolve ident and leave store-var reference intact: {content}"
        );
        assert!(
            content.contains(r#"data-nd-body='{"status":"in_progress"}'"#),
            "PATCH body must be literal JSON with a concrete status: {content}"
        );
        assert!(
            content.contains("refresh:#task-modal-meta"),
            "card click must refresh #task-modal-meta (deferred description+specification panel): {content}"
        );
        assert!(
            !content.contains("nd-gap-md"),
            "kanban row must not carry nd-gap-md (stacks on top of row gutter): {content}"
        );
        assert!(
            content.contains(r#"<div class="nd-col-4">"#),
            "column must be a separate nd-col-4 wrapper, not merged into nd-card: {content}"
        );
        assert!(
            content.contains(r#"data-nd-sortable-group="tasks""#),
            "sortable columns must declare group=\"tasks\" for cross-column drag: {content}"
        );
        assert!(
            content.contains(r#"href="/projects/demo-project/tasks/new""#)
                && !content.contains("new-task-modal"),
            "board must link to the dedicated new-task page instead of opening the old modal: {content}"
        );
        assert!(
            content.contains(r#"class="task-card-button""#)
                && content.contains(r#"class="task-card-title""#),
            "task cards must carry wrapping-specific classes: {content}"
        );
        assert!(
            content
                .contains(r#"<ul class="task-card-meta"><li>{{comment_count}} comments</li></ul>"#),
            "task comment count must render as a bullet below the title: {content}"
        );
        assert!(
            content.contains(r#"name="specification""#)
                && content.contains(r#"<pre>{{specification}}</pre>"#),
            "task UI must use Specification naming for the long-form handoff field: {content}"
        );
    }

    fn sample_task() -> db::Task {
        db::Task {
            id: "task-1".to_string(),
            project_ident: "demo-project".to_string(),
            title: "Demo task".to_string(),
            description: Some("Short context".to_string()),
            details: Some("Long handoff spec".to_string()),
            status: "todo".to_string(),
            rank: 1,
            labels: vec!["demo".to_string()],
            hostname: Some("host".to_string()),
            owner_agent_id: None,
            reporter: "agent".to_string(),
            created_at: 1,
            updated_at: 1,
            started_at: None,
            done_at: None,
            kind: "normal".to_string(),
            delegated_to_project_ident: None,
            delegated_to_task_id: None,
        }
    }

    #[test]
    fn task_create_response_includes_specification_alias_and_hint() {
        let value = serde_json::to_value(TaskCreateResponse::new(sample_task())).unwrap();

        assert_eq!(value["details"], "Long handoff spec");
        assert_eq!(value["specification"], "Long handoff spec");
        assert_eq!(value["hint"], TASK_SPECIFICATION_HINT);
    }

    #[test]
    fn task_detail_response_includes_specification_alias() {
        let value = serde_json::to_value(TaskWithComments::new(sample_task(), Vec::new())).unwrap();

        assert_eq!(value["details"], "Long handoff spec");
        assert_eq!(value["specification"], "Long handoff spec");
        assert!(value["actions"].is_array());
    }

    #[test]
    fn control_panel_nav_exposes_api_docs() {
        let html = control_panel_open("API Docs", "api-docs");

        assert!(html.contains(r#"<li><a href="/api-docs" class="nd-active">API Docs</a></li>"#));
    }

    #[test]
    fn control_panel_nav_exposes_artifacts() {
        let html = control_panel_open("Artifacts", "artifacts");

        assert!(html.contains(r#"<li><a href="/artifacts" class="nd-active">Artifacts</a></li>"#));
    }

    fn test_state_with_operations(artifact_operations: db::ArtifactOperationsEnvelope) -> AppState {
        let db = db::open(":memory:").unwrap();
        {
            let conn = db.lock().unwrap();
            db::insert_project(
                &conn,
                &db::Project {
                    ident: "demo".to_string(),
                    channel_name: "discord".to_string(),
                    room_id: "room-demo".to_string(),
                    last_msg_id: None,
                    created_at: db::now_ms(),
                    repo_provider: None,
                    repo_namespace: None,
                    repo_name: None,
                    repo_full_name: None,
                },
            )
            .unwrap();
        }
        AppState {
            db,
            plugins: std::sync::Arc::new(std::collections::HashMap::new()),
            default_channel: "discord".to_string(),
            api_key: "test-key".to_string(),
            artifact_operations,
            artifact_body_schema_enabled: true,
            artifact_auth_enforced: false,
            update_available: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn test_state() -> AppState {
        test_state_with_operations(db::ArtifactOperationsEnvelope::production_defaults())
    }

    fn hardened_test_state() -> AppState {
        let mut state = test_state();
        state.artifact_auth_enforced = true;
        state
    }

    fn t008_operations_fixture() -> db::ArtifactOperationsEnvelope {
        db::t008_shrunken_artifact_operations_fixture()
    }

    fn mutation_headers(key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-agent-id", HeaderValue::from_static("codex-test"));
        headers.insert("x-agent-system", HeaderValue::from_static("codex"));
        headers.insert("idempotency-key", HeaderValue::from_str(key).unwrap());
        headers
    }

    fn authorized_headers(key: &str, scopes: &str) -> HeaderMap {
        let mut headers = mutation_headers(key);
        headers.insert("x-agent-project", HeaderValue::from_static("demo"));
        headers.insert("x-agent-scopes", HeaderValue::from_str(scopes).unwrap());
        headers
    }

    fn read_headers(scopes: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-agent-project", HeaderValue::from_static("demo"));
        headers.insert("x-agent-scopes", HeaderValue::from_str(scopes).unwrap());
        headers
    }

    async fn response_json(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap();
        (status, value)
    }

    async fn create_test_artifact(state: &AppState, key: &str) -> String {
        let response = create_artifact_handler(
            State(state.clone()),
            Path("demo".to_string()),
            mutation_headers(key),
            Json(CreateArtifactRequest {
                kind: "spec".to_string(),
                subkind: Some("implementation".to_string()),
                title: "Test spec".to_string(),
                labels: Some(vec!["test".to_string()]),
                actor_display_name: Some("Codex Test".to_string()),
            }),
        )
        .await
        .unwrap();
        let (status, value) = response_json(response).await;
        assert_eq!(status, StatusCode::CREATED, "{value}");
        assert_eq!(
            value["provenance"]["authorization"]["boundary"],
            "trusted-single-tenant"
        );
        value["data"]["artifact_id"].as_str().unwrap().to_string()
    }

    async fn create_test_version(
        state: &AppState,
        artifact_id: &str,
        key: &str,
        workflow_run_id: Option<&str>,
        parent_version_id: Option<&str>,
        body: &str,
    ) -> (StatusCode, Value) {
        let mut headers = mutation_headers(key);
        if let Some(run_id) = workflow_run_id {
            headers.insert("x-workflow-run-id", HeaderValue::from_str(run_id).unwrap());
        }
        let response = create_artifact_version_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.to_string())),
            headers,
            Json(CreateArtifactVersionRequest {
                version_label: Some(key.to_string()),
                parent_version_id: parent_version_id.map(str::to_string),
                body_format: Some("markdown".to_string()),
                body: Some(body.to_string()),
                structured_payload: None,
                source_format: None,
                version_state: Some("draft".to_string()),
                actor_display_name: Some("Codex Test".to_string()),
            }),
        )
        .await
        .unwrap();
        response_json(response).await
    }

    async fn start_test_run(state: &AppState, artifact_id: &str, key: &str) -> String {
        let response = start_artifact_workflow_run_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.to_string())),
            mutation_headers(key),
            Json(StartWorkflowRunRequest {
                workflow_kind: "spec_iteration".to_string(),
                phase: Some("test".to_string()),
                round_id: None,
                participant_actor_ids: None,
                source_artifact_version_id: None,
                read_set: None,
                is_resumable: Some(false),
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        let (status, value) = response_json(response).await;
        assert_eq!(status, StatusCode::CREATED, "{value}");
        value["data"]["workflow_run_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn artifact_workspace_pages_show_operational_sections() {
        let state = test_state();
        let artifact_id = create_test_artifact(&state, "workspace-artifact").await;
        let (status, _version) = create_test_version(
            &state,
            &artifact_id,
            "workspace-version",
            None,
            None,
            "body",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let list = artifact_workspace_page(
            State(state.clone()),
            Path("demo".to_string()),
            Query(ArtifactWorkspaceQuery {
                kind: Some("spec".to_string()),
                status: None,
                label: Some("test".to_string()),
                actor: None,
                q: Some("Test spec".to_string()),
                chunk_q: None,
                include_history: None,
            }),
        )
        .await
        .unwrap();
        assert!(list.0.contains("Artifact filters"));
        assert!(list.0.contains("Documentation browser"));
        assert!(list.0.contains(&artifact_id));

        let detail = artifact_detail_page(
            State(state),
            Path(("demo".to_string(), artifact_id)),
            Query(ArtifactWorkspaceQuery::default()),
        )
        .await
        .unwrap();
        assert!(detail.0.contains("Version history"));
        assert!(detail.0.contains("Contributions"));
        assert!(detail.0.contains("Comments"));
        assert!(detail.0.contains("Links"));
        assert!(detail.0.contains("Test spec"));
    }

    #[tokio::test]
    async fn artifact_workspace_page_survives_lazy_oversized_api_doc_chunking() {
        let state = test_state();
        {
            let conn = state.db.lock().unwrap();
            let content = serde_json::json!({
                "workflows": format!("{}needle-tail", "x".repeat(9 * 1024)),
            });
            conn.execute(
                "INSERT INTO api_docs (
                     id, project_ident, app, title, summary, kind, source_format,
                     source_ref, version, labels, content_json, author, created_at,
                     updated_at
                 )
                 VALUES (?1, 'demo', 'gateway-smoke', 'Gateway smoke context',
                         NULL, 'agent_context', 'agent_context', '.agent/api/smoke.yaml',
                         '2026-05-14', ?2, ?3, 'tester', ?4, ?4)",
                rusqlite::params![
                    "legacy-smoke-doc",
                    serde_json::to_string(&vec!["smoke"]).unwrap(),
                    serde_json::to_string(&content).unwrap(),
                    db::now_ms(),
                ],
            )
            .unwrap();
        }

        let page = artifact_workspace_page(
            State(state),
            Path("demo".to_string()),
            Query(ArtifactWorkspaceQuery::default()),
        )
        .await
        .unwrap();
        assert!(page.0.contains("Artifact filters"));
        assert!(page.0.contains("gateway-smoke"));
    }

    #[tokio::test]
    async fn design_review_routes_cover_two_pass_query_and_synthesis_version() {
        let state = test_state();
        let create_response = create_design_review_handler(
            State(state.clone()),
            Path("demo".to_string()),
            mutation_headers("review-create"),
            Json(DesignReviewCreateRequest {
                title: "Gateway review".to_string(),
                labels: Some(vec!["review".to_string()]),
                body: Some("# Design".to_string()),
                body_format: Some("markdown".to_string()),
                source_artifact_id: None,
                source_artifact_version_id: None,
                actor_display_name: Some("Codex Test".to_string()),
            }),
        )
        .await
        .unwrap();
        let (status, value) = response_json(create_response).await;
        assert_eq!(status, StatusCode::CREATED, "{value}");
        let artifact_id = value["data"]["artifact"]["artifact_id"]
            .as_str()
            .unwrap()
            .to_string();
        let source_version_id = value["data"]["version"]["artifact_version_id"]
            .as_str()
            .unwrap()
            .to_string();

        let round_response = create_design_review_round_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            mutation_headers("review-round"),
            Json(DesignReviewRoundRequest {
                round_id: Some("round-a".to_string()),
                participant_actor_ids: None,
                source_artifact_version_id: source_version_id.clone(),
                read_set: None,
                actor_display_name: Some("Codex Test".to_string()),
            }),
        )
        .await
        .unwrap();
        let (status, value) = response_json(round_response).await;
        assert_eq!(status, StatusCode::CREATED, "{value}");
        let workflow_run_id = value["data"]["workflow_run"]["workflow_run_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            value["data"]["artifact"]["review_state"],
            "collecting_reviews"
        );

        let pass1_response = create_design_review_contribution_handler(
            State(state.clone()),
            Path((
                "demo".to_string(),
                artifact_id.clone(),
                workflow_run_id.clone(),
            )),
            mutation_headers("review-pass1"),
            Json(DesignReviewContributionRequest {
                phase: "pass_1".to_string(),
                role: None,
                reviewed_version_id: None,
                read_set: None,
                body_format: None,
                body: "pass one".to_string(),
                actor_display_name: Some("Codex Test".to_string()),
            }),
        )
        .await
        .unwrap();
        let (status, value) = response_json(pass1_response).await;
        assert_eq!(status, StatusCode::CREATED, "{value}");
        let pass1_id = value["data"]["contribution_id"]
            .as_str()
            .unwrap()
            .to_string();

        let pass2_read_set = serde_json::json!({
            "versions": [source_version_id],
            "contributions": [pass1_id.clone()],
        });
        let pass2_response = create_design_review_contribution_handler(
            State(state.clone()),
            Path((
                "demo".to_string(),
                artifact_id.clone(),
                workflow_run_id.clone(),
            )),
            mutation_headers("review-pass2"),
            Json(DesignReviewContributionRequest {
                phase: "pass_2".to_string(),
                role: None,
                reviewed_version_id: None,
                read_set: Some(pass2_read_set.clone()),
                body_format: None,
                body: "pass two".to_string(),
                actor_display_name: Some("Codex Test".to_string()),
            }),
        )
        .await
        .unwrap();
        let (status, value) = response_json(pass2_response).await;
        assert_eq!(status, StatusCode::CREATED, "{value}");
        let pass2_id = value["data"]["contribution_id"]
            .as_str()
            .unwrap()
            .to_string();

        let list = list_design_review_contributions_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            Query(ListDesignReviewContributionsQuery {
                round_id: Some("round-a".to_string()),
                phase: Some("pass_2".to_string()),
                role: Some("reviewer".to_string()),
                reviewed_version_id: None,
                read_set_contains: Some(pass1_id.clone()),
            }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(list.data.len(), 1);
        assert_eq!(list.data[0].contribution_id, pass2_id);

        let synthesis_response = create_design_review_synthesis_handler(
            State(state.clone()),
            Path((
                "demo".to_string(),
                artifact_id.clone(),
                workflow_run_id.clone(),
            )),
            mutation_headers("review-synthesis"),
            Json(DesignReviewSynthesisRequest {
                reviewed_version_id: None,
                read_set: serde_json::json!({ "contributions": [pass2_id] }),
                body_format: None,
                body: "# Synthesized design".to_string(),
                create_version: Some(true),
                version_label: Some("synthesis".to_string()),
                actor_display_name: Some("Codex Test".to_string()),
            }),
        )
        .await
        .unwrap();
        let (status, value) = response_json(synthesis_response).await;
        assert_eq!(status, StatusCode::CREATED, "{value}");
        assert!(value["data"]["version"]["artifact_version_id"]
            .as_str()
            .is_some());
        assert_eq!(
            value["data"]["artifact"]["review_state"],
            "needs_user_decision"
        );
    }

    fn spec_fixture_manifest() -> Value {
        serde_json::json!({
            "spec_version": 1,
            "source_doc": "../gateway-features.md",
            "phases": [
                {
                    "id": "phase-1",
                    "name": "Phase 1",
                    "tasks": [
                        {
                            "id": "T001",
                            "team": "backend",
                            "title": "Create substrate contract",
                            "depends_on": [],
                            "labels": ["gateway-features", "phase-1"],
                            "touch_surface": ["docs/artifact-substrate-v1.md"],
                            "acceptance_criteria": ["contract is durable"],
                            "validation_plan": ["cargo test -p gateway"],
                            "spec_file": "backend/T001.md",
                            "status": "todo"
                        },
                        {
                            "id": "T002",
                            "team": "backend",
                            "title": "Implement task generation",
                            "depends_on": ["T001"],
                            "labels": ["gateway-features", "phase-1"],
                            "touch_surface": ["crates/gateway/src/routes.rs"],
                            "acceptance_criteria": ["tasks are generated idempotently"],
                            "validation_plan": ["cargo test -p gateway"],
                            "spec_file": "backend/T002.md",
                            "status": "todo"
                        }
                    ]
                }
            ]
        })
    }

    fn spec_fixture_files() -> std::collections::HashMap<String, String> {
        std::collections::HashMap::from([
            (
                "backend/T001.md".to_string(),
                "# T001\n\nFocused handoff for substrate.".to_string(),
            ),
            (
                "backend/T002.md".to_string(),
                "# T002\n\nFocused handoff for task generation.".to_string(),
            ),
        ])
    }

    async fn import_spec_fixture(state: &AppState, key: &str) -> (String, String) {
        let response = create_spec_handler(
            State(state.clone()),
            Path("demo".to_string()),
            mutation_headers(key),
            Json(SpecImportRequest {
                title: "Gateway Features".to_string(),
                labels: Some(vec!["gateway-features".to_string()]),
                body: Some("# Gateway Features Spec".to_string()),
                manifest: spec_fixture_manifest(),
                file_bodies: Some(spec_fixture_files()),
                source_doc: Some("../gateway-features.md".to_string()),
                source_artifact_id: None,
                source_artifact_version_id: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        let (status, value) = response_json(response).await;
        assert_eq!(status, StatusCode::CREATED, "{value}");
        (
            value["data"]["artifact"]["artifact_id"]
                .as_str()
                .unwrap()
                .to_string(),
            value["data"]["version"]["artifact_version_id"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    }

    async fn accept_spec_fixture(state: &AppState, artifact_id: &str, version_id: &str) {
        let response = accept_spec_version_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.to_string())),
            mutation_headers("accept-spec-fixture"),
            Json(SpecAcceptRequest {
                version_id: version_id.to_string(),
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        let (status, value) = response_json(response).await;
        assert_eq!(status, StatusCode::OK, "{value}");
    }

    async fn generate_spec_tasks(
        state: &AppState,
        artifact_id: &str,
        key: &str,
        manifest_item_ids: Option<Vec<String>>,
    ) -> (StatusCode, Value) {
        let response = generate_spec_tasks_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.to_string())),
            mutation_headers(key),
            Json(GenerateSpecTasksRequest {
                confirmed: true,
                manifest_item_ids,
                reporter: Some("codex-test".to_string()),
                hostname: Some("test-host".to_string()),
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        response_json(response).await
    }

    #[tokio::test]
    async fn spec_routes_import_manifest_round_trip_and_fetch_stable_item() {
        let state = test_state();
        let (artifact_id, version_id) = import_spec_fixture(&state, "spec-import-roundtrip").await;

        let manifest = get_spec_manifest_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            Query(SpecManifestQuery {
                version_id: Some(version_id.clone()),
            }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(manifest.0.data.artifact_version_id, version_id);
        assert_eq!(manifest.0.data.items.len(), 2);
        assert_eq!(manifest.0.data.items[0].manifest_item_id, "phase-1:T001");
        assert_eq!(
            manifest.0.data.items[0].spec_body.as_deref(),
            Some("# T001\n\nFocused handoff for substrate.")
        );
        assert_eq!(
            manifest.0.data.stability_policy["renamed"],
            "preserve manifest_item_id when the task id/code is unchanged"
        );

        let item = get_spec_manifest_item_handler(
            State(state),
            Path(("demo".to_string(), artifact_id, "phase-1:T002".to_string())),
            Query(SpecManifestQuery { version_id: None }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(item.0.data.task_code.as_deref(), Some("T002"));
        assert_eq!(
            item.0.data.acceptance_criteria,
            vec!["tasks are generated idempotently".to_string()]
        );
    }

    #[tokio::test]
    async fn spec_generate_tasks_uses_accepted_version_and_idempotent_rerun() {
        let state = test_state();
        let (artifact_id, version_id) = import_spec_fixture(&state, "spec-generate").await;
        accept_spec_fixture(&state, &artifact_id, &version_id).await;

        let (status, first) = generate_spec_tasks(&state, &artifact_id, "generate-run", None).await;
        assert_eq!(status, StatusCode::CREATED, "{first}");
        let first_tasks = first["data"]["generated_task_ids"].as_array().unwrap();
        let first_links = first["data"]["generated_link_ids"].as_array().unwrap();
        assert_eq!(first_tasks.len(), 2, "{first}");
        assert_eq!(first_links.len(), 2, "{first}");

        let (status, replay) =
            generate_spec_tasks(&state, &artifact_id, "generate-run", None).await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(replay["provenance"]["replay"], true);
        assert_eq!(
            replay["data"]["generated_task_ids"],
            first["data"]["generated_task_ids"]
        );
        assert_eq!(
            replay["data"]["generated_link_ids"],
            first["data"]["generated_link_ids"]
        );

        let task_id = first_tasks[0].as_str().unwrap();
        let detail = {
            let conn = state.db.lock().unwrap();
            db::get_task_detail(&conn, "demo", task_id)
                .unwrap()
                .unwrap()
        };
        let details = detail.task.details.unwrap();
        assert!(details.contains(&format!("source_spec_artifact_id: {artifact_id}")));
        assert!(details.contains(&format!("source_spec_version_id: {version_id}")));
        assert!(details.contains("manifest_item_id: phase-1:T001"));
        assert!(detail.comments.iter().any(|comment| {
            comment
                .content
                .contains("current task schema has no dedicated source fields")
        }));

        let links = {
            let conn = state.db.lock().unwrap();
            db::list_artifact_links(
                &conn,
                "demo",
                &db::ArtifactLinkFilters {
                    link_type: Some("task_generated_from_spec"),
                    source_kind: Some("artifact_version"),
                    source_id: Some(&version_id),
                    target_kind: Some("task"),
                    target_id: Some(task_id),
                },
            )
            .unwrap()
        };
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].source_version_id.as_deref(),
            Some(version_id.as_str())
        );
        assert_eq!(
            links[0].source_child_address.as_deref(),
            Some("manifest.items[phase-1:T001]")
        );
        assert_eq!(links[0].target_kind, "task");
    }

    #[tokio::test]
    async fn spec_generate_tasks_recovers_existing_unlinked_task_after_partial_failure() {
        let state = test_state();
        let (artifact_id, version_id) = import_spec_fixture(&state, "spec-partial").await;
        accept_spec_fixture(&state, &artifact_id, &version_id).await;
        let orphan_task = {
            let conn = state.db.lock().unwrap();
            db::insert_task(
                &conn,
                "demo",
                "T001 Create substrate contract",
                Some("orphan from interrupted generation"),
                Some(&format!(
                    "Source:\nsource_spec_artifact_id: {artifact_id}\nsource_spec_version_id: {version_id}\nmanifest_item_id: phase-1:T001\n"
                )),
                &["generated-from-spec".to_string()],
                Some("test-host"),
                "codex-test",
            )
            .unwrap()
        };

        let (status, value) = generate_spec_tasks(
            &state,
            &artifact_id,
            "recover-partial",
            Some(vec!["phase-1:T001".to_string()]),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{value}");
        assert_eq!(
            value["data"]["generated_task_ids"][0].as_str().unwrap(),
            orphan_task.id
        );
        let task_count: i64 = {
            let conn = state.db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM tasks WHERE project_ident = 'demo'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            task_count, 1,
            "rerun should link existing task, not duplicate it"
        );
    }

    #[tokio::test]
    async fn spec_link_existing_task_creates_back_link() {
        let state = test_state();
        let (artifact_id, version_id) = import_spec_fixture(&state, "spec-link-existing").await;
        accept_spec_fixture(&state, &artifact_id, &version_id).await;
        let task = {
            let conn = state.db.lock().unwrap();
            db::insert_task(
                &conn,
                "demo",
                "Existing implementation task",
                None,
                Some("pre-existing handoff"),
                &[],
                None,
                "codex-test",
            )
            .unwrap()
        };
        let response = link_existing_spec_task_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            mutation_headers("link-existing-task"),
            Json(LinkSpecTaskRequest {
                version_id: None,
                manifest_item_id: "phase-1:T002".to_string(),
                task_id: task.id.clone(),
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        let (status, value) = response_json(response).await;
        assert_eq!(status, StatusCode::CREATED, "{value}");
        assert_eq!(value["data"]["target_id"].as_str().unwrap(), task.id);
        assert_eq!(
            value["data"]["source_child_address"].as_str().unwrap(),
            "manifest.items[phase-1:T002]"
        );
    }

    #[tokio::test]
    async fn artifact_routes_cover_happy_path_and_idempotent_retry() {
        let state = test_state();
        let artifact_id = create_test_artifact(&state, "artifact-create-1").await;
        let run_id = start_test_run(&state, &artifact_id, "version-run-1").await;

        let (status, first_version) = create_test_version(
            &state,
            &artifact_id,
            "version-1",
            Some(&run_id),
            None,
            "hello",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{first_version}");
        let version_id = first_version["data"]["artifact_version_id"]
            .as_str()
            .unwrap()
            .to_string();

        let (status, replayed_version) = create_test_version(
            &state,
            &artifact_id,
            "version-1",
            Some(&run_id),
            None,
            "hello",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{replayed_version}");
        assert_eq!(replayed_version["provenance"]["replay"], true);
        assert_eq!(
            replayed_version["data"]["artifact_version_id"],
            first_version["data"]["artifact_version_id"]
        );

        let (status, second_version) = create_test_version(
            &state,
            &artifact_id,
            "version-2",
            Some(&run_id),
            Some(&version_id),
            "hello\nnew",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{second_version}");
        let second_version_id = second_version["data"]["artifact_version_id"]
            .as_str()
            .unwrap()
            .to_string();

        let diff = diff_artifact_version_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone(), second_version_id)),
            Query(DiffQuery {
                base_version_id: Some(version_id.clone()),
            }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        assert!(diff.0.data.diff.contains("+new"));
        assert_eq!(diff.0.chunking_status.status, "none");

        let comment_response = create_artifact_comment_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            mutation_headers("comment-1"),
            Json(CreateCommentRequest {
                target_kind: "artifact".to_string(),
                target_id: artifact_id.clone(),
                child_address: None,
                parent_comment_id: None,
                body: "Looks good".to_string(),
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(response_json(comment_response).await.0, StatusCode::CREATED);

        let contribution_response = create_artifact_contribution_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            mutation_headers("contribution-1"),
            Json(CreateContributionRequest {
                target_kind: "artifact".to_string(),
                target_id: artifact_id.clone(),
                contribution_kind: "note".to_string(),
                phase: None,
                role: "author".to_string(),
                read_set: None,
                body_format: Some("markdown".to_string()),
                body: "Implementation note".to_string(),
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            response_json(contribution_response).await.0,
            StatusCode::CREATED
        );

        let link_response = create_artifact_link_handler(
            State(state.clone()),
            Path("demo".to_string()),
            mutation_headers("link-1"),
            Json(CreateLinkRequest {
                link_type: "doc_referenced_by_spec".to_string(),
                source_kind: "artifact".to_string(),
                source_id: artifact_id.clone(),
                source_version_id: None,
                source_child_address: None,
                target_kind: "artifact_version".to_string(),
                target_id: version_id.clone(),
                target_version_id: Some(version_id),
                target_child_address: None,
                supersedes_link_id: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(response_json(link_response).await.0, StatusCode::CREATED);

        let fetched = get_artifact_handler(
            State(state),
            Path(("demo".to_string(), artifact_id)),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        assert!(fetched.0.data.current_version.is_some());
        assert_eq!(fetched.0.chunking_status.status, "none");
    }

    #[tokio::test]
    async fn artifact_authorization_enforces_project_and_scopes_when_hardened() {
        let state = hardened_test_state();

        let missing_project = create_artifact_handler(
            State(state.clone()),
            Path("demo".to_string()),
            mutation_headers("auth-missing-project"),
            Json(CreateArtifactRequest {
                kind: "spec".to_string(),
                subkind: None,
                title: "Unauthorized".to_string(),
                labels: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(missing_project.0, StatusCode::FORBIDDEN);
        assert!(missing_project.1.contains("x-agent-project"));

        let missing_scope = create_artifact_handler(
            State(state.clone()),
            Path("demo".to_string()),
            authorized_headers("auth-missing-scope", "artifact.read"),
            Json(CreateArtifactRequest {
                kind: "spec".to_string(),
                subkind: None,
                title: "Unauthorized".to_string(),
                labels: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(missing_scope.0, StatusCode::FORBIDDEN);
        assert!(missing_scope.1.contains("artifact.write"));

        let response = create_artifact_handler(
            State(state.clone()),
            Path("demo".to_string()),
            authorized_headers("auth-create", "artifact.write"),
            Json(CreateArtifactRequest {
                kind: "spec".to_string(),
                subkind: None,
                title: "Authorized".to_string(),
                labels: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        let (status, value) = response_json(response).await;
        assert_eq!(status, StatusCode::CREATED, "{value}");
        assert_eq!(
            value["provenance"]["authorization"]["boundary"],
            "project-scoped"
        );
        assert_eq!(
            value["provenance"]["authorization"]["required_scopes"][0],
            "artifact.write"
        );
        let artifact_id = value["data"]["artifact_id"].as_str().unwrap().to_string();

        let read_reject = get_artifact_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            HeaderMap::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(read_reject.0, StatusCode::FORBIDDEN);

        let fetched = get_artifact_handler(
            State(state),
            Path(("demo".to_string(), artifact_id)),
            read_headers("artifact.read"),
        )
        .await
        .unwrap();
        assert_eq!(fetched.0.data.artifact.title, "Authorized");
    }

    #[tokio::test]
    async fn artifact_quota_override_requires_project_administer_when_hardened() {
        let state = hardened_test_state();

        let mut non_admin = authorized_headers("quota-override-forbidden", "artifact.write");
        non_admin.insert(
            "x-artifact-quota-override",
            HeaderValue::from_static("true"),
        );
        let err = create_artifact_handler(
            State(state.clone()),
            Path("demo".to_string()),
            non_admin,
            Json(CreateArtifactRequest {
                kind: "spec".to_string(),
                subkind: None,
                title: "Override denied".to_string(),
                labels: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert!(err.1.contains("project.administer"));

        let mut admin =
            authorized_headers("quota-override-admin", "artifact.write project.administer");
        admin.insert(
            "x-artifact-quota-override",
            HeaderValue::from_static("true"),
        );
        let response = create_artifact_handler(
            State(state),
            Path("demo".to_string()),
            admin,
            Json(CreateArtifactRequest {
                kind: "spec".to_string(),
                subkind: None,
                title: "Override allowed".to_string(),
                labels: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(response_json(response).await.0, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn artifact_routes_validate_actor_idempotency_and_immutable_refs() {
        let state = test_state();
        let artifact_id = create_test_artifact(&state, "artifact-create-validation").await;

        let mut missing_agent = mutation_headers("missing-agent");
        missing_agent.remove("x-agent-id");
        let missing_agent_error = create_artifact_version_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            missing_agent,
            Json(CreateArtifactVersionRequest {
                version_label: None,
                parent_version_id: None,
                body_format: Some("markdown".to_string()),
                body: Some("body".to_string()),
                structured_payload: None,
                source_format: None,
                version_state: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(missing_agent_error.0, StatusCode::BAD_REQUEST);
        assert_eq!(missing_agent_error.1, "x_agent_id_required");

        let missing_parent = create_artifact_version_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            mutation_headers("missing-parent"),
            Json(CreateArtifactVersionRequest {
                version_label: None,
                parent_version_id: Some("missing-version".to_string()),
                body_format: Some("markdown".to_string()),
                body: Some("body".to_string()),
                structured_payload: None,
                source_format: None,
                version_state: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(missing_parent.0, StatusCode::BAD_REQUEST);

        let read_set_error = create_artifact_contribution_handler(
            State(state),
            Path(("demo".to_string(), artifact_id.clone())),
            mutation_headers("read-set-required"),
            Json(CreateContributionRequest {
                target_kind: "artifact".to_string(),
                target_id: artifact_id,
                contribution_kind: "synthesis".to_string(),
                phase: Some("synthesis".to_string()),
                role: "analyst".to_string(),
                read_set: None,
                body_format: Some("markdown".to_string()),
                body: "synthesis".to_string(),
                actor_display_name: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(read_set_error.0, StatusCode::BAD_REQUEST);
        assert_eq!(read_set_error.1, "read_set_required");
    }

    #[tokio::test]
    async fn artifact_routes_return_stable_t004_size_limit_errors() {
        let state = test_state_with_operations(t008_operations_fixture());
        let artifact_id = create_test_artifact(&state, "t008-size-artifact").await;

        let oversized_version = create_artifact_version_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            mutation_headers("t008-oversized-version"),
            Json(CreateArtifactVersionRequest {
                version_label: None,
                parent_version_id: None,
                body_format: Some("markdown".to_string()),
                body: Some("x".repeat(4097)),
                structured_payload: None,
                source_format: None,
                version_state: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(oversized_version.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(oversized_version.1, "artifact_version_body_too_large");

        let oversized_contribution = create_artifact_contribution_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            mutation_headers("t008-oversized-contribution"),
            Json(CreateContributionRequest {
                target_kind: "artifact".to_string(),
                target_id: artifact_id.clone(),
                contribution_kind: "note".to_string(),
                phase: None,
                role: "author".to_string(),
                read_set: None,
                body_format: Some("markdown".to_string()),
                body: "x".repeat(2049),
                actor_display_name: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(oversized_contribution.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(oversized_contribution.1, "contribution_body_too_large");

        let oversized_comment = create_artifact_comment_handler(
            State(state),
            Path(("demo".to_string(), artifact_id.clone())),
            mutation_headers("t008-oversized-comment"),
            Json(CreateCommentRequest {
                target_kind: "artifact".to_string(),
                target_id: artifact_id,
                child_address: None,
                parent_comment_id: None,
                body: "x".repeat(513),
                actor_display_name: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(oversized_comment.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(oversized_comment.1, "comment_body_too_large");
    }

    #[tokio::test]
    async fn artifact_routes_surface_soft_quota_warning_and_hard_reject() {
        let state = test_state_with_operations(t008_operations_fixture());

        async fn post_artifact(
            state: &AppState,
            key: &str,
            title: &str,
        ) -> std::result::Result<Response, AppError> {
            create_artifact_handler(
                State(state.clone()),
                Path("demo".to_string()),
                mutation_headers(key),
                Json(CreateArtifactRequest {
                    kind: "spec".to_string(),
                    subkind: None,
                    title: title.to_string(),
                    labels: Some(vec!["qa".to_string()]),
                    actor_display_name: None,
                }),
            )
            .await
        }

        for idx in 1..=2 {
            let response = post_artifact(
                &state,
                &format!("t008-quota-{idx}"),
                &format!("quota fixture {idx}"),
            )
            .await
            .unwrap();
            let (status, value) = response_json(response).await;
            assert_eq!(status, StatusCode::CREATED, "{value}");
            assert_eq!(value["provenance"]["warnings"].as_array().unwrap().len(), 0);
        }

        let soft_response = post_artifact(&state, "t008-quota-3", "quota fixture 3")
            .await
            .unwrap();
        let (status, soft_value) = response_json(soft_response).await;
        assert_eq!(status, StatusCode::CREATED, "{soft_value}");
        assert_eq!(
            soft_value["provenance"]["warnings"][0],
            "quota_artifact_soft"
        );

        let hard_reject = post_artifact(&state, "t008-quota-4", "quota fixture 4")
            .await
            .unwrap_err();
        assert_eq!(hard_reject.0, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(hard_reject.1, "quota_artifact_exceeded");

        let count: i64 = {
            let conn = state.db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM artifacts WHERE project_ident = 'demo'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(count, 3, "hard quota reject must not insert a row");
    }

    #[tokio::test]
    async fn artifact_routes_expose_stale_and_partial_chunking_status() {
        let state = test_state();
        let artifact_id = create_test_artifact(&state, "t008-chunks-artifact").await;
        let (status, v1) =
            create_test_version(&state, &artifact_id, "t008-chunks-v1", None, None, "v1").await;
        assert_eq!(status, StatusCode::CREATED, "{v1}");
        let v1_id = v1["data"]["artifact_version_id"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, v2) = create_test_version(
            &state,
            &artifact_id,
            "t008-chunks-v2",
            None,
            Some(&v1_id),
            "v2",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{v2}");
        let v2_id = v2["data"]["artifact_version_id"]
            .as_str()
            .unwrap()
            .to_string();

        {
            let conn = state.db.lock().unwrap();
            db::create_artifact_chunk(
                &conn,
                &state.artifact_operations,
                &db::ArtifactChunkInsert {
                    artifact_id: &artifact_id,
                    artifact_version_id: &v1_id,
                    child_address: "manifest.items[T008-stale]",
                    text: "stale chunk",
                    embedding_model: None,
                    embedding_vector: None,
                    app: Some("spec"),
                    label: Some("qa"),
                    kind: Some("manifest_item"),
                    metadata: Some(&serde_json::json!({"status": "ok"})),
                },
            )
            .unwrap();
        }

        let stale = get_artifact_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(stale.0.chunking_status.status, "stale");
        assert_eq!(stale.0.chunking_status.current_chunk_count, 0);
        assert_eq!(stale.0.chunking_status.stale_chunk_count, 1);
        assert_eq!(stale.0.chunking_status.superseded_chunk_count, 0);
        assert!(stale.0.chunking_status.failed_addresses.is_empty());

        {
            let conn = state.db.lock().unwrap();
            db::create_artifact_chunk(
                &conn,
                &state.artifact_operations,
                &db::ArtifactChunkInsert {
                    artifact_id: &artifact_id,
                    artifact_version_id: &v2_id,
                    child_address: "manifest.items[T008-partial]",
                    text: "partial chunk",
                    embedding_model: None,
                    embedding_vector: None,
                    app: Some("spec"),
                    label: Some("qa"),
                    kind: Some("manifest_item"),
                    metadata: Some(&serde_json::json!({"status": "failed"})),
                },
            )
            .unwrap();
        }

        let partial_version = get_artifact_version_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone(), v2_id.clone())),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(partial_version.0.chunking_status.status, "partial");
        assert_eq!(partial_version.0.data.chunking_status.status, "partial");
        assert_eq!(
            partial_version.0.chunking_status.failed_addresses,
            vec!["manifest.items[T008-partial]".to_string()]
        );

        let partial_diff = diff_artifact_version_handler(
            State(state),
            Path(("demo".to_string(), artifact_id, v2_id)),
            Query(DiffQuery {
                base_version_id: Some(v1_id),
            }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(partial_diff.0.chunking_status.status, "partial");
        assert_eq!(partial_diff.0.data.chunking_status.status, "partial");
        assert_eq!(
            partial_diff.0.data.chunking_status.failed_addresses,
            vec!["manifest.items[T008-partial]".to_string()]
        );
    }

    #[tokio::test]
    async fn api_doc_chunks_route_keeps_legacy_array_and_opt_in_envelope() {
        let state = test_state();
        let created = create_api_doc_handler(
            State(state.clone()),
            HeaderMap::new(),
            Path("demo".to_string()),
            Json(CreateApiDocRequest {
                app: "billing-api".to_string(),
                title: "Billing API agent context".to_string(),
                summary: Some("System of record for invoices.".to_string()),
                kind: "agent_context".to_string(),
                source_format: "agent_context".to_string(),
                source_ref: Some(".agent/api/billing.yaml".to_string()),
                version: Some("2026-05-13".to_string()),
                labels: serde_json::json!(["billing"]),
                content: serde_json::json!({
                    "purpose": "Owns invoice state.",
                    "endpoints": [{
                        "method": "POST",
                        "path": "/v1/invoices",
                        "intent": "Create draft invoice"
                    }]
                }),
                author: Some("tester".to_string()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(created.0.kind, "agent_context");
        assert_eq!(created.0.subkind, "api_context");

        let legacy = list_api_doc_chunks_handler(
            State(state.clone()),
            Path("demo".to_string()),
            Query(ListApiDocsQuery {
                q: Some("draft invoice".to_string()),
                app: None,
                label: Some("billing".to_string()),
                kind: Some("agent_context".to_string()),
                include_history: false,
                envelope: false,
            }),
        )
        .await
        .unwrap();
        let (status, value) = response_json(legacy).await;
        assert_eq!(status, StatusCode::OK);
        let chunks = value.as_array().unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0]["doc_id"], created.0.id);
        assert_eq!(chunks[0]["chunk_type"], "endpoints");
        assert_eq!(chunks[0]["freshness"], "current");
        assert_eq!(chunks[0]["retrieval_scope"], "current");
        assert_eq!(chunks[0]["artifact_id"], created.0.artifact_id);
        assert_eq!(
            chunks[0]["artifact_version_id"],
            created.0.artifact_version_id.clone().unwrap()
        );

        let envelope = list_api_doc_chunks_handler(
            State(state),
            Path("demo".to_string()),
            Query(ListApiDocsQuery {
                q: Some("draft invoice".to_string()),
                app: None,
                label: Some("billing".to_string()),
                kind: Some("agent_context".to_string()),
                include_history: false,
                envelope: true,
            }),
        )
        .await
        .unwrap();
        let (status, value) = response_json(envelope).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["retrieval_scope"], "current");
        assert_eq!(value["include_history"], false);
        assert_eq!(value["chunking_status"]["status"], "current");
        assert_eq!(value["chunking_status"]["current_chunk_count"], 1);
        assert_eq!(value["chunks"].as_array().unwrap().len(), 1);
    }

    /// Regression for T030: after consolidating artifact mutation route
    /// scaffolding into `begin_artifact_mutation` / `finalize_mutation`,
    /// replayed mutations across the four create-style handlers must still
    /// return `200 OK`, surface `provenance.replay == true`, echo the
    /// originally-generated resource id, and not double-count metric writes.
    /// The original (first) responses must continue to return `201 CREATED`
    /// with `provenance.replay == false`.
    #[tokio::test]
    async fn artifact_mutation_helpers_preserve_replay_envelope_after_consolidation() {
        let state = test_state();
        let artifact_id = create_test_artifact(&state, "t030-artifact").await;
        let run_id = start_test_run(&state, &artifact_id, "t030-run").await;

        // 1. version: idempotent retry must return OK + replay=true + same id.
        //    Versions' idempotency index is keyed on
        //    (artifact_id, created_via_workflow_run_id, idempotency_key) so we
        //    must hold the workflow_run_id constant across both calls.
        let (first_status, first_version) = create_test_version(
            &state,
            &artifact_id,
            "t030-version",
            Some(&run_id),
            None,
            "body-a",
        )
        .await;
        assert_eq!(first_status, StatusCode::CREATED, "{first_version}");
        assert_eq!(first_version["provenance"]["replay"], false);
        let version_id = first_version["data"]["artifact_version_id"]
            .as_str()
            .unwrap()
            .to_string();

        let (replay_status, replay_version) = create_test_version(
            &state,
            &artifact_id,
            "t030-version",
            Some(&run_id),
            None,
            "body-a",
        )
        .await;
        assert_eq!(replay_status, StatusCode::OK, "{replay_version}");
        assert_eq!(replay_version["provenance"]["replay"], true);
        assert_eq!(
            replay_version["data"]["artifact_version_id"]
                .as_str()
                .unwrap(),
            version_id
        );
        assert_eq!(
            replay_version["provenance"]["generated_resources"]["artifact_version_id"]
                .as_str()
                .unwrap(),
            version_id
        );
        // Authorization scopes must be preserved by `finalize_mutation`.
        assert_eq!(
            replay_version["provenance"]["authorization"]["required_scopes"][0],
            "artifact_version.create"
        );

        // 2. contribution: same replay invariants through the consolidated helper.
        async fn post_contribution(
            state: &AppState,
            artifact_id: &str,
            key: &str,
        ) -> (StatusCode, Value) {
            let response = create_artifact_contribution_handler(
                State(state.clone()),
                Path(("demo".to_string(), artifact_id.to_string())),
                mutation_headers(key),
                Json(CreateContributionRequest {
                    target_kind: "artifact".to_string(),
                    target_id: artifact_id.to_string(),
                    contribution_kind: "note".to_string(),
                    phase: None,
                    role: "author".to_string(),
                    read_set: None,
                    body_format: Some("markdown".to_string()),
                    body: "consolidated".to_string(),
                    actor_display_name: None,
                }),
            )
            .await
            .unwrap();
            response_json(response).await
        }
        let (status, first_contrib) = post_contribution(&state, &artifact_id, "t030-contrib").await;
        assert_eq!(status, StatusCode::CREATED);
        let contrib_id = first_contrib["data"]["contribution_id"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, replay_contrib) =
            post_contribution(&state, &artifact_id, "t030-contrib").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(replay_contrib["provenance"]["replay"], true);
        assert_eq!(
            replay_contrib["data"]["contribution_id"].as_str().unwrap(),
            contrib_id
        );

        // 3. link: project-scoped helper must apply the same idempotency
        //    semantics. Links only carry idempotency uniqueness when a
        //    workflow_run_id is supplied (per the DB unique-index condition),
        //    so set `x-workflow-run-id` to exercise the replay path through
        //    `load_workflow_run_for_link`.
        async fn post_link(
            state: &AppState,
            artifact_id: &str,
            version_id: &str,
            run_id: &str,
            key: &str,
        ) -> (StatusCode, Value) {
            let mut headers = mutation_headers(key);
            headers.insert("x-workflow-run-id", HeaderValue::from_str(run_id).unwrap());
            let response = create_artifact_link_handler(
                State(state.clone()),
                Path("demo".to_string()),
                headers,
                Json(CreateLinkRequest {
                    link_type: "doc_referenced_by_spec".to_string(),
                    source_kind: "artifact".to_string(),
                    source_id: artifact_id.to_string(),
                    source_version_id: None,
                    source_child_address: None,
                    target_kind: "artifact_version".to_string(),
                    target_id: version_id.to_string(),
                    target_version_id: Some(version_id.to_string()),
                    target_child_address: None,
                    supersedes_link_id: None,
                    actor_display_name: None,
                }),
            )
            .await
            .unwrap();
            response_json(response).await
        }
        let (status, first_link) =
            post_link(&state, &artifact_id, &version_id, &run_id, "t030-link").await;
        assert_eq!(status, StatusCode::CREATED);
        let link_id = first_link["data"]["link_id"].as_str().unwrap().to_string();
        let (status, replay_link) =
            post_link(&state, &artifact_id, &version_id, &run_id, "t030-link").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(replay_link["provenance"]["replay"], true);
        assert_eq!(replay_link["data"]["link_id"].as_str().unwrap(), link_id);
        assert_eq!(
            replay_link["provenance"]["authorization"]["required_scopes"][0],
            "link.write"
        );
    }

    #[tokio::test]
    async fn artifact_routes_allow_resumable_failed_run_retry_and_reject_cancelled() {
        let state = test_state();
        let artifact_id = create_test_artifact(&state, "artifact-create-workflow").await;
        let (_, version) = create_test_version(
            &state,
            &artifact_id,
            "workflow-source-version",
            None,
            None,
            "source",
        )
        .await;
        let source_version_id = version["data"]["artifact_version_id"]
            .as_str()
            .unwrap()
            .to_string();

        let started = start_artifact_workflow_run_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            mutation_headers("run-resumable"),
            Json(StartWorkflowRunRequest {
                workflow_kind: "spec_task_generation".to_string(),
                phase: Some("generation".to_string()),
                round_id: None,
                participant_actor_ids: None,
                source_artifact_version_id: Some(source_version_id.clone()),
                read_set: None,
                is_resumable: Some(true),
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        let (_, started_json) = response_json(started).await;
        let run_id = started_json["data"]["workflow_run_id"]
            .as_str()
            .unwrap()
            .to_string();

        let failed = complete_artifact_workflow_run_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone(), run_id.clone())),
            mutation_headers("run-resumable-fail"),
            Json(CompleteWorkflowRunRequest {
                state: "failed".to_string(),
                failure_reason: Some("transient".to_string()),
                generated_contribution_ids: None,
                generated_version_ids: None,
                generated_task_ids: None,
                generated_link_ids: None,
                generated_chunk_ids: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(response_json(failed).await.0, StatusCode::OK);

        let mut retry_headers = mutation_headers("version-after-failed-run");
        retry_headers.insert("x-workflow-run-id", HeaderValue::from_str(&run_id).unwrap());
        let recovered = create_artifact_version_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            retry_headers,
            Json(CreateArtifactVersionRequest {
                version_label: Some("resumed".to_string()),
                parent_version_id: Some(source_version_id.clone()),
                body_format: Some("markdown".to_string()),
                body: Some("recovered".to_string()),
                structured_payload: None,
                source_format: None,
                version_state: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(response_json(recovered).await.0, StatusCode::CREATED);

        let cancelled = start_artifact_workflow_run_handler(
            State(state.clone()),
            Path(("demo".to_string(), artifact_id.clone())),
            mutation_headers("run-cancelled"),
            Json(StartWorkflowRunRequest {
                workflow_kind: "doc_publish".to_string(),
                phase: None,
                round_id: None,
                participant_actor_ids: None,
                source_artifact_version_id: Some(source_version_id.clone()),
                read_set: None,
                is_resumable: Some(true),
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        let (_, cancelled_json) = response_json(cancelled).await;
        let cancelled_run_id = cancelled_json["data"]["workflow_run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let cancelled_done = complete_artifact_workflow_run_handler(
            State(state.clone()),
            Path((
                "demo".to_string(),
                artifact_id.clone(),
                cancelled_run_id.clone(),
            )),
            mutation_headers("run-cancelled-complete"),
            Json(CompleteWorkflowRunRequest {
                state: "cancelled".to_string(),
                failure_reason: None,
                generated_contribution_ids: None,
                generated_version_ids: None,
                generated_task_ids: None,
                generated_link_ids: None,
                generated_chunk_ids: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(response_json(cancelled_done).await.0, StatusCode::OK);

        let mut cancelled_retry_headers = mutation_headers("version-after-cancelled-run");
        cancelled_retry_headers.insert(
            "x-workflow-run-id",
            HeaderValue::from_str(&cancelled_run_id).unwrap(),
        );
        let rejected = create_artifact_version_handler(
            State(state),
            Path(("demo".to_string(), artifact_id)),
            cancelled_retry_headers,
            Json(CreateArtifactVersionRequest {
                version_label: Some("cancelled".to_string()),
                parent_version_id: Some(source_version_id),
                body_format: Some("markdown".to_string()),
                body: Some("must reject".to_string()),
                structured_payload: None,
                source_format: None,
                version_state: None,
                actor_display_name: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(rejected.0, StatusCode::BAD_REQUEST);
    }
}
