#![allow(dead_code)]

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::{Arc, Mutex};

pub type Db = Arc<Mutex<Connection>>;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Project {
    pub ident: String,
    /// Name of the channel plugin handling this project ("discord", "slack", …).
    pub channel_name: String,
    /// Opaque, plugin-specific room identifier.
    pub room_id: String,
    /// Opaque ID of the last inbound message seen (backfill cursor).
    pub last_msg_id: Option<String>,
    pub created_at: i64,
    pub repo_provider: Option<String>,
    pub repo_namespace: Option<String>,
    pub repo_name: Option<String>,
    pub repo_full_name: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Message {
    pub id: i64,
    pub project_ident: String,
    /// "agent" | "user" | "system"
    pub source: String,
    /// Opaque, plugin-specific message identifier.
    pub external_message_id: Option<String>,
    pub content: String,
    pub sent_at: i64,
    /// Timestamp (ms) when the agent confirmed this message, or None if unconfirmed.
    pub confirmed_at: Option<i64>,
    pub parent_message_id: Option<i64>,
    pub agent_id: Option<String>,
    /// "message" | "reply" | "action"
    pub message_type: String,
    /// Short headline supplied by the agent (or auto-derived from the body).
    pub subject: Option<String>,
    /// Origin host the agent claims to be running on (defaults to agent_id).
    pub hostname: Option<String>,
    /// Event time (epoch ms) supplied by the agent — distinct from sent_at,
    /// which is the gateway-receive time.
    pub event_at: Option<i64>,
    /// System messages are only delivered to agents when explicitly enabled.
    pub deliver_to_agents: bool,
}

pub fn open(path: &str) -> Result<Db> {
    let conn = Connection::open(path).context("open sqlite database")?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;",
    )?;
    apply_schema(&conn)?;
    Ok(Arc::new(Mutex::new(conn)))
}

fn apply_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects (
            ident         TEXT PRIMARY KEY,
            channel_name  TEXT NOT NULL,
            room_id       TEXT NOT NULL,
            last_msg_id   TEXT,
            created_at    INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS messages (
            id                   INTEGER PRIMARY KEY AUTOINCREMENT,
            project_ident        TEXT NOT NULL REFERENCES projects(ident),
            source               TEXT NOT NULL CHECK(source IN ('agent','user','system')),
            external_message_id  TEXT,
            content              TEXT NOT NULL,
            sent_at              INTEGER NOT NULL,
            deliver_to_agents    INTEGER NOT NULL DEFAULT 0
        );

        CREATE INDEX IF NOT EXISTS idx_messages_project
            ON messages(project_ident, id);

        CREATE TABLE IF NOT EXISTS cursors (
            project_ident  TEXT PRIMARY KEY REFERENCES projects(ident),
            last_read_id   INTEGER NOT NULL DEFAULT 0,
            updated_at     INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS skills (
            name        TEXT PRIMARY KEY,
            zip_data    BLOB NOT NULL,
            size        INTEGER NOT NULL,
            checksum    TEXT NOT NULL,
            uploaded_at INTEGER NOT NULL
        );",
    )
    .context("apply schema")?;

    // ── Migration: add per-message confirmation column ────────────────────────
    // Idempotent: ALTER fails silently if the column already exists.
    let _ = conn.execute("ALTER TABLE messages ADD COLUMN confirmed_at INTEGER", []);

    // Migrate old cursor state: mark all previously-read messages as confirmed.
    conn.execute(
        "UPDATE messages SET confirmed_at = sent_at
         WHERE confirmed_at IS NULL
           AND id <= (
               SELECT COALESCE(c.last_read_id, 0)
               FROM cursors c
               WHERE c.project_ident = messages.project_ident
           )",
        [],
    )?;

    // Partial index for fast unconfirmed-message lookups.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_messages_unconfirmed
             ON messages(project_ident, id) WHERE confirmed_at IS NULL;",
    )?;

    // ── Migration: add kind/content columns for command support ───────────────
    let _ = conn.execute(
        "ALTER TABLE skills ADD COLUMN kind TEXT NOT NULL DEFAULT 'skill'",
        [],
    );
    let _ = conn.execute("ALTER TABLE skills ADD COLUMN content TEXT", []);

    // ── Migration: per-agent message buffers ─────────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agents (
            project_ident  TEXT NOT NULL REFERENCES projects(ident),
            agent_id       TEXT NOT NULL,
            registered_at  INTEGER NOT NULL,
            PRIMARY KEY (project_ident, agent_id)
        );

        CREATE TABLE IF NOT EXISTS agent_confirmations (
            agent_id       TEXT NOT NULL,
            project_ident  TEXT NOT NULL,
            message_id     INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
            confirmed_at   INTEGER NOT NULL,
            PRIMARY KEY (agent_id, project_ident, message_id)
        );

        CREATE INDEX IF NOT EXISTS idx_agent_conf_project
            ON agent_confirmations(project_ident, message_id);",
    )?;

    let _ = conn.execute(
        "ALTER TABLE messages ADD COLUMN parent_message_id INTEGER",
        [],
    );
    let _ = conn.execute("ALTER TABLE messages ADD COLUMN agent_id TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE messages ADD COLUMN message_type TEXT NOT NULL DEFAULT 'message'",
        [],
    );

    // ── Migration: structured-message fields (subject/hostname/event_at) ─────
    let _ = conn.execute("ALTER TABLE messages ADD COLUMN subject TEXT", []);
    let _ = conn.execute("ALTER TABLE messages ADD COLUMN hostname TEXT", []);
    let _ = conn.execute("ALTER TABLE messages ADD COLUMN event_at INTEGER", []);
    let _ = conn.execute(
        "ALTER TABLE messages ADD COLUMN deliver_to_agents INTEGER NOT NULL DEFAULT 0",
        [],
    );
    migrate_messages_for_system_delivery(conn)?;

    // ── Migration: optional provider-aware repository mapping for projects ───
    let _ = conn.execute("ALTER TABLE projects ADD COLUMN repo_provider TEXT", []);
    let _ = conn.execute("ALTER TABLE projects ADD COLUMN repo_namespace TEXT", []);
    let _ = conn.execute("ALTER TABLE projects ADD COLUMN repo_name TEXT", []);
    let _ = conn.execute("ALTER TABLE projects ADD COLUMN repo_full_name TEXT", []);

    // Migrate existing confirmed messages to agent_confirmations for "_default" agent.
    conn.execute(
        "INSERT OR IGNORE INTO agent_confirmations (agent_id, project_ident, message_id, confirmed_at)
         SELECT '_default', project_ident, id, confirmed_at
         FROM messages
         WHERE confirmed_at IS NOT NULL",
        [],
    )?;

    // ── Settings: simple key/value store for UI prefs (theme, etc.) ──────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )?;

    // ── Tasks: per-project kanban (todo/in_progress/done) ────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (
            id              TEXT PRIMARY KEY,
            project_ident   TEXT NOT NULL REFERENCES projects(ident),
            title           TEXT NOT NULL,
            description     TEXT,
            details         TEXT,
            status          TEXT NOT NULL DEFAULT 'todo'
                            CHECK(status IN ('todo','in_progress','done')),
            rank            INTEGER NOT NULL DEFAULT 0,
            labels          TEXT,
            hostname        TEXT,
            owner_agent_id  TEXT,
            reporter        TEXT NOT NULL,
            created_at      INTEGER NOT NULL,
            updated_at      INTEGER NOT NULL,
            started_at      INTEGER,
            done_at         INTEGER,
            kind            TEXT NOT NULL DEFAULT 'normal',
            delegated_to_project_ident TEXT,
            delegated_to_task_id TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_tasks_project_status
            ON tasks(project_ident, status);
        CREATE INDEX IF NOT EXISTS idx_tasks_project_rank
            ON tasks(project_ident, status, rank);

        CREATE TABLE IF NOT EXISTS task_comments (
            id           TEXT PRIMARY KEY,
            task_id      TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            author       TEXT NOT NULL,
            author_type  TEXT NOT NULL CHECK(author_type IN ('agent','user','system')),
            content      TEXT NOT NULL,
            created_at   INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_task_comments_task
            ON task_comments(task_id, created_at);",
    )?;

    let _ = conn.execute(
        "ALTER TABLE tasks ADD COLUMN kind TEXT NOT NULL DEFAULT 'normal'",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE tasks ADD COLUMN delegated_to_project_ident TEXT",
        [],
    );
    let _ = conn.execute("ALTER TABLE tasks ADD COLUMN delegated_to_task_id TEXT", []);

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_delegations (
            id                    TEXT PRIMARY KEY,
            source_project_ident  TEXT NOT NULL REFERENCES projects(ident),
            source_task_id        TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            target_project_ident  TEXT NOT NULL REFERENCES projects(ident),
            target_task_id        TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            requester_agent_id    TEXT,
            requester_hostname    TEXT,
            created_at            INTEGER NOT NULL,
            completed_at          INTEGER,
            completion_message_id INTEGER REFERENCES messages(id)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_task_delegations_source
            ON task_delegations(source_project_ident, source_task_id);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_task_delegations_target
            ON task_delegations(target_project_ident, target_task_id);",
    )?;

    // ── Patterns: global markdown pattern library ────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS patterns (
            id          TEXT PRIMARY KEY,
            title       TEXT NOT NULL,
            slug        TEXT NOT NULL UNIQUE,
            summary     TEXT,
            body        TEXT NOT NULL,
            labels      TEXT,
            version     TEXT NOT NULL DEFAULT 'draft'
                        CHECK(version IN ('draft','latest','superseded')),
            state       TEXT NOT NULL DEFAULT 'active'
                        CHECK(state IN ('active','archived')),
            superseded_by TEXT,
            author      TEXT NOT NULL,
            created_at  INTEGER NOT NULL,
            updated_at  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_patterns_updated
            ON patterns(updated_at DESC);

        CREATE TABLE IF NOT EXISTS pattern_comments (
            id           TEXT PRIMARY KEY,
            pattern_id   TEXT NOT NULL REFERENCES patterns(id) ON DELETE CASCADE,
            author       TEXT NOT NULL,
            author_type  TEXT NOT NULL CHECK(author_type IN ('agent','user','system')),
            content      TEXT NOT NULL,
            created_at   INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_pattern_comments_pattern
            ON pattern_comments(pattern_id, created_at);",
    )?;

    let _ = conn.execute(
        "ALTER TABLE patterns ADD COLUMN version TEXT NOT NULL DEFAULT 'draft'",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE patterns ADD COLUMN state TEXT NOT NULL DEFAULT 'active'",
        [],
    );
    let _ = conn.execute("ALTER TABLE patterns ADD COLUMN superseded_by TEXT", []);
    conn.execute(
        "UPDATE patterns
         SET superseded_by = substr(state, length('superseded-by:') + 1),
             state = 'active'
         WHERE superseded_by IS NULL
           AND state LIKE 'superseded-by:%'",
        [],
    )?;

    // ── Agent API docs: project-scoped, agent-native API context ─────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS api_docs (
            id             TEXT PRIMARY KEY,
            project_ident  TEXT NOT NULL REFERENCES projects(ident) ON DELETE CASCADE,
            app            TEXT NOT NULL,
            title          TEXT NOT NULL,
            summary        TEXT,
            kind           TEXT NOT NULL DEFAULT 'agent_context',
            source_format  TEXT NOT NULL DEFAULT 'agent_context',
            source_ref     TEXT,
            version        TEXT,
            labels         TEXT,
            content_json   TEXT NOT NULL,
            author         TEXT NOT NULL,
            created_at     INTEGER NOT NULL,
            updated_at     INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_api_docs_project_app
            ON api_docs(project_ident, app, updated_at DESC);
        CREATE INDEX IF NOT EXISTS idx_api_docs_project_updated
            ON api_docs(project_ident, updated_at DESC);",
    )?;

    apply_artifact_substrate_schema(conn)?;

    Ok(())
}

// ─── Artifact substrate v1 (T002 / T003 / T004) ──────────────────────────────
//
// This block introduces the shared artifact substrate that backs spec, design-
// review, and documentation workflows on the gateway. Migrations are additive:
// every statement is `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS`
// or a tolerant `ALTER TABLE` so re-running on an existing database (with the
// pre-existing `tasks`, `api_docs`, `messages` tables) preserves prior data.
//
// Contract-to-table traceability matrix
// ─────────────────────────────────────
// Each row maps a contract decision to its storage location, the constraint
// or index that enforces the rule, and the unit test that proves it. Routes
// (T007) are listed by anchor only because route handlers are not in scope
// for T005.
//
// | Contract decision (source)                                  | Storage                                                    | Constraint / index                                                              | Test                                       | Route anchor |
// |-------------------------------------------------------------|------------------------------------------------------------|----------------------------------------------------------------------------------|--------------------------------------------|--------------|
// | Actor identity upsert key (T002 §Actor, T003 §2.3)          | `artifact_actors` (actor_type, agent_system, agent_id, host) | `idx_artifact_actors_identity` UNIQUE                                            | `artifact_actor_upsert_is_idempotent`      | T007         |
// | Artifact = mutable container, version = immutable (T002 §1) | `artifacts` + `artifact_versions`                           | `artifact_versions` is treated as append-only via `prevent_artifact_version_*` triggers | `artifact_version_body_is_immutable`       | T007         |
// | current_version_id ≠ accepted_version_id (T002 §State)      | `artifacts.current_version_id`, `artifacts.accepted_version_id` | Distinct nullable FK columns; neither derived                                    | `artifact_current_and_accepted_versions_diverge` | T007  |
// | Five-field state model not collapsed (T002 §State)          | `artifacts.lifecycle_state`, `review_state`, `implementation_state`, plus `artifact_versions.version_state` | CHECK constraints per field                                                      | `artifact_state_fields_are_independent`   | T007         |
// | Version body immutability + structured payload (T002 §3)    | `artifact_versions.body`, `structured_payload`              | Triggers `prevent_artifact_version_body_update` / `prevent_artifact_version_payload_update` | `artifact_version_body_is_immutable`     | T007         |
// | Idempotency uniqueness `(workflow_run_id, key)` (T003 §4.3) | `artifact_versions`, `artifact_contributions`, `artifact_links`, `workflow_runs` | UNIQUE indexes per resource                                                      | `artifact_link_idempotency_is_unique_per_run`, `workflow_run_idempotency_is_unique_per_kind` | T007 |
// | Comment v1 targets + child_address only on version (T002 §Comment) | `artifact_comments.target_kind`, `child_address`           | CHECK + index `idx_artifact_comments_target`                                     | `artifact_comment_anchors_to_manifest_item` | T007       |
// | Audit-path link version refs (T002 invariant 2, T003 §7)    | `artifact_links.source_version_id`, `target_version_id`     | Stored nullable; T006 enforces per-link-type rule. Doc'd here for downstream. | `artifact_link_idempotency_is_unique_per_run` | T006/T007 |
// | Chunks anchor on immutable version + child_address (T002 §Chunk) | `artifact_chunks.artifact_version_id`, `child_address`      | UNIQUE `(artifact_version_id, child_address)`                                    | `artifact_chunk_supersession_preserves_history` | T011  |
// | Chunk soft-supersession (T002 §Chunk, T004 §6)              | `artifact_chunks.superseded_by_chunk_id`                    | Self-referential FK                                                              | `artifact_chunk_supersession_preserves_history` | T011  |
// | Workflow run resumable failed→succeeded (T003 §5.1)         | `workflow_runs.state` + `is_resumable`                      | CHECK on state set; T006 enforces resumable transition rule via test            | `workflow_run_resumable_kind_can_recover_from_failed` | T007 |
// | Workflow run cancelled is terminal (T003 §5.1)              | same                                                        | Trigger `prevent_workflow_run_state_after_terminal`                              | `workflow_run_cancelled_is_terminal`       | T007         |
// | Body purge fields for archived retention (T004 §3)          | `artifact_versions.body_purged_at`                          | Nullable column; populated by SRE purge job (T004)                               | `artifact_schema_apply_preserves_existing_tables` (column presence) | SRE purge |
// | Migration preserves existing tables (T004 Phase 0)          | All artifact tables additive                                | `CREATE TABLE IF NOT EXISTS` everywhere                                          | `artifact_schema_apply_preserves_existing_tables` | T014  |
//
// Notes for downstream tasks
// --------------------------
// * T006 (repository): API-level immutability of `artifact_versions` is also
//   enforced here at the DB layer via UPDATE triggers on body /
//   structured_payload / source_format / parent_version_id / body_format —
//   see `prevent_artifact_version_*` below. `version_state` IS allowed to
//   change because the substrate models state transitions explicitly.
// * T006 / T007: link audit-path validation (which `link_type` requires
//   `source_version_id` / `target_version_id`) is policy that lives in the
//   repository layer because the registry of link types is owned by T003.
//   The schema only stores the columns; it does not constrain the registry.
// * T016 / T007: `workflow_runs.is_resumable` is a column the workflow-kind
//   registry (owned by T003) populates at run-start time. The schema treats
//   it as authoritative for the failed→succeeded recovery rule so the
//   substrate is not lock-stepped to changes in the kind registry.
fn apply_artifact_substrate_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        // ── Actors ────────────────────────────────────────────────────────────
        "CREATE TABLE IF NOT EXISTS artifact_actors (
            actor_id              TEXT PRIMARY KEY,
            actor_type            TEXT NOT NULL
                                  CHECK(actor_type IN ('user','agent','system')),
            agent_system          TEXT
                                  CHECK(agent_system IS NULL OR agent_system IN ('claude','codex','gemini','other')),
            agent_system_label    TEXT,
            agent_id              TEXT,
            host                  TEXT,
            display_name          TEXT NOT NULL,
            runtime_metadata      TEXT,
            created_at            INTEGER NOT NULL,
            updated_at            INTEGER NOT NULL
        );
        -- The identity tuple is (actor_type, agent_system, agent_id, host).
        -- COALESCE the nullables to a sentinel so SQLite's UNIQUE treats a
        -- consistent absent value as identical (NULL != NULL otherwise).
        CREATE UNIQUE INDEX IF NOT EXISTS idx_artifact_actors_identity
            ON artifact_actors(
                actor_type,
                COALESCE(agent_system, ''),
                COALESCE(agent_id, ''),
                COALESCE(host, '')
            );

        -- ── Artifacts (mutable containers) ───────────────────────────────────
        CREATE TABLE IF NOT EXISTS artifacts (
            artifact_id            TEXT PRIMARY KEY,
            project_ident          TEXT NOT NULL REFERENCES projects(ident),
            kind                   TEXT NOT NULL
                                   CHECK(kind IN ('design_review','spec','documentation')),
            subkind                TEXT,
            title                  TEXT NOT NULL,
            labels                 TEXT,
            -- T002 §State model: five independent state fields. None collapses.
            lifecycle_state        TEXT NOT NULL DEFAULT 'draft'
                                   CHECK(lifecycle_state IN ('draft','active','superseded','archived')),
            review_state           TEXT NOT NULL DEFAULT 'none'
                                   CHECK(review_state IN ('none','collecting_reviews','synthesizing','needs_user_decision','accepted','rejected')),
            implementation_state   TEXT NOT NULL DEFAULT 'not_applicable'
                                   CHECK(implementation_state IN ('not_applicable','not_started','in_progress','complete','blocked')),
            current_version_id     TEXT REFERENCES artifact_versions(artifact_version_id) DEFERRABLE INITIALLY DEFERRED,
            accepted_version_id    TEXT REFERENCES artifact_versions(artifact_version_id) DEFERRABLE INITIALLY DEFERRED,
            created_by_actor_id    TEXT NOT NULL REFERENCES artifact_actors(actor_id),
            created_at             INTEGER NOT NULL,
            updated_at             INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_artifacts_project_kind
            ON artifacts(project_ident, kind, updated_at DESC);
        CREATE INDEX IF NOT EXISTS idx_artifacts_project_lifecycle
            ON artifacts(project_ident, lifecycle_state, updated_at DESC);

        -- ── Artifact versions (immutable snapshots) ──────────────────────────
        CREATE TABLE IF NOT EXISTS artifact_versions (
            artifact_version_id          TEXT PRIMARY KEY,
            artifact_id                  TEXT NOT NULL REFERENCES artifacts(artifact_id) ON DELETE CASCADE,
            version_label                TEXT,
            parent_version_id            TEXT REFERENCES artifact_versions(artifact_version_id),
            body_format                  TEXT NOT NULL
                                          CHECK(body_format IN ('markdown','application/agent-context+json','openapi','swagger')),
            body                         TEXT,
            structured_payload           TEXT,
            source_format                TEXT,
            created_by_actor_id          TEXT NOT NULL REFERENCES artifact_actors(actor_id),
            created_via_workflow_run_id  TEXT REFERENCES workflow_runs(workflow_run_id) DEFERRABLE INITIALLY DEFERRED,
            version_state                TEXT NOT NULL DEFAULT 'draft'
                                          CHECK(version_state IN ('draft','under_review','accepted','superseded','rejected')),
            idempotency_key              TEXT,
            -- T004 §3 retention: when a body is purged the bytes go to NULL
            -- and the timestamp is recorded. ids and audit links survive.
            body_purged_at               INTEGER,
            created_at                   INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_artifact_versions_by_artifact
            ON artifact_versions(artifact_id, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_artifact_versions_state
            ON artifact_versions(artifact_id, version_state);
        -- T003 §4.3 idempotency: (artifact_id, run_id, key) is unique when key set.
        CREATE UNIQUE INDEX IF NOT EXISTS idx_artifact_versions_idempotency
            ON artifact_versions(artifact_id, created_via_workflow_run_id, idempotency_key)
            WHERE idempotency_key IS NOT NULL;

        -- Triggers enforce immutability of the body / payload / parent /
        -- body_format / source_format. version_state IS allowed to change
        -- (state transitions are first-class in the contract).
        -- body_purged_at is allowed to transition NULL -> timestamp ONCE,
        -- and after that the body must remain NULL (T004 §3 purge).
        CREATE TRIGGER IF NOT EXISTS prevent_artifact_version_body_update
        BEFORE UPDATE OF body ON artifact_versions
        FOR EACH ROW
        WHEN OLD.body_purged_at IS NULL
             AND NEW.body IS NOT OLD.body
             AND NOT (NEW.body IS NULL AND NEW.body_purged_at IS NOT NULL)
        BEGIN
            SELECT RAISE(ABORT, 'artifact_version body is immutable');
        END;
        CREATE TRIGGER IF NOT EXISTS prevent_artifact_version_body_repopulate
        BEFORE UPDATE OF body ON artifact_versions
        FOR EACH ROW
        WHEN OLD.body_purged_at IS NOT NULL AND NEW.body IS NOT NULL
        BEGIN
            SELECT RAISE(ABORT, 'artifact_version body cannot be repopulated after purge');
        END;
        CREATE TRIGGER IF NOT EXISTS prevent_artifact_version_payload_update
        BEFORE UPDATE OF structured_payload ON artifact_versions
        FOR EACH ROW
        WHEN OLD.body_purged_at IS NULL AND NEW.structured_payload IS NOT OLD.structured_payload
        BEGIN
            SELECT RAISE(ABORT, 'artifact_version structured_payload is immutable');
        END;
        CREATE TRIGGER IF NOT EXISTS prevent_artifact_version_meta_update
        BEFORE UPDATE OF body_format, source_format, parent_version_id, artifact_id, idempotency_key, created_via_workflow_run_id ON artifact_versions
        FOR EACH ROW
        WHEN
            NEW.body_format IS NOT OLD.body_format
            OR NEW.source_format IS NOT OLD.source_format
            OR NEW.parent_version_id IS NOT OLD.parent_version_id
            OR NEW.artifact_id IS NOT OLD.artifact_id
            OR NEW.idempotency_key IS NOT OLD.idempotency_key
            OR NEW.created_via_workflow_run_id IS NOT OLD.created_via_workflow_run_id
        BEGIN
            SELECT RAISE(ABORT, 'artifact_version metadata is immutable');
        END;

        -- ── Workflow runs ────────────────────────────────────────────────────
        CREATE TABLE IF NOT EXISTS workflow_runs (
            workflow_run_id               TEXT PRIMARY KEY,
            artifact_id                   TEXT NOT NULL REFERENCES artifacts(artifact_id) ON DELETE CASCADE,
            workflow_kind                 TEXT NOT NULL
                                           CHECK(workflow_kind IN ('design_review_round','spec_iteration','spec_acceptance','spec_task_generation','doc_publish')),
            phase                         TEXT,
            round_id                      TEXT,
            coordinator_actor_id          TEXT NOT NULL REFERENCES artifact_actors(actor_id),
            participant_actor_ids         TEXT,
            source_artifact_version_id    TEXT REFERENCES artifact_versions(artifact_version_id),
            read_set                      TEXT,
            idempotency_key               TEXT,
            -- T003 §5.1 resumable rule: a kind that opts into resumability
            -- may transition failed -> succeeded under the same idempotency
            -- scope while finishing missing fan-out work. cancelled is
            -- always terminal regardless of is_resumable.
            is_resumable                  INTEGER NOT NULL DEFAULT 0,
            state                         TEXT NOT NULL DEFAULT 'started'
                                           CHECK(state IN ('started','succeeded','failed','cancelled')),
            generated_contribution_ids    TEXT,
            generated_version_ids         TEXT,
            generated_task_ids            TEXT,
            generated_link_ids            TEXT,
            generated_chunk_ids           TEXT,
            failure_reason                TEXT,
            started_at                    INTEGER NOT NULL,
            ended_at                      INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_workflow_runs_artifact
            ON workflow_runs(artifact_id, started_at DESC);
        CREATE INDEX IF NOT EXISTS idx_workflow_runs_state
            ON workflow_runs(state, started_at DESC);
        -- T003 §4.3: starting a run for the same coordinator+artifact+kind
        -- with the same key is idempotent.
        CREATE UNIQUE INDEX IF NOT EXISTS idx_workflow_runs_idempotency
            ON workflow_runs(coordinator_actor_id, artifact_id, workflow_kind, idempotency_key)
            WHERE idempotency_key IS NOT NULL;

        -- Trigger: cancelled is terminal; succeeded is terminal too unless
        -- the kind opts into resumability AND the previous state was failed.
        CREATE TRIGGER IF NOT EXISTS prevent_workflow_run_state_after_terminal
        BEFORE UPDATE OF state ON workflow_runs
        FOR EACH ROW
        WHEN
            (OLD.state = 'cancelled' AND NEW.state IS NOT OLD.state)
            OR (OLD.state = 'succeeded' AND NEW.state IS NOT OLD.state)
            OR (OLD.state = 'failed' AND NEW.state = 'succeeded' AND OLD.is_resumable = 0)
            OR (OLD.state = 'failed' AND NEW.state IN ('started','cancelled'))
        BEGIN
            SELECT RAISE(ABORT, 'invalid workflow_run state transition');
        END;

        -- ── Contributions ────────────────────────────────────────────────────
        CREATE TABLE IF NOT EXISTS artifact_contributions (
            contribution_id        TEXT PRIMARY KEY,
            artifact_id            TEXT NOT NULL REFERENCES artifacts(artifact_id) ON DELETE CASCADE,
            target_kind            TEXT NOT NULL
                                   CHECK(target_kind IN ('artifact','artifact_version','contribution')),
            target_id              TEXT NOT NULL,
            contribution_kind      TEXT NOT NULL
                                   CHECK(contribution_kind IN ('review','synthesis','decision','note','completion','state_transition')),
            phase                  TEXT,
            role                   TEXT NOT NULL
                                   CHECK(role IN ('author','reviewer','analyst','implementer','coordinator','user')),
            actor_id               TEXT NOT NULL REFERENCES artifact_actors(actor_id),
            workflow_run_id        TEXT REFERENCES workflow_runs(workflow_run_id),
            read_set               TEXT,
            body_format            TEXT NOT NULL
                                   CHECK(body_format IN ('markdown','application/agent-context+json','openapi','swagger')),
            body                   TEXT NOT NULL,
            idempotency_key        TEXT,
            created_at             INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_contributions_artifact
            ON artifact_contributions(artifact_id, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_contributions_target
            ON artifact_contributions(target_kind, target_id);
        CREATE INDEX IF NOT EXISTS idx_contributions_run
            ON artifact_contributions(workflow_run_id);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_contributions_idempotency_run
            ON artifact_contributions(workflow_run_id, idempotency_key)
            WHERE workflow_run_id IS NOT NULL AND idempotency_key IS NOT NULL;
        -- T003 §4.3 run-less contributions: scoped by (artifact_id, actor_id, key).
        CREATE UNIQUE INDEX IF NOT EXISTS idx_contributions_idempotency_runless
            ON artifact_contributions(artifact_id, actor_id, idempotency_key)
            WHERE workflow_run_id IS NULL AND idempotency_key IS NOT NULL;

        -- Contributions are immutable. Allow no updates at all.
        CREATE TRIGGER IF NOT EXISTS prevent_artifact_contribution_update
        BEFORE UPDATE ON artifact_contributions
        FOR EACH ROW
        BEGIN
            SELECT RAISE(ABORT, 'artifact_contribution rows are immutable');
        END;

        -- ── Comments ─────────────────────────────────────────────────────────
        CREATE TABLE IF NOT EXISTS artifact_comments (
            comment_id                    TEXT PRIMARY KEY,
            artifact_id                   TEXT NOT NULL REFERENCES artifacts(artifact_id) ON DELETE CASCADE,
            target_kind                   TEXT NOT NULL
                                           CHECK(target_kind IN ('artifact','artifact_version','contribution')),
            target_id                     TEXT NOT NULL,
            -- child_address is only valid when target_kind = artifact_version.
            -- See T002 §Comment lifecycle. Block-level targets deferred to v2.
            child_address                 TEXT,
            parent_comment_id             TEXT REFERENCES artifact_comments(comment_id),
            actor_id                      TEXT NOT NULL REFERENCES artifact_actors(actor_id),
            body                          TEXT NOT NULL,
            state                         TEXT NOT NULL DEFAULT 'open'
                                           CHECK(state IN ('open','resolved')),
            resolved_by_actor_id          TEXT REFERENCES artifact_actors(actor_id),
            resolved_by_workflow_run_id   TEXT REFERENCES workflow_runs(workflow_run_id),
            resolved_at                   INTEGER,
            resolution_note               TEXT,
            idempotency_key               TEXT,
            created_at                    INTEGER NOT NULL,
            updated_at                    INTEGER NOT NULL,
            CHECK (child_address IS NULL OR target_kind = 'artifact_version')
        );
        CREATE INDEX IF NOT EXISTS idx_artifact_comments_target
            ON artifact_comments(target_kind, target_id);
        CREATE INDEX IF NOT EXISTS idx_artifact_comments_artifact_state
            ON artifact_comments(artifact_id, state, created_at DESC);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_artifact_comments_idempotency
            ON artifact_comments(target_kind, target_id, actor_id, idempotency_key)
            WHERE idempotency_key IS NOT NULL;

        -- ── Links ────────────────────────────────────────────────────────────
        CREATE TABLE IF NOT EXISTS artifact_links (
            link_id                      TEXT PRIMARY KEY,
            link_type                    TEXT NOT NULL,
            source_kind                  TEXT NOT NULL
                                          CHECK(source_kind IN ('artifact','artifact_version','contribution','comment','task','chunk','pattern','memory','commit','external_url')),
            source_id                    TEXT NOT NULL,
            source_version_id            TEXT REFERENCES artifact_versions(artifact_version_id),
            source_child_address         TEXT,
            target_kind                  TEXT NOT NULL
                                          CHECK(target_kind IN ('artifact','artifact_version','contribution','comment','task','chunk','pattern','memory','commit','external_url')),
            target_id                    TEXT NOT NULL,
            target_version_id            TEXT REFERENCES artifact_versions(artifact_version_id),
            target_child_address         TEXT,
            created_by_actor_id          TEXT NOT NULL REFERENCES artifact_actors(actor_id),
            created_via_workflow_run_id  TEXT REFERENCES workflow_runs(workflow_run_id),
            idempotency_key              TEXT,
            supersedes_link_id           TEXT REFERENCES artifact_links(link_id),
            created_at                   INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_artifact_links_source
            ON artifact_links(source_kind, source_id);
        CREATE INDEX IF NOT EXISTS idx_artifact_links_target
            ON artifact_links(target_kind, target_id);
        CREATE INDEX IF NOT EXISTS idx_artifact_links_type
            ON artifact_links(link_type, created_at DESC);
        -- T003 §4.3 / T002 §Link contract: workflow-emitted links carry
        -- `(workflow_run_id, idempotency_key)` uniqueness.
        CREATE UNIQUE INDEX IF NOT EXISTS idx_artifact_links_idempotency
            ON artifact_links(created_via_workflow_run_id, idempotency_key)
            WHERE created_via_workflow_run_id IS NOT NULL AND idempotency_key IS NOT NULL;

        -- ── Chunks ───────────────────────────────────────────────────────────
        CREATE TABLE IF NOT EXISTS artifact_chunks (
            chunk_id                  TEXT PRIMARY KEY,
            artifact_id               TEXT NOT NULL REFERENCES artifacts(artifact_id) ON DELETE CASCADE,
            artifact_version_id       TEXT NOT NULL REFERENCES artifact_versions(artifact_version_id),
            child_address             TEXT NOT NULL,
            text                      TEXT NOT NULL,
            embedding_model           TEXT,
            embedding_vector          BLOB,
            -- Retrieval filters (T002 §Chunk + T011 documentation needs).
            app                       TEXT,
            label                     TEXT,
            kind                      TEXT,
            metadata_json             TEXT,
            superseded_by_chunk_id    TEXT REFERENCES artifact_chunks(chunk_id),
            created_at                INTEGER NOT NULL
        );
        -- Natural key (T003 §4.3 chunks): repeated emission for the same
        -- (version, child_address) returns the same chunk_id.
        CREATE UNIQUE INDEX IF NOT EXISTS idx_artifact_chunks_natural_key
            ON artifact_chunks(artifact_version_id, child_address);
        CREATE INDEX IF NOT EXISTS idx_artifact_chunks_artifact_current
            ON artifact_chunks(artifact_id, superseded_by_chunk_id);
        CREATE INDEX IF NOT EXISTS idx_artifact_chunks_app_label
            ON artifact_chunks(app, label, kind);
        ",
    )
    .context("apply artifact substrate schema")?;
    Ok(())
}

// ─── Artifact substrate Rust models ──────────────────────────────────────────
//
// These structs are the substrate's wire / repository surface. T006 will add
// repository functions that take inputs and return these summary/detail
// shapes; T007 turns them into HTTP responses. The structs deliberately do
// not borrow from `rusqlite::Row` so SQL row layout can change without
// breaking downstream layers.

/// Resolved actor identity. Identity is `(actor_type, agent_system, agent_id, host)`;
/// `display_name` is UI-only. See T002 §Actor model details.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct ArtifactActor {
    pub actor_id: String,
    pub actor_type: String,
    pub agent_system: Option<String>,
    pub agent_system_label: Option<String>,
    pub agent_id: Option<String>,
    pub host: Option<String>,
    pub display_name: String,
    pub runtime_metadata: Option<serde_json::Value>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Identity tuple used to upsert an actor (T003 §2.3).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArtifactActorIdentity<'a> {
    pub actor_type: &'a str,
    pub agent_system: Option<&'a str>,
    pub agent_system_label: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub host: Option<&'a str>,
    pub display_name: &'a str,
    pub runtime_metadata: Option<&'a serde_json::Value>,
}

/// Mutable artifact container summary (T002 §Artifact).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct ArtifactSummary {
    pub artifact_id: String,
    pub project_ident: String,
    pub kind: String,
    pub subkind: Option<String>,
    pub title: String,
    pub labels: Vec<String>,
    pub lifecycle_state: String,
    pub review_state: String,
    pub implementation_state: String,
    pub current_version_id: Option<String>,
    pub accepted_version_id: Option<String>,
    pub created_by_actor_id: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Detail view including the resolved current/accepted version (when present).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct ArtifactDetail {
    pub artifact: ArtifactSummary,
    pub current_version: Option<ArtifactVersion>,
    pub accepted_version: Option<ArtifactVersion>,
}

/// Insert input for a new artifact container.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArtifactInsert<'a> {
    pub project_ident: &'a str,
    pub kind: &'a str,
    pub subkind: Option<&'a str>,
    pub title: &'a str,
    pub labels: &'a [String],
    pub created_by_actor_id: &'a str,
}

/// Update input for a mutable artifact field set. Pointer transitions go
/// through dedicated repository functions (T006); this is for title / labels /
/// state-field rewrites that the workflow layer drives.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct ArtifactUpdate<'a> {
    pub title: Option<&'a str>,
    pub labels: Option<&'a [String]>,
    pub lifecycle_state: Option<&'a str>,
    pub review_state: Option<&'a str>,
    pub implementation_state: Option<&'a str>,
}

/// Immutable artifact version (T002 §Artifact version).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct ArtifactVersion {
    pub artifact_version_id: String,
    pub artifact_id: String,
    pub version_label: Option<String>,
    pub parent_version_id: Option<String>,
    pub body_format: String,
    pub body: Option<String>,
    pub structured_payload: Option<serde_json::Value>,
    pub source_format: Option<String>,
    pub created_by_actor_id: String,
    pub created_via_workflow_run_id: Option<String>,
    pub version_state: String,
    pub idempotency_key: Option<String>,
    pub body_purged_at: Option<i64>,
    pub created_at: i64,
}

/// Insert input for a new immutable version row.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArtifactVersionInsert<'a> {
    pub artifact_id: &'a str,
    pub version_label: Option<&'a str>,
    pub parent_version_id: Option<&'a str>,
    pub body_format: &'a str,
    pub body: Option<&'a str>,
    pub structured_payload: Option<&'a serde_json::Value>,
    pub source_format: Option<&'a str>,
    pub created_by_actor_id: &'a str,
    pub created_via_workflow_run_id: Option<&'a str>,
    pub version_state: &'a str,
    pub idempotency_key: Option<&'a str>,
}

/// Contribution row (T002 §Contribution). Immutable once written.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct ArtifactContribution {
    pub contribution_id: String,
    pub artifact_id: String,
    pub target_kind: String,
    pub target_id: String,
    pub contribution_kind: String,
    pub phase: Option<String>,
    pub role: String,
    pub actor_id: String,
    pub workflow_run_id: Option<String>,
    pub read_set: Option<serde_json::Value>,
    pub body_format: String,
    pub body: String,
    pub idempotency_key: Option<String>,
    pub created_at: i64,
}

/// Insert input for a contribution.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArtifactContributionInsert<'a> {
    pub artifact_id: &'a str,
    pub target_kind: &'a str,
    pub target_id: &'a str,
    pub contribution_kind: &'a str,
    pub phase: Option<&'a str>,
    pub role: &'a str,
    pub actor_id: &'a str,
    pub workflow_run_id: Option<&'a str>,
    pub read_set: Option<&'a serde_json::Value>,
    pub body_format: &'a str,
    pub body: &'a str,
    pub idempotency_key: Option<&'a str>,
}

/// Comment (T002 §Comment).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct ArtifactComment {
    pub comment_id: String,
    pub artifact_id: String,
    pub target_kind: String,
    pub target_id: String,
    pub child_address: Option<String>,
    pub parent_comment_id: Option<String>,
    pub actor_id: String,
    pub body: String,
    pub state: String,
    pub resolved_by_actor_id: Option<String>,
    pub resolved_by_workflow_run_id: Option<String>,
    pub resolved_at: Option<i64>,
    pub resolution_note: Option<String>,
    pub idempotency_key: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Insert input for a comment.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArtifactCommentInsert<'a> {
    pub artifact_id: &'a str,
    pub target_kind: &'a str,
    pub target_id: &'a str,
    pub child_address: Option<&'a str>,
    pub parent_comment_id: Option<&'a str>,
    pub actor_id: &'a str,
    pub body: &'a str,
    pub idempotency_key: Option<&'a str>,
}

/// Link row (T002 §Link).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct ArtifactLink {
    pub link_id: String,
    pub link_type: String,
    pub source_kind: String,
    pub source_id: String,
    pub source_version_id: Option<String>,
    pub source_child_address: Option<String>,
    pub target_kind: String,
    pub target_id: String,
    pub target_version_id: Option<String>,
    pub target_child_address: Option<String>,
    pub created_by_actor_id: String,
    pub created_via_workflow_run_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub supersedes_link_id: Option<String>,
    pub created_at: i64,
}

/// Insert input for a link.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArtifactLinkInsert<'a> {
    pub link_type: &'a str,
    pub source_kind: &'a str,
    pub source_id: &'a str,
    pub source_version_id: Option<&'a str>,
    pub source_child_address: Option<&'a str>,
    pub target_kind: &'a str,
    pub target_id: &'a str,
    pub target_version_id: Option<&'a str>,
    pub target_child_address: Option<&'a str>,
    pub created_by_actor_id: &'a str,
    pub created_via_workflow_run_id: Option<&'a str>,
    pub idempotency_key: Option<&'a str>,
    pub supersedes_link_id: Option<&'a str>,
}

/// Chunk row (T002 §Chunk).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct ArtifactChunk {
    pub chunk_id: String,
    pub artifact_id: String,
    pub artifact_version_id: String,
    pub child_address: String,
    pub text: String,
    pub embedding_model: Option<String>,
    pub embedding_vector: Option<Vec<u8>>,
    pub app: Option<String>,
    pub label: Option<String>,
    pub kind: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub superseded_by_chunk_id: Option<String>,
    pub created_at: i64,
}

/// Insert input for a chunk.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArtifactChunkInsert<'a> {
    pub artifact_id: &'a str,
    pub artifact_version_id: &'a str,
    pub child_address: &'a str,
    pub text: &'a str,
    pub embedding_model: Option<&'a str>,
    pub embedding_vector: Option<&'a [u8]>,
    pub app: Option<&'a str>,
    pub label: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub metadata: Option<&'a serde_json::Value>,
}

/// Workflow run / activity (T002 §Workflow run).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct WorkflowRun {
    pub workflow_run_id: String,
    pub artifact_id: String,
    pub workflow_kind: String,
    pub phase: Option<String>,
    pub round_id: Option<String>,
    pub coordinator_actor_id: String,
    pub participant_actor_ids: Vec<String>,
    pub source_artifact_version_id: Option<String>,
    pub read_set: Option<serde_json::Value>,
    pub idempotency_key: Option<String>,
    pub is_resumable: bool,
    pub state: String,
    pub generated_contribution_ids: Vec<String>,
    pub generated_version_ids: Vec<String>,
    pub generated_task_ids: Vec<String>,
    pub generated_link_ids: Vec<String>,
    pub generated_chunk_ids: Vec<String>,
    pub failure_reason: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

/// Insert input for starting a workflow run.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WorkflowRunInsert<'a> {
    pub artifact_id: &'a str,
    pub workflow_kind: &'a str,
    pub phase: Option<&'a str>,
    pub round_id: Option<&'a str>,
    pub coordinator_actor_id: &'a str,
    pub participant_actor_ids: &'a [String],
    pub source_artifact_version_id: Option<&'a str>,
    pub read_set: Option<&'a serde_json::Value>,
    pub idempotency_key: Option<&'a str>,
    pub is_resumable: bool,
}

/// Update input for a workflow run state transition (T003 §5.1).
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct WorkflowRunUpdate<'a> {
    pub state: Option<&'a str>,
    pub failure_reason: Option<Option<&'a str>>,
    pub generated_contribution_ids: Option<&'a [String]>,
    pub generated_version_ids: Option<&'a [String]>,
    pub generated_task_ids: Option<&'a [String]>,
    pub generated_link_ids: Option<&'a [String]>,
    pub generated_chunk_ids: Option<&'a [String]>,
    pub ended_at: Option<Option<i64>>,
}

/// Repository write result. `replayed` is true when an idempotency key matched
/// an existing row and no new resource was created.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArtifactWriteResult<T> {
    pub record: T,
    pub warnings: Vec<QuotaWarning>,
    pub replayed: bool,
}

/// Artifact list/search filters. All filters are project-scoped by the
/// repository function so route handlers do not assemble SQL.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct ArtifactFilters<'a> {
    pub kind: Option<&'a str>,
    pub subkind: Option<&'a str>,
    pub lifecycle_state: Option<&'a str>,
    pub label: Option<&'a str>,
    pub actor_id: Option<&'a str>,
    pub query: Option<&'a str>,
}

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct ArtifactLinkFilters<'a> {
    pub link_type: Option<&'a str>,
    pub source_kind: Option<&'a str>,
    pub source_id: Option<&'a str>,
    pub target_kind: Option<&'a str>,
    pub target_id: Option<&'a str>,
}

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct ArtifactChunkFilters<'a> {
    pub artifact_version_id: Option<&'a str>,
    pub app: Option<&'a str>,
    pub label: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub include_superseded: bool,
    pub query: Option<&'a str>,
}

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct DesignReviewContributionFilters<'a> {
    pub round_id: Option<&'a str>,
    pub phase: Option<&'a str>,
    pub role: Option<&'a str>,
    pub reviewed_version_id: Option<&'a str>,
    pub read_set_contains: Option<&'a str>,
}

// ─── Artifact operations envelope (T016 — runtime support for T004) ─────────
//
// T004 (`docs/artifact-operations-rollout.md`) fixes the production operations
// envelope: per-write size limits, per-project soft/hard quotas, retention/
// archive policy, restore-check expectations. T016 provides the *runtime*
// surface that downstream tasks consume:
//
// * T006 repository: returns the typed errors below instead of reimplementing
//   policy. Calls `purge_archived_version_body` from the SRE nightly job entry
//   point.
// * T007 generic HTTP API: pulls the envelope from process state once
//   (no env reads / no hardcoded T004 constants in handler code) and maps
//   `OperationsError` variants to stable HTTP responses; `QuotaWarning`
//   values ride inside `provenance.warnings[]` per T003 §2.2.
// * T008 regression tests: shrinks limits via the T004 env keys (see
//   `from_env_with_keys`) so a single fixture exercises both accept and
//   reject paths without duplicating the production constants.
// * T014 rollout validation: drives the restore-check helpers and reports
//   inconsistencies *without* silent repair (per T004 §4.3).
//
// Layering rule: this section MUST stay free of HTTP / route concerns so the
// repository (T006) and worker (purge job) can share it. The route layer
// (T007) is responsible for translating the typed values to wire shapes.

/// Resource kinds that map to a configurable size limit per T004 §1.
/// String-typed in errors so the wire shape can name the exact limit
/// without exposing the enum to the HTTP layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SizeLimitKind {
    /// `artifact_version.body` for markdown / agent-context+json (T004 §1).
    ArtifactVersionBody,
    /// `artifact_version.body` for openapi / swagger source.
    ArtifactVersionSourceBody,
    /// `artifact_version.structured_payload`, serialized JSON.
    ArtifactVersionStructuredPayload,
    /// `contribution.body`.
    ContributionBody,
    /// `comment.body`.
    CommentBody,
    /// `chunk.text`.
    ChunkText,
    /// `artifact.labels` count.
    ArtifactLabelsCount,
    /// Per-label UTF-8 byte length.
    ArtifactLabelBytes,
    /// `read_set` referenced id count.
    ReadSetRefs,
}

impl SizeLimitKind {
    /// Stable wire token for HTTP error responses. Mirrors T004 §1 rejection
    /// codes so T007 can interpolate `<token>_too_large` directly.
    #[allow(dead_code)]
    pub fn token(self) -> &'static str {
        match self {
            SizeLimitKind::ArtifactVersionBody => "artifact_version_body",
            SizeLimitKind::ArtifactVersionSourceBody => "artifact_version_source_body",
            SizeLimitKind::ArtifactVersionStructuredPayload => "structured_payload",
            SizeLimitKind::ContributionBody => "contribution_body",
            SizeLimitKind::CommentBody => "comment_body",
            SizeLimitKind::ChunkText => "chunk_text",
            SizeLimitKind::ArtifactLabelsCount => "labels_too_many",
            SizeLimitKind::ArtifactLabelBytes => "label_too_long",
            SizeLimitKind::ReadSetRefs => "read_set_too_large",
        }
    }
}

/// Per-project counters that carry soft / hard thresholds per T004 §2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum QuotaCounter {
    Artifact,
    Version,
    Contribution,
    OpenComment,
    Link,
    Chunk,
    RunningWorkflow,
    WriteRpm,
}

impl QuotaCounter {
    /// Stable wire token: `quota_<token>_soft` / `quota_<token>_exceeded`
    /// (T004 §2). Used by T007 to emit provenance warnings and HTTP 429
    /// `quota_<token>_exceeded` rejections.
    #[allow(dead_code)]
    pub fn token(self) -> &'static str {
        match self {
            QuotaCounter::Artifact => "artifact",
            QuotaCounter::Version => "version",
            QuotaCounter::Contribution => "contribution",
            QuotaCounter::OpenComment => "open_comment",
            QuotaCounter::Link => "link",
            QuotaCounter::Chunk => "chunk",
            QuotaCounter::RunningWorkflow => "running_workflow",
            QuotaCounter::WriteRpm => "write_rpm",
        }
    }
}

/// Typed errors surfaced by the operations envelope. T006 returns these from
/// validation entry points; T007 maps them to HTTP responses per T004.
///
/// Soft quota warnings are *not* errors — they ride alongside successful
/// writes inside `QuotaWarning`. This split is load-bearing: a hard reject
/// MUST NOT write a row, while a soft warning MUST still write the row and
/// attach the warning to `provenance.warnings[]` (T003 §2.2).
#[derive(Debug)]
#[allow(dead_code)]
pub enum OperationsError {
    /// Per-write size limit exceeded — T007 maps to `HTTP 413
    /// <kind.token()>_too_large` per T004 §1.
    SizeLimit {
        kind: SizeLimitKind,
        limit: usize,
        actual: usize,
    },
    /// Hard per-project quota exceeded — T007 maps to `HTTP 429
    /// quota_<counter.token()>_exceeded` per T004 §2. No row written.
    QuotaHardReject {
        counter: QuotaCounter,
        limit: u64,
        current: u64,
    },
    /// Misconfigured environment value — failed to parse or non-positive.
    /// Surfaced at process start so SRE catches it before traffic lands.
    InvalidEnvValue { key: &'static str, detail: String },
    /// Soft warning threshold inverted with hard limit (config error).
    InvalidQuotaThresholds {
        counter: QuotaCounter,
        soft: u64,
        hard: u64,
    },
}

impl std::fmt::Display for OperationsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OperationsError::SizeLimit {
                kind,
                limit,
                actual,
            } => write!(
                f,
                "size limit exceeded for {kind:?}: {actual} > {limit} bytes"
            ),
            OperationsError::QuotaHardReject {
                counter,
                limit,
                current,
            } => write!(
                f,
                "hard quota exceeded for {counter:?}: {current} >= {limit}"
            ),
            OperationsError::InvalidEnvValue { key, detail } => {
                write!(f, "invalid value for env key {key}: {detail}")
            }
            OperationsError::InvalidQuotaThresholds {
                counter,
                soft,
                hard,
            } => write!(
                f,
                "soft threshold for {counter:?} ({soft}) must be <= hard limit ({hard})"
            ),
        }
    }
}

impl std::error::Error for OperationsError {}

/// Soft-warning result for a quota counter (T004 §2). Returned alongside
/// successful writes; T007 places it in `provenance.warnings[]`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct QuotaWarning {
    pub counter: QuotaCounter,
    pub soft_threshold: u64,
    pub hard_limit: u64,
    pub current: u64,
}

impl QuotaWarning {
    /// Stable wire token per T004 §2: `quota_<counter>_soft`.
    #[allow(dead_code)]
    pub fn token(&self) -> String {
        format!("quota_{}_soft", self.counter.token())
    }
}

// ── Per-resource size limits ────────────────────────────────────────────────

/// Per-write size limits (T004 §1). Bytes for body-like resources, counts
/// for label-list / read_set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct SizeLimits {
    pub artifact_version_body_max_bytes: usize,
    pub artifact_version_source_body_max_bytes: usize,
    pub artifact_version_structured_payload_max_bytes: usize,
    pub contribution_body_max_bytes: usize,
    pub comment_body_max_bytes: usize,
    pub chunk_text_max_bytes: usize,
    pub artifact_labels_max_count: usize,
    pub artifact_label_max_bytes: usize,
    pub read_set_max_refs: usize,
}

impl SizeLimits {
    /// Production defaults from T004 §1 ("Default" column). Any change to
    /// this table MUST be mirrored in `docs/artifact-operations-rollout.md`.
    #[allow(dead_code)]
    pub const fn production_defaults() -> Self {
        Self {
            artifact_version_body_max_bytes: 1024 * 1024, // 1 MiB
            artifact_version_source_body_max_bytes: 4 * 1024 * 1024, // 4 MiB
            artifact_version_structured_payload_max_bytes: 512 * 1024, // 512 KiB
            contribution_body_max_bytes: 256 * 1024,      // 256 KiB
            comment_body_max_bytes: 32 * 1024,            // 32 KiB
            chunk_text_max_bytes: 8 * 1024,               // 8 KiB
            artifact_labels_max_count: 32,
            artifact_label_max_bytes: 64,
            read_set_max_refs: 256,
        }
    }

    /// Look up the configured limit for a given resource kind.
    #[allow(dead_code)]
    pub fn limit_for(&self, kind: SizeLimitKind) -> usize {
        match kind {
            SizeLimitKind::ArtifactVersionBody => self.artifact_version_body_max_bytes,
            SizeLimitKind::ArtifactVersionSourceBody => self.artifact_version_source_body_max_bytes,
            SizeLimitKind::ArtifactVersionStructuredPayload => {
                self.artifact_version_structured_payload_max_bytes
            }
            SizeLimitKind::ContributionBody => self.contribution_body_max_bytes,
            SizeLimitKind::CommentBody => self.comment_body_max_bytes,
            SizeLimitKind::ChunkText => self.chunk_text_max_bytes,
            SizeLimitKind::ArtifactLabelsCount => self.artifact_labels_max_count,
            SizeLimitKind::ArtifactLabelBytes => self.artifact_label_max_bytes,
            SizeLimitKind::ReadSetRefs => self.read_set_max_refs,
        }
    }

    /// Enforce a size/count check. Returns `Err(OperationsError::SizeLimit)`
    /// when `actual > limit`. The caller decides what `actual` means: byte
    /// length for body-like resources, item count for labels / read_set.
    #[allow(dead_code)]
    pub fn check(
        &self,
        kind: SizeLimitKind,
        actual: usize,
    ) -> std::result::Result<(), OperationsError> {
        let limit = self.limit_for(kind);
        if actual > limit {
            Err(OperationsError::SizeLimit {
                kind,
                limit,
                actual,
            })
        } else {
            Ok(())
        }
    }
}

// ── Per-project quotas ──────────────────────────────────────────────────────

/// Soft + hard threshold pair for one quota counter (T004 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct QuotaPair {
    pub soft: u64,
    pub hard: u64,
}

/// Full quota envelope for one project's mutation flow (T004 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct QuotaThresholds {
    pub artifact: QuotaPair,
    pub version: QuotaPair,
    pub contribution: QuotaPair,
    pub open_comment: QuotaPair,
    pub link: QuotaPair,
    pub chunk: QuotaPair,
    pub running_workflow: QuotaPair,
    pub write_rpm: QuotaPair,
}

impl QuotaThresholds {
    /// Production defaults from T004 §2.
    #[allow(dead_code)]
    pub const fn production_defaults() -> Self {
        Self {
            artifact: QuotaPair {
                soft: 5_000,
                hard: 10_000,
            },
            version: QuotaPair {
                soft: 50_000,
                hard: 100_000,
            },
            contribution: QuotaPair {
                soft: 250_000,
                hard: 500_000,
            },
            open_comment: QuotaPair {
                soft: 5_000,
                hard: 10_000,
            },
            link: QuotaPair {
                soft: 200_000,
                hard: 400_000,
            },
            chunk: QuotaPair {
                soft: 100_000,
                hard: 200_000,
            },
            running_workflow: QuotaPair {
                soft: 50,
                hard: 100,
            },
            write_rpm: QuotaPair {
                soft: 600,
                hard: 1_200,
            },
        }
    }

    /// Look up the configured pair for a counter.
    #[allow(dead_code)]
    pub fn pair_for(&self, counter: QuotaCounter) -> QuotaPair {
        match counter {
            QuotaCounter::Artifact => self.artifact,
            QuotaCounter::Version => self.version,
            QuotaCounter::Contribution => self.contribution,
            QuotaCounter::OpenComment => self.open_comment,
            QuotaCounter::Link => self.link,
            QuotaCounter::Chunk => self.chunk,
            QuotaCounter::RunningWorkflow => self.running_workflow,
            QuotaCounter::WriteRpm => self.write_rpm,
        }
    }

    /// Evaluate a counter's current value against its thresholds.
    /// Hard limit takes precedence over soft warning. Returns:
    /// * `Err(QuotaHardReject)` when `current >= hard` — caller MUST NOT
    ///   write the row (T004 §2 "hard limit ... no row written").
    /// * `Ok(Some(QuotaWarning))` when `current >= soft` (and `< hard`) —
    ///   caller writes the row AND attaches the warning to provenance.
    /// * `Ok(None)` when below soft threshold.
    #[allow(dead_code)]
    pub fn evaluate(
        &self,
        counter: QuotaCounter,
        current: u64,
    ) -> std::result::Result<Option<QuotaWarning>, OperationsError> {
        let pair = self.pair_for(counter);
        if current >= pair.hard {
            return Err(OperationsError::QuotaHardReject {
                counter,
                limit: pair.hard,
                current,
            });
        }
        if current >= pair.soft {
            Ok(Some(QuotaWarning {
                counter,
                soft_threshold: pair.soft,
                hard_limit: pair.hard,
                current,
            }))
        } else {
            Ok(None)
        }
    }
}

// ── Retention & purge ───────────────────────────────────────────────────────

/// Retention configuration from T004 §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct RetentionPolicy {
    /// After this many days an `archived` artifact's body and structured
    /// payload bytes MAY be purged. Audit metadata is preserved.
    pub archive_body_ttl_days: u32,
    /// `started`-state workflow runs older than this are flipped to
    /// `failed` during restore verification (T004 §4.3 step 4).
    pub workflow_run_stuck_ttl_hours: u32,
    /// Label that suppresses body purge regardless of state (T004 §3).
    pub retain_permanent_label: &'static str,
}

impl RetentionPolicy {
    #[allow(dead_code)]
    pub const fn production_defaults() -> Self {
        Self {
            archive_body_ttl_days: 180,
            workflow_run_stuck_ttl_hours: 24,
            retain_permanent_label: "retain:permanent",
        }
    }
}

// ── Restore-check configuration ─────────────────────────────────────────────

/// Restore verification knobs from T004 §4.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct RestoreCheckConfig {
    /// Sample size for idempotency-mapping spot check (T004 §4.3 step 5).
    pub idempotency_sample_size: u32,
}

impl RestoreCheckConfig {
    #[allow(dead_code)]
    pub const fn production_defaults() -> Self {
        Self {
            idempotency_sample_size: 100,
        }
    }
}

// ── Envelope ────────────────────────────────────────────────────────────────

/// Top-level typed envelope read once at process start. T007 stores this in
/// `AppState` so route handlers consume typed values rather than calling
/// `std::env::var` per request. T008 fixtures construct one directly with
/// shrunken values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ArtifactOperationsEnvelope {
    pub sizes: SizeLimits,
    pub quotas: QuotaThresholds,
    pub retention: RetentionPolicy,
    pub restore: RestoreCheckConfig,
}

impl Default for ArtifactOperationsEnvelope {
    fn default() -> Self {
        Self::production_defaults()
    }
}

impl ArtifactOperationsEnvelope {
    /// Production defaults from T004. Used when no env override is set.
    #[allow(dead_code)]
    pub const fn production_defaults() -> Self {
        Self {
            sizes: SizeLimits::production_defaults(),
            quotas: QuotaThresholds::production_defaults(),
            retention: RetentionPolicy::production_defaults(),
            restore: RestoreCheckConfig::production_defaults(),
        }
    }

    /// Load the envelope from process environment variables using the T004
    /// env-key surface. Missing keys fall back to production defaults; the
    /// same key surface is shared with T008 fixtures (which set shrunken
    /// values per T004 §1 "Named fixture values for T008").
    ///
    /// Returns `Err(OperationsError::InvalidEnvValue)` if any key is
    /// present but malformed (non-numeric, zero where positive required,
    /// or soft >= hard). Process startup should propagate this so SRE
    /// catches misconfiguration before traffic lands.
    #[allow(dead_code)]
    pub fn from_env() -> std::result::Result<Self, OperationsError> {
        Self::from_env_with(|k| std::env::var(k).ok())
    }

    /// Test seam for env loading. Production callers should use `from_env`.
    #[allow(dead_code)]
    pub fn from_env_with<F: Fn(&str) -> Option<String>>(
        get: F,
    ) -> std::result::Result<Self, OperationsError> {
        let mut envelope = Self::production_defaults();

        // ── Sizes (T004 §1) ──
        macro_rules! size {
            ($key:literal, $field:ident) => {
                if let Some(raw) = get($key) {
                    envelope.sizes.$field = parse_positive_usize($key, &raw)?;
                }
            };
        }
        size!(
            "ARTIFACT_VERSION_BODY_MAX_BYTES",
            artifact_version_body_max_bytes
        );
        size!(
            "ARTIFACT_VERSION_SOURCE_BODY_MAX_BYTES",
            artifact_version_source_body_max_bytes
        );
        size!(
            "ARTIFACT_VERSION_STRUCTURED_PAYLOAD_MAX_BYTES",
            artifact_version_structured_payload_max_bytes
        );
        size!("CONTRIBUTION_BODY_MAX_BYTES", contribution_body_max_bytes);
        size!("COMMENT_BODY_MAX_BYTES", comment_body_max_bytes);
        size!("CHUNK_TEXT_MAX_BYTES", chunk_text_max_bytes);
        size!("ARTIFACT_LABELS_MAX_COUNT", artifact_labels_max_count);
        size!("ARTIFACT_LABEL_MAX_BYTES", artifact_label_max_bytes);
        size!("READ_SET_MAX_REFS", read_set_max_refs);

        // ── Quotas (T004 §2) ──
        macro_rules! pair {
            ($soft_key:literal, $hard_key:literal, $field:ident, $counter:expr) => {{
                let soft = match get($soft_key) {
                    Some(raw) => parse_positive_u64($soft_key, &raw)?,
                    None => envelope.quotas.$field.soft,
                };
                let hard = match get($hard_key) {
                    Some(raw) => parse_positive_u64($hard_key, &raw)?,
                    None => envelope.quotas.$field.hard,
                };
                if soft > hard {
                    return Err(OperationsError::InvalidQuotaThresholds {
                        counter: $counter,
                        soft,
                        hard,
                    });
                }
                envelope.quotas.$field = QuotaPair { soft, hard };
            }};
        }
        pair!(
            "PROJECT_ARTIFACT_SOFT",
            "PROJECT_ARTIFACT_HARD",
            artifact,
            QuotaCounter::Artifact
        );
        pair!(
            "PROJECT_VERSION_SOFT",
            "PROJECT_VERSION_HARD",
            version,
            QuotaCounter::Version
        );
        pair!(
            "PROJECT_CONTRIBUTION_SOFT",
            "PROJECT_CONTRIBUTION_HARD",
            contribution,
            QuotaCounter::Contribution
        );
        pair!(
            "PROJECT_OPEN_COMMENT_SOFT",
            "PROJECT_OPEN_COMMENT_HARD",
            open_comment,
            QuotaCounter::OpenComment
        );
        pair!(
            "PROJECT_LINK_SOFT",
            "PROJECT_LINK_HARD",
            link,
            QuotaCounter::Link
        );
        pair!(
            "PROJECT_CHUNK_SOFT",
            "PROJECT_CHUNK_HARD",
            chunk,
            QuotaCounter::Chunk
        );
        pair!(
            "PROJECT_RUNNING_WORKFLOW_SOFT",
            "PROJECT_RUNNING_WORKFLOW_HARD",
            running_workflow,
            QuotaCounter::RunningWorkflow
        );
        pair!(
            "PROJECT_WRITE_RPM_SOFT",
            "PROJECT_WRITE_RPM_HARD",
            write_rpm,
            QuotaCounter::WriteRpm
        );

        // ── Retention (T004 §3) ──
        if let Some(raw) = get("ARCHIVE_BODY_TTL_DAYS") {
            envelope.retention.archive_body_ttl_days =
                parse_positive_u32("ARCHIVE_BODY_TTL_DAYS", &raw)?;
        }
        if let Some(raw) = get("WORKFLOW_RUN_STUCK_TTL_HOURS") {
            envelope.retention.workflow_run_stuck_ttl_hours =
                parse_positive_u32("WORKFLOW_RUN_STUCK_TTL_HOURS", &raw)?;
        }

        // ── Restore (T004 §4.3) ──
        if let Some(raw) = get("ARTIFACT_RESTORE_IDEMPOTENCY_SAMPLE_SIZE") {
            envelope.restore.idempotency_sample_size =
                parse_positive_u32("ARTIFACT_RESTORE_IDEMPOTENCY_SAMPLE_SIZE", &raw)?;
        }

        Ok(envelope)
    }
}

#[cfg(test)]
pub(crate) fn t008_shrunken_artifact_operations_fixture_env(
) -> std::collections::HashMap<&'static str, &'static str> {
    [
        ("ARTIFACT_VERSION_BODY_MAX_BYTES", "4096"),
        ("ARTIFACT_VERSION_SOURCE_BODY_MAX_BYTES", "8192"),
        ("ARTIFACT_VERSION_STRUCTURED_PAYLOAD_MAX_BYTES", "2048"),
        ("CONTRIBUTION_BODY_MAX_BYTES", "2048"),
        ("COMMENT_BODY_MAX_BYTES", "512"),
        ("CHUNK_TEXT_MAX_BYTES", "512"),
        ("ARTIFACT_LABELS_MAX_COUNT", "4"),
        ("ARTIFACT_LABEL_MAX_BYTES", "32"),
        ("READ_SET_MAX_REFS", "8"),
        ("PROJECT_ARTIFACT_SOFT", "2"),
        ("PROJECT_ARTIFACT_HARD", "3"),
        ("PROJECT_VERSION_SOFT", "3"),
        ("PROJECT_VERSION_HARD", "4"),
        ("PROJECT_CONTRIBUTION_SOFT", "3"),
        ("PROJECT_CONTRIBUTION_HARD", "4"),
        ("PROJECT_OPEN_COMMENT_SOFT", "2"),
        ("PROJECT_OPEN_COMMENT_HARD", "3"),
        ("PROJECT_LINK_SOFT", "3"),
        ("PROJECT_LINK_HARD", "4"),
        ("PROJECT_CHUNK_SOFT", "3"),
        ("PROJECT_CHUNK_HARD", "4"),
        ("PROJECT_RUNNING_WORKFLOW_SOFT", "1"),
        ("PROJECT_RUNNING_WORKFLOW_HARD", "2"),
        ("PROJECT_WRITE_RPM_SOFT", "3"),
        ("PROJECT_WRITE_RPM_HARD", "4"),
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
pub(crate) fn t008_shrunken_artifact_operations_fixture() -> ArtifactOperationsEnvelope {
    let fixture = t008_shrunken_artifact_operations_fixture_env();
    ArtifactOperationsEnvelope::from_env_with(|k| fixture.get(k).map(|s| (*s).to_string()))
        .expect("T008 shrunken artifact operations fixture parses")
}

fn parse_positive_usize(
    key: &'static str,
    raw: &str,
) -> std::result::Result<usize, OperationsError> {
    parse_positive(key, raw)
}

fn parse_positive_u64(key: &'static str, raw: &str) -> std::result::Result<u64, OperationsError> {
    parse_positive(key, raw)
}

fn parse_positive_u32(key: &'static str, raw: &str) -> std::result::Result<u32, OperationsError> {
    parse_positive(key, raw)
}

fn parse_positive<T>(key: &'static str, raw: &str) -> std::result::Result<T, OperationsError>
where
    T: std::str::FromStr + PartialOrd + From<u8>,
    T::Err: std::fmt::Display,
{
    let parsed: T = raw
        .trim()
        .parse()
        .map_err(|e: T::Err| OperationsError::InvalidEnvValue {
            key,
            detail: e.to_string(),
        })?;
    if parsed <= T::from(0) {
        return Err(OperationsError::InvalidEnvValue {
            key,
            detail: "must be > 0".into(),
        });
    }
    Ok(parsed)
}

// ── Retention purge helpers ─────────────────────────────────────────────────

/// Outcome of `purge_archived_version_body`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PurgeOutcome {
    /// Body bytes were nulled and `body_purged_at` was stamped.
    Purged,
    /// Version did not meet criteria (not archived, already purged, or
    /// owning artifact is `retain:permanent`). Caller may log; no error.
    Skipped(PurgeSkipReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PurgeSkipReason {
    NotArchived,
    AlreadyPurged,
    RetainPermanent,
    AgeBelowTtl,
    Missing,
}

/// Purge the body and structured payload of one archived artifact version
/// when it meets T004 §3 criteria. Preserves all immutable metadata
/// (`artifact_id`, `artifact_version_id`, `version_state`, `version_label`,
/// `parent_version_id`, `source_format`, `created_by_actor_id`,
/// `created_via_workflow_run_id`) plus every link, comment, workflow run,
/// and idempotency mapping anchored to this version.
///
/// Skips when:
/// * artifact's `lifecycle_state != 'archived'`
/// * version `body_purged_at IS NOT NULL` (idempotent)
/// * artifact carries `retain:permanent` label
/// * version age is below `archive_body_ttl_days`
#[allow(dead_code)]
pub fn purge_archived_version_body(
    conn: &Connection,
    artifact_version_id: &str,
    retention: &RetentionPolicy,
    now_ms_value: i64,
) -> Result<PurgeOutcome> {
    let row: Option<(String, Option<i64>, i64)> = conn
        .query_row(
            "SELECT artifact_id, body_purged_at, created_at
             FROM artifact_versions WHERE artifact_version_id = ?1",
            params![artifact_version_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (artifact_id, body_purged_at, created_at) = match row {
        Some(t) => t,
        None => return Ok(PurgeOutcome::Skipped(PurgeSkipReason::Missing)),
    };
    if body_purged_at.is_some() {
        return Ok(PurgeOutcome::Skipped(PurgeSkipReason::AlreadyPurged));
    }
    let (lifecycle, labels_json): (String, Option<String>) = conn.query_row(
        "SELECT lifecycle_state, labels FROM artifacts WHERE artifact_id = ?1",
        params![artifact_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if lifecycle != "archived" {
        return Ok(PurgeOutcome::Skipped(PurgeSkipReason::NotArchived));
    }
    if has_label(&labels_json, retention.retain_permanent_label) {
        return Ok(PurgeOutcome::Skipped(PurgeSkipReason::RetainPermanent));
    }
    let ttl_ms = (retention.archive_body_ttl_days as i64) * 24 * 60 * 60 * 1000;
    if now_ms_value.saturating_sub(created_at) < ttl_ms {
        return Ok(PurgeOutcome::Skipped(PurgeSkipReason::AgeBelowTtl));
    }
    // The structured_payload immutability trigger fires unless
    // `OLD.body_purged_at IS NOT NULL`. Sequence the update so the body
    // purge stamps `body_purged_at` first (the body trigger explicitly
    // allows `NEW.body IS NULL AND NEW.body_purged_at IS NOT NULL`), then
    // a second statement clears `structured_payload` against the now-
    // purged row. Both statements run on the same connection — callers
    // SHOULD wrap this in a transaction when batching.
    conn.execute(
        "UPDATE artifact_versions
         SET body = NULL, body_purged_at = ?2
         WHERE artifact_version_id = ?1",
        params![artifact_version_id, now_ms_value],
    )?;
    conn.execute(
        "UPDATE artifact_versions
         SET structured_payload = NULL
         WHERE artifact_version_id = ?1 AND structured_payload IS NOT NULL",
        params![artifact_version_id],
    )?;
    Ok(PurgeOutcome::Purged)
}

/// Returns true when the JSON-encoded label list contains `needle`.
fn has_label(labels_json: &Option<String>, needle: &str) -> bool {
    match labels_json {
        Some(raw) => serde_json::from_str::<Vec<String>>(raw)
            .ok()
            .map(|labels| labels.iter().any(|l| l == needle))
            .unwrap_or(false),
        None => false,
    }
}

/// Aggregate purge counters returned by `purge_archived_bodies_due`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub struct PurgeRunSummary {
    pub purged: usize,
    pub skipped_retain_permanent: usize,
    pub skipped_already_purged: usize,
}

/// Scan all archived artifact versions older than the retention TTL and
/// purge bodies. Intended for the nightly SRE job (T004 §3
/// `gateway artifacts purge`). Idempotent — repeated runs are no-ops once
/// every eligible version has been purged.
#[allow(dead_code)]
pub fn purge_archived_bodies_due(
    conn: &Connection,
    retention: &RetentionPolicy,
    now_ms_value: i64,
) -> Result<PurgeRunSummary> {
    let ttl_ms = (retention.archive_body_ttl_days as i64) * 24 * 60 * 60 * 1000;
    let cutoff = now_ms_value.saturating_sub(ttl_ms);
    let mut stmt = conn.prepare(
        "SELECT v.artifact_version_id
         FROM artifact_versions v
         JOIN artifacts a ON a.artifact_id = v.artifact_id
         WHERE a.lifecycle_state = 'archived'
           AND v.body_purged_at IS NULL
           AND v.created_at <= ?1",
    )?;
    let ids: Vec<String> = stmt
        .query_map(params![cutoff], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);

    let mut summary = PurgeRunSummary::default();
    for id in ids {
        match purge_archived_version_body(conn, &id, retention, now_ms_value)? {
            PurgeOutcome::Purged => summary.purged += 1,
            PurgeOutcome::Skipped(PurgeSkipReason::RetainPermanent) => {
                summary.skipped_retain_permanent += 1
            }
            PurgeOutcome::Skipped(PurgeSkipReason::AlreadyPurged) => {
                summary.skipped_already_purged += 1
            }
            // Other skip reasons should not occur for rows selected above;
            // if they do, treat as a no-op without surfacing a hard error.
            PurgeOutcome::Skipped(_) => {}
        }
    }
    Ok(summary)
}

// ── Restore-check helpers (T004 §4.3) ───────────────────────────────────────

/// Finding emitted by a restore-check helper. Reporting only — never auto-
/// repaired (T004 §4.3 "Mismatches are logged ... rather than silently
/// corrected"). T014 surfaces these on the rollout-gate dashboard.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[allow(dead_code)]
pub struct RestoreFinding {
    /// Stable warning tag — mirrors the labels T004 §4.3 names
    /// (`restore:pointer_mismatch`, `restore:dangling`,
    /// `restore:stuck_workflow_run`, `restore:idempotency_mismatch`,
    /// `restore:chunk_count_mismatch`).
    pub tag: String,
    /// Logical kind of the offending row (e.g. `artifact`, `link`,
    /// `workflow_run`, `idempotency_mapping`, `chunk`).
    pub entity_kind: String,
    /// Primary key of the offending row.
    pub entity_id: String,
    /// Human-readable detail line for the operator log.
    pub detail: String,
}

/// Aggregate report from a restore-check pass. Each list corresponds to one
/// step of T004 §4.3. An empty report means all checks passed.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
#[allow(dead_code)]
pub struct RestoreCheckReport {
    pub artifact_pointer_mismatches: Vec<RestoreFinding>,
    pub dangling_links: Vec<RestoreFinding>,
    pub workflow_run_inconsistencies: Vec<RestoreFinding>,
    pub idempotency_mapping_issues: Vec<RestoreFinding>,
    pub chunk_regeneration_issues: Vec<RestoreFinding>,
}

impl RestoreCheckReport {
    /// `true` when every restore-check step found zero issues.
    #[allow(dead_code)]
    pub fn is_clean(&self) -> bool {
        self.artifact_pointer_mismatches.is_empty()
            && self.dangling_links.is_empty()
            && self.workflow_run_inconsistencies.is_empty()
            && self.idempotency_mapping_issues.is_empty()
            && self.chunk_regeneration_issues.is_empty()
    }

    /// Total finding count across every category.
    #[allow(dead_code)]
    pub fn total_findings(&self) -> usize {
        self.artifact_pointer_mismatches.len()
            + self.dangling_links.len()
            + self.workflow_run_inconsistencies.len()
            + self.idempotency_mapping_issues.len()
            + self.chunk_regeneration_issues.len()
    }
}

struct RestoreFindingCollector<'a> {
    sql: &'a str,
}

impl<'a> RestoreFindingCollector<'a> {
    fn new(sql: &'a str) -> Self {
        Self { sql }
    }

    fn collect<P, R, RowMapper, FindingMapper>(
        self,
        conn: &Connection,
        params: P,
        mut row_mapper: RowMapper,
        mut finding_mapper: FindingMapper,
    ) -> Result<Vec<RestoreFinding>>
    where
        P: rusqlite::Params,
        RowMapper: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<R>,
        FindingMapper: FnMut(R) -> Result<Vec<RestoreFinding>>,
    {
        let mut stmt = conn.prepare(self.sql)?;
        let rows: Vec<R> = stmt
            .query_map(params, |r| row_mapper(r))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let mut findings = Vec::new();
        for row in rows {
            findings.extend(finding_mapper(row)?);
        }
        Ok(findings)
    }
}

/// Run T004 §4.3 step 2: artifact pointer consistency. Every artifact with
/// `current_version_id` or `accepted_version_id` set MUST point at an
/// `artifact_version` row that belongs to the same `artifact_id`.
#[allow(dead_code)]
pub fn restore_check_artifact_pointers(conn: &Connection) -> Result<Vec<RestoreFinding>> {
    RestoreFindingCollector::new(
        "SELECT a.artifact_id, a.current_version_id, a.accepted_version_id
         FROM artifacts a
         WHERE a.current_version_id IS NOT NULL OR a.accepted_version_id IS NOT NULL",
    )
    .collect(
        conn,
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        |(artifact_id, current, accepted): (String, Option<String>, Option<String>)| {
            let mut findings = Vec::new();
            for (label, ptr) in [
                ("current_version_id", current),
                ("accepted_version_id", accepted),
            ] {
                if let Some(version_id) = ptr {
                    let owner: Option<String> = conn
                        .query_row(
                            "SELECT artifact_id FROM artifact_versions
                             WHERE artifact_version_id = ?1",
                            params![version_id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    match owner {
                        None => findings.push(RestoreFinding {
                            tag: "restore:pointer_mismatch".into(),
                            entity_kind: "artifact".into(),
                            entity_id: artifact_id.clone(),
                            detail: format!("{label}={version_id} references missing version"),
                        }),
                        Some(owner_id) if owner_id != artifact_id => {
                            findings.push(RestoreFinding {
                                tag: "restore:pointer_mismatch".into(),
                                entity_kind: "artifact".into(),
                                entity_id: artifact_id.clone(),
                                detail: format!(
                                    "{label}={version_id} belongs to artifact {owner_id}"
                                ),
                            })
                        }
                        Some(_) => {}
                    }
                }
            }
            Ok(findings)
        },
    )
}

/// Run T004 §4.3 step 3: audit-path link integrity. Reports links whose
/// declared source/target version refs do not resolve. Does not opine on
/// whether the link type *requires* a version ref — that's the audit-path
/// registry's job (T003 §7); restore only flags references that look set
/// but do not point at extant rows.
#[allow(dead_code)]
pub fn restore_check_audit_links(conn: &Connection) -> Result<Vec<RestoreFinding>> {
    RestoreFindingCollector::new(
        "SELECT link_id, link_type, source_version_id, target_version_id
         FROM artifact_links
         WHERE source_version_id IS NOT NULL OR target_version_id IS NOT NULL",
    )
    .collect(
        conn,
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        |(link_id, link_type, src, tgt): (String, String, Option<String>, Option<String>)| {
            let mut findings = Vec::new();
            for (label, ptr) in [("source_version_id", src), ("target_version_id", tgt)] {
                if let Some(version_id) = ptr {
                    let exists: Option<String> = conn
                        .query_row(
                            "SELECT artifact_version_id FROM artifact_versions
                             WHERE artifact_version_id = ?1",
                            params![version_id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if exists.is_none() {
                        findings.push(RestoreFinding {
                            tag: "restore:dangling".into(),
                            entity_kind: "link".into(),
                            entity_id: link_id.clone(),
                            detail: format!("link_type={link_type} {label}={version_id} missing"),
                        });
                    }
                }
            }
            Ok(findings)
        },
    )
}

/// Run T004 §4.3 step 4: workflow_run consistency. Reports:
/// * `succeeded` runs whose `generated_*_ids` arrays reference missing rows.
/// * `started`-state runs older than `workflow_run_stuck_ttl_hours` (these
///   are *reported only*; the actual flip to `failed` is owned by the SRE
///   migration tool to keep this helper read-only).
#[allow(dead_code)]
pub fn restore_check_workflow_runs(
    conn: &Connection,
    retention: &RetentionPolicy,
    now_ms_value: i64,
) -> Result<Vec<RestoreFinding>> {
    let mut findings = Vec::new();
    let stuck_cutoff_ms =
        now_ms_value.saturating_sub((retention.workflow_run_stuck_ttl_hours as i64) * 3600 * 1000);

    findings.extend(
        RestoreFindingCollector::new(
            "SELECT workflow_run_id, started_at FROM workflow_runs
         WHERE state = 'started' AND started_at <= ?1",
        )
        .collect(
            conn,
            params![stuck_cutoff_ms],
            |r| Ok((r.get(0)?, r.get(1)?)),
            |(run_id, started_at): (String, i64)| {
                Ok(vec![RestoreFinding {
                    tag: "restore:stuck_workflow_run".into(),
                    entity_kind: "workflow_run".into(),
                    entity_id: run_id,
                    detail: format!(
                        "state=started started_at={started_at} older than ttl_hours={}",
                        retention.workflow_run_stuck_ttl_hours
                    ),
                }])
            },
        )?,
    );

    findings.extend(
        RestoreFindingCollector::new(
            "SELECT workflow_run_id, generated_version_ids, generated_contribution_ids,
                generated_link_ids, generated_chunk_ids
         FROM workflow_runs WHERE state = 'succeeded'",
        )
        .collect(
            conn,
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            |(run_id, ver, contrib, link, chunk): GeneratedIdsRow| {
                let mut findings = Vec::new();
                let checks: [(&str, &str, &str); 4] = [
                    ("artifact_versions", "artifact_version_id", "version"),
                    ("artifact_contributions", "contribution_id", "contribution"),
                    ("artifact_links", "link_id", "link"),
                    ("artifact_chunks", "chunk_id", "chunk"),
                ];
                for ((table, pk, label), raw) in checks.iter().zip([ver, contrib, link, chunk]) {
                    let ids: Vec<String> = match raw {
                        Some(s) => serde_json::from_str(&s).unwrap_or_default(),
                        None => Vec::new(),
                    };
                    for id in ids {
                        let sql = format!("SELECT {pk} FROM {table} WHERE {pk} = ?1");
                        let exists: Option<String> =
                            conn.query_row(&sql, params![id], |r| r.get(0)).optional()?;
                        if exists.is_none() {
                            findings.push(RestoreFinding {
                                tag: "restore:dangling".into(),
                                entity_kind: "workflow_run".into(),
                                entity_id: run_id.clone(),
                                detail: format!("generated {label} {id} missing"),
                            });
                        }
                    }
                }
                Ok(findings)
            },
        )?,
    );
    Ok(findings)
}

type GeneratedIdsRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Run T004 §4.3 step 5: idempotency mapping spot-check. Samples up to
/// `restore.idempotency_sample_size` `(workflow_run_id, idempotency_key)`
/// pairs across versions/contributions/links/comments/chunks and reports
/// any pair whose declared `workflow_run_id` does not resolve to an extant
/// `workflow_runs` row. (Full payload equivalence is the caller's job —
/// this helper guarantees referential integrity which is what survives a
/// restore.)
#[allow(dead_code)]
pub fn restore_check_idempotency_mappings(
    conn: &Connection,
    restore: &RestoreCheckConfig,
) -> Result<Vec<RestoreFinding>> {
    let mut findings = Vec::new();
    let sample = restore.idempotency_sample_size as i64;
    let tables: [(&str, &str, &str, &str); 5] = [
        (
            "artifact_versions",
            "artifact_version_id",
            "created_via_workflow_run_id",
            "version",
        ),
        (
            "artifact_contributions",
            "contribution_id",
            "workflow_run_id",
            "contribution",
        ),
        (
            "artifact_links",
            "link_id",
            "created_via_workflow_run_id",
            "link",
        ),
        (
            "artifact_comments",
            "comment_id",
            "resolved_by_workflow_run_id",
            "comment",
        ),
        (
            "artifact_chunks",
            "chunk_id",
            "artifact_version_id",
            "chunk",
        ),
    ];
    for (table, pk, run_col, label) in tables {
        // For chunks the "run" association is indirect (via the version
        // that emitted it); skip the join here — restore_check_chunk_*
        // covers chunk freshness/regeneration. We only verify the four
        // direct mappings.
        if table == "artifact_chunks" {
            continue;
        }
        let sql = format!(
            "SELECT {pk}, {run_col}, idempotency_key FROM {table}
             WHERE {run_col} IS NOT NULL AND idempotency_key IS NOT NULL
             LIMIT ?1"
        );
        findings.extend(RestoreFindingCollector::new(&sql).collect(
            conn,
            params![sample],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            |(entity_id, run_id, key): (String, String, String)| {
                let exists: Option<String> = conn
                    .query_row(
                        "SELECT workflow_run_id FROM workflow_runs WHERE workflow_run_id = ?1",
                        params![run_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if exists.is_none() {
                    Ok(vec![RestoreFinding {
                        tag: "restore:idempotency_mismatch".into(),
                        entity_kind: label.into(),
                        entity_id,
                        detail: format!(
                            "idempotency_key={key} references missing workflow_run={run_id}"
                        ),
                    }])
                } else {
                    Ok(Vec::new())
                }
            },
        )?);
    }
    Ok(findings)
}

/// Run T004 §4.3 step 6: chunk regeneration validation. For every artifact
/// whose `accepted_version_id` is set, report any of:
/// * Zero chunks anchored on the accepted version (chunks excluded from
///   backup and not yet regenerated).
/// * Chunk's owning artifact mismatch (anchored chunk row claims a
///   different `artifact_id` than its declared owner — schema-level FKs
///   prevent this, but the helper still reports if it appears).
///
/// `manifest.chunk_count` validation (T004 §4.3 step 6) requires the T011
/// documentation workflow's structured_payload schema, which is not in
/// scope for T016; that check ships with T011/T014 and uses the same
/// finding shape.
#[allow(dead_code)]
pub fn restore_check_chunks(conn: &Connection) -> Result<Vec<RestoreFinding>> {
    RestoreFindingCollector::new(
        "SELECT artifact_id, accepted_version_id FROM artifacts
         WHERE accepted_version_id IS NOT NULL",
    )
    .collect(
        conn,
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
        |(artifact_id, accepted): (String, String)| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM artifact_chunks
                 WHERE artifact_version_id = ?1 AND superseded_by_chunk_id IS NULL",
                params![accepted],
                |r| r.get(0),
            )?;
            if count == 0 {
                Ok(vec![RestoreFinding {
                    tag: "restore:chunk_count_mismatch".into(),
                    entity_kind: "artifact".into(),
                    entity_id: artifact_id,
                    detail: format!(
                        "accepted_version_id={accepted} has zero live chunks (regenerate via doc_publish)"
                    ),
                }])
            } else {
                Ok(Vec::new())
            }
        },
    )
}

/// Convenience driver that runs every restore-check step and returns the
/// aggregate report. T014 calls this from the rollout-gate runbook before
/// mutating traffic is re-enabled after a restore.
#[allow(dead_code)]
pub fn run_restore_check(
    conn: &Connection,
    envelope: &ArtifactOperationsEnvelope,
    now_ms_value: i64,
) -> Result<RestoreCheckReport> {
    Ok(RestoreCheckReport {
        artifact_pointer_mismatches: restore_check_artifact_pointers(conn)?,
        dangling_links: restore_check_audit_links(conn)?,
        workflow_run_inconsistencies: restore_check_workflow_runs(
            conn,
            &envelope.retention,
            now_ms_value,
        )?,
        idempotency_mapping_issues: restore_check_idempotency_mappings(conn, &envelope.restore)?,
        chunk_regeneration_issues: restore_check_chunks(conn)?,
    })
}

// ─── Minimal helpers used by the artifact substrate tests ────────────────────
//
// T006 will introduce the full repository surface. The helpers below are the
// minimum subset required to write meaningful db-level tests at T005 without
// duplicating the future repository code. They live in this module so they
// stay private to db.rs.
// Inserts stay explicit so T006 can keep each artifact table's SQL visible as
// column sets diverge while still sharing serialization helpers.

// Used by T005 + T006 CRUD.
fn serialize_json_or_null(value: &Option<&serde_json::Value>) -> Option<String> {
    value
        .as_ref()
        .map(|value| serde_json::to_string(*value).unwrap_or_else(|_| String::from("null")))
}

#[allow(dead_code)]
fn artifact_actor_upsert(
    conn: &Connection,
    identity: &ArtifactActorIdentity<'_>,
) -> Result<String> {
    let now = now_ms();
    let runtime = serialize_json_or_null(&identity.runtime_metadata);
    // First, try to find an existing actor by identity tuple.
    let existing: Option<String> = conn
        .query_row(
            "SELECT actor_id FROM artifact_actors
             WHERE actor_type = ?1
               AND COALESCE(agent_system, '') = COALESCE(?2, '')
               AND COALESCE(agent_id,     '') = COALESCE(?3, '')
               AND COALESCE(host,         '') = COALESCE(?4, '')",
            params![
                identity.actor_type,
                identity.agent_system,
                identity.agent_id,
                identity.host,
            ],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        conn.execute(
            "UPDATE artifact_actors SET display_name = ?1, agent_system_label = ?2,
                 runtime_metadata = ?3, updated_at = ?4 WHERE actor_id = ?5",
            params![
                identity.display_name,
                identity.agent_system_label,
                runtime,
                now,
                id,
            ],
        )?;
        return Ok(id);
    }
    let id = new_uuid();
    conn.execute(
        "INSERT INTO artifact_actors
         (actor_id, actor_type, agent_system, agent_system_label, agent_id, host,
          display_name, runtime_metadata, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
        params![
            id,
            identity.actor_type,
            identity.agent_system,
            identity.agent_system_label,
            identity.agent_id,
            identity.host,
            identity.display_name,
            runtime,
            now,
        ],
    )?;
    Ok(id)
}

#[allow(dead_code)]
fn artifact_insert(conn: &Connection, input: &ArtifactInsert<'_>) -> Result<String> {
    let id = new_uuid();
    let now = now_ms();
    conn.execute(
        "INSERT INTO artifacts
         (artifact_id, project_ident, kind, subkind, title, labels,
          lifecycle_state, review_state, implementation_state,
          current_version_id, accepted_version_id, created_by_actor_id,
          created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'draft', 'none',
                 CASE WHEN ?3 = 'spec' THEN 'not_started' ELSE 'not_applicable' END,
                 NULL, NULL, ?7, ?8, ?8)",
        params![
            id,
            input.project_ident,
            input.kind,
            input.subkind,
            input.title,
            serialize_string_array(input.labels),
            input.created_by_actor_id,
            now,
        ],
    )?;
    Ok(id)
}

#[allow(dead_code)]
fn artifact_version_insert(conn: &Connection, input: &ArtifactVersionInsert<'_>) -> Result<String> {
    let id = new_uuid();
    let payload = serialize_json_or_null(&input.structured_payload);
    conn.execute(
        "INSERT INTO artifact_versions
         (artifact_version_id, artifact_id, version_label, parent_version_id,
          body_format, body, structured_payload, source_format,
          created_by_actor_id, created_via_workflow_run_id, version_state,
          idempotency_key, body_purged_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, ?13)",
        params![
            id,
            input.artifact_id,
            input.version_label,
            input.parent_version_id,
            input.body_format,
            input.body,
            payload,
            input.source_format,
            input.created_by_actor_id,
            input.created_via_workflow_run_id,
            input.version_state,
            input.idempotency_key,
            now_ms(),
        ],
    )?;
    Ok(id)
}

#[allow(dead_code)]
fn artifact_set_pointers(
    conn: &Connection,
    artifact_id: &str,
    current: Option<&str>,
    accepted: Option<&str>,
) -> Result<()> {
    for (label, version_id) in [
        ("current_version_id", current),
        ("accepted_version_id", accepted),
    ] {
        if let Some(version_id) = version_id {
            match get_version_artifact_id(conn, version_id)? {
                Some(owner) if owner == artifact_id => {}
                Some(owner) => anyhow::bail!(
                    "{label}={version_id} belongs to artifact {owner}, not {artifact_id}"
                ),
                None => anyhow::bail!("{label}={version_id} references missing version"),
            }
        }
    }

    conn.execute(
        "UPDATE artifacts
         SET current_version_id = COALESCE(?2, current_version_id),
             accepted_version_id = COALESCE(?3, accepted_version_id),
             updated_at = ?4
         WHERE artifact_id = ?1",
        params![artifact_id, current, accepted, now_ms()],
    )?;
    Ok(())
}

#[allow(dead_code)]
fn artifact_link_insert(conn: &Connection, input: &ArtifactLinkInsert<'_>) -> Result<String> {
    let id = new_uuid();
    conn.execute(
        "INSERT INTO artifact_links
         (link_id, link_type, source_kind, source_id, source_version_id, source_child_address,
          target_kind, target_id, target_version_id, target_child_address,
          created_by_actor_id, created_via_workflow_run_id, idempotency_key,
          supersedes_link_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            id,
            input.link_type,
            input.source_kind,
            input.source_id,
            input.source_version_id,
            input.source_child_address,
            input.target_kind,
            input.target_id,
            input.target_version_id,
            input.target_child_address,
            input.created_by_actor_id,
            input.created_via_workflow_run_id,
            input.idempotency_key,
            input.supersedes_link_id,
            now_ms(),
        ],
    )?;
    Ok(id)
}

#[allow(dead_code)]
fn workflow_run_insert(conn: &Connection, input: &WorkflowRunInsert<'_>) -> Result<String> {
    let id = new_uuid();
    let participants = serialize_string_array(input.participant_actor_ids);
    let read_set = serialize_json_or_null(&input.read_set);
    conn.execute(
        "INSERT INTO workflow_runs
         (workflow_run_id, artifact_id, workflow_kind, phase, round_id,
          coordinator_actor_id, participant_actor_ids, source_artifact_version_id,
          read_set, idempotency_key, is_resumable, state,
          generated_contribution_ids, generated_version_ids, generated_task_ids,
          generated_link_ids, generated_chunk_ids, failure_reason,
          started_at, ended_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'started',
                 NULL, NULL, NULL, NULL, NULL, NULL, ?12, NULL)",
        params![
            id,
            input.artifact_id,
            input.workflow_kind,
            input.phase,
            input.round_id,
            input.coordinator_actor_id,
            participants,
            input.source_artifact_version_id,
            read_set,
            input.idempotency_key,
            input.is_resumable as i64,
            now_ms(),
        ],
    )?;
    Ok(id)
}

#[allow(dead_code)]
fn workflow_run_set_state(
    conn: &Connection,
    run_id: &str,
    new_state: &str,
    failure_reason: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE workflow_runs
         SET state = ?2, failure_reason = ?3, ended_at = CASE WHEN ?2 = 'started' THEN NULL ELSE ?4 END
         WHERE workflow_run_id = ?1",
        params![run_id, new_state, failure_reason, now_ms()],
    )?;
    Ok(())
}

#[allow(dead_code)]
fn artifact_chunk_insert(conn: &Connection, input: &ArtifactChunkInsert<'_>) -> Result<String> {
    let id = new_uuid();
    let metadata = serialize_json_or_null(&input.metadata);
    conn.execute(
        "INSERT INTO artifact_chunks
         (chunk_id, artifact_id, artifact_version_id, child_address, text,
          embedding_model, embedding_vector, app, label, kind, metadata_json,
          superseded_by_chunk_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, ?12)",
        params![
            id,
            input.artifact_id,
            input.artifact_version_id,
            input.child_address,
            input.text,
            input.embedding_model,
            input.embedding_vector,
            input.app,
            input.label,
            input.kind,
            metadata,
            now_ms(),
        ],
    )?;
    Ok(id)
}

#[allow(dead_code)]
fn artifact_chunk_mark_superseded(
    conn: &Connection,
    old_chunk_id: &str,
    new_chunk_id: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE artifact_chunks SET superseded_by_chunk_id = ?2 WHERE chunk_id = ?1",
        params![old_chunk_id, new_chunk_id],
    )?;
    Ok(())
}

#[allow(dead_code)]
fn artifact_comment_insert(conn: &Connection, input: &ArtifactCommentInsert<'_>) -> Result<String> {
    let id = new_uuid();
    let now = now_ms();
    conn.execute(
        "INSERT INTO artifact_comments
         (comment_id, artifact_id, target_kind, target_id, child_address,
          parent_comment_id, actor_id, body, state, resolved_by_actor_id,
          resolved_by_workflow_run_id, resolved_at, resolution_note,
          idempotency_key, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'open', NULL, NULL, NULL, NULL,
                 ?9, ?10, ?10)",
        params![
            id,
            input.artifact_id,
            input.target_kind,
            input.target_id,
            input.child_address,
            input.parent_comment_id,
            input.actor_id,
            input.body,
            input.idempotency_key,
            now,
        ],
    )?;
    Ok(id)
}

#[allow(dead_code)]
fn artifact_contribution_insert(
    conn: &Connection,
    input: &ArtifactContributionInsert<'_>,
) -> Result<String> {
    let id = new_uuid();
    let read_set = serialize_json_or_null(&input.read_set);
    conn.execute(
        "INSERT INTO artifact_contributions
         (contribution_id, artifact_id, target_kind, target_id, contribution_kind,
          phase, role, actor_id, workflow_run_id, read_set, body_format, body,
          idempotency_key, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id,
            input.artifact_id,
            input.target_kind,
            input.target_id,
            input.contribution_kind,
            input.phase,
            input.role,
            input.actor_id,
            input.workflow_run_id,
            read_set,
            input.body_format,
            input.body,
            input.idempotency_key,
            now_ms(),
        ],
    )?;
    Ok(id)
}

// ─── Artifact repository functions (T006) ───────────────────────────────────

const ARTIFACT_SUMMARY_SELECT_COLS: &str = "artifact_id, project_ident, kind, subkind, title, labels, lifecycle_state, review_state, implementation_state, current_version_id, accepted_version_id, created_by_actor_id, created_at, updated_at";
const ARTIFACT_VERSION_SELECT_COLS: &str = "artifact_version_id, artifact_id, version_label, parent_version_id, body_format, body, structured_payload, source_format, created_by_actor_id, created_via_workflow_run_id, version_state, idempotency_key, body_purged_at, created_at";
const ARTIFACT_CONTRIBUTION_SELECT_COLS: &str = "contribution_id, artifact_id, target_kind, target_id, contribution_kind, phase, role, actor_id, workflow_run_id, read_set, body_format, body, idempotency_key, created_at";
const ARTIFACT_COMMENT_SELECT_COLS: &str = "comment_id, artifact_id, target_kind, target_id, child_address, parent_comment_id, actor_id, body, state, resolved_by_actor_id, resolved_by_workflow_run_id, resolved_at, resolution_note, idempotency_key, created_at, updated_at";
const ARTIFACT_LINK_SELECT_COLS: &str = "link_id, link_type, source_kind, source_id, source_version_id, source_child_address, target_kind, target_id, target_version_id, target_child_address, created_by_actor_id, created_via_workflow_run_id, idempotency_key, supersedes_link_id, created_at";
const ARTIFACT_CHUNK_SELECT_COLS: &str = "chunk_id, artifact_id, artifact_version_id, child_address, text, embedding_model, embedding_vector, app, label, kind, metadata_json, superseded_by_chunk_id, created_at";
const WORKFLOW_RUN_SELECT_COLS: &str = "workflow_run_id, artifact_id, workflow_kind, phase, round_id, coordinator_actor_id, participant_actor_ids, source_artifact_version_id, read_set, idempotency_key, is_resumable, state, generated_contribution_ids, generated_version_ids, generated_task_ids, generated_link_ids, generated_chunk_ids, failure_reason, started_at, ended_at";

fn parse_json_value(
    raw: Option<String>,
    col: usize,
) -> rusqlite::Result<Option<serde_json::Value>> {
    raw.map(|value| {
        serde_json::from_str(&value).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, Box::new(e))
        })
    })
    .transpose()
}

fn row_to_artifact_actor(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactActor> {
    Ok(ArtifactActor {
        actor_id: row.get(0)?,
        actor_type: row.get(1)?,
        agent_system: row.get(2)?,
        agent_system_label: row.get(3)?,
        agent_id: row.get(4)?,
        host: row.get(5)?,
        display_name: row.get(6)?,
        runtime_metadata: parse_json_value(row.get(7)?, 7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn row_to_artifact_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactSummary> {
    Ok(ArtifactSummary {
        artifact_id: row.get(0)?,
        project_ident: row.get(1)?,
        kind: row.get(2)?,
        subkind: row.get(3)?,
        title: row.get(4)?,
        labels: parse_labels(row.get::<_, Option<String>>(5)?),
        lifecycle_state: row.get(6)?,
        review_state: row.get(7)?,
        implementation_state: row.get(8)?,
        current_version_id: row.get(9)?,
        accepted_version_id: row.get(10)?,
        created_by_actor_id: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

fn row_to_artifact_version(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactVersion> {
    Ok(ArtifactVersion {
        artifact_version_id: row.get(0)?,
        artifact_id: row.get(1)?,
        version_label: row.get(2)?,
        parent_version_id: row.get(3)?,
        body_format: row.get(4)?,
        body: row.get(5)?,
        structured_payload: parse_json_value(row.get(6)?, 6)?,
        source_format: row.get(7)?,
        created_by_actor_id: row.get(8)?,
        created_via_workflow_run_id: row.get(9)?,
        version_state: row.get(10)?,
        idempotency_key: row.get(11)?,
        body_purged_at: row.get(12)?,
        created_at: row.get(13)?,
    })
}

fn row_to_artifact_contribution(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactContribution> {
    Ok(ArtifactContribution {
        contribution_id: row.get(0)?,
        artifact_id: row.get(1)?,
        target_kind: row.get(2)?,
        target_id: row.get(3)?,
        contribution_kind: row.get(4)?,
        phase: row.get(5)?,
        role: row.get(6)?,
        actor_id: row.get(7)?,
        workflow_run_id: row.get(8)?,
        read_set: parse_json_value(row.get(9)?, 9)?,
        body_format: row.get(10)?,
        body: row.get(11)?,
        idempotency_key: row.get(12)?,
        created_at: row.get(13)?,
    })
}

fn row_to_artifact_comment(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactComment> {
    Ok(ArtifactComment {
        comment_id: row.get(0)?,
        artifact_id: row.get(1)?,
        target_kind: row.get(2)?,
        target_id: row.get(3)?,
        child_address: row.get(4)?,
        parent_comment_id: row.get(5)?,
        actor_id: row.get(6)?,
        body: row.get(7)?,
        state: row.get(8)?,
        resolved_by_actor_id: row.get(9)?,
        resolved_by_workflow_run_id: row.get(10)?,
        resolved_at: row.get(11)?,
        resolution_note: row.get(12)?,
        idempotency_key: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
    })
}

fn row_to_artifact_link(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactLink> {
    Ok(ArtifactLink {
        link_id: row.get(0)?,
        link_type: row.get(1)?,
        source_kind: row.get(2)?,
        source_id: row.get(3)?,
        source_version_id: row.get(4)?,
        source_child_address: row.get(5)?,
        target_kind: row.get(6)?,
        target_id: row.get(7)?,
        target_version_id: row.get(8)?,
        target_child_address: row.get(9)?,
        created_by_actor_id: row.get(10)?,
        created_via_workflow_run_id: row.get(11)?,
        idempotency_key: row.get(12)?,
        supersedes_link_id: row.get(13)?,
        created_at: row.get(14)?,
    })
}

fn row_to_artifact_chunk(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactChunk> {
    Ok(ArtifactChunk {
        chunk_id: row.get(0)?,
        artifact_id: row.get(1)?,
        artifact_version_id: row.get(2)?,
        child_address: row.get(3)?,
        text: row.get(4)?,
        embedding_model: row.get(5)?,
        embedding_vector: row.get(6)?,
        app: row.get(7)?,
        label: row.get(8)?,
        kind: row.get(9)?,
        metadata: parse_json_value(row.get(10)?, 10)?,
        superseded_by_chunk_id: row.get(11)?,
        created_at: row.get(12)?,
    })
}

fn row_to_workflow_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowRun> {
    Ok(WorkflowRun {
        workflow_run_id: row.get(0)?,
        artifact_id: row.get(1)?,
        workflow_kind: row.get(2)?,
        phase: row.get(3)?,
        round_id: row.get(4)?,
        coordinator_actor_id: row.get(5)?,
        participant_actor_ids: parse_labels(row.get::<_, Option<String>>(6)?),
        source_artifact_version_id: row.get(7)?,
        read_set: parse_json_value(row.get(8)?, 8)?,
        idempotency_key: row.get(9)?,
        is_resumable: row.get::<_, i64>(10)? != 0,
        state: row.get(11)?,
        generated_contribution_ids: parse_labels(row.get::<_, Option<String>>(12)?),
        generated_version_ids: parse_labels(row.get::<_, Option<String>>(13)?),
        generated_task_ids: parse_labels(row.get::<_, Option<String>>(14)?),
        generated_link_ids: parse_labels(row.get::<_, Option<String>>(15)?),
        generated_chunk_ids: parse_labels(row.get::<_, Option<String>>(16)?),
        failure_reason: row.get(17)?,
        started_at: row.get(18)?,
        ended_at: row.get(19)?,
    })
}

struct ArtifactQueryBuilder {
    sql: String,
    binds: Vec<Box<dyn rusqlite::ToSql>>,
}

impl ArtifactQueryBuilder {
    fn new(sql: String) -> Self {
        Self {
            sql,
            binds: Vec::new(),
        }
    }

    fn with_bind<T>(mut self, value: T) -> Self
    where
        T: rusqlite::ToSql + 'static,
    {
        self.push_bind(value);
        self
    }

    fn push_bind<T>(&mut self, value: T) -> usize
    where
        T: rusqlite::ToSql + 'static,
    {
        self.binds.push(Box::new(value));
        self.binds.len()
    }

    fn push_sql(&mut self, clause: &str) {
        self.sql.push_str(clause);
    }

    fn and_trimmed_exact(&mut self, column: &str, value: Option<&str>) {
        if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
            let ph = self.push_bind(value.to_string());
            self.push_sql(&format!(" AND {column} = ?{ph}"));
        }
    }

    fn and_trimmed_like<F>(&mut self, clause: F, value: Option<&str>)
    where
        F: FnOnce(usize) -> String,
    {
        if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
            let ph = self.push_bind(format!("%{value}%"));
            self.push_sql(&clause(ph));
        }
    }

    fn and_trimmed_like_pattern<F>(&mut self, clause: F, value: Option<&str>, pattern: String)
    where
        F: FnOnce(usize) -> String,
    {
        if value.map(str::trim).filter(|v| !v.is_empty()).is_some() {
            let ph = self.push_bind(pattern);
            self.push_sql(&clause(ph));
        }
    }

    fn collect<T, F>(&self, conn: &Connection, mapper: F) -> Result<Vec<T>>
    where
        F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        let params_vec: Vec<&dyn rusqlite::ToSql> = self.binds.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&self.sql)?;
        let rows = stmt
            .query_map(params_vec.as_slice(), mapper)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn artifact_project_ident(conn: &Connection, artifact_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT project_ident FROM artifacts WHERE artifact_id = ?1",
            params![artifact_id],
            |r| r.get(0),
        )
        .optional()?)
}

fn require_artifact_project(
    conn: &Connection,
    project_ident: &str,
    artifact_id: &str,
) -> Result<()> {
    let found = artifact_project_ident(conn, artifact_id)?;
    if found.as_deref() != Some(project_ident) {
        anyhow::bail!("artifact not found in project");
    }
    Ok(())
}

fn get_version_artifact_id(conn: &Connection, version_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT artifact_id FROM artifact_versions WHERE artifact_version_id = ?1",
            params![version_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// SQL fragment expressing project-scoped visibility of an `artifact_links`
/// row aliased as `link_alias`. The clause resolves to true when *any* side
/// of the link (source artifact, target artifact, source version, or target
/// version) belongs to an artifact owned by the project bound at
/// `?<project_placeholder>`. Pure external / discovery-only links — those
/// whose four side fields all sit outside the project — are excluded.
///
/// This is the canonical predicate for project-scoped link queries. It is
/// shared by the link quota counter and `list_artifact_links` today, and is
/// shaped so future link routes (T007) and authorization checks (T017) can
/// reuse it without redefining membership semantics. Keep the contract
/// stable; if you change it, update every site routed through here at once.
///
/// Returned fragment is an unparenthesized `EXISTS (...)` clause; embed it
/// directly after `WHERE`/`AND` as a boolean.
pub(crate) fn artifact_link_visibility_clause(
    link_alias: &str,
    project_placeholder: usize,
) -> String {
    let l = link_alias;
    let ph = project_placeholder;
    format!(
        "EXISTS (\n            SELECT 1 FROM artifacts a\n            WHERE a.project_ident = ?{ph}\n              AND (\n                ({l}.source_kind = 'artifact' AND {l}.source_id = a.artifact_id)\n                OR ({l}.target_kind = 'artifact' AND {l}.target_id = a.artifact_id)\n                OR {l}.source_version_id IN (SELECT artifact_version_id FROM artifact_versions WHERE artifact_id = a.artifact_id)\n                OR {l}.target_version_id IN (SELECT artifact_version_id FROM artifact_versions WHERE artifact_id = a.artifact_id)\n              )\n         )"
    )
}

fn artifact_quota_count(
    conn: &Connection,
    project_ident: &str,
    counter: QuotaCounter,
) -> Result<u64> {
    let sql: String = match counter {
        QuotaCounter::Artifact => {
            "SELECT COUNT(*) FROM artifacts WHERE project_ident = ?1 AND lifecycle_state != 'archived'"
                .to_string()
        }
        QuotaCounter::Version => "SELECT COUNT(*) FROM artifact_versions v
             JOIN artifacts a ON a.artifact_id = v.artifact_id
             WHERE a.project_ident = ?1"
            .to_string(),
        QuotaCounter::Contribution => "SELECT COUNT(*) FROM artifact_contributions c
             JOIN artifacts a ON a.artifact_id = c.artifact_id
             WHERE a.project_ident = ?1"
            .to_string(),
        QuotaCounter::OpenComment => "SELECT COUNT(*) FROM artifact_comments c
             JOIN artifacts a ON a.artifact_id = c.artifact_id
             WHERE a.project_ident = ?1 AND c.state = 'open'"
            .to_string(),
        QuotaCounter::Link => {
            // Route project-scoped link membership through the canonical
            // visibility predicate so list/count semantics cannot drift.
            format!(
                "SELECT COUNT(*) FROM artifact_links l WHERE {}",
                artifact_link_visibility_clause("l", 1),
            )
        }
        QuotaCounter::Chunk => "SELECT COUNT(*) FROM artifact_chunks c
             JOIN artifacts a ON a.artifact_id = c.artifact_id
             WHERE a.project_ident = ?1 AND c.superseded_by_chunk_id IS NULL"
            .to_string(),
        QuotaCounter::RunningWorkflow => "SELECT COUNT(*) FROM workflow_runs w
             JOIN artifacts a ON a.artifact_id = w.artifact_id
             WHERE a.project_ident = ?1 AND w.state = 'started'"
            .to_string(),
        QuotaCounter::WriteRpm => "SELECT 0".to_string(),
    };
    let count: i64 = if counter == QuotaCounter::WriteRpm {
        conn.query_row(&sql, [], |r| r.get(0))?
    } else {
        conn.query_row(&sql, params![project_ident], |r| r.get(0))?
    };
    Ok(count as u64)
}

fn check_quota(
    conn: &Connection,
    project_ident: &str,
    counter: QuotaCounter,
    envelope: &ArtifactOperationsEnvelope,
) -> Result<Vec<QuotaWarning>> {
    let current = artifact_quota_count(conn, project_ident, counter)?;
    Ok(envelope
        .quotas
        .evaluate(counter, current)
        .map_err(anyhow::Error::new)?
        .into_iter()
        .collect())
}

fn check_labels(labels: &[String], envelope: &ArtifactOperationsEnvelope) -> Result<()> {
    envelope
        .sizes
        .check(SizeLimitKind::ArtifactLabelsCount, labels.len())
        .map_err(anyhow::Error::new)?;
    for label in labels {
        envelope
            .sizes
            .check(SizeLimitKind::ArtifactLabelBytes, label.len())
            .map_err(anyhow::Error::new)?;
    }
    Ok(())
}

fn count_json_refs(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(values) => values.iter().map(count_json_refs).sum(),
        serde_json::Value::Object(map) => map.values().map(count_json_refs).sum(),
        serde_json::Value::String(_) => 1,
        _ => 0,
    }
}

fn check_read_set(
    read_set: Option<&serde_json::Value>,
    envelope: &ArtifactOperationsEnvelope,
) -> Result<()> {
    if let Some(read_set) = read_set {
        envelope
            .sizes
            .check(SizeLimitKind::ReadSetRefs, count_json_refs(read_set))
            .map_err(anyhow::Error::new)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// T028 — contribution/comment target + read_set reference validation
// ---------------------------------------------------------------------------
//
// **Validation boundary (T028):** repository layer.
//
// Both contribution and comment writes flow through `add_artifact_contribution`
// and `add_artifact_comment` regardless of caller (T007 public routes, T009
// workflow writers, internal reopen helpers). Keeping validation here means
// every public write path is protected uniformly without each caller having
// to remember to re-validate. Route-level handlers (T007) may still translate
// the resulting errors into specific HTTP shapes, but they MUST NOT bypass
// these helpers.
//
// **Resolvable read_set kinds** (validated for existence here):
//   - `artifact_version` / `versions`          -> artifact_versions
//   - `contribution`     / `contributions`     -> artifact_contributions
//   - `chunk`            / `chunks`            -> artifact_chunks
//   - `comment`          / `comments`          -> artifact_comments
//   - `workflow_run`     / `workflow_runs`     -> workflow_runs
//
// **Deferred read_set kinds** (size-counted only, NOT resolved here):
//   - `manifest_item` / `manifest_items` — block-address resolution depends on
//     T011 chunk addressing (manifest.items[<id>]); deferred to T011.
//   - `task`                                                       — T009 owns workflow task resolution.
//   - `pattern`, `memory`, `commit`, `external_url`                — external surfaces; deferred.
// Any unknown top-level key is treated as a deferred kind: refs inside are
// only size-counted (via `check_read_set`), not row-resolved.

/// Validate a contribution/comment `target_kind` + `target_id` resolves to a
/// real row that belongs to `artifact_id`. Rejects missing references and
/// cross-artifact references.
fn validate_target_ref(
    conn: &Connection,
    artifact_id: &str,
    target_kind: &str,
    target_id: &str,
) -> Result<()> {
    let owner_artifact_id: Option<String> = match target_kind {
        "artifact" => conn
            .query_row(
                "SELECT artifact_id FROM artifacts WHERE artifact_id = ?1",
                params![target_id],
                |r| r.get(0),
            )
            .optional()?,
        "artifact_version" => conn
            .query_row(
                "SELECT artifact_id FROM artifact_versions WHERE artifact_version_id = ?1",
                params![target_id],
                |r| r.get(0),
            )
            .optional()?,
        "contribution" => conn
            .query_row(
                "SELECT artifact_id FROM artifact_contributions WHERE contribution_id = ?1",
                params![target_id],
                |r| r.get(0),
            )
            .optional()?,
        "comment" => conn
            .query_row(
                "SELECT artifact_id FROM artifact_comments WHERE comment_id = ?1",
                params![target_id],
                |r| r.get(0),
            )
            .optional()?,
        other => {
            anyhow::bail!("unsupported target_kind '{other}'");
        }
    };
    match owner_artifact_id {
        None => anyhow::bail!("{target_kind} target '{target_id}' not found"),
        Some(owner) if owner != artifact_id => anyhow::bail!(
            "{target_kind} target '{target_id}' belongs to artifact '{owner}', not '{artifact_id}'"
        ),
        Some(_) => Ok(()),
    }
}

/// Map a read_set key to (table, pk-column) for resolvable kinds. Returns
/// `None` for kinds that are deferred to downstream tasks (T009/T011/etc.) —
/// see module comment above for the deferral list.
fn read_set_kind_table(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "artifact_version" | "versions" => Some(("artifact_versions", "artifact_version_id")),
        "contribution" | "contributions" => Some(("artifact_contributions", "contribution_id")),
        "chunk" | "chunks" => Some(("artifact_chunks", "chunk_id")),
        "comment" | "comments" => Some(("artifact_comments", "comment_id")),
        "workflow_run" | "workflow_runs" => Some(("workflow_runs", "workflow_run_id")),
        _ => None,
    }
}

/// Resolve every read_set ref for kinds we can validate. Unknown / deferred
/// kinds are skipped (still subject to `check_read_set` size limits upstream).
///
/// Accepts the shape `{ "<kind>": ["<id>", ...], ... }`. Arrays nested deeper
/// or other JSON shapes are tolerated — only the top-level recognized keys
/// trigger resolution. This matches the body produced by T006 contribution /
/// workflow callers and gives T011 room to extend the shape later.
fn validate_read_set_refs(conn: &Connection, read_set: Option<&serde_json::Value>) -> Result<()> {
    let Some(serde_json::Value::Object(map)) = read_set else {
        return Ok(());
    };
    for (kind, value) in map {
        let Some((table, pk)) = read_set_kind_table(kind) else {
            continue; // deferred kind — size-only validation upstream
        };
        let serde_json::Value::Array(ids) = value else {
            continue;
        };
        for id_value in ids {
            let Some(id) = id_value.as_str() else {
                continue;
            };
            let sql = format!("SELECT 1 FROM {table} WHERE {pk} = ?1");
            let exists: Option<i64> = conn.query_row(&sql, params![id], |r| r.get(0)).optional()?;
            if exists.is_none() {
                anyhow::bail!("read_set {kind} reference '{id}' does not resolve");
            }
        }
    }
    Ok(())
}

pub fn resolve_artifact_actor(
    conn: &Connection,
    identity: &ArtifactActorIdentity<'_>,
) -> Result<ArtifactActor> {
    let id = artifact_actor_upsert(conn, identity)?;
    get_artifact_actor(conn, &id)?.ok_or_else(|| anyhow::anyhow!("inserted actor not found"))
}

pub fn get_artifact_actor(conn: &Connection, actor_id: &str) -> Result<Option<ArtifactActor>> {
    let mut stmt = conn.prepare(
        "SELECT actor_id, actor_type, agent_system, agent_system_label, agent_id, host,
                display_name, runtime_metadata, created_at, updated_at
         FROM artifact_actors WHERE actor_id = ?1",
    )?;
    let mut rows = stmt.query_map(params![actor_id], row_to_artifact_actor)?;
    Ok(rows.next().transpose()?)
}

pub fn create_artifact(
    conn: &Connection,
    envelope: &ArtifactOperationsEnvelope,
    input: &ArtifactInsert<'_>,
) -> Result<ArtifactWriteResult<ArtifactSummary>> {
    check_labels(input.labels, envelope)?;
    let warnings = check_quota(conn, input.project_ident, QuotaCounter::Artifact, envelope)?;
    let id = artifact_insert(conn, input)?;
    let record = get_artifact_summary(conn, input.project_ident, &id)?
        .ok_or_else(|| anyhow::anyhow!("inserted artifact not found"))?;
    Ok(ArtifactWriteResult {
        record,
        warnings,
        replayed: false,
    })
}

#[allow(clippy::drop_non_drop)]
pub fn update_artifact(
    conn: &Connection,
    project_ident: &str,
    artifact_id: &str,
    update: &ArtifactUpdate<'_>,
    envelope: &ArtifactOperationsEnvelope,
) -> Result<Option<ArtifactSummary>> {
    require_artifact_project(conn, project_ident, artifact_id)?;
    if let Some(labels) = update.labels {
        check_labels(labels, envelope)?;
    }

    let mut sets: Vec<String> = Vec::new();
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut push = |col: &str, val: Box<dyn rusqlite::ToSql>| {
        binds.push(val);
        sets.push(format!("{col} = ?{}", binds.len()));
    };
    if let Some(title) = update.title {
        push("title", Box::new(title.to_string()));
    }
    if let Some(labels) = update.labels {
        push("labels", Box::new(serialize_string_array(labels)));
    }
    if let Some(state) = update.lifecycle_state {
        push("lifecycle_state", Box::new(state.to_string()));
    }
    if let Some(state) = update.review_state {
        push("review_state", Box::new(state.to_string()));
    }
    if let Some(state) = update.implementation_state {
        push("implementation_state", Box::new(state.to_string()));
    }
    push("updated_at", Box::new(now_ms()));
    drop(push);

    binds.push(Box::new(project_ident.to_string()));
    let project_ph = binds.len();
    binds.push(Box::new(artifact_id.to_string()));
    let artifact_ph = binds.len();
    let sql = format!(
        "UPDATE artifacts SET {} WHERE project_ident = ?{project_ph} AND artifact_id = ?{artifact_ph}",
        sets.join(", ")
    );
    let params_vec: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let changed = conn.execute(&sql, params_vec.as_slice())?;
    if changed == 0 {
        return Ok(None);
    }
    get_artifact_summary(conn, project_ident, artifact_id)
}

pub fn list_artifacts(
    conn: &Connection,
    project_ident: &str,
    filters: &ArtifactFilters<'_>,
) -> Result<Vec<ArtifactSummary>> {
    let mut query = ArtifactQueryBuilder::new(format!(
        "SELECT {ARTIFACT_SUMMARY_SELECT_COLS} FROM artifacts a WHERE a.project_ident = ?1"
    ))
    .with_bind(project_ident.to_string());

    query.and_trimmed_exact("a.kind", filters.kind);
    query.and_trimmed_exact("a.subkind", filters.subkind);
    query.and_trimmed_exact("a.lifecycle_state", filters.lifecycle_state);
    if let Some(label) = filters.label.map(str::trim).filter(|v| !v.is_empty()) {
        query.and_trimmed_like_pattern(
            |ph| format!(" AND COALESCE(a.labels, '') LIKE ?{ph}"),
            Some(label),
            format!("%\"{label}\"%"),
        );
    }
    if let Some(actor_id) = filters.actor_id.map(str::trim).filter(|v| !v.is_empty()) {
        let ph = query.push_bind(actor_id.to_string());
        query.push_sql(&format!(
            " AND (a.created_by_actor_id = ?{ph}
                   OR EXISTS (SELECT 1 FROM artifact_contributions c WHERE c.artifact_id = a.artifact_id AND c.actor_id = ?{ph})
                   OR EXISTS (SELECT 1 FROM artifact_comments c WHERE c.artifact_id = a.artifact_id AND c.actor_id = ?{ph})
                   OR EXISTS (SELECT 1 FROM artifact_links l WHERE l.created_by_actor_id = ?{ph}
                       AND (l.source_id = a.artifact_id OR l.target_id = a.artifact_id)))"
        ));
    }
    query.and_trimmed_like(
        |ph| {
            format!(
            " AND (a.artifact_id LIKE ?{ph}
                   OR a.title LIKE ?{ph}
                   OR a.kind LIKE ?{ph}
                   OR COALESCE(a.subkind, '') LIKE ?{ph}
                   OR COALESCE(a.labels, '') LIKE ?{ph}
                   OR EXISTS (SELECT 1 FROM artifact_versions v WHERE v.artifact_id = a.artifact_id AND (COALESCE(v.body, '') LIKE ?{ph} OR COALESCE(v.structured_payload, '') LIKE ?{ph}))
                   OR EXISTS (SELECT 1 FROM artifact_contributions c WHERE c.artifact_id = a.artifact_id AND c.body LIKE ?{ph})
                   OR EXISTS (
                       SELECT 1 FROM artifact_links l
                       WHERE (l.source_id = a.artifact_id
                              OR l.target_id = a.artifact_id
                              OR l.source_version_id IN (SELECT v.artifact_version_id FROM artifact_versions v WHERE v.artifact_id = a.artifact_id)
                              OR l.target_version_id IN (SELECT v.artifact_version_id FROM artifact_versions v WHERE v.artifact_id = a.artifact_id))
                         AND (COALESCE(l.source_id, '') LIKE ?{ph}
                              OR COALESCE(l.target_id, '') LIKE ?{ph}
                              OR COALESCE(l.source_version_id, '') LIKE ?{ph}
                              OR COALESCE(l.target_version_id, '') LIKE ?{ph}
                              OR l.link_type LIKE ?{ph})
                   ))"
            )
        },
        filters.query,
    );
    query.push_sql(" ORDER BY a.updated_at DESC, a.title ASC");
    query.collect(conn, row_to_artifact_summary)
}

pub fn get_artifact_summary(
    conn: &Connection,
    project_ident: &str,
    artifact_id: &str,
) -> Result<Option<ArtifactSummary>> {
    let sql = format!(
        "SELECT {ARTIFACT_SUMMARY_SELECT_COLS} FROM artifacts
         WHERE project_ident = ?1 AND artifact_id = ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![project_ident, artifact_id], row_to_artifact_summary)?;
    Ok(rows.next().transpose()?)
}

pub fn get_artifact(
    conn: &Connection,
    project_ident: &str,
    artifact_id: &str,
) -> Result<Option<ArtifactDetail>> {
    let artifact = match get_artifact_summary(conn, project_ident, artifact_id)? {
        Some(artifact) => artifact,
        None => return Ok(None),
    };
    let current_version = artifact
        .current_version_id
        .as_deref()
        .map(|id| get_artifact_version(conn, project_ident, artifact_id, id))
        .transpose()?
        .flatten();
    let accepted_version = artifact
        .accepted_version_id
        .as_deref()
        .map(|id| get_artifact_version(conn, project_ident, artifact_id, id))
        .transpose()?
        .flatten();
    Ok(Some(ArtifactDetail {
        artifact,
        current_version,
        accepted_version,
    }))
}

fn existing_artifact_version_for_key(
    conn: &Connection,
    input: &ArtifactVersionInsert<'_>,
) -> Result<Option<ArtifactVersion>> {
    let (Some(run_id), Some(key)) = (input.created_via_workflow_run_id, input.idempotency_key)
    else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT {ARTIFACT_VERSION_SELECT_COLS} FROM artifact_versions
         WHERE artifact_id = ?1 AND created_via_workflow_run_id = ?2 AND idempotency_key = ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(
        params![input.artifact_id, run_id, key],
        row_to_artifact_version,
    )?;
    Ok(rows.next().transpose()?)
}

/// T024 stale-replay non-regression guard.
///
/// `create_artifact_version` honors a partial-failure repair semantic from
/// T003 (`docs/workflow-mutation-contract.md` §4): if an earlier attempt
/// inserted the version row but crashed before updating
/// `artifacts.current_version_id`, a subsequent replay with the same
/// `(workflow_run_id, idempotency_key)` should backfill that pointer rather
/// than silently leaving the artifact dangling.
///
/// However, a *stale* replay of an older key (e.g. an at-least-once delivery
/// system redelivering a long-superseded request) must NEVER regress the
/// pointer to an older version after a newer version has already been
/// written and pointed-to. The original implementation called
/// `artifact_set_pointers(..., Some(&existing.artifact_version_id), None)`
/// unconditionally on every replay; combined with the `COALESCE(?, current)`
/// SQL update that always overwrites when the supplied value is non-NULL,
/// this would happily clobber the newer pointer.
///
/// Repair predicate: only adopt the replayed version as `current_version_id`
/// when the pointer is currently NULL (true partial-failure repair) OR
/// already equals the replayed version (no-op idempotent confirmation).
/// Any other current value means a newer or concurrent write has won — leave
/// it alone. Returns `true` when a repair was actually performed so the
/// caller (or future tests) can surface a warning if desired.
fn repair_artifact_current_pointer_for_replay(
    conn: &Connection,
    artifact_id: &str,
    replayed_version_id: &str,
) -> Result<bool> {
    let current: Option<String> = conn
        .query_row(
            "SELECT current_version_id FROM artifacts WHERE artifact_id = ?1",
            params![artifact_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    match current.as_deref() {
        None => {
            artifact_set_pointers(conn, artifact_id, Some(replayed_version_id), None)?;
            Ok(true)
        }
        Some(id) if id == replayed_version_id => Ok(false),
        Some(_) => Ok(false),
    }
}

/// Build a replayed `ArtifactWriteResult` (no warnings, no inserts). DRY
/// shorthand for the 6 idempotent-replay sites that all return the same
/// envelope shape — extracted in T024 to keep replay branches uniform.
fn replayed_write_result<T>(record: T) -> ArtifactWriteResult<T> {
    ArtifactWriteResult {
        record,
        warnings: Vec::new(),
        replayed: true,
    }
}

pub fn create_artifact_version(
    conn: &Connection,
    envelope: &ArtifactOperationsEnvelope,
    input: &ArtifactVersionInsert<'_>,
) -> Result<ArtifactWriteResult<ArtifactVersion>> {
    let project_ident = artifact_project_ident(conn, input.artifact_id)?
        .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
    if let Some(parent_id) = input.parent_version_id {
        if get_version_artifact_id(conn, parent_id)?.as_deref() != Some(input.artifact_id) {
            anyhow::bail!("parent version does not belong to artifact");
        }
    }
    if let Some(existing) = existing_artifact_version_for_key(conn, input)? {
        // Guarded pointer repair — see `repair_artifact_current_pointer_for_replay`.
        // A stale replay must never regress a newer current_version_id.
        repair_artifact_current_pointer_for_replay(
            conn,
            input.artifact_id,
            &existing.artifact_version_id,
        )?;
        return Ok(replayed_write_result(existing));
    }
    if let Some(body) = input.body {
        let kind = if input.body_format == "openapi" || input.body_format == "swagger" {
            SizeLimitKind::ArtifactVersionSourceBody
        } else {
            SizeLimitKind::ArtifactVersionBody
        };
        envelope
            .sizes
            .check(kind, body.len())
            .map_err(anyhow::Error::new)?;
    }
    if let Some(payload) = input.structured_payload {
        let actual = serde_json::to_string(payload)?.len();
        envelope
            .sizes
            .check(SizeLimitKind::ArtifactVersionStructuredPayload, actual)
            .map_err(anyhow::Error::new)?;
    }
    let warnings = check_quota(conn, &project_ident, QuotaCounter::Version, envelope)?;
    let id = artifact_version_insert(conn, input)?;
    artifact_set_pointers(conn, input.artifact_id, Some(&id), None)?;
    let record = get_artifact_version(conn, &project_ident, input.artifact_id, &id)?
        .ok_or_else(|| anyhow::anyhow!("inserted artifact version not found"))?;
    Ok(ArtifactWriteResult {
        record,
        warnings,
        replayed: false,
    })
}

pub fn list_artifact_versions(
    conn: &Connection,
    project_ident: &str,
    artifact_id: &str,
) -> Result<Vec<ArtifactVersion>> {
    require_artifact_project(conn, project_ident, artifact_id)?;
    let sql = format!(
        "SELECT {ARTIFACT_VERSION_SELECT_COLS} FROM artifact_versions
         WHERE artifact_id = ?1 ORDER BY created_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![artifact_id], row_to_artifact_version)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_artifact_version(
    conn: &Connection,
    project_ident: &str,
    artifact_id: &str,
    artifact_version_id: &str,
) -> Result<Option<ArtifactVersion>> {
    require_artifact_project(conn, project_ident, artifact_id)?;
    let sql = format!(
        "SELECT {ARTIFACT_VERSION_SELECT_COLS} FROM artifact_versions
         WHERE artifact_id = ?1 AND artifact_version_id = ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(
        params![artifact_id, artifact_version_id],
        row_to_artifact_version,
    )?;
    Ok(rows.next().transpose()?)
}

pub fn accept_artifact_version(
    conn: &Connection,
    project_ident: &str,
    artifact_id: &str,
    artifact_version_id: &str,
    actor_id: &str,
    workflow_run_id: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<ArtifactContribution> {
    require_artifact_project(conn, project_ident, artifact_id)?;
    let version = get_artifact_version(conn, project_ident, artifact_id, artifact_version_id)?
        .ok_or_else(|| anyhow::anyhow!("version not found"))?;
    if !matches!(
        version.version_state.as_str(),
        "draft" | "under_review" | "accepted"
    ) {
        anyhow::bail!("version cannot be accepted from current state");
    }
    conn.execute(
        "UPDATE artifact_versions SET version_state = 'accepted'
         WHERE artifact_id = ?1 AND artifact_version_id = ?2",
        params![artifact_id, artifact_version_id],
    )?;
    artifact_set_pointers(
        conn,
        artifact_id,
        Some(artifact_version_id),
        Some(artifact_version_id),
    )?;
    add_artifact_contribution(
        conn,
        &ArtifactOperationsEnvelope::production_defaults(),
        &ArtifactContributionInsert {
            artifact_id,
            target_kind: "artifact_version",
            target_id: artifact_version_id,
            contribution_kind: "state_transition",
            phase: Some("acceptance"),
            role: "coordinator",
            actor_id,
            workflow_run_id,
            read_set: None,
            body_format: "markdown",
            body: "accepted artifact version",
            idempotency_key,
        },
    )
    .map(|result| result.record)
}

fn existing_contribution_for_key(
    conn: &Connection,
    input: &ArtifactContributionInsert<'_>,
) -> Result<Option<ArtifactContribution>> {
    let sql = if input.workflow_run_id.is_some() {
        format!(
            "SELECT {ARTIFACT_CONTRIBUTION_SELECT_COLS} FROM artifact_contributions
             WHERE workflow_run_id = ?1 AND idempotency_key = ?2"
        )
    } else {
        format!(
            "SELECT {ARTIFACT_CONTRIBUTION_SELECT_COLS} FROM artifact_contributions
             WHERE artifact_id = ?3 AND actor_id = ?4 AND idempotency_key = ?2"
        )
    };
    let Some(key) = input.idempotency_key else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = if let Some(run_id) = input.workflow_run_id {
        stmt.query_map(params![run_id, key], row_to_artifact_contribution)?
    } else {
        stmt.query_map(
            params![
                rusqlite::types::Null,
                key,
                input.artifact_id,
                input.actor_id
            ],
            row_to_artifact_contribution,
        )?
    };
    Ok(rows.next().transpose()?)
}

pub fn add_artifact_contribution(
    conn: &Connection,
    envelope: &ArtifactOperationsEnvelope,
    input: &ArtifactContributionInsert<'_>,
) -> Result<ArtifactWriteResult<ArtifactContribution>> {
    let project_ident = artifact_project_ident(conn, input.artifact_id)?
        .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
    if let Some(existing) = existing_contribution_for_key(conn, input)? {
        return Ok(replayed_write_result(existing));
    }
    envelope
        .sizes
        .check(SizeLimitKind::ContributionBody, input.body.len())
        .map_err(anyhow::Error::new)?;
    check_read_set(input.read_set, envelope)?;
    // T028: target row must exist and belong to this artifact; read_set refs
    // for resolvable kinds must resolve. See module comment above
    // `validate_target_ref` for the boundary rationale and deferred kinds.
    validate_target_ref(conn, input.artifact_id, input.target_kind, input.target_id)?;
    validate_read_set_refs(conn, input.read_set)?;
    let warnings = check_quota(conn, &project_ident, QuotaCounter::Contribution, envelope)?;
    let id = artifact_contribution_insert(conn, input)?;
    let record = get_artifact_contribution(conn, input.artifact_id, &id)?
        .ok_or_else(|| anyhow::anyhow!("inserted contribution not found"))?;
    Ok(ArtifactWriteResult {
        record,
        warnings,
        replayed: false,
    })
}

pub fn get_artifact_contribution(
    conn: &Connection,
    artifact_id: &str,
    contribution_id: &str,
) -> Result<Option<ArtifactContribution>> {
    let sql = format!(
        "SELECT {ARTIFACT_CONTRIBUTION_SELECT_COLS} FROM artifact_contributions
         WHERE artifact_id = ?1 AND contribution_id = ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(
        params![artifact_id, contribution_id],
        row_to_artifact_contribution,
    )?;
    Ok(rows.next().transpose()?)
}

pub fn list_artifact_contributions(
    conn: &Connection,
    project_ident: &str,
    artifact_id: &str,
) -> Result<Vec<ArtifactContribution>> {
    require_artifact_project(conn, project_ident, artifact_id)?;
    let sql = format!(
        "SELECT {ARTIFACT_CONTRIBUTION_SELECT_COLS} FROM artifact_contributions
         WHERE artifact_id = ?1 ORDER BY created_at ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![artifact_id], row_to_artifact_contribution)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn json_contains_string(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::String(s) => s == needle,
        serde_json::Value::Array(values) => values.iter().any(|v| json_contains_string(v, needle)),
        serde_json::Value::Object(map) => map.values().any(|v| json_contains_string(v, needle)),
        _ => false,
    }
}

pub fn list_design_review_contributions(
    conn: &Connection,
    project_ident: &str,
    artifact_id: &str,
    filters: &DesignReviewContributionFilters<'_>,
) -> Result<Vec<ArtifactContribution>> {
    let artifact = get_artifact_summary(conn, project_ident, artifact_id)?
        .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
    if artifact.kind != "design_review" {
        anyhow::bail!("artifact is not a design_review");
    }

    let select_cols = ARTIFACT_CONTRIBUTION_SELECT_COLS
        .split(", ")
        .map(|col| format!("c.{col}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut query = ArtifactQueryBuilder::new(format!(
        "SELECT {select_cols}
         FROM artifact_contributions c
         LEFT JOIN workflow_runs wr ON wr.workflow_run_id = c.workflow_run_id
         WHERE c.artifact_id = ?1"
    ))
    .with_bind(artifact_id.to_string());
    query.and_trimmed_exact("wr.round_id", filters.round_id);
    query.and_trimmed_exact("c.phase", filters.phase);
    query.and_trimmed_exact("c.role", filters.role);
    if let Some(reviewed_version_id) = filters.reviewed_version_id.map(str::trim) {
        if !reviewed_version_id.is_empty() {
            let target_ph = query.push_bind(reviewed_version_id.to_string());
            let source_ph = query.push_bind(reviewed_version_id.to_string());
            query.push_sql(&format!(
                " AND ((c.target_kind = 'artifact_version' AND c.target_id = ?{target_ph}) \
                 OR wr.source_artifact_version_id = ?{source_ph})"
            ));
        }
    }
    query.push_sql(" ORDER BY c.created_at ASC");
    let mut rows: Vec<ArtifactContribution> = query.collect(conn, row_to_artifact_contribution)?;
    if let Some(needle) = filters
        .read_set_contains
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        rows.retain(|contribution| {
            contribution
                .read_set
                .as_ref()
                .is_some_and(|read_set| json_contains_string(read_set, needle))
        });
    }
    Ok(rows)
}

fn existing_comment_for_key(
    conn: &Connection,
    input: &ArtifactCommentInsert<'_>,
) -> Result<Option<ArtifactComment>> {
    let Some(key) = input.idempotency_key else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT {ARTIFACT_COMMENT_SELECT_COLS} FROM artifact_comments
         WHERE target_kind = ?1 AND target_id = ?2 AND actor_id = ?3 AND idempotency_key = ?4"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(
        params![input.target_kind, input.target_id, input.actor_id, key],
        row_to_artifact_comment,
    )?;
    Ok(rows.next().transpose()?)
}

pub fn add_artifact_comment(
    conn: &Connection,
    envelope: &ArtifactOperationsEnvelope,
    input: &ArtifactCommentInsert<'_>,
) -> Result<ArtifactWriteResult<ArtifactComment>> {
    let project_ident = artifact_project_ident(conn, input.artifact_id)?
        .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
    if let Some(parent) = input.parent_comment_id {
        validate_target_ref(conn, input.artifact_id, "comment", parent)?;
    }
    if let Some(existing) = existing_comment_for_key(conn, input)? {
        return Ok(replayed_write_result(existing));
    }
    envelope
        .sizes
        .check(SizeLimitKind::CommentBody, input.body.len())
        .map_err(anyhow::Error::new)?;
    // T028: target row must exist and belong to this artifact. Comments
    // currently have no read_set field; if/when added, plug in
    // `validate_read_set_refs` here too.
    validate_target_ref(conn, input.artifact_id, input.target_kind, input.target_id)?;
    let warnings = check_quota(conn, &project_ident, QuotaCounter::OpenComment, envelope)?;
    let id = artifact_comment_insert(conn, input)?;
    let record = get_artifact_comment(conn, input.artifact_id, &id)?
        .ok_or_else(|| anyhow::anyhow!("inserted comment not found"))?;
    Ok(ArtifactWriteResult {
        record,
        warnings,
        replayed: false,
    })
}

pub fn get_artifact_comment(
    conn: &Connection,
    artifact_id: &str,
    comment_id: &str,
) -> Result<Option<ArtifactComment>> {
    let sql = format!(
        "SELECT {ARTIFACT_COMMENT_SELECT_COLS} FROM artifact_comments
         WHERE artifact_id = ?1 AND comment_id = ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![artifact_id, comment_id], row_to_artifact_comment)?;
    Ok(rows.next().transpose()?)
}

pub fn list_artifact_comments(
    conn: &Connection,
    project_ident: &str,
    artifact_id: &str,
) -> Result<Vec<ArtifactComment>> {
    require_artifact_project(conn, project_ident, artifact_id)?;
    let sql = format!(
        "SELECT {ARTIFACT_COMMENT_SELECT_COLS} FROM artifact_comments
         WHERE artifact_id = ?1 ORDER BY created_at ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![artifact_id], row_to_artifact_comment)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn resolve_artifact_comment(
    conn: &Connection,
    project_ident: &str,
    artifact_id: &str,
    comment_id: &str,
    actor_id: &str,
    workflow_run_id: Option<&str>,
    resolution_note: Option<&str>,
) -> Result<Option<ArtifactComment>> {
    require_artifact_project(conn, project_ident, artifact_id)?;
    let existing = match get_artifact_comment(conn, artifact_id, comment_id)? {
        Some(comment) => comment,
        None => return Ok(None),
    };
    if existing.state == "resolved" {
        return Ok(Some(existing));
    }
    conn.execute(
        "UPDATE artifact_comments
         SET state = 'resolved',
             resolved_by_actor_id = ?3,
             resolved_by_workflow_run_id = ?4,
             resolved_at = ?5,
             resolution_note = ?6,
             updated_at = ?5
         WHERE artifact_id = ?1 AND comment_id = ?2 AND state = 'open'",
        params![
            artifact_id,
            comment_id,
            actor_id,
            workflow_run_id,
            now_ms(),
            resolution_note,
        ],
    )?;
    get_artifact_comment(conn, artifact_id, comment_id)
}

#[allow(clippy::too_many_arguments)]
pub fn reopen_artifact_comment(
    conn: &Connection,
    envelope: &ArtifactOperationsEnvelope,
    project_ident: &str,
    artifact_id: &str,
    comment_id: &str,
    actor_id: &str,
    note_body: &str,
    idempotency_key: Option<&str>,
) -> Result<Option<ArtifactComment>> {
    require_artifact_project(conn, project_ident, artifact_id)?;
    let existing = match get_artifact_comment(conn, artifact_id, comment_id)? {
        Some(comment) => comment,
        None => return Ok(None),
    };
    if existing.state != "resolved" {
        return Ok(Some(existing));
    }
    conn.execute(
        "UPDATE artifact_comments
         SET state = 'open',
             resolved_by_actor_id = NULL,
             resolved_by_workflow_run_id = NULL,
             resolved_at = NULL,
             resolution_note = NULL,
             updated_at = ?3
         WHERE artifact_id = ?1 AND comment_id = ?2",
        params![artifact_id, comment_id, now_ms()],
    )?;
    let _ = add_artifact_comment(
        conn,
        envelope,
        &ArtifactCommentInsert {
            artifact_id,
            target_kind: &existing.target_kind,
            target_id: &existing.target_id,
            child_address: existing.child_address.as_deref(),
            parent_comment_id: Some(comment_id),
            actor_id,
            body: note_body,
            idempotency_key,
        },
    )?;
    get_artifact_comment(conn, artifact_id, comment_id)
}

fn audit_link_required_refs(link_type: &str) -> Option<(bool, bool)> {
    match link_type {
        "spec_implements_design" | "supersedes_version" => Some((true, true)),
        "task_generated_from_spec" | "chunk_of_version" | "pattern_applied_to_artifact_version" => {
            Some((false, true))
        }
        "decision_resolves_comment" => Some((false, false)),
        "supersedes_artifact" => Some((false, false)),
        "doc_referenced_by_spec" | "comment_references_task" => None,
        _ => None,
    }
}

fn validate_artifact_link(conn: &Connection, input: &ArtifactLinkInsert<'_>) -> Result<()> {
    if input.created_via_workflow_run_id.is_some() && input.idempotency_key.is_none() {
        anyhow::bail!("workflow-emitted links require idempotency_key");
    }
    if let Some((source_required, target_required)) = audit_link_required_refs(input.link_type) {
        if source_required && input.source_version_id.is_none() {
            anyhow::bail!("audit-path link requires source_version_id");
        }
        if target_required && input.target_version_id.is_none() {
            anyhow::bail!("audit-path link requires target_version_id");
        }
    }
    if input.link_type == "supersedes_artifact" {
        let source_project = artifact_project_ident(conn, input.source_id)?;
        let target_project = artifact_project_ident(conn, input.target_id)?;
        if source_project.is_none() || source_project != target_project {
            anyhow::bail!("supersedes_artifact requires artifacts in the same project");
        }
    }
    Ok(())
}

fn project_for_link(conn: &Connection, input: &ArtifactLinkInsert<'_>) -> Result<Option<String>> {
    if input.source_kind == "artifact" {
        return artifact_project_ident(conn, input.source_id);
    }
    if input.target_kind == "artifact" {
        return artifact_project_ident(conn, input.target_id);
    }
    if let Some(version_id) = input.source_version_id.or(input.target_version_id) {
        if let Some(artifact_id) = get_version_artifact_id(conn, version_id)? {
            return artifact_project_ident(conn, &artifact_id);
        }
    }
    Ok(None)
}

fn existing_link_for_key(
    conn: &Connection,
    input: &ArtifactLinkInsert<'_>,
) -> Result<Option<ArtifactLink>> {
    let (Some(run_id), Some(key)) = (input.created_via_workflow_run_id, input.idempotency_key)
    else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT {ARTIFACT_LINK_SELECT_COLS} FROM artifact_links
         WHERE created_via_workflow_run_id = ?1 AND idempotency_key = ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![run_id, key], row_to_artifact_link)?;
    Ok(rows.next().transpose()?)
}

pub fn create_artifact_link(
    conn: &Connection,
    envelope: &ArtifactOperationsEnvelope,
    input: &ArtifactLinkInsert<'_>,
) -> Result<ArtifactWriteResult<ArtifactLink>> {
    validate_artifact_link(conn, input)?;
    if let Some(existing) = existing_link_for_key(conn, input)? {
        return Ok(replayed_write_result(existing));
    }
    let project_ident = project_for_link(conn, input)?
        .ok_or_else(|| anyhow::anyhow!("link project not resolvable"))?;
    let warnings = check_quota(conn, &project_ident, QuotaCounter::Link, envelope)?;
    let id = artifact_link_insert(conn, input)?;
    let record =
        get_artifact_link(conn, &id)?.ok_or_else(|| anyhow::anyhow!("inserted link not found"))?;
    Ok(ArtifactWriteResult {
        record,
        warnings,
        replayed: false,
    })
}

pub fn get_artifact_link(conn: &Connection, link_id: &str) -> Result<Option<ArtifactLink>> {
    let sql = format!("SELECT {ARTIFACT_LINK_SELECT_COLS} FROM artifact_links WHERE link_id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![link_id], row_to_artifact_link)?;
    Ok(rows.next().transpose()?)
}

pub fn list_artifact_links(
    conn: &Connection,
    project_ident: &str,
    filters: &ArtifactLinkFilters<'_>,
) -> Result<Vec<ArtifactLink>> {
    // Project-scoped visibility is shared with the link quota counter via
    // `artifact_link_visibility_clause`; downstream T007 routes and T017
    // authorization checks should compose with that helper rather than
    // open-coding membership semantics.
    let mut query = ArtifactQueryBuilder::new(format!(
        "SELECT {ARTIFACT_LINK_SELECT_COLS} FROM artifact_links l WHERE {}",
        artifact_link_visibility_clause("l", 1),
    ))
    .with_bind(project_ident.to_string());
    query.and_trimmed_exact("l.link_type", filters.link_type);
    query.and_trimmed_exact("l.source_kind", filters.source_kind);
    query.and_trimmed_exact("l.source_id", filters.source_id);
    query.and_trimmed_exact("l.target_kind", filters.target_kind);
    query.and_trimmed_exact("l.target_id", filters.target_id);
    query.push_sql(" ORDER BY l.created_at DESC");
    query.collect(conn, row_to_artifact_link)
}

pub fn create_artifact_chunk(
    conn: &Connection,
    envelope: &ArtifactOperationsEnvelope,
    input: &ArtifactChunkInsert<'_>,
) -> Result<ArtifactWriteResult<ArtifactChunk>> {
    let project_ident = artifact_project_ident(conn, input.artifact_id)?
        .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
    if get_version_artifact_id(conn, input.artifact_version_id)?.as_deref()
        != Some(input.artifact_id)
    {
        anyhow::bail!("chunk version does not belong to artifact");
    }
    if let Some(existing) =
        get_artifact_chunk_by_address(conn, input.artifact_version_id, input.child_address)?
    {
        return Ok(replayed_write_result(existing));
    }
    envelope
        .sizes
        .check(SizeLimitKind::ChunkText, input.text.len())
        .map_err(anyhow::Error::new)?;
    let warnings = check_quota(conn, &project_ident, QuotaCounter::Chunk, envelope)?;
    let id = artifact_chunk_insert(conn, input)?;
    let record = get_artifact_chunk(conn, &id)?
        .ok_or_else(|| anyhow::anyhow!("inserted chunk not found"))?;
    Ok(ArtifactWriteResult {
        record,
        warnings,
        replayed: false,
    })
}

pub fn get_artifact_chunk(conn: &Connection, chunk_id: &str) -> Result<Option<ArtifactChunk>> {
    let sql =
        format!("SELECT {ARTIFACT_CHUNK_SELECT_COLS} FROM artifact_chunks WHERE chunk_id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![chunk_id], row_to_artifact_chunk)?;
    Ok(rows.next().transpose()?)
}

pub fn get_artifact_chunk_by_address(
    conn: &Connection,
    artifact_version_id: &str,
    child_address: &str,
) -> Result<Option<ArtifactChunk>> {
    let sql = format!(
        "SELECT {ARTIFACT_CHUNK_SELECT_COLS} FROM artifact_chunks
         WHERE artifact_version_id = ?1 AND child_address = ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(
        params![artifact_version_id, child_address],
        row_to_artifact_chunk,
    )?;
    Ok(rows.next().transpose()?)
}

pub fn list_artifact_chunks(
    conn: &Connection,
    project_ident: &str,
    artifact_id: &str,
    filters: &ArtifactChunkFilters<'_>,
) -> Result<Vec<ArtifactChunk>> {
    require_artifact_project(conn, project_ident, artifact_id)?;
    let mut query = ArtifactQueryBuilder::new(format!(
        "SELECT {ARTIFACT_CHUNK_SELECT_COLS} FROM artifact_chunks c WHERE c.artifact_id = ?1"
    ))
    .with_bind(artifact_id.to_string());
    if !filters.include_superseded {
        query.push_sql(" AND c.superseded_by_chunk_id IS NULL");
    }
    query.and_trimmed_exact("c.artifact_version_id", filters.artifact_version_id);
    query.and_trimmed_exact("c.app", filters.app);
    query.and_trimmed_exact("c.label", filters.label);
    query.and_trimmed_exact("c.kind", filters.kind);
    query.and_trimmed_like(
        |ph| {
            format!(
            " AND (c.child_address LIKE ?{ph} OR c.text LIKE ?{ph} OR COALESCE(c.metadata_json, '') LIKE ?{ph})"
            )
        },
        filters.query,
    );
    query.push_sql(" ORDER BY c.created_at DESC, c.child_address ASC");
    query.collect(conn, row_to_artifact_chunk)
}

fn existing_workflow_run_for_key(
    conn: &Connection,
    input: &WorkflowRunInsert<'_>,
) -> Result<Option<WorkflowRun>> {
    let Some(key) = input.idempotency_key else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT {WORKFLOW_RUN_SELECT_COLS} FROM workflow_runs
         WHERE coordinator_actor_id = ?1 AND artifact_id = ?2 AND workflow_kind = ?3 AND idempotency_key = ?4"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(
        params![
            input.coordinator_actor_id,
            input.artifact_id,
            input.workflow_kind,
            key,
        ],
        row_to_workflow_run,
    )?;
    Ok(rows.next().transpose()?)
}

pub fn start_workflow_run(
    conn: &Connection,
    envelope: &ArtifactOperationsEnvelope,
    input: &WorkflowRunInsert<'_>,
) -> Result<ArtifactWriteResult<WorkflowRun>> {
    let project_ident = artifact_project_ident(conn, input.artifact_id)?
        .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
    if let Some(source_version_id) = input.source_artifact_version_id {
        if get_version_artifact_id(conn, source_version_id)?.as_deref() != Some(input.artifact_id) {
            anyhow::bail!("source artifact version does not belong to artifact");
        }
    }
    check_read_set(input.read_set, envelope)?;
    if let Some(existing) = existing_workflow_run_for_key(conn, input)? {
        return Ok(replayed_write_result(existing));
    }
    let warnings = check_quota(
        conn,
        &project_ident,
        QuotaCounter::RunningWorkflow,
        envelope,
    )?;
    let id = workflow_run_insert(conn, input)?;
    let record = get_workflow_run(conn, &id)?
        .ok_or_else(|| anyhow::anyhow!("inserted workflow run not found"))?;
    Ok(ArtifactWriteResult {
        record,
        warnings,
        replayed: false,
    })
}

pub fn get_workflow_run(conn: &Connection, workflow_run_id: &str) -> Result<Option<WorkflowRun>> {
    let sql =
        format!("SELECT {WORKFLOW_RUN_SELECT_COLS} FROM workflow_runs WHERE workflow_run_id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![workflow_run_id], row_to_workflow_run)?;
    Ok(rows.next().transpose()?)
}

pub fn list_workflow_runs(
    conn: &Connection,
    project_ident: &str,
    artifact_id: &str,
) -> Result<Vec<WorkflowRun>> {
    require_artifact_project(conn, project_ident, artifact_id)?;
    let sql = format!(
        "SELECT {WORKFLOW_RUN_SELECT_COLS} FROM workflow_runs
         WHERE artifact_id = ?1 ORDER BY started_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![artifact_id], row_to_workflow_run)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[allow(clippy::drop_non_drop)]
pub fn update_workflow_run(
    conn: &Connection,
    workflow_run_id: &str,
    update: &WorkflowRunUpdate<'_>,
) -> Result<Option<WorkflowRun>> {
    let current = match get_workflow_run(conn, workflow_run_id)? {
        Some(run) => run,
        None => return Ok(None),
    };
    if update.state == Some("failed") && matches!(update.failure_reason, None | Some(None)) {
        anyhow::bail!("failed workflow runs require failure_reason");
    }

    let mut sets: Vec<String> = Vec::new();
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut push = |col: &str, val: Box<dyn rusqlite::ToSql>| {
        binds.push(val);
        sets.push(format!("{col} = ?{}", binds.len()));
    };
    if let Some(state) = update.state {
        push("state", Box::new(state.to_string()));
    }
    if let Some(reason) = update.failure_reason {
        push("failure_reason", Box::new(reason.map(str::to_string)));
    }
    if let Some(ids) = update.generated_contribution_ids {
        push(
            "generated_contribution_ids",
            Box::new(serialize_string_array(ids)),
        );
    }
    if let Some(ids) = update.generated_version_ids {
        push(
            "generated_version_ids",
            Box::new(serialize_string_array(ids)),
        );
    }
    if let Some(ids) = update.generated_task_ids {
        push("generated_task_ids", Box::new(serialize_string_array(ids)));
    }
    if let Some(ids) = update.generated_link_ids {
        push("generated_link_ids", Box::new(serialize_string_array(ids)));
    }
    if let Some(ids) = update.generated_chunk_ids {
        push("generated_chunk_ids", Box::new(serialize_string_array(ids)));
    }
    if let Some(ended_at) = update.ended_at {
        push("ended_at", Box::new(ended_at));
    } else if matches!(update.state, Some("succeeded" | "failed" | "cancelled")) {
        push("ended_at", Box::new(Some(now_ms())));
    }
    drop(push);

    if sets.is_empty() {
        return Ok(Some(current));
    }
    binds.push(Box::new(workflow_run_id.to_string()));
    let id_ph = binds.len();
    let sql = format!(
        "UPDATE workflow_runs SET {} WHERE workflow_run_id = ?{id_ph}",
        sets.join(", ")
    );
    let params_vec: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    conn.execute(&sql, params_vec.as_slice())?;
    get_workflow_run(conn, workflow_run_id)
}

pub fn append_workflow_run_outputs(
    conn: &Connection,
    workflow_run_id: &str,
    contribution_id: Option<&str>,
    version_id: Option<&str>,
) -> Result<Option<WorkflowRun>> {
    let Some(run) = get_workflow_run(conn, workflow_run_id)? else {
        return Ok(None);
    };
    let mut contribution_ids = run.generated_contribution_ids.clone();
    let mut version_ids = run.generated_version_ids.clone();
    if let Some(id) = contribution_id {
        let id = id.to_string();
        if !contribution_ids.contains(&id) {
            contribution_ids.push(id);
        }
    }
    if let Some(id) = version_id {
        let id = id.to_string();
        if !version_ids.contains(&id) {
            version_ids.push(id);
        }
    }
    update_workflow_run(
        conn,
        workflow_run_id,
        &WorkflowRunUpdate {
            state: None,
            failure_reason: None,
            generated_contribution_ids: Some(&contribution_ids),
            generated_version_ids: Some(&version_ids),
            generated_task_ids: Some(&run.generated_task_ids),
            generated_link_ids: Some(&run.generated_link_ids),
            generated_chunk_ids: Some(&run.generated_chunk_ids),
            ended_at: Some(run.ended_at),
        },
    )
}

fn migrate_messages_for_system_delivery(conn: &Connection) -> Result<()> {
    let sql: Option<String> = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'messages'",
        [],
        |r| r.get(0),
    )?;
    let Some(sql) = sql else {
        return Ok(());
    };
    if sql.contains("'system'") {
        return Ok(());
    }

    conn.execute_batch(
        "PRAGMA foreign_keys=OFF;
         ALTER TABLE agent_confirmations RENAME TO agent_confirmations_old;
         ALTER TABLE messages RENAME TO messages_old;
         CREATE TABLE messages (
            id                   INTEGER PRIMARY KEY AUTOINCREMENT,
            project_ident        TEXT NOT NULL REFERENCES projects(ident),
            source               TEXT NOT NULL CHECK(source IN ('agent','user','system')),
            external_message_id  TEXT,
            content              TEXT NOT NULL,
            sent_at              INTEGER NOT NULL,
            confirmed_at         INTEGER,
            parent_message_id    INTEGER,
            agent_id             TEXT,
            message_type         TEXT NOT NULL DEFAULT 'message',
            subject              TEXT,
            hostname             TEXT,
            event_at             INTEGER,
            deliver_to_agents    INTEGER NOT NULL DEFAULT 0
         );
         INSERT INTO messages (
            id, project_ident, source, external_message_id, content, sent_at,
            confirmed_at, parent_message_id, agent_id, message_type, subject,
            hostname, event_at, deliver_to_agents
         )
         SELECT
            id, project_ident, source, external_message_id, content, sent_at,
            confirmed_at, parent_message_id, agent_id,
            COALESCE(message_type, 'message'), subject, hostname, event_at, 0
         FROM messages_old;
         CREATE TABLE agent_confirmations (
            agent_id       TEXT NOT NULL,
            project_ident  TEXT NOT NULL,
            message_id     INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
            confirmed_at   INTEGER NOT NULL,
            PRIMARY KEY (agent_id, project_ident, message_id)
         );
         INSERT OR IGNORE INTO agent_confirmations (
            agent_id, project_ident, message_id, confirmed_at
         )
         SELECT agent_id, project_ident, message_id, confirmed_at
         FROM agent_confirmations_old;
         DROP TABLE agent_confirmations_old;
         DROP TABLE messages_old;
         CREATE INDEX IF NOT EXISTS idx_messages_project
            ON messages(project_ident, id);
         CREATE INDEX IF NOT EXISTS idx_messages_unconfirmed
            ON messages(project_ident, id) WHERE confirmed_at IS NULL;
         CREATE INDEX IF NOT EXISTS idx_agent_conf_project
            ON agent_confirmations(project_ident, message_id);
         PRAGMA foreign_keys=ON;",
    )?;
    Ok(())
}

// ── Settings ─────────────────────────────────────────────────────────────────

pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare_cached("SELECT value FROM settings WHERE key = ?1")?;
    let mut rows = stmt.query_map(params![key], |r| r.get::<_, String>(0))?;
    Ok(rows.next().transpose()?)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Default theme used when nothing is stored yet.
pub const DEFAULT_THEME: &str = "dark";

pub fn get_theme(conn: &Connection) -> Result<String> {
    Ok(get_setting(conn, "theme")?.unwrap_or_else(|| DEFAULT_THEME.to_string()))
}

pub fn set_theme(conn: &Connection, theme: &str) -> Result<()> {
    set_setting(conn, "theme", theme)
}

// ── Projects ─────────────────────────────────────────────────────────────────

pub fn get_project(conn: &Connection, ident: &str) -> Result<Option<Project>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ident, channel_name, room_id, last_msg_id, created_at,
                repo_provider, repo_namespace, repo_name, repo_full_name
         FROM projects WHERE ident = ?1",
    )?;
    let mut rows = stmt.query_map(params![ident], row_to_project)?;
    Ok(rows.next().transpose()?)
}

/// Find a project by its plugin-specific room_id and channel_name.
pub fn get_project_by_room(
    conn: &Connection,
    channel_name: &str,
    room_id: &str,
) -> Result<Option<Project>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ident, channel_name, room_id, last_msg_id, created_at,
                repo_provider, repo_namespace, repo_name, repo_full_name
         FROM projects WHERE channel_name = ?1 AND room_id = ?2",
    )?;
    let mut rows = stmt.query_map(params![channel_name, room_id], row_to_project)?;
    Ok(rows.next().transpose()?)
}

pub fn insert_project(conn: &Connection, p: &Project) -> Result<()> {
    conn.execute(
        "INSERT INTO projects (
            ident, channel_name, room_id, last_msg_id, created_at,
            repo_provider, repo_namespace, repo_name, repo_full_name
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(ident) DO NOTHING",
        params![
            p.ident,
            p.channel_name,
            p.room_id,
            p.last_msg_id,
            p.created_at,
            p.repo_provider,
            p.repo_namespace,
            p.repo_name,
            p.repo_full_name
        ],
    )?;
    conn.execute(
        "INSERT INTO cursors (project_ident, last_read_id, updated_at)
         VALUES (?1, 0, ?2)
         ON CONFLICT(project_ident) DO NOTHING",
        params![p.ident, p.created_at],
    )?;
    Ok(())
}

pub fn all_projects(conn: &Connection) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ident, channel_name, room_id, last_msg_id, created_at,
                repo_provider, repo_namespace, repo_name, repo_full_name
         FROM projects",
    )?;
    let collected = stmt
        .query_map([], row_to_project)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(collected)
}

pub fn update_last_msg_id(conn: &Connection, ident: &str, msg_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE projects SET last_msg_id = ?1 WHERE ident = ?2",
        params![msg_id, ident],
    )?;
    Ok(())
}

fn row_to_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        ident: row.get(0)?,
        channel_name: row.get(1)?,
        room_id: row.get(2)?,
        last_msg_id: row.get(3)?,
        created_at: row.get(4)?,
        repo_provider: row.get(5)?,
        repo_namespace: row.get(6)?,
        repo_name: row.get(7)?,
        repo_full_name: row.get(8)?,
    })
}

pub fn update_project_repo_mapping(
    conn: &Connection,
    ident: &str,
    provider: Option<&str>,
    namespace: Option<&str>,
    repo_name: Option<&str>,
) -> Result<Option<Project>> {
    let provider = provider.map(str::trim).filter(|s| !s.is_empty());
    let namespace = namespace.map(str::trim).filter(|s| !s.is_empty());
    let repo_name = repo_name.map(str::trim).filter(|s| !s.is_empty());
    let repo_full_name = match (namespace, repo_name) {
        (Some(ns), Some(repo)) => Some(format!("{ns}/{repo}")),
        _ => None,
    };

    conn.execute(
        "UPDATE projects
         SET repo_provider = ?1,
             repo_namespace = ?2,
             repo_name = ?3,
             repo_full_name = ?4
         WHERE ident = ?5",
        params![provider, namespace, repo_name, repo_full_name, ident],
    )?;
    get_project(conn, ident)
}

pub fn bulk_fill_missing_repo_mappings(
    conn: &Connection,
    provider: &str,
    namespace: &str,
) -> Result<usize> {
    let provider = provider.trim();
    let namespace = namespace.trim();
    if provider.is_empty() || namespace.is_empty() {
        return Ok(0);
    }

    let changed = conn.execute(
        "UPDATE projects
         SET repo_provider = ?1,
             repo_namespace = ?2,
             repo_name = ident,
             repo_full_name = ?2 || '/' || ident
         WHERE COALESCE(repo_full_name, '') = ''",
        params![provider, namespace],
    )?;
    Ok(changed)
}

// ── Messages ─────────────────────────────────────────────────────────────────

pub fn insert_message(conn: &Connection, m: &Message) -> Result<i64> {
    let confirmed_at = if m.source == "agent" || (m.source == "system" && !m.deliver_to_agents) {
        Some(now_ms())
    } else {
        None
    };
    conn.execute(
        "INSERT INTO messages (project_ident, source, external_message_id, content, sent_at, confirmed_at, parent_message_id, agent_id, message_type, subject, hostname, event_at, deliver_to_agents)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            m.project_ident,
            m.source,
            m.external_message_id,
            m.content,
            m.sent_at,
            confirmed_at,
            m.parent_message_id,
            m.agent_id,
            m.message_type,
            m.subject,
            m.hostname,
            m.event_at,
            m.deliver_to_agents,
        ],
    )?;
    let msg_id = conn.last_insert_rowid();

    // Auto-confirm for the sending agent so it doesn't appear in their unread queue.
    if m.source == "agent" {
        if let Some(ref aid) = m.agent_id {
            conn.execute(
                "INSERT OR IGNORE INTO agent_confirmations (agent_id, project_ident, message_id, confirmed_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![aid, m.project_ident, msg_id, now_ms()],
            )?;
        }
    }

    Ok(msg_id)
}

/// Lazily register an agent for a project.
pub fn upsert_agent(conn: &Connection, project_ident: &str, agent_id: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO agents (project_ident, agent_id, registered_at)
         VALUES (?1, ?2, ?3)",
        params![project_ident, agent_id, now_ms()],
    )?;
    Ok(())
}

/// Get all user-authored messages not yet confirmed by a specific agent.
pub fn get_unconfirmed_for_agent(
    conn: &Connection,
    ident: &str,
    agent_id: &str,
) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare_cached(
        "SELECT m.id, m.project_ident, m.source, m.external_message_id,
                m.content, m.sent_at, m.confirmed_at,
                m.parent_message_id, m.agent_id, m.message_type,
                m.subject, m.hostname, m.event_at, m.deliver_to_agents
         FROM messages m
         WHERE m.project_ident = ?1
           AND (m.source = 'user' OR m.deliver_to_agents = 1)
           AND NOT EXISTS (
               SELECT 1 FROM agent_confirmations ac
               WHERE ac.agent_id = ?2
                 AND ac.project_ident = ?1
                 AND ac.message_id = m.id
           )
         ORDER BY m.id ASC",
    )?;
    let collected = stmt
        .query_map(params![ident, agent_id], row_to_message)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(collected)
}

/// Confirm a message for a specific agent. Returns true if newly confirmed.
pub fn confirm_message_for_agent(
    conn: &Connection,
    project_ident: &str,
    agent_id: &str,
    msg_id: i64,
) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO agent_confirmations (agent_id, project_ident, message_id, confirmed_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![agent_id, project_ident, msg_id, now_ms()],
    )?;
    Ok(n > 0)
}

/// Fetch a single message by ID within a project.
pub fn get_message_by_id(
    conn: &Connection,
    project_ident: &str,
    msg_id: i64,
) -> Result<Option<Message>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, project_ident, source, external_message_id, content, sent_at, confirmed_at,
                parent_message_id, agent_id, message_type, subject, hostname, event_at, deliver_to_agents
         FROM messages
         WHERE id = ?1 AND project_ident = ?2",
    )?;
    let mut rows = stmt.query_map(params![msg_id, project_ident], row_to_message)?;
    Ok(rows.next().transpose()?)
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(0)?,
        project_ident: row.get(1)?,
        source: row.get(2)?,
        external_message_id: row.get(3)?,
        content: row.get(4)?,
        sent_at: row.get(5)?,
        confirmed_at: row.get(6)?,
        parent_message_id: row.get(7)?,
        agent_id: row.get(8)?,
        message_type: row
            .get::<_, Option<String>>(9)?
            .unwrap_or_else(|| "message".into()),
        subject: row.get(10)?,
        hostname: row.get(11)?,
        event_at: row.get(12)?,
        deliver_to_agents: row.get(13)?,
    })
}

// ── Retention ─────────────────────────────────────────────────────────────────

pub fn purge_old_messages(conn: &Connection, cutoff_ms: i64) -> Result<usize> {
    // agent_confirmations cleaned up via ON DELETE CASCADE on messages(id).
    let n = conn.execute(
        "DELETE FROM messages
         WHERE sent_at < ?1
           AND confirmed_at IS NOT NULL",
        params![cutoff_ms],
    )?;
    Ok(n)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ── Dashboard ─────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct ProjectStats {
    pub ident: String,
    pub channel_name: String,
    pub room_id: String,
    pub total_messages: i64,
    pub unread_count: i64,
    pub api_doc_count: i64,
    pub repo_provider: Option<String>,
    pub repo_namespace: Option<String>,
    pub repo_name: Option<String>,
    pub repo_full_name: Option<String>,
}

#[derive(serde::Serialize)]
pub struct ProjectTaskStats {
    pub ident: String,
    pub todo_count: i64,
    pub in_progress_count: i64,
    pub done_count: i64,
}

#[derive(serde::Serialize)]
pub struct DashboardData {
    pub project_count: i64,
    pub total_messages: i64,
    pub agent_messages: i64,
    pub user_messages: i64,
    pub skill_count: i64,
    pub api_doc_count: i64,
    pub projects: Vec<ProjectStats>,
}

/// Return per-project stats ordered by most-recently-created first.
///
/// Shared by the HTML dashboard (via [`get_dashboard_data`]) and the JSON
/// helper endpoint the task picker binds to. Each row contains the project's
/// identity, its channel, the originating room id, the total message count,
/// and the number of unconfirmed user-sourced messages.
pub fn list_project_stats(conn: &Connection) -> Result<Vec<ProjectStats>> {
    let mut stmt = conn.prepare_cached(
        "SELECT p.ident, p.channel_name, p.room_id,
                COUNT(m.id),
                (SELECT COUNT(*) FROM messages m2
                 WHERE m2.project_ident = p.ident
                   AND m2.confirmed_at IS NULL
                   AND m2.source = 'user'),
                (SELECT COUNT(*) FROM api_docs d
                 WHERE d.project_ident = p.ident),
                p.repo_provider, p.repo_namespace, p.repo_name, p.repo_full_name
         FROM projects p
         LEFT JOIN messages m ON m.project_ident = p.ident
         GROUP BY p.ident
         ORDER BY p.created_at DESC",
    )?;
    let projects = stmt
        .query_map([], |r| {
            Ok(ProjectStats {
                ident: r.get(0)?,
                channel_name: r.get(1)?,
                room_id: r.get(2)?,
                total_messages: r.get(3)?,
                unread_count: r.get(4)?,
                api_doc_count: r.get(5)?,
                repo_provider: r.get(6)?,
                repo_namespace: r.get(7)?,
                repo_name: r.get(8)?,
                repo_full_name: r.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(projects)
}

pub fn list_project_task_stats(conn: &Connection) -> Result<Vec<ProjectTaskStats>> {
    let mut stmt = conn.prepare_cached(
        "SELECT p.ident,
                COALESCE(SUM(CASE WHEN t.status = 'todo' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN t.status = 'in_progress' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN t.status = 'done' THEN 1 ELSE 0 END), 0)
         FROM projects p
         LEFT JOIN tasks t ON t.project_ident = p.ident
         GROUP BY p.ident
         ORDER BY
            COALESCE(SUM(CASE WHEN t.status = 'todo' THEN 1 ELSE 0 END), 0) DESC,
            COALESCE(SUM(CASE WHEN t.status = 'in_progress' THEN 1 ELSE 0 END), 0) DESC,
            COALESCE(SUM(CASE WHEN t.status = 'done' THEN 1 ELSE 0 END), 0) DESC,
            p.ident ASC",
    )?;
    let projects = stmt
        .query_map([], |r| {
            Ok(ProjectTaskStats {
                ident: r.get(0)?,
                todo_count: r.get(1)?,
                in_progress_count: r.get(2)?,
                done_count: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(projects)
}

pub fn get_dashboard_data(conn: &Connection) -> Result<DashboardData> {
    let project_count: i64 = conn.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))?;
    let total_messages: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
    let agent_messages: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE source='agent'",
        [],
        |r| r.get(0),
    )?;
    let user_messages: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE source='user'",
        [],
        |r| r.get(0),
    )?;
    let skill_count: i64 = conn.query_row("SELECT COUNT(*) FROM skills", [], |r| r.get(0))?;
    let api_doc_count: i64 = conn.query_row("SELECT COUNT(*) FROM api_docs", [], |r| r.get(0))?;

    let projects = list_project_stats(conn)?;

    Ok(DashboardData {
        project_count,
        total_messages,
        agent_messages,
        user_messages,
        skill_count,
        api_doc_count,
        projects,
    })
}

// ── Skills ────────────────────────────────────────────────────────────────────

pub struct SkillRecord {
    pub name: String,
    /// "skill", "command", or "agent"
    pub kind: String,
    pub zip_data: Vec<u8>,
    /// Raw markdown content for commands; None for skills.
    pub content: Option<String>,
    pub size: i64,
    pub checksum: String,
    pub uploaded_at: i64,
}

#[derive(serde::Serialize)]
pub struct SkillMeta {
    pub name: String,
    pub kind: String,
    pub size: i64,
    pub checksum: String,
    pub uploaded_at: i64,
}

pub fn upsert_skill(conn: &Connection, r: &SkillRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO skills (name, kind, zip_data, content, size, checksum, uploaded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(name) DO UPDATE SET
             kind = excluded.kind,
             zip_data = excluded.zip_data,
             content = excluded.content,
             size = excluded.size,
             checksum = excluded.checksum,
             uploaded_at = excluded.uploaded_at",
        params![
            r.name,
            r.kind,
            r.zip_data,
            r.content,
            r.size,
            r.checksum,
            r.uploaded_at
        ],
    )?;
    Ok(())
}

pub fn get_skill(conn: &Connection, name: &str) -> Result<Option<SkillRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT name, kind, zip_data, content, size, checksum, uploaded_at FROM skills WHERE name = ?1",
    )?;
    let mut rows = stmt.query_map(params![name], |r| {
        Ok(SkillRecord {
            name: r.get(0)?,
            kind: r.get(1)?,
            zip_data: r.get(2)?,
            content: r.get(3)?,
            size: r.get(4)?,
            checksum: r.get(5)?,
            uploaded_at: r.get(6)?,
        })
    })?;
    Ok(rows.next().transpose()?)
}

/// List skill/command/agent metadata. Pass `Some(kind)` to restrict to a
/// single kind (`"skill" | "command" | "agent"`); `None` returns everything.
pub fn list_skills(conn: &Connection, kind: Option<&str>) -> Result<Vec<SkillMeta>> {
    let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<SkillMeta> {
        Ok(SkillMeta {
            name: r.get(0)?,
            kind: r.get(1)?,
            size: r.get(2)?,
            checksum: r.get(3)?,
            uploaded_at: r.get(4)?,
        })
    };

    let collected = match kind {
        Some(k) => {
            let mut stmt = conn.prepare_cached(
                "SELECT name, kind, size, checksum, uploaded_at
                 FROM skills
                 WHERE kind = ?1
                 ORDER BY name ASC",
            )?;
            let rows = stmt
                .query_map(params![k], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        }
        None => {
            let mut stmt = conn.prepare_cached(
                "SELECT name, kind, size, checksum, uploaded_at
                 FROM skills
                 ORDER BY kind ASC, name ASC",
            )?;
            let rows = stmt
                .query_map([], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        }
    };
    Ok(collected)
}

pub fn delete_skill(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM skills WHERE name = ?1", params![name])?;
    Ok(n > 0)
}

// ── Tasks ─────────────────────────────────────────────────────────────────────

/// How long an `in_progress` task with no activity before it is considered
/// abandoned and returned to `todo` by `reclaim_stale_tasks`.
pub const TASK_RECLAIM_MS: i64 = 60 * 60 * 1000; // 1 hour
/// How long a `done` task remains in the default list view before it falls off.
pub const TASK_DONE_FALLOFF_MS: i64 = 7 * 24 * 60 * 60 * 1000; // 7 days

#[derive(Debug, Clone, serde::Serialize)]
pub struct Task {
    pub id: String,
    pub project_ident: String,
    pub title: String,
    pub description: Option<String>,
    pub details: Option<String>,
    /// One of `"todo"`, `"in_progress"`, `"done"`.
    pub status: String,
    pub rank: i64,
    /// Parsed from the JSON-array-encoded `labels` column; empty when the
    /// column is NULL or malformed.
    pub labels: Vec<String>,
    pub hostname: Option<String>,
    pub owner_agent_id: Option<String>,
    pub reporter: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub done_at: Option<i64>,
    /// "normal" | "delegated"
    pub kind: String,
    pub delegated_to_project_ident: Option<String>,
    pub delegated_to_task_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskComment {
    pub id: String,
    pub task_id: String,
    pub author: String,
    /// One of `"agent"`, `"user"`, `"system"`.
    pub author_type: String,
    pub content: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskDelegation {
    pub id: String,
    pub source_project_ident: String,
    pub source_task_id: String,
    pub target_project_ident: String,
    pub target_task_id: String,
    pub requester_agent_id: Option<String>,
    pub requester_hostname: Option<String>,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub completion_message_id: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
    pub status: String,
    pub rank: i64,
    pub labels: Vec<String>,
    pub owner_agent_id: Option<String>,
    pub hostname: Option<String>,
    pub reporter: String,
    pub comment_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub kind: String,
    pub delegated_to_project_ident: Option<String>,
    pub delegated_to_task_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskDetail {
    pub task: Task,
    pub comments: Vec<TaskComment>,
}

/// Dynamic update payload for `update_task`. `None` on any field means
/// "do not touch"; for the `Option<Option<_>>` fields, `Some(None)` means
/// "clear the column" and `Some(Some(x))` means "set to x".
pub struct TaskUpdate<'a> {
    pub status: Option<&'a str>,
    pub owner_agent_id: Option<Option<&'a str>>,
    pub rank: Option<i64>,
    pub title: Option<&'a str>,
    pub description: Option<Option<&'a str>>,
    pub details: Option<Option<&'a str>>,
    pub labels: Option<&'a [String]>,
    pub hostname: Option<Option<&'a str>>,
}

pub struct DelegatedTaskInsert<'a> {
    pub project_ident: &'a str,
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub details: Option<&'a str>,
    pub labels: &'a [String],
    pub hostname: Option<&'a str>,
    pub reporter: &'a str,
    pub target_project_ident: &'a str,
    pub target_task_id: &'a str,
}

fn new_uuid() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Serializes a string vector as a JSON array, storing empty arrays as NULL.
// Used by T005 + T006 CRUD.
fn serialize_string_array(values: &[String]) -> Option<String> {
    if values.is_empty() {
        None
    } else {
        serde_json::to_string(values).ok()
    }
}

fn parse_labels(raw: Option<String>) -> Vec<String> {
    match raw {
        Some(s) => serde_json::from_str::<Vec<String>>(&s).unwrap_or_default(),
        None => Vec::new(),
    }
}

// ── Patterns ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct Pattern {
    pub id: String,
    pub title: String,
    pub slug: String,
    pub summary: Option<String>,
    pub body: String,
    pub labels: Vec<String>,
    pub version: String,
    pub state: String,
    pub superseded_by: Option<String>,
    pub author: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PatternSummary {
    pub id: String,
    pub title: String,
    pub slug: String,
    pub summary: Option<String>,
    pub labels: Vec<String>,
    pub version: String,
    pub state: String,
    pub superseded_by: Option<String>,
    pub author: String,
    pub comment_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PatternComment {
    pub id: String,
    pub pattern_id: String,
    pub author: String,
    /// One of `"agent"`, `"user"`, `"system"`.
    pub author_type: String,
    pub content: String,
    pub created_at: i64,
}

/// Dynamic update payload for `update_pattern`. `None` on any field means
/// "do not touch"; for `summary`, `Some(None)` clears the column.
pub struct PatternUpdate<'a> {
    pub title: Option<&'a str>,
    pub slug: Option<&'a str>,
    pub summary: Option<Option<&'a str>>,
    pub body: Option<&'a str>,
    pub labels: Option<&'a [String]>,
    pub version: Option<&'a str>,
    pub state: Option<&'a str>,
    pub superseded_by: Option<Option<&'a str>>,
}

fn row_to_pattern(row: &rusqlite::Row<'_>) -> rusqlite::Result<Pattern> {
    Ok(Pattern {
        id: row.get(0)?,
        title: row.get(1)?,
        slug: row.get(2)?,
        summary: row.get(3)?,
        body: row.get(4)?,
        labels: parse_labels(row.get::<_, Option<String>>(5)?),
        version: row.get(6)?,
        state: row.get(7)?,
        superseded_by: row.get(8)?,
        author: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

const PATTERN_SELECT_COLS: &str =
    "id, title, slug, summary, body, labels, version, state, superseded_by, author, created_at, updated_at";

pub fn validate_pattern_version(version: &str) -> Result<()> {
    if version == "draft" || version == "latest" || version == "superseded" {
        Ok(())
    } else {
        anyhow::bail!("invalid pattern version '{version}': must be draft|latest|superseded");
    }
}

pub fn validate_pattern_state(state: &str) -> Result<()> {
    if state == "active" || state == "archived" {
        Ok(())
    } else {
        anyhow::bail!("invalid pattern state '{state}': must be active|archived");
    }
}

pub fn slugify_pattern_title(title: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in title.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "pattern".to_string()
    } else {
        out
    }
}

#[allow(clippy::too_many_arguments)]
pub fn insert_pattern(
    conn: &Connection,
    title: &str,
    slug: Option<&str>,
    summary: Option<&str>,
    body: &str,
    labels: &[String],
    version: &str,
    state: &str,
    superseded_by: Option<&str>,
    author: &str,
) -> Result<Pattern> {
    validate_pattern_version(version)?;
    let state = state.trim();
    validate_pattern_state(state)?;
    let superseded_by = superseded_by.map(str::trim).filter(|s| !s.is_empty());
    if version == "superseded" && superseded_by.is_none() {
        anyhow::bail!("superseded_by is required when version is superseded");
    }
    if version != "superseded" && superseded_by.is_some() {
        anyhow::bail!("superseded_by can only be set when version is superseded");
    }

    let id = new_uuid();
    let now = now_ms();
    let slug = slug
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| slugify_pattern_title(title));
    let labels_json = serialize_string_array(labels);

    conn.execute(
        "INSERT INTO patterns (
             id, title, slug, summary, body, labels, version, state, superseded_by, author, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
        params![
            id,
            title,
            slug,
            summary,
            body,
            labels_json,
            version,
            state,
            superseded_by,
            author,
            now,
        ],
    )?;

    Ok(Pattern {
        id,
        title: title.to_string(),
        slug,
        summary: summary.map(str::to_string),
        body: body.to_string(),
        labels: labels.to_vec(),
        version: version.to_string(),
        state: state.to_string(),
        superseded_by: superseded_by.map(str::to_string),
        author: author.to_string(),
        created_at: now,
        updated_at: now,
    })
}

#[derive(Debug, Default)]
pub struct PatternFilters<'a> {
    pub query: Option<&'a str>,
    pub label: Option<&'a str>,
    pub version: Option<&'a str>,
    pub state: Option<&'a str>,
    pub superseded_by: Option<&'a str>,
}

pub fn list_patterns(
    conn: &Connection,
    filters: &PatternFilters<'_>,
) -> Result<Vec<PatternSummary>> {
    let mut sql = String::from(
        "SELECT p.id, p.title, p.slug, p.summary, p.labels, p.version, p.state, p.superseded_by, p.author,
                (SELECT COUNT(*) FROM pattern_comments pc WHERE pc.pattern_id = p.id),
                p.created_at, p.updated_at
         FROM patterns p",
    );
    let mut clauses: Vec<String> = Vec::new();
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(q) = filters.query.map(str::trim).filter(|q| !q.is_empty()) {
        binds.push(Box::new(format!("%{q}%")));
        let ph = binds.len();
        clauses.push(format!(
            "(p.title LIKE ?{ph}
              OR p.slug LIKE ?{ph}
              OR COALESCE(p.summary, '') LIKE ?{ph}
              OR p.body LIKE ?{ph}
              OR COALESCE(p.labels, '') LIKE ?{ph}
              OR p.version LIKE ?{ph}
              OR p.state LIKE ?{ph}
              OR COALESCE(p.superseded_by, '') LIKE ?{ph})"
        ));
    }

    if let Some(label) = filters.label.map(str::trim).filter(|v| !v.is_empty()) {
        binds.push(Box::new(format!("%\"{label}\"%")));
        clauses.push(format!("COALESCE(p.labels, '') LIKE ?{}", binds.len()));
    }

    if let Some(version) = filters.version.map(str::trim).filter(|v| !v.is_empty()) {
        validate_pattern_version(version)?;
        binds.push(Box::new(version.to_string()));
        clauses.push(format!("p.version = ?{}", binds.len()));
    }

    if let Some(state) = filters.state.map(str::trim).filter(|v| !v.is_empty()) {
        validate_pattern_state(state)?;
        binds.push(Box::new(state.to_string()));
        clauses.push(format!("p.state = ?{}", binds.len()));
    }

    if let Some(target) = filters
        .superseded_by
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        binds.push(Box::new(target.to_string()));
        let ph = binds.len();
        clauses.push(format!("(p.superseded_by = ?{ph} OR p.superseded_by IN (SELECT id FROM patterns WHERE slug = ?{ph}))"));
    }

    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY p.updated_at DESC, p.title ASC");

    let mut stmt = conn.prepare(&sql)?;
    let map_row = |r: &rusqlite::Row<'_>| {
        Ok(PatternSummary {
            id: r.get(0)?,
            title: r.get(1)?,
            slug: r.get(2)?,
            summary: r.get(3)?,
            labels: parse_labels(r.get::<_, Option<String>>(4)?),
            version: r.get(5)?,
            state: r.get(6)?,
            superseded_by: r.get(7)?,
            author: r.get(8)?,
            comment_count: r.get(9)?,
            created_at: r.get(10)?,
            updated_at: r.get(11)?,
        })
    };

    let params_vec: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let rows = stmt
        .query_map(params_vec.as_slice(), map_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_pattern(conn: &Connection, id_or_slug: &str) -> Result<Option<Pattern>> {
    let sql = format!("SELECT {PATTERN_SELECT_COLS} FROM patterns WHERE id = ?1 OR slug = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![id_or_slug], row_to_pattern)?;
    Ok(rows.next().transpose()?)
}

pub fn update_pattern(
    conn: &Connection,
    id_or_slug: &str,
    upd: &PatternUpdate<'_>,
) -> Result<Option<Pattern>> {
    let current = match get_pattern(conn, id_or_slug)? {
        Some(p) => p,
        None => return Ok(None),
    };

    let now = now_ms();
    let mut sets: Vec<String> = Vec::new();
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let push = |col: &str,
                val: Box<dyn rusqlite::ToSql>,
                sets: &mut Vec<String>,
                binds: &mut Vec<Box<dyn rusqlite::ToSql>>| {
        binds.push(val);
        sets.push(format!("{col} = ?{}", binds.len()));
    };

    if let Some(title) = upd.title {
        push("title", Box::new(title.to_string()), &mut sets, &mut binds);
    }
    if let Some(slug) = upd.slug {
        push("slug", Box::new(slug.to_string()), &mut sets, &mut binds);
    }
    if let Some(summary) = upd.summary {
        push(
            "summary",
            Box::new(summary.map(str::to_string)),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(body) = upd.body {
        push("body", Box::new(body.to_string()), &mut sets, &mut binds);
    }
    if let Some(labels) = upd.labels {
        push(
            "labels",
            Box::new(serialize_string_array(labels)),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(version) = upd.version {
        validate_pattern_version(version)?;
        push(
            "version",
            Box::new(version.to_string()),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(state) = upd.state {
        validate_pattern_state(state.trim())?;
        push(
            "state",
            Box::new(state.trim().to_string()),
            &mut sets,
            &mut binds,
        );
    }
    let next_version = upd.version.unwrap_or(&current.version);
    let next_superseded_by = match upd.superseded_by {
        Some(v) => v.map(str::trim).filter(|s| !s.is_empty()),
        None => current.superseded_by.as_deref(),
    };
    if next_version == "superseded" && next_superseded_by.is_none() {
        anyhow::bail!("superseded_by is required when version is superseded");
    }
    if next_version != "superseded"
        && upd
            .superseded_by
            .and_then(|v| v.map(str::trim).filter(|s| !s.is_empty()))
            .is_some()
    {
        anyhow::bail!("superseded_by can only be set when version is superseded");
    }
    if next_version != "superseded" && next_superseded_by.is_some() {
        push(
            "superseded_by",
            Box::new(None::<String>),
            &mut sets,
            &mut binds,
        );
    } else if let Some(superseded_by) = upd.superseded_by {
        let superseded_by = superseded_by
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        push(
            "superseded_by",
            Box::new(superseded_by),
            &mut sets,
            &mut binds,
        );
    }
    push("updated_at", Box::new(now), &mut sets, &mut binds);

    binds.push(Box::new(current.id.clone()));
    let id_ph = binds.len();
    let sql = format!(
        "UPDATE patterns SET {} WHERE id = ?{id_ph}",
        sets.join(", ")
    );
    let params_vec: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    conn.execute(&sql, params_vec.as_slice())?;
    get_pattern(conn, &current.id)
}

pub fn delete_pattern(conn: &Connection, id_or_slug: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM patterns WHERE id = ?1 OR slug = ?1",
        params![id_or_slug],
    )?;
    Ok(n > 0)
}

// ── Agent API docs ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiDoc {
    pub id: String,
    pub project_ident: String,
    pub app: String,
    pub title: String,
    pub summary: Option<String>,
    pub kind: String,
    pub source_format: String,
    pub source_ref: Option<String>,
    pub version: Option<String>,
    pub labels: Vec<String>,
    pub content: serde_json::Value,
    pub author: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub artifact_id: String,
    pub artifact_version_id: Option<String>,
    pub subkind: String,
    pub manifest_chunk_count: Option<usize>,
    pub chunking_status: String,
    pub linked_ids: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiDocSummary {
    pub id: String,
    pub project_ident: String,
    pub app: String,
    pub title: String,
    pub summary: Option<String>,
    pub kind: String,
    pub source_format: String,
    pub source_ref: Option<String>,
    pub version: Option<String>,
    pub labels: Vec<String>,
    pub author: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub artifact_id: String,
    pub artifact_version_id: Option<String>,
    pub subkind: String,
    pub manifest_chunk_count: Option<usize>,
    pub chunking_status: String,
    pub linked_ids: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ApiDocFilters<'a> {
    pub query: Option<&'a str>,
    pub app: Option<&'a str>,
    pub label: Option<&'a str>,
    pub kind: Option<&'a str>,
}

pub struct ApiDocInsert<'a> {
    pub app: &'a str,
    pub title: &'a str,
    pub summary: Option<&'a str>,
    pub kind: &'a str,
    pub source_format: &'a str,
    pub source_ref: Option<&'a str>,
    pub version: Option<&'a str>,
    pub labels: &'a [String],
    pub content: &'a serde_json::Value,
    pub author: &'a str,
}

#[derive(Default)]
pub struct ApiDocUpdate<'a> {
    pub app: Option<&'a str>,
    pub title: Option<&'a str>,
    pub summary: Option<Option<&'a str>>,
    pub kind: Option<&'a str>,
    pub source_format: Option<&'a str>,
    pub source_ref: Option<Option<&'a str>>,
    pub version: Option<Option<&'a str>>,
    pub labels: Option<&'a [String]>,
    pub content: Option<&'a serde_json::Value>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiDocChunkRecord {
    pub doc_id: String,
    pub project_ident: String,
    pub app: String,
    pub title: String,
    pub chunk_type: String,
    pub labels: Vec<String>,
    pub text: String,
    pub updated_at: i64,
    pub chunk_id: String,
    pub artifact_id: String,
    pub artifact_version_id: String,
    pub accepted_version_id: Option<String>,
    pub child_address: String,
    pub subkind: String,
    pub freshness: String,
    pub retrieval_scope: String,
    pub chunking_status: String,
    pub linked_ids: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiDocChunkingStatus {
    pub status: String,
    pub current_chunk_count: usize,
    pub stale_chunk_count: usize,
    pub superseded_chunk_count: usize,
    pub failed_addresses: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiDocChunkList {
    pub chunks: Vec<ApiDocChunkRecord>,
    pub chunking_status: ApiDocChunkingStatus,
    pub retrieval_scope: String,
    pub include_history: bool,
}

fn parse_content_json(raw: String) -> serde_json::Value {
    serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({ "raw": raw }))
}

fn parse_newline_strings(raw: Option<String>) -> Vec<String> {
    raw.unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn manifest_chunk_count(payload: Option<String>) -> Option<usize> {
    payload
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| {
            value
                .get("manifest")
                .and_then(|manifest| manifest.get("chunk_count"))
                .and_then(serde_json::Value::as_u64)
        })
        .map(|count| count as usize)
}

fn api_doc_chunking_status_value(
    manifest_chunk_count: Option<usize>,
    current: usize,
    stale: usize,
    superseded: usize,
    failed_addresses: &[String],
) -> String {
    if !failed_addresses.is_empty()
        || manifest_chunk_count.is_some_and(|expected| expected > current)
    {
        "partial".to_string()
    } else if stale > 0 {
        "stale".to_string()
    } else if current > 0 {
        "current".to_string()
    } else if superseded > 0 {
        "history_only".to_string()
    } else {
        "none".to_string()
    }
}

fn row_to_api_doc(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiDoc> {
    let content_json: String = row.get(10)?;
    let failed_addresses = parse_newline_strings(row.get(20)?);
    let manifest_chunk_count = manifest_chunk_count(row.get(17)?);
    let current = row.get::<_, i64>(18)? as usize;
    let stale = row.get::<_, i64>(19)? as usize;
    let superseded = row.get::<_, i64>(22)? as usize;
    Ok(ApiDoc {
        id: row.get(0)?,
        project_ident: row.get(1)?,
        app: row.get(2)?,
        title: row.get(3)?,
        summary: row.get(4)?,
        kind: row.get(5)?,
        source_format: row.get(6)?,
        source_ref: row.get(7)?,
        version: row.get(8)?,
        labels: parse_labels(row.get::<_, Option<String>>(9)?),
        content: parse_content_json(content_json),
        author: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
        artifact_id: row
            .get::<_, Option<String>>(14)?
            .unwrap_or_else(|| row.get(0).unwrap()),
        artifact_version_id: row.get(15)?,
        subkind: row
            .get::<_, Option<String>>(16)?
            .unwrap_or_else(|| API_DOC_SUBKIND.to_string()),
        manifest_chunk_count,
        chunking_status: api_doc_chunking_status_value(
            manifest_chunk_count,
            current,
            stale,
            superseded,
            &failed_addresses,
        ),
        linked_ids: parse_newline_strings(row.get(21)?),
    })
}

fn row_to_api_doc_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiDocSummary> {
    let failed_addresses = parse_newline_strings(row.get(19)?);
    let manifest_chunk_count = manifest_chunk_count(row.get(16)?);
    let current = row.get::<_, i64>(17)? as usize;
    let stale = row.get::<_, i64>(18)? as usize;
    let superseded = row.get::<_, i64>(21)? as usize;
    Ok(ApiDocSummary {
        id: row.get(0)?,
        project_ident: row.get(1)?,
        app: row.get(2)?,
        title: row.get(3)?,
        summary: row.get(4)?,
        kind: row.get(5)?,
        source_format: row.get(6)?,
        source_ref: row.get(7)?,
        version: row.get(8)?,
        labels: parse_labels(row.get::<_, Option<String>>(9)?),
        author: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        artifact_id: row
            .get::<_, Option<String>>(13)?
            .unwrap_or_else(|| row.get(0).unwrap()),
        artifact_version_id: row.get(14)?,
        subkind: row
            .get::<_, Option<String>>(15)?
            .unwrap_or_else(|| API_DOC_SUBKIND.to_string()),
        manifest_chunk_count,
        chunking_status: api_doc_chunking_status_value(
            manifest_chunk_count,
            current,
            stale,
            superseded,
            &failed_addresses,
        ),
        linked_ids: parse_newline_strings(row.get(20)?),
    })
}

const API_DOC_SUBKIND: &str = "api_context";
const API_DOC_RETAIN_LABEL: &str = "retain:permanent";
const API_DOC_ARTIFACT_KIND: &str = "documentation";
const API_DOC_BODY_FORMAT: &str = "application/agent-context+json";

const API_DOC_ARTIFACT_SELECT_COLS: &str = "a.artifact_id, a.accepted_version_id, a.subkind, av.structured_payload,
    (SELECT COUNT(*) FROM artifact_chunks c WHERE c.artifact_id = d.id AND c.superseded_by_chunk_id IS NULL AND a.accepted_version_id IS NOT NULL AND c.artifact_version_id = a.accepted_version_id),
    (SELECT COUNT(*) FROM artifact_chunks c WHERE c.artifact_id = d.id AND c.superseded_by_chunk_id IS NULL AND (a.accepted_version_id IS NULL OR c.artifact_version_id != a.accepted_version_id)),
    (SELECT GROUP_CONCAT(c.child_address, char(10)) FROM artifact_chunks c WHERE c.artifact_id = d.id AND (json_extract(c.metadata_json, '$.status') = 'failed' OR json_extract(c.metadata_json, '$.chunking_status') = 'failed')),
    (SELECT GROUP_CONCAT(linked_id, char(10)) FROM (
        SELECT CASE WHEN l.source_id = d.id THEN l.target_id ELSE l.source_id END AS linked_id
        FROM artifact_links l
        WHERE l.source_id = d.id OR l.target_id = d.id
    )),
    (SELECT COUNT(*) FROM artifact_chunks c WHERE c.artifact_id = d.id AND c.superseded_by_chunk_id IS NOT NULL)";

const API_DOC_SELECT_COLS: &str = "d.id, d.project_ident, d.app, d.title, d.summary, d.kind, d.source_format, d.source_ref, d.version, d.labels, d.content_json, d.author, d.created_at, d.updated_at";
const API_DOC_SUMMARY_SELECT_COLS: &str = "d.id, d.project_ident, d.app, d.title, d.summary, d.kind, d.source_format, d.source_ref, d.version, d.labels, d.author, d.created_at, d.updated_at";

fn api_doc_select_sql(full: bool) -> String {
    let cols = if full {
        API_DOC_SELECT_COLS
    } else {
        API_DOC_SUMMARY_SELECT_COLS
    };
    format!(
        "SELECT {cols}, {API_DOC_ARTIFACT_SELECT_COLS}
         FROM api_docs d
         LEFT JOIN artifacts a
           ON a.artifact_id = d.id
          AND a.project_ident = d.project_ident
          AND a.kind = '{API_DOC_ARTIFACT_KIND}'
          AND a.subkind = '{API_DOC_SUBKIND}'
         LEFT JOIN artifact_versions av
           ON av.artifact_version_id = a.accepted_version_id"
    )
}

fn api_doc_actor(conn: &Connection, author: &str) -> Result<String> {
    artifact_actor_upsert(
        conn,
        &ArtifactActorIdentity {
            actor_type: "user",
            agent_system: None,
            agent_system_label: None,
            agent_id: Some(author),
            host: None,
            display_name: author,
            runtime_metadata: None,
        },
    )
}

fn api_doc_artifact_labels(labels: &[String]) -> Vec<String> {
    let mut out = labels.to_vec();
    if !out.iter().any(|label| label == API_DOC_RETAIN_LABEL) {
        out.push(API_DOC_RETAIN_LABEL.to_string());
    }
    out
}

fn api_doc_chunk_specs(
    app: &str,
    title: &str,
    summary: Option<&str>,
    kind: &str,
    source_format: &str,
    version: Option<&str>,
    content: &serde_json::Value,
) -> Vec<(String, String, String)> {
    let mut chunks = Vec::new();
    let mut overview = format!("{title} ({app})");
    if let Some(summary) = summary.map(str::trim).filter(|s| !s.is_empty()) {
        overview.push_str("\n\n");
        overview.push_str(summary);
    }
    overview.push_str("\n\nkind: ");
    overview.push_str(kind);
    overview.push_str("\nsource_format: ");
    overview.push_str(source_format);
    if let Some(version) = version.map(str::trim).filter(|s| !s.is_empty()) {
        overview.push_str("\nversion: ");
        overview.push_str(version);
    }
    chunks.push(("overview".to_string(), overview, "overview".to_string()));

    if let serde_json::Value::Object(map) = content {
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
                    serde_json::Value::String(s) => s.clone(),
                    _ => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
                };
                if rendered.trim().is_empty() {
                    continue;
                }
                chunks.push((key.to_string(), rendered, key.to_string()));
            }
        }
    } else {
        chunks.push((
            "content".to_string(),
            content.to_string(),
            "content".to_string(),
        ));
    }
    chunks
}

struct ApiDocArtifactVersionInput<'a> {
    id: &'a str,
    project_ident: &'a str,
    app: &'a str,
    title: &'a str,
    summary: Option<&'a str>,
    kind: &'a str,
    source_format: &'a str,
    source_ref: Option<&'a str>,
    version: Option<&'a str>,
    labels: &'a [String],
    content: &'a serde_json::Value,
    author: &'a str,
    created_at: i64,
}

fn ensure_api_doc_artifact_version(
    conn: &Connection,
    input: &ApiDocArtifactVersionInput<'_>,
) -> Result<String> {
    let actor_id = api_doc_actor(conn, input.author)?;
    let artifact_labels = api_doc_artifact_labels(input.labels);
    let labels_json = serialize_string_array(&artifact_labels);
    let existing_artifact: Option<String> = conn
        .query_row(
            "SELECT artifact_id FROM artifacts WHERE artifact_id = ?1",
            params![input.id],
            |r| r.get(0),
        )
        .optional()?;
    if existing_artifact.is_none() {
        conn.execute(
            "INSERT INTO artifacts
             (artifact_id, project_ident, kind, subkind, title, labels,
              lifecycle_state, review_state, implementation_state,
              current_version_id, accepted_version_id, created_by_actor_id,
              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', 'none', 'not_applicable',
                     NULL, NULL, ?7, ?8, ?8)",
            params![
                input.id,
                input.project_ident,
                API_DOC_ARTIFACT_KIND,
                API_DOC_SUBKIND,
                input.title,
                labels_json,
                actor_id,
                input.created_at,
            ],
        )?;
    } else {
        conn.execute(
            "UPDATE artifacts
             SET title = ?2, labels = ?3, subkind = ?4, lifecycle_state = 'active',
                 updated_at = ?5
             WHERE artifact_id = ?1",
            params![
                input.id,
                input.title,
                labels_json,
                API_DOC_SUBKIND,
                now_ms()
            ],
        )?;
    }

    let chunks = api_doc_chunk_specs(
        input.app,
        input.title,
        input.summary,
        input.kind,
        input.source_format,
        input.version,
        input.content,
    );
    let payload = serde_json::json!({
        "manifest": {
            "chunk_count": chunks.len(),
            "chunk_store": "artifact_chunks",
            "chunk_source": "artifact_version_id",
            "retrieval_default": "current",
            "include_history_supported": true
        },
        "compatibility": {
            "api_doc_id": input.id,
            "legacy_kind": input.kind,
            "subkind": API_DOC_SUBKIND,
            "source_ref": input.source_ref
        }
    });
    let content_json = serde_json::to_string(input.content)?;
    let parent_version_id: Option<String> = conn
        .query_row(
            "SELECT current_version_id FROM artifacts WHERE artifact_id = ?1",
            params![input.id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let version_id = artifact_version_insert(
        conn,
        &ArtifactVersionInsert {
            artifact_id: input.id,
            version_label: input.version,
            parent_version_id: parent_version_id.as_deref(),
            body_format: API_DOC_BODY_FORMAT,
            body: Some(&content_json),
            structured_payload: Some(&payload),
            source_format: Some(input.source_format),
            created_by_actor_id: &actor_id,
            created_via_workflow_run_id: None,
            version_state: "accepted",
            idempotency_key: None,
        },
    )?;
    artifact_set_pointers(conn, input.id, Some(&version_id), Some(&version_id))?;

    for (child_address, text, chunk_kind) in chunks {
        let metadata = serde_json::json!({
            "status": "ok",
            "labels": input.labels,
            "legacy_kind": input.kind,
            "source_format": input.source_format,
            "source_ref": input.source_ref
        });
        let result = create_artifact_chunk(
            conn,
            &ArtifactOperationsEnvelope::production_defaults(),
            &ArtifactChunkInsert {
                artifact_id: input.id,
                artifact_version_id: &version_id,
                child_address: &child_address,
                text: &text,
                embedding_model: None,
                embedding_vector: None,
                app: Some(input.app),
                label: input.labels.first().map(String::as_str),
                kind: Some(&chunk_kind),
                metadata: Some(&metadata),
            },
        )?;
        if let Some(parent_version_id) = parent_version_id.as_deref() {
            if let Some(old) =
                get_artifact_chunk_by_address(conn, parent_version_id, &child_address)?
            {
                artifact_chunk_mark_superseded(conn, &old.chunk_id, &result.record.chunk_id)?;
            }
        }
    }

    Ok(version_id)
}

fn ensure_existing_api_doc_artifacts(conn: &Connection, project_ident: &str) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT d.id, d.project_ident, d.app, d.title, d.summary, d.kind,
                d.source_format, d.source_ref, d.version, d.labels, d.content_json,
                d.author, d.created_at
         FROM api_docs d
         LEFT JOIN artifacts a ON a.artifact_id = d.id
         WHERE d.project_ident = ?1 AND a.artifact_id IS NULL",
    )?;
    let rows = stmt
        .query_map(params![project_ident], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                parse_labels(row.get::<_, Option<String>>(9)?),
                parse_content_json(row.get::<_, String>(10)?),
                row.get::<_, String>(11)?,
                row.get::<_, i64>(12)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    for (
        id,
        project_ident,
        app,
        title,
        summary,
        kind,
        source_format,
        source_ref,
        version,
        labels,
        content,
        author,
        created_at,
    ) in rows
    {
        ensure_api_doc_artifact_version(
            conn,
            &ApiDocArtifactVersionInput {
                id: &id,
                project_ident: &project_ident,
                app: &app,
                title: &title,
                summary: summary.as_deref(),
                kind: &kind,
                source_format: &source_format,
                source_ref: source_ref.as_deref(),
                version: version.as_deref(),
                labels: &labels,
                content: &content,
                author: &author,
                created_at,
            },
        )?;
    }
    Ok(())
}

pub fn insert_api_doc(
    conn: &Connection,
    project_ident: &str,
    doc: &ApiDocInsert<'_>,
) -> Result<ApiDoc> {
    let id = new_uuid();
    let now = now_ms();
    let labels_json = serialize_string_array(doc.labels);
    let content_json = serde_json::to_string(doc.content)?;
    conn.execute(
        "INSERT INTO api_docs (
             id, project_ident, app, title, summary, kind, source_format, source_ref,
             version, labels, content_json, author, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
        params![
            id,
            project_ident,
            doc.app,
            doc.title,
            doc.summary,
            doc.kind,
            doc.source_format,
            doc.source_ref,
            doc.version,
            labels_json,
            content_json,
            doc.author,
            now,
        ],
    )?;
    ensure_api_doc_artifact_version(
        conn,
        &ApiDocArtifactVersionInput {
            id: &id,
            project_ident,
            app: doc.app,
            title: doc.title,
            summary: doc.summary,
            kind: doc.kind,
            source_format: doc.source_format,
            source_ref: doc.source_ref,
            version: doc.version,
            labels: doc.labels,
            content: doc.content,
            author: doc.author,
            created_at: now,
        },
    )?;
    get_api_doc(conn, project_ident, &id)?
        .ok_or_else(|| anyhow::anyhow!("inserted api doc not found"))
}

pub fn list_api_docs(
    conn: &Connection,
    project_ident: &str,
    filters: &ApiDocFilters<'_>,
) -> Result<Vec<ApiDocSummary>> {
    ensure_existing_api_doc_artifacts(conn, project_ident)?;
    let mut sql = format!("{} WHERE d.project_ident = ?1", api_doc_select_sql(false));
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(project_ident.to_string())];

    if let Some(app) = filters.app.map(str::trim).filter(|v| !v.is_empty()) {
        binds.push(Box::new(app.to_string()));
        sql.push_str(&format!(" AND d.app = ?{}", binds.len()));
    }
    if let Some(kind) = filters.kind.map(str::trim).filter(|v| !v.is_empty()) {
        binds.push(Box::new(kind.to_string()));
        sql.push_str(&format!(" AND d.kind = ?{}", binds.len()));
    }
    if let Some(label) = filters.label.map(str::trim).filter(|v| !v.is_empty()) {
        binds.push(Box::new(format!("%\"{label}\"%")));
        sql.push_str(&format!(
            " AND COALESCE(d.labels, '') LIKE ?{}",
            binds.len()
        ));
    }
    if let Some(q) = filters.query.map(str::trim).filter(|v| !v.is_empty()) {
        binds.push(Box::new(format!("%{q}%")));
        let ph = binds.len();
        sql.push_str(&format!(
            " AND (d.id LIKE ?{ph}
                   OR d.app LIKE ?{ph}
                   OR d.title LIKE ?{ph}
                   OR COALESCE(d.summary, '') LIKE ?{ph}
                   OR d.kind LIKE ?{ph}
                   OR d.source_format LIKE ?{ph}
                   OR COALESCE(d.source_ref, '') LIKE ?{ph}
                   OR COALESCE(d.version, '') LIKE ?{ph}
                   OR COALESCE(d.labels, '') LIKE ?{ph}
                   OR d.content_json LIKE ?{ph}
                   OR COALESCE(a.artifact_id, '') LIKE ?{ph}
                   OR COALESCE(a.accepted_version_id, '') LIKE ?{ph}
                   OR EXISTS (
                       SELECT 1 FROM artifact_links l
                       WHERE (l.source_id = d.id OR l.target_id = d.id)
                         AND (l.source_id LIKE ?{ph}
                              OR l.target_id LIKE ?{ph}
                              OR COALESCE(l.source_version_id, '') LIKE ?{ph}
                              OR COALESCE(l.target_version_id, '') LIKE ?{ph}
                              OR l.link_type LIKE ?{ph})
                   ))"
        ));
    }
    sql.push_str(" ORDER BY d.updated_at DESC, d.app ASC, d.title ASC");

    let params_vec: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_vec.as_slice(), row_to_api_doc_summary)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_api_doc(conn: &Connection, project_ident: &str, id: &str) -> Result<Option<ApiDoc>> {
    ensure_existing_api_doc_artifacts(conn, project_ident)?;
    let sql = format!(
        "{} WHERE d.project_ident = ?1 AND d.id = ?2",
        api_doc_select_sql(true)
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![project_ident, id], row_to_api_doc)?;
    Ok(rows.next().transpose()?)
}

pub fn update_api_doc(
    conn: &Connection,
    project_ident: &str,
    id: &str,
    upd: &ApiDocUpdate<'_>,
) -> Result<Option<ApiDoc>> {
    let existing = match get_api_doc(conn, project_ident, id)? {
        Some(doc) => doc,
        None => {
            return Ok(None);
        }
    };

    let now = now_ms();
    let mut sets: Vec<String> = Vec::new();
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let push = |col: &str,
                val: Box<dyn rusqlite::ToSql>,
                sets: &mut Vec<String>,
                binds: &mut Vec<Box<dyn rusqlite::ToSql>>| {
        binds.push(val);
        sets.push(format!("{col} = ?{}", binds.len()));
    };

    if let Some(app) = upd.app {
        push("app", Box::new(app.to_string()), &mut sets, &mut binds);
    }
    if let Some(title) = upd.title {
        push("title", Box::new(title.to_string()), &mut sets, &mut binds);
    }
    if let Some(summary) = upd.summary {
        push(
            "summary",
            Box::new(summary.map(str::to_string)),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(kind) = upd.kind {
        push("kind", Box::new(kind.to_string()), &mut sets, &mut binds);
    }
    if let Some(source_format) = upd.source_format {
        push(
            "source_format",
            Box::new(source_format.to_string()),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(source_ref) = upd.source_ref {
        push(
            "source_ref",
            Box::new(source_ref.map(str::to_string)),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(version) = upd.version {
        push(
            "version",
            Box::new(version.map(str::to_string)),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(labels) = upd.labels {
        push(
            "labels",
            Box::new(serialize_string_array(labels)),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(content) = upd.content {
        push(
            "content_json",
            Box::new(serde_json::to_string(content)?),
            &mut sets,
            &mut binds,
        );
    }

    if sets.is_empty() {
        return get_api_doc(conn, project_ident, id);
    }
    push("updated_at", Box::new(now), &mut sets, &mut binds);
    binds.push(Box::new(project_ident.to_string()));
    let project_ph = binds.len();
    binds.push(Box::new(id.to_string()));
    let id_ph = binds.len();

    let sql = format!(
        "UPDATE api_docs SET {} WHERE project_ident = ?{project_ph} AND id = ?{id_ph}",
        sets.join(", ")
    );
    let params_vec: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    conn.execute(&sql, params_vec.as_slice())?;
    let updated = get_api_doc(conn, project_ident, id)?
        .ok_or_else(|| anyhow::anyhow!("updated api doc not found"))?;
    ensure_api_doc_artifact_version(
        conn,
        &ApiDocArtifactVersionInput {
            id,
            project_ident,
            app: &updated.app,
            title: &updated.title,
            summary: updated.summary.as_deref(),
            kind: &updated.kind,
            source_format: &updated.source_format,
            source_ref: updated.source_ref.as_deref(),
            version: updated.version.as_deref(),
            labels: &updated.labels,
            content: &updated.content,
            author: &existing.author,
            created_at: now,
        },
    )?;
    get_api_doc(conn, project_ident, id)
}

pub fn delete_api_doc(conn: &Connection, project_ident: &str, id: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM api_docs WHERE project_ident = ?1 AND id = ?2",
        params![project_ident, id],
    )?;
    if n > 0 {
        conn.execute(
            "UPDATE artifacts SET lifecycle_state = 'archived', updated_at = ?3
             WHERE project_ident = ?1 AND artifact_id = ?2",
            params![project_ident, id, now_ms()],
        )?;
    }
    Ok(n > 0)
}

pub fn list_api_doc_chunks(
    conn: &Connection,
    project_ident: &str,
    filters: &ApiDocFilters<'_>,
    include_history: bool,
) -> Result<ApiDocChunkList> {
    ensure_existing_api_doc_artifacts(conn, project_ident)?;
    let docs = list_api_docs(conn, project_ident, filters)?;
    let retrieval_scope = if include_history {
        "history"
    } else {
        "current"
    }
    .to_string();
    let mut chunks = Vec::new();
    let mut status = ApiDocChunkingStatus {
        status: "none".to_string(),
        current_chunk_count: 0,
        stale_chunk_count: 0,
        superseded_chunk_count: 0,
        failed_addresses: Vec::new(),
    };

    for doc in docs {
        let mut query = ArtifactQueryBuilder::new(
            "SELECT c.chunk_id, c.artifact_id, c.artifact_version_id, c.child_address,
                    c.text, c.kind, c.metadata_json, c.superseded_by_chunk_id,
                    c.created_at, a.accepted_version_id
             FROM artifact_chunks c
             JOIN artifacts a ON a.artifact_id = c.artifact_id
             WHERE c.artifact_id = ?1"
                .to_string(),
        )
        .with_bind(doc.artifact_id.clone());
        if !include_history {
            query.push_sql(" AND c.superseded_by_chunk_id IS NULL");
        }
        query.and_trimmed_like(
            |ph| {
                format!(
                    " AND (c.child_address LIKE ?{ph}
                           OR c.text LIKE ?{ph}
                           OR COALESCE(c.metadata_json, '') LIKE ?{ph})"
                )
            },
            filters.query,
        );
        query.push_sql(" ORDER BY c.created_at DESC, c.child_address ASC");
        let mut stmt = conn.prepare(&query.sql)?;
        let params_vec: Vec<&dyn rusqlite::ToSql> =
            query.binds.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(params_vec.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        for (
            chunk_id,
            artifact_id,
            artifact_version_id,
            child_address,
            text,
            chunk_kind,
            metadata_json,
            superseded_by_chunk_id,
            created_at,
            accepted_version_id,
        ) in rows
        {
            let metadata = metadata_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
            if let Some(label) = filters
                .label
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                let matches_doc = doc.labels.iter().any(|value| value == label);
                let matches_metadata = metadata
                    .as_ref()
                    .and_then(|value| value.get("labels"))
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|labels| labels.iter().any(|value| value.as_str() == Some(label)));
                if !matches_doc && !matches_metadata {
                    continue;
                }
            }
            let freshness = if superseded_by_chunk_id.is_some() {
                status.superseded_chunk_count += 1;
                "superseded_history"
            } else if accepted_version_id.as_deref() == Some(artifact_version_id.as_str()) {
                status.current_chunk_count += 1;
                "current"
            } else {
                status.stale_chunk_count += 1;
                "stale"
            };
            let failed = metadata.as_ref().and_then(|value| {
                value
                    .get("status")
                    .or_else(|| value.get("chunking_status"))
                    .and_then(serde_json::Value::as_str)
            }) == Some("failed");
            if failed {
                status.failed_addresses.push(child_address.clone());
            }
            chunks.push(ApiDocChunkRecord {
                doc_id: doc.id.clone(),
                project_ident: doc.project_ident.clone(),
                app: doc.app.clone(),
                title: doc.title.clone(),
                chunk_type: chunk_kind.unwrap_or_else(|| child_address.clone()),
                labels: doc.labels.clone(),
                text,
                updated_at: created_at,
                chunk_id,
                artifact_id,
                artifact_version_id: artifact_version_id.clone(),
                accepted_version_id: accepted_version_id.clone(),
                child_address,
                subkind: doc.subkind.clone(),
                freshness: freshness.to_string(),
                retrieval_scope: retrieval_scope.clone(),
                chunking_status: doc.chunking_status.clone(),
                linked_ids: doc.linked_ids.clone(),
            });
        }

        if doc.chunking_status == "partial" {
            status.status = "partial".to_string();
        }
    }

    if status.status != "partial" {
        status.status = if !status.failed_addresses.is_empty() {
            "partial".to_string()
        } else if status.stale_chunk_count > 0 {
            "stale".to_string()
        } else if status.current_chunk_count > 0 {
            "current".to_string()
        } else if status.superseded_chunk_count > 0 {
            "history_only".to_string()
        } else {
            "none".to_string()
        };
    }

    Ok(ApiDocChunkList {
        chunks,
        chunking_status: status,
        retrieval_scope,
        include_history,
    })
}

pub fn list_pattern_comments(
    conn: &Connection,
    pattern_id_or_slug: &str,
) -> Result<Option<Vec<PatternComment>>> {
    let pattern = match get_pattern(conn, pattern_id_or_slug)? {
        Some(p) => p,
        None => return Ok(None),
    };
    let mut stmt = conn.prepare_cached(
        "SELECT id, pattern_id, author, author_type, content, created_at
         FROM pattern_comments
         WHERE pattern_id = ?1
         ORDER BY created_at ASC",
    )?;
    let comments = stmt
        .query_map(params![pattern.id], |r| {
            Ok(PatternComment {
                id: r.get(0)?,
                pattern_id: r.get(1)?,
                author: r.get(2)?,
                author_type: r.get(3)?,
                content: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(comments))
}

pub fn insert_pattern_comment(
    conn: &Connection,
    pattern_id_or_slug: &str,
    author: &str,
    author_type: &str,
    content: &str,
) -> Result<Option<PatternComment>> {
    if author_type != "agent" && author_type != "user" && author_type != "system" {
        anyhow::bail!("invalid author_type '{author_type}': must be agent|user|system");
    }
    let pattern = match get_pattern(conn, pattern_id_or_slug)? {
        Some(p) => p,
        None => return Ok(None),
    };

    let id = new_uuid();
    let now = now_ms();
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO pattern_comments (id, pattern_id, author, author_type, content, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, pattern.id, author, author_type, content, now],
    )?;
    tx.execute(
        "UPDATE patterns SET updated_at = ?1 WHERE id = ?2",
        params![now, pattern.id],
    )?;
    tx.commit()?;

    Ok(Some(PatternComment {
        id,
        pattern_id: pattern.id,
        author: author.to_string(),
        author_type: author_type.to_string(),
        content: content.to_string(),
        created_at: now,
    }))
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        project_ident: row.get(1)?,
        title: row.get(2)?,
        description: row.get(3)?,
        details: row.get(4)?,
        status: row.get(5)?,
        rank: row.get(6)?,
        labels: parse_labels(row.get::<_, Option<String>>(7)?),
        hostname: row.get(8)?,
        owner_agent_id: row.get(9)?,
        reporter: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        started_at: row.get(13)?,
        done_at: row.get(14)?,
        kind: row.get(15)?,
        delegated_to_project_ident: row.get(16)?,
        delegated_to_task_id: row.get(17)?,
    })
}

const TASK_SELECT_COLS: &str =
    "id, project_ident, title, description, details, status, rank, labels, \
     hostname, owner_agent_id, reporter, created_at, updated_at, started_at, done_at, \
     kind, delegated_to_project_ident, delegated_to_task_id";

/// Reclaim any task in this project that has been `in_progress` for longer than
/// [`TASK_RECLAIM_MS`] without any `updated_at` activity. Reclaimed tasks are
/// flipped back to `todo`, the owner is cleared, and a system comment is
/// appended so the next agent knows to verify prior progress. Runs in a single
/// transaction.
pub fn reclaim_stale_tasks(conn: &Connection, project_ident: &str) -> Result<usize> {
    let now = now_ms();
    let cutoff = now - TASK_RECLAIM_MS;

    let tx = conn.unchecked_transaction()?;
    let stale_ids: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT id FROM tasks
             WHERE project_ident = ?1
               AND status = 'in_progress'
               AND started_at IS NOT NULL
               AND started_at < ?2
               AND updated_at < ?2",
        )?;
        let rows = stmt
            .query_map(params![project_ident, cutoff], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    if stale_ids.is_empty() {
        tx.commit()?;
        return Ok(0);
    }

    let comment_body = "Reclaimed after 1h of inactivity. Next agent please \
                        verify prior progress before continuing.";

    for task_id in &stale_ids {
        tx.execute(
            "INSERT INTO task_comments (id, task_id, author, author_type, content, created_at)
             VALUES (?1, ?2, 'system', 'system', ?3, ?4)",
            params![new_uuid(), task_id, comment_body, now],
        )?;
        tx.execute(
            "UPDATE tasks
             SET status = 'todo',
                 owner_agent_id = NULL,
                 started_at = NULL,
                 updated_at = ?1
             WHERE id = ?2",
            params![now, task_id],
        )?;
    }

    tx.commit()?;
    Ok(stale_ids.len())
}

/// Insert a new task in the `todo` column. Rank is auto-assigned as
/// `MAX(rank) + 1` among existing `todo` rows for this project.
#[allow(clippy::too_many_arguments)]
pub fn insert_task(
    conn: &Connection,
    project_ident: &str,
    title: &str,
    description: Option<&str>,
    details: Option<&str>,
    labels: &[String],
    hostname: Option<&str>,
    reporter: &str,
) -> Result<Task> {
    let id = new_uuid();
    let now = now_ms();
    let labels_json = serialize_string_array(labels);

    let rank: i64 = conn.query_row(
        "SELECT COALESCE(MAX(rank), 0) + 1 FROM tasks
         WHERE project_ident = ?1 AND status = 'todo'",
        params![project_ident],
        |r| r.get(0),
    )?;

    conn.execute(
        "INSERT INTO tasks (
             id, project_ident, title, description, details, status, rank,
             labels, hostname, owner_agent_id, reporter,
             created_at, updated_at, started_at, done_at, kind,
             delegated_to_project_ident, delegated_to_task_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'todo', ?6, ?7, ?8, NULL, ?9, ?10, ?10, NULL, NULL, 'normal', NULL, NULL)",
        params![
            id,
            project_ident,
            title,
            description,
            details,
            rank,
            labels_json,
            hostname,
            reporter,
            now,
        ],
    )?;

    Ok(Task {
        id,
        project_ident: project_ident.to_string(),
        title: title.to_string(),
        description: description.map(str::to_string),
        details: details.map(str::to_string),
        status: "todo".to_string(),
        rank,
        labels: labels.to_vec(),
        hostname: hostname.map(str::to_string),
        owner_agent_id: None,
        reporter: reporter.to_string(),
        created_at: now,
        updated_at: now,
        started_at: None,
        done_at: None,
        kind: "normal".to_string(),
        delegated_to_project_ident: None,
        delegated_to_task_id: None,
    })
}

pub fn insert_delegated_task(conn: &Connection, task: &DelegatedTaskInsert<'_>) -> Result<Task> {
    let mut inserted = insert_task(
        conn,
        task.project_ident,
        task.title,
        task.description,
        task.details,
        task.labels,
        task.hostname,
        task.reporter,
    )?;
    conn.execute(
        "UPDATE tasks
         SET kind = 'delegated',
             delegated_to_project_ident = ?1,
             delegated_to_task_id = ?2
         WHERE id = ?3 AND project_ident = ?4",
        params![
            task.target_project_ident,
            task.target_task_id,
            inserted.id,
            task.project_ident
        ],
    )?;
    inserted.kind = "delegated".to_string();
    inserted.delegated_to_project_ident = Some(task.target_project_ident.to_string());
    inserted.delegated_to_task_id = Some(task.target_task_id.to_string());
    Ok(inserted)
}

/// List task summaries for a project filtered by status. When `statuses`
/// contains `"done"` and `include_stale_done` is false, only done tasks whose
/// `done_at > now - 7d` are returned. Results are sorted by `rank ASC` then
/// `updated_at DESC`.
pub fn list_tasks(
    conn: &Connection,
    project_ident: &str,
    statuses: &[String],
    include_stale_done: bool,
) -> Result<Vec<TaskSummary>> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }

    // Build an IN (?, ?, ...) clause. Placeholders start at 2 because
    // placeholder 1 is project_ident. A trailing `done_cutoff` placeholder is
    // appended when we need to filter stale-done rows.
    let mut placeholders = Vec::with_capacity(statuses.len());
    for i in 0..statuses.len() {
        placeholders.push(format!("?{}", i + 2));
    }
    let in_clause = placeholders.join(",");

    let has_done = statuses.iter().any(|s| s == "done");
    let apply_done_filter = has_done && !include_stale_done;

    let sql = if apply_done_filter {
        let cutoff_ph = statuses.len() + 2;
        format!(
            "SELECT t.id, t.title, t.status, t.rank, t.labels,
                    t.owner_agent_id, t.hostname, t.reporter,
                    (SELECT COUNT(*) FROM task_comments tc WHERE tc.task_id = t.id),
                    t.created_at, t.updated_at, t.kind,
                    t.delegated_to_project_ident, t.delegated_to_task_id
             FROM tasks t
             WHERE t.project_ident = ?1
               AND t.status IN ({in_clause})
               AND (t.status != 'done' OR (t.done_at IS NOT NULL AND t.done_at > ?{cutoff_ph}))
             ORDER BY t.rank ASC, t.updated_at DESC"
        )
    } else {
        format!(
            "SELECT t.id, t.title, t.status, t.rank, t.labels,
                    t.owner_agent_id, t.hostname, t.reporter,
                    (SELECT COUNT(*) FROM task_comments tc WHERE tc.task_id = t.id),
                    t.created_at, t.updated_at, t.kind,
                    t.delegated_to_project_ident, t.delegated_to_task_id
             FROM tasks t
             WHERE t.project_ident = ?1
               AND t.status IN ({in_clause})
             ORDER BY t.rank ASC, t.updated_at DESC"
        )
    };

    // Bind params: project_ident, statuses..., [done_cutoff]
    let mut bound: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(statuses.len() + 2);
    bound.push(Box::new(project_ident.to_string()));
    for s in statuses {
        bound.push(Box::new(s.clone()));
    }
    if apply_done_filter {
        bound.push(Box::new(now_ms() - TASK_DONE_FALLOFF_MS));
    }
    let params_vec: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b.as_ref()).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_vec.as_slice(), |r| {
            Ok(TaskSummary {
                id: r.get(0)?,
                title: r.get(1)?,
                status: r.get(2)?,
                rank: r.get(3)?,
                labels: parse_labels(r.get::<_, Option<String>>(4)?),
                owner_agent_id: r.get(5)?,
                hostname: r.get(6)?,
                reporter: r.get(7)?,
                comment_count: r.get(8)?,
                created_at: r.get(9)?,
                updated_at: r.get(10)?,
                kind: r.get(11)?,
                delegated_to_project_ident: r.get(12)?,
                delegated_to_task_id: r.get(13)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(rows)
}

/// Fetch a task and all of its comments, scoped by `project_ident` for safety.
/// Comments are ordered ascending by `created_at`.
pub fn get_task_detail(
    conn: &Connection,
    project_ident: &str,
    task_id: &str,
) -> Result<Option<TaskDetail>> {
    let task = {
        let sql =
            format!("SELECT {TASK_SELECT_COLS} FROM tasks WHERE id = ?1 AND project_ident = ?2");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query_map(params![task_id, project_ident], row_to_task)?;
        match rows.next() {
            Some(r) => r?,
            None => return Ok(None),
        }
    };

    let mut stmt = conn.prepare_cached(
        "SELECT id, task_id, author, author_type, content, created_at
         FROM task_comments
         WHERE task_id = ?1
         ORDER BY created_at ASC",
    )?;
    let comments = stmt
        .query_map(params![task_id], |r| {
            Ok(TaskComment {
                id: r.get(0)?,
                task_id: r.get(1)?,
                author: r.get(2)?,
                author_type: r.get(3)?,
                content: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(Some(TaskDetail { task, comments }))
}

/// Compatibility lookup for T009 generated spec tasks.
///
/// The current task schema has no dedicated source tuple columns. The spec
/// workflow therefore embeds `source_spec_artifact_id`,
/// `source_spec_version_id`, and `manifest_item_id` in the task details and
/// writes the durable graph edge as an artifact link. If a task insert
/// succeeds but the link write is interrupted, this lookup lets reruns recover
/// the existing task and add the missing link instead of duplicating work.
pub fn find_task_by_spec_source(
    conn: &Connection,
    project_ident: &str,
    source_spec_artifact_id: &str,
    source_spec_version_id: &str,
    manifest_item_id: &str,
) -> Result<Option<Task>> {
    let sql = format!(
        "SELECT {TASK_SELECT_COLS} FROM tasks
         WHERE project_ident = ?1
           AND details LIKE ?2
           AND details LIKE ?3
           AND details LIKE ?4
         ORDER BY created_at ASC
         LIMIT 1"
    );
    let source_artifact_pattern = format!("%source_spec_artifact_id: {source_spec_artifact_id}%");
    let source_version_pattern = format!("%source_spec_version_id: {source_spec_version_id}%");
    let manifest_item_pattern = format!("%manifest_item_id: {manifest_item_id}%");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(
        params![
            project_ident,
            source_artifact_pattern,
            source_version_pattern,
            manifest_item_pattern,
        ],
        row_to_task,
    )?;
    Ok(rows.next().transpose()?)
}

/// Apply a partial update to a task, enforcing status-transition side effects
/// (auto-set `started_at`/`owner_agent_id` on todo→in_progress, `done_at` on
/// `* → done`, clearing of timestamps on reverse transitions, etc.). Returns
/// the refreshed task, or `Ok(None)` if no such task exists in that project.
/// Invalid status strings or impossible transitions bubble up via
/// `anyhow::bail!`.
pub fn update_task(
    conn: &Connection,
    project_ident: &str,
    task_id: &str,
    upd: &TaskUpdate<'_>,
    actor_agent_id: Option<&str>,
) -> Result<Option<Task>> {
    // Validate requested status early.
    if let Some(s) = upd.status {
        if s != "todo" && s != "in_progress" && s != "done" {
            anyhow::bail!("invalid status '{s}': must be todo|in_progress|done");
        }
    }

    // Load current state scoped to project for safety.
    let current = {
        let sql =
            format!("SELECT {TASK_SELECT_COLS} FROM tasks WHERE id = ?1 AND project_ident = ?2");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query_map(params![task_id, project_ident], row_to_task)?;
        match rows.next() {
            Some(r) => r?,
            None => return Ok(None),
        }
    };

    let now = now_ms();
    let new_status = upd.status.unwrap_or(&current.status).to_string();
    let owner_explicit = upd.owner_agent_id.is_some();
    let transitioning = upd.status.is_some() && new_status != current.status;

    // Compute derived fields based on the transition.
    // started_at: Some(Some(v)) = set, Some(None) = clear, None = leave alone
    #[allow(clippy::type_complexity)]
    let mut started_at: Option<Option<i64>> = None;
    let mut done_at: Option<Option<i64>> = None;
    // owner: mirrors TaskUpdate's owner semantics, starting from upd.owner_agent_id.
    let mut owner: Option<Option<String>> =
        upd.owner_agent_id.map(|inner| inner.map(|s| s.to_string()));

    if transitioning {
        match (current.status.as_str(), new_status.as_str()) {
            ("todo", "in_progress") => {
                started_at = Some(Some(now));
                done_at = Some(None);
                if !owner_explicit {
                    if let Some(aid) = actor_agent_id {
                        owner = Some(Some(aid.to_string()));
                    }
                }
            }
            ("in_progress", "todo") => {
                started_at = Some(None);
                if !owner_explicit {
                    owner = Some(None);
                }
            }
            (_, "done") => {
                done_at = Some(Some(now));
            }
            ("done", "todo") | ("done", "in_progress") => {
                done_at = Some(None);
                if new_status == "in_progress" {
                    started_at = Some(Some(now));
                    if !owner_explicit {
                        if let Some(aid) = actor_agent_id {
                            owner = Some(Some(aid.to_string()));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Build dynamic UPDATE.
    let mut sets: Vec<String> = Vec::new();
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    let push = |col: &str,
                val: Box<dyn rusqlite::ToSql>,
                sets: &mut Vec<String>,
                binds: &mut Vec<Box<dyn rusqlite::ToSql>>| {
        binds.push(val);
        sets.push(format!("{col} = ?{}", binds.len()));
    };

    if let Some(title) = upd.title {
        push("title", Box::new(title.to_string()), &mut sets, &mut binds);
    }
    if let Some(desc) = upd.description {
        push(
            "description",
            Box::new(desc.map(str::to_string)),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(det) = upd.details {
        push(
            "details",
            Box::new(det.map(str::to_string)),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(labels) = upd.labels {
        push(
            "labels",
            Box::new(serialize_string_array(labels)),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(host) = upd.hostname {
        push(
            "hostname",
            Box::new(host.map(str::to_string)),
            &mut sets,
            &mut binds,
        );
    }
    if upd.status.is_some() {
        push(
            "status",
            Box::new(new_status.clone()),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(rank) = upd.rank {
        push("rank", Box::new(rank), &mut sets, &mut binds);
    }
    if let Some(owner_val) = owner {
        push("owner_agent_id", Box::new(owner_val), &mut sets, &mut binds);
    }
    if let Some(started) = started_at {
        push("started_at", Box::new(started), &mut sets, &mut binds);
    }
    if let Some(done) = done_at {
        push("done_at", Box::new(done), &mut sets, &mut binds);
    }

    // Always bump updated_at.
    push("updated_at", Box::new(now), &mut sets, &mut binds);

    if sets.is_empty() {
        // Nothing to update — just return the current row.
        return Ok(Some(current));
    }

    // WHERE bindings come last.
    binds.push(Box::new(task_id.to_string()));
    let id_ph = binds.len();
    binds.push(Box::new(project_ident.to_string()));
    let proj_ph = binds.len();

    let sql = format!(
        "UPDATE tasks SET {} WHERE id = ?{} AND project_ident = ?{}",
        sets.join(", "),
        id_ph,
        proj_ph,
    );

    let params_vec: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let n = conn.execute(&sql, params_vec.as_slice())?;
    if n == 0 {
        return Ok(None);
    }

    // Re-read the row to return a fully-consistent Task.
    let sql = format!("SELECT {TASK_SELECT_COLS} FROM tasks WHERE id = ?1 AND project_ident = ?2");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![task_id, project_ident], row_to_task)?;
    Ok(rows.next().transpose()?)
}

/// Append a comment to a task and bump the parent task's `updated_at`. Runs
/// inside a single transaction. `author_type` must be `"agent"`, `"user"`, or
/// `"system"`.
pub fn insert_comment(
    conn: &Connection,
    task_id: &str,
    author: &str,
    author_type: &str,
    content: &str,
) -> Result<TaskComment> {
    if author_type != "agent" && author_type != "user" && author_type != "system" {
        anyhow::bail!("invalid author_type '{author_type}': must be agent|user|system");
    }

    let id = new_uuid();
    let now = now_ms();

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO task_comments (id, task_id, author, author_type, content, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, task_id, author, author_type, content, now],
    )?;
    tx.execute(
        "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
        params![now, task_id],
    )?;
    tx.commit()?;

    Ok(TaskComment {
        id,
        task_id: task_id.to_string(),
        author: author.to_string(),
        author_type: author_type.to_string(),
        content: content.to_string(),
        created_at: now,
    })
}

fn row_to_delegation(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskDelegation> {
    Ok(TaskDelegation {
        id: row.get(0)?,
        source_project_ident: row.get(1)?,
        source_task_id: row.get(2)?,
        target_project_ident: row.get(3)?,
        target_task_id: row.get(4)?,
        requester_agent_id: row.get(5)?,
        requester_hostname: row.get(6)?,
        created_at: row.get(7)?,
        completed_at: row.get(8)?,
        completion_message_id: row.get(9)?,
    })
}

pub fn insert_task_delegation(
    conn: &Connection,
    source_project_ident: &str,
    source_task_id: &str,
    target_project_ident: &str,
    target_task_id: &str,
    requester_agent_id: Option<&str>,
    requester_hostname: Option<&str>,
) -> Result<TaskDelegation> {
    let id = new_uuid();
    let now = now_ms();
    conn.execute(
        "INSERT INTO task_delegations (
            id, source_project_ident, source_task_id, target_project_ident,
            target_task_id, requester_agent_id, requester_hostname, created_at,
            completed_at, completion_message_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL)",
        params![
            id,
            source_project_ident,
            source_task_id,
            target_project_ident,
            target_task_id,
            requester_agent_id,
            requester_hostname,
            now,
        ],
    )?;
    Ok(TaskDelegation {
        id,
        source_project_ident: source_project_ident.to_string(),
        source_task_id: source_task_id.to_string(),
        target_project_ident: target_project_ident.to_string(),
        target_task_id: target_task_id.to_string(),
        requester_agent_id: requester_agent_id.map(str::to_string),
        requester_hostname: requester_hostname.map(str::to_string),
        created_at: now,
        completed_at: None,
        completion_message_id: None,
    })
}

pub fn get_delegation_by_source(
    conn: &Connection,
    project_ident: &str,
    task_id: &str,
) -> Result<Option<TaskDelegation>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, source_project_ident, source_task_id, target_project_ident,
                target_task_id, requester_agent_id, requester_hostname, created_at,
                completed_at, completion_message_id
         FROM task_delegations
         WHERE source_project_ident = ?1 AND source_task_id = ?2",
    )?;
    let delegation = stmt
        .query_map(params![project_ident, task_id], row_to_delegation)?
        .next()
        .transpose()?;
    Ok(delegation)
}

pub fn get_delegation_by_target(
    conn: &Connection,
    project_ident: &str,
    task_id: &str,
) -> Result<Option<TaskDelegation>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, source_project_ident, source_task_id, target_project_ident,
                target_task_id, requester_agent_id, requester_hostname, created_at,
                completed_at, completion_message_id
         FROM task_delegations
         WHERE target_project_ident = ?1 AND target_task_id = ?2",
    )?;
    let delegation = stmt
        .query_map(params![project_ident, task_id], row_to_delegation)?
        .next()
        .transpose()?;
    Ok(delegation)
}

pub fn mark_delegation_complete(
    conn: &Connection,
    delegation_id: &str,
    completion_message_id: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE task_delegations
         SET completed_at = COALESCE(completed_at, ?1),
             completion_message_id = COALESCE(completion_message_id, ?2)
         WHERE id = ?3",
        params![now_ms(), completion_message_id, delegation_id],
    )?;
    Ok(())
}

/// Delete a task (and its comments via ON DELETE CASCADE) scoped to a project.
/// Returns true if a row was removed.
pub fn delete_task(conn: &Connection, project_ident: &str, task_id: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM tasks WHERE id = ?1 AND project_ident = ?2",
        params![task_id, project_ident],
    )?;
    Ok(n > 0)
}

/// Current row snapshot read at the top of `reorder_tasks_in_column`:
/// `(status, owner_agent_id, started_at, done_at)`.
type TaskStateSnapshot = (String, Option<String>, Option<i64>, Option<i64>);

/// Apply a client-driven order to one status column. For each id in `order`,
/// set `status = target_status` and `rank = index` (0-based). Any status
/// transition also maintains the invariants enforced by [`update_task`]:
///
/// - transitioning into `in_progress` sets `started_at = now`, auto-assigns
///   `owner_agent_id = actor_agent_id` (when provided and the current owner
///   is `NULL`),
/// - transitioning into `done` sets `done_at = now`,
/// - transitioning out of `done` clears `done_at`,
/// - transitioning out of `in_progress` clears `started_at` and clears
///   `owner_agent_id`.
///
/// All writes happen inside a single transaction; any id that does not exist
/// in this project causes the whole batch to be rolled back via
/// `anyhow::bail!`.
pub fn reorder_tasks_in_column(
    conn: &Connection,
    project_ident: &str,
    target_status: &str,
    order: &[String],
    actor_agent_id: Option<&str>,
) -> Result<()> {
    if target_status != "todo" && target_status != "in_progress" && target_status != "done" {
        anyhow::bail!("invalid status '{target_status}': must be todo|in_progress|done");
    }

    if order.is_empty() {
        return Ok(());
    }

    let now = now_ms();
    let tx = conn.unchecked_transaction()?;

    for (idx, task_id) in order.iter().enumerate() {
        // Fetch current status + owner for this row, scoped to the project.
        let current: Option<TaskStateSnapshot> = {
            let mut stmt = tx.prepare(
                "SELECT status, owner_agent_id, started_at, done_at
                 FROM tasks WHERE id = ?1 AND project_ident = ?2",
            )?;
            let mut rows = stmt.query_map(params![task_id, project_ident], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            })?;
            rows.next().transpose()?
        };

        let (old_status, old_owner, old_started_at, old_done_at) = match current {
            Some(v) => v,
            None => anyhow::bail!("task '{task_id}' not found in project '{project_ident}'"),
        };

        // Compute new timestamps + owner by mirroring the transition logic
        // in `update_task`. Fields we do not change are preserved explicitly
        // (not cleared) — this handler only touches status/rank/timestamps.
        let mut new_started_at = old_started_at;
        let mut new_done_at = old_done_at;
        let mut new_owner = old_owner.clone();

        if old_status != target_status {
            match (old_status.as_str(), target_status) {
                ("todo", "in_progress") => {
                    new_started_at = Some(now);
                    new_done_at = None;
                    if new_owner.is_none() {
                        if let Some(aid) = actor_agent_id {
                            new_owner = Some(aid.to_string());
                        }
                    }
                }
                ("in_progress", "todo") => {
                    new_started_at = None;
                    new_owner = None;
                }
                ("in_progress", "done") => {
                    new_done_at = Some(now);
                    // started_at stays put as a historical record of the
                    // in_progress window; `update_task` behaves the same way
                    // when transitioning directly to done.
                }
                ("todo", "done") => {
                    new_done_at = Some(now);
                }
                ("done", "todo") => {
                    new_done_at = None;
                }
                ("done", "in_progress") => {
                    new_done_at = None;
                    new_started_at = Some(now);
                    if new_owner.is_none() {
                        if let Some(aid) = actor_agent_id {
                            new_owner = Some(aid.to_string());
                        }
                    }
                }
                _ => {}
            }
        }

        tx.execute(
            "UPDATE tasks
             SET status = ?1,
                 rank = ?2,
                 started_at = ?3,
                 done_at = ?4,
                 owner_agent_id = ?5,
                 updated_at = ?6
             WHERE id = ?7 AND project_ident = ?8",
            params![
                target_status,
                idx as i64,
                new_started_at,
                new_done_at,
                new_owner,
                now,
                task_id,
                project_ident,
            ],
        )?;
    }

    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        apply_schema(&conn).unwrap();
        conn
    }

    fn test_project(ident: &str) -> Project {
        Project {
            ident: ident.into(),
            channel_name: "test".into(),
            room_id: format!("room-{ident}"),
            last_msg_id: None,
            created_at: now_ms(),
            repo_provider: None,
            repo_namespace: None,
            repo_name: None,
            repo_full_name: None,
        }
    }

    fn test_message(ident: &str, source: &str, content: &str, agent_id: Option<&str>) -> Message {
        Message {
            id: 0,
            project_ident: ident.into(),
            source: source.into(),
            external_message_id: None,
            content: content.into(),
            sent_at: now_ms(),
            confirmed_at: None,
            parent_message_id: None,
            agent_id: agent_id.map(str::to_string),
            message_type: "message".into(),
            subject: None,
            hostname: None,
            event_at: None,
            deliver_to_agents: source == "user",
        }
    }

    #[test]
    fn unconfirmed_for_agent_only_returns_unread_user_messages() {
        let conn = test_conn();
        insert_project(&conn, &test_project("proj")).unwrap();
        upsert_agent(&conn, "proj", "agent-a").unwrap();

        let first_user = insert_message(
            &conn,
            &test_message("proj", "user", "please handle this", None),
        )
        .unwrap();
        let agent_message = insert_message(
            &conn,
            &test_message("proj", "agent", "system/agent status", Some("agent-b")),
        )
        .unwrap();
        let second_user =
            insert_message(&conn, &test_message("proj", "user", "also this", None)).unwrap();

        assert!(confirm_message_for_agent(&conn, "proj", "agent-a", first_user).unwrap());

        let unread = get_unconfirmed_for_agent(&conn, "proj", "agent-a").unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, second_user);
        assert_eq!(unread[0].source, "user");
        assert_ne!(unread[0].id, agent_message);
    }

    #[test]
    fn unconfirmed_for_agent_includes_deliverable_system_messages() {
        let conn = test_conn();
        insert_project(&conn, &test_project("proj")).unwrap();
        upsert_agent(&conn, "proj", "agent-a").unwrap();

        let hidden_system = insert_message(
            &conn,
            &Message {
                deliver_to_agents: false,
                source: "system".into(),
                ..test_message("proj", "user", "internal bookkeeping", None)
            },
        )
        .unwrap();
        let delivered_system = insert_message(
            &conn,
            &Message {
                deliver_to_agents: true,
                source: "system".into(),
                ..test_message("proj", "user", "delegated task created", None)
            },
        )
        .unwrap();

        let unread = get_unconfirmed_for_agent(&conn, "proj", "agent-a").unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, delivered_system);
        assert_ne!(unread[0].id, hidden_system);
    }

    #[test]
    fn schema_migration_preserves_confirmations_when_system_source_is_added() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE projects (
                ident TEXT PRIMARY KEY,
                channel_name TEXT NOT NULL,
                room_id TEXT NOT NULL,
                last_msg_id TEXT,
                created_at INTEGER NOT NULL
             );
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_ident TEXT NOT NULL REFERENCES projects(ident),
                source TEXT NOT NULL CHECK(source IN ('agent','user')),
                external_message_id TEXT,
                content TEXT NOT NULL,
                sent_at INTEGER NOT NULL,
                confirmed_at INTEGER,
                parent_message_id INTEGER,
                agent_id TEXT,
                message_type TEXT NOT NULL DEFAULT 'message',
                subject TEXT,
                hostname TEXT,
                event_at INTEGER,
                deliver_to_agents INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE agent_confirmations (
                agent_id TEXT NOT NULL,
                project_ident TEXT NOT NULL,
                message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                confirmed_at INTEGER NOT NULL,
                PRIMARY KEY (agent_id, project_ident, message_id)
             );
             INSERT INTO projects VALUES ('proj', 'test', 'room-proj', NULL, 1);
             INSERT INTO messages (
                id, project_ident, source, content, sent_at, confirmed_at
             ) VALUES (1, 'proj', 'user', 'already read', 1, 1);
             INSERT INTO agent_confirmations VALUES ('agent-a', 'proj', 1, 1);",
        )
        .unwrap();

        apply_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO messages (project_ident, source, content, sent_at, deliver_to_agents)
             VALUES ('proj', 'system', 'delegated', 2, 1)",
            [],
        )
        .unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_confirmations
                 WHERE message_id = 1 AND agent_id = 'agent-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        let fk_errors: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk_errors, 0);
    }

    #[test]
    fn task_delegation_links_source_and_target_tasks() {
        let conn = test_conn();
        insert_project(&conn, &test_project("source")).unwrap();
        insert_project(&conn, &test_project("target")).unwrap();

        let target = insert_task(
            &conn,
            "target",
            "Build dependency",
            Some("Context"),
            Some("Specification"),
            &[],
            None,
            "tester",
        )
        .unwrap();
        let source = insert_delegated_task(
            &conn,
            &DelegatedTaskInsert {
                project_ident: "source",
                title: "Build dependency (DELEGATED)",
                description: Some("Context"),
                details: Some("Specification"),
                labels: &[],
                hostname: None,
                reporter: "tester",
                target_project_ident: "target",
                target_task_id: &target.id,
            },
        )
        .unwrap();
        let delegation = insert_task_delegation(
            &conn,
            "source",
            &source.id,
            "target",
            &target.id,
            Some("agent-a"),
            Some("host-a"),
        )
        .unwrap();

        assert_eq!(source.kind, "delegated");
        assert_eq!(source.delegated_to_project_ident.as_deref(), Some("target"));
        assert_eq!(
            source.delegated_to_task_id.as_deref(),
            Some(target.id.as_str())
        );
        assert_eq!(
            get_delegation_by_source(&conn, "source", &source.id)
                .unwrap()
                .unwrap()
                .id,
            delegation.id
        );
        assert_eq!(
            get_delegation_by_target(&conn, "target", &target.id)
                .unwrap()
                .unwrap()
                .id,
            delegation.id
        );
    }

    #[test]
    fn project_task_stats_sort_by_active_work_counts() {
        let conn = test_conn();
        insert_project(&conn, &test_project("alpha")).unwrap();
        insert_project(&conn, &test_project("bravo")).unwrap();
        insert_project(&conn, &test_project("charlie")).unwrap();

        let bravo_todo_1 =
            insert_task(&conn, "bravo", "b1", None, None, &[], None, "tester").unwrap();
        let bravo_todo_2 =
            insert_task(&conn, "bravo", "b2", None, None, &[], None, "tester").unwrap();
        let alpha_todo =
            insert_task(&conn, "alpha", "a1", None, None, &[], None, "tester").unwrap();
        let charlie_done =
            insert_task(&conn, "charlie", "c1", None, None, &[], None, "tester").unwrap();

        let in_progress = TaskUpdate {
            status: Some("in_progress"),
            owner_agent_id: None,
            rank: None,
            title: None,
            description: None,
            details: None,
            labels: None,
            hostname: None,
        };
        update_task(
            &conn,
            "alpha",
            &alpha_todo.id,
            &in_progress,
            Some("agent-a"),
        )
        .unwrap();

        let done = TaskUpdate {
            status: Some("done"),
            ..in_progress
        };
        update_task(&conn, "charlie", &charlie_done.id, &done, Some("agent-a")).unwrap();

        let stats = list_project_task_stats(&conn).unwrap();
        let idents = stats.into_iter().map(|s| s.ident).collect::<Vec<_>>();
        assert_eq!(idents, vec!["bravo", "alpha", "charlie"]);
        assert_eq!(bravo_todo_1.status, "todo");
        assert_eq!(bravo_todo_2.status, "todo");
    }

    #[test]
    fn pattern_get_does_not_include_comments_and_comments_are_explicit() {
        let conn = test_conn();
        let pattern = insert_pattern(
            &conn,
            "Deploying Eventic Applications",
            None,
            Some("Main deploys dev and tags deploy prod."),
            "# Deploying Eventic Applications\n\nUse main and tags.",
            &["eventic".into(), "deploy".into()],
            "draft",
            "active",
            None,
            "tester",
        )
        .unwrap();

        let comment =
            insert_pattern_comment(&conn, &pattern.id, "reviewer", "user", "Clarify tag gates.")
                .unwrap()
                .unwrap();

        let fetched = get_pattern(&conn, &pattern.slug).unwrap().unwrap();
        assert_eq!(fetched.body, pattern.body);
        assert_eq!(fetched.labels, vec!["eventic", "deploy"]);

        let comments = list_pattern_comments(&conn, &pattern.slug)
            .unwrap()
            .unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, comment.id);
        assert_eq!(comments[0].content, "Clarify tag gates.");
    }

    #[test]
    fn pattern_search_checks_markdown_body_and_summary_counts_comments() {
        let conn = test_conn();
        let pattern = insert_pattern(
            &conn,
            "Settings Encryption",
            Some("settings-encryption"),
            Some("Encrypt stored settings."),
            "Use envelope encryption for sensitive values.",
            &["security".into()],
            "latest",
            "active",
            None,
            "tester",
        )
        .unwrap();
        insert_pattern_comment(
            &conn,
            &pattern.id,
            "reviewer",
            "user",
            "Add key rotation notes.",
        )
        .unwrap()
        .unwrap();

        let results = list_patterns(
            &conn,
            &PatternFilters {
                query: Some("envelope"),
                ..PatternFilters::default()
            },
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].slug, "settings-encryption");
        assert_eq!(results[0].version, "latest");
        assert_eq!(results[0].state, "active");
        assert_eq!(results[0].comment_count, 1);
    }

    #[test]
    fn pattern_lifecycle_metadata_is_required_and_searchable() {
        let conn = test_conn();
        let old = insert_pattern(
            &conn,
            "Old Systemd Build Units",
            Some("old-systemd-build-units"),
            Some("Compile in the service unit."),
            "Do not use for new deployments.",
            &["linux".into(), "systemd".into(), "services".into()],
            "superseded",
            "active",
            Some("eventic-build-units"),
            "tester",
        )
        .unwrap();

        assert!(insert_pattern(
            &conn,
            "Bad Version",
            Some("bad-version"),
            None,
            "Invalid lifecycle.",
            &[],
            "v1",
            "active",
            None,
            "tester",
        )
        .is_err());

        let fetched = get_pattern(&conn, &old.slug).unwrap().unwrap();
        assert_eq!(fetched.version, "superseded");
        assert_eq!(fetched.state, "active");
        assert_eq!(
            fetched.superseded_by.as_deref(),
            Some("eventic-build-units")
        );

        let results = list_patterns(
            &conn,
            &PatternFilters {
                label: Some("systemd"),
                version: Some("superseded"),
                ..PatternFilters::default()
            },
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].slug, "old-systemd-build-units");

        let results = list_patterns(
            &conn,
            &PatternFilters {
                superseded_by: Some("eventic-build-units"),
                ..PatternFilters::default()
            },
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].version, "superseded");
    }

    #[test]
    fn api_docs_store_docs_first_context_and_search_content() {
        let conn = test_conn();
        insert_project(&conn, &test_project("billing")).unwrap();

        let content = serde_json::json!({
            "purpose": "Owns invoice state.",
            "workflows": [{
                "name": "Create invoice",
                "steps": ["Create draft", "Finalize after confirmation"]
            }],
            "endpoints": [{
                "method": "POST",
                "path": "/v1/invoices",
                "intent": "Create a draft invoice"
            }]
        });
        let doc = insert_api_doc(
            &conn,
            "billing",
            &ApiDocInsert {
                app: "billing-api",
                title: "Billing API agent context",
                summary: Some("System of record for invoices."),
                kind: "agent_context",
                source_format: "agent_context",
                source_ref: Some(".agent/api/billing.yaml"),
                version: Some("2026-04-28"),
                labels: &["billing".into(), "invoices".into()],
                content: &content,
                author: "tester",
            },
        )
        .unwrap();

        let fetched = get_api_doc(&conn, "billing", &doc.id).unwrap().unwrap();
        assert_eq!(fetched.app, "billing-api");
        assert_eq!(fetched.content["purpose"], "Owns invoice state.");

        let results = list_api_docs(
            &conn,
            "billing",
            &ApiDocFilters {
                query: Some("draft invoice"),
                label: Some("invoices"),
                ..ApiDocFilters::default()
            },
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, doc.id);

        let updated = update_api_doc(
            &conn,
            "billing",
            &doc.id,
            &ApiDocUpdate {
                summary: Some(Some("Updated agent guidance.")),
                version: Some(None),
                ..ApiDocUpdate::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(updated.summary.as_deref(), Some("Updated agent guidance."));
        assert_eq!(updated.version, None);

        let project_stats = list_project_stats(&conn).unwrap();
        assert_eq!(project_stats[0].api_doc_count, 1);
        let dashboard = get_dashboard_data(&conn).unwrap();
        assert_eq!(dashboard.api_doc_count, 1);
    }

    #[test]
    fn api_docs_preserve_legacy_kind_and_publish_artifact_backed_chunks() {
        let conn = test_conn();
        insert_project(&conn, &test_project("billing")).unwrap();

        let content_v1 = serde_json::json!({
            "purpose": "Owns invoice state.",
            "endpoints": [{
                "method": "POST",
                "path": "/v1/invoices",
                "intent": "Create a draft invoice"
            }]
        });
        let doc = insert_api_doc(
            &conn,
            "billing",
            &ApiDocInsert {
                app: "billing-api",
                title: "Billing API agent context",
                summary: Some("System of record for invoices."),
                kind: "agent_context",
                source_format: "agent_context",
                source_ref: Some(".agent/api/billing.yaml"),
                version: Some("2026-04-28"),
                labels: &["billing".into(), "invoices".into()],
                content: &content_v1,
                author: "tester",
            },
        )
        .unwrap();
        let v1_id = doc.artifact_version_id.clone().unwrap();

        assert_eq!(doc.kind, "agent_context");
        assert_eq!(doc.subkind, API_DOC_SUBKIND);
        assert_eq!(doc.artifact_id, doc.id);
        assert_eq!(doc.manifest_chunk_count, Some(3));
        assert_eq!(doc.chunking_status, "current");

        let artifact = get_artifact_summary(&conn, "billing", &doc.artifact_id)
            .unwrap()
            .unwrap();
        assert_eq!(artifact.kind, API_DOC_ARTIFACT_KIND);
        assert_eq!(artifact.subkind.as_deref(), Some(API_DOC_SUBKIND));
        assert!(artifact
            .labels
            .iter()
            .any(|label| label == API_DOC_RETAIN_LABEL));

        let legacy_kind = list_api_docs(
            &conn,
            "billing",
            &ApiDocFilters {
                kind: Some("agent_context"),
                ..ApiDocFilters::default()
            },
        )
        .unwrap();
        assert_eq!(legacy_kind.len(), 1);
        let artifact_kind = list_api_docs(
            &conn,
            "billing",
            &ApiDocFilters {
                kind: Some(API_DOC_ARTIFACT_KIND),
                ..ApiDocFilters::default()
            },
        )
        .unwrap();
        assert!(artifact_kind.is_empty());

        let current = list_api_doc_chunks(
            &conn,
            "billing",
            &ApiDocFilters {
                query: Some("draft invoice"),
                label: Some("invoices"),
                ..ApiDocFilters::default()
            },
            false,
        )
        .unwrap();
        assert_eq!(current.retrieval_scope, "current");
        assert!(!current.include_history);
        assert_eq!(current.chunking_status.status, "current");
        assert_eq!(current.chunks.len(), 1);
        assert_eq!(current.chunks[0].artifact_version_id, v1_id);
        assert_eq!(current.chunks[0].freshness, "current");
        assert_eq!(current.chunks[0].child_address, "endpoints");

        let content_v2 = serde_json::json!({
            "purpose": "Owns invoice and credit memo state.",
            "endpoints": [{
                "method": "POST",
                "path": "/v1/credit-memos",
                "intent": "Create a credit memo"
            }]
        });
        let updated = update_api_doc(
            &conn,
            "billing",
            &doc.id,
            &ApiDocUpdate {
                content: Some(&content_v2),
                version: Some(Some("2026-05-01")),
                ..ApiDocUpdate::default()
            },
        )
        .unwrap()
        .unwrap();
        let v2_id = updated.artifact_version_id.clone().unwrap();
        assert_ne!(v1_id, v2_id);

        let current_after_update =
            list_api_doc_chunks(&conn, "billing", &ApiDocFilters::default(), false).unwrap();
        assert!(current_after_update
            .chunks
            .iter()
            .all(|chunk| chunk.artifact_version_id == v2_id && chunk.freshness == "current"));
        let history =
            list_api_doc_chunks(&conn, "billing", &ApiDocFilters::default(), true).unwrap();
        assert_eq!(history.retrieval_scope, "history");
        assert!(history.chunks.iter().any(|chunk| {
            chunk.artifact_version_id == v1_id && chunk.freshness == "superseded_history"
        }));
        assert!(history
            .chunks
            .iter()
            .any(|chunk| chunk.artifact_version_id == v2_id && chunk.freshness == "current"));

        let actor_id = api_doc_actor(&conn, "tester").unwrap();
        let partial_payload = serde_json::json!({
            "manifest": {
                "chunk_count": 1,
                "chunk_store": "artifact_chunks"
            }
        });
        let partial_body = serde_json::json!({"purpose": "accepted but not chunked yet"});
        let partial_body_json = serde_json::to_string(&partial_body).unwrap();
        let v3_id = artifact_version_insert(
            &conn,
            &ArtifactVersionInsert {
                artifact_id: &doc.artifact_id,
                version_label: Some("2026-05-02"),
                parent_version_id: Some(&v2_id),
                body_format: API_DOC_BODY_FORMAT,
                body: Some(&partial_body_json),
                structured_payload: Some(&partial_payload),
                source_format: Some("agent_context"),
                created_by_actor_id: &actor_id,
                created_via_workflow_run_id: None,
                version_state: "accepted",
                idempotency_key: None,
            },
        )
        .unwrap();
        artifact_set_pointers(&conn, &doc.artifact_id, Some(&v3_id), Some(&v3_id)).unwrap();
        let partial =
            list_api_doc_chunks(&conn, "billing", &ApiDocFilters::default(), false).unwrap();
        assert_eq!(partial.chunking_status.status, "partial");
        assert!(partial
            .chunks
            .iter()
            .any(|chunk| chunk.freshness == "stale"));
        let partial_doc = get_api_doc(&conn, "billing", &doc.id).unwrap().unwrap();
        assert_eq!(
            partial_doc.artifact_version_id.as_deref(),
            Some(v3_id.as_str())
        );
        assert_eq!(partial_doc.manifest_chunk_count, Some(1));
        assert_eq!(partial_doc.chunking_status, "partial");
    }

    #[test]
    fn api_docs_search_linked_artifact_task_pattern_and_repo_refs() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        insert_project(&conn, &test_project("docs")).unwrap();
        let doc = insert_api_doc(
            &conn,
            "docs",
            &ApiDocInsert {
                app: "docs-api",
                title: "Docs API context",
                summary: None,
                kind: "agent_context",
                source_format: "agent_context",
                source_ref: Some(".agent/api/docs.yaml"),
                version: None,
                labels: &["docs".into()],
                content: &serde_json::json!({"purpose": "links to project resources"}),
                author: "tester",
            },
        )
        .unwrap();
        let actor_id = api_doc_actor(&conn, "tester").unwrap();
        let spec = create_artifact(
            &conn,
            &envelope,
            &ArtifactInsert {
                project_ident: "docs",
                kind: "spec",
                subkind: Some("implementation"),
                title: "Docs spec",
                labels: &[],
                created_by_actor_id: &actor_id,
            },
        )
        .unwrap()
        .record;
        let task = insert_task(
            &conn,
            "docs",
            "Implement docs",
            None,
            None,
            &[],
            None,
            "tester",
        )
        .unwrap();
        let pattern = insert_pattern(
            &conn,
            "Docs pattern",
            Some("docs-pattern"),
            None,
            "pattern body",
            &[],
            "latest",
            "active",
            None,
            "tester",
        )
        .unwrap();
        let commit = "abcdef1234567890";
        for (target_kind, target_id) in [
            ("artifact", spec.artifact_id.as_str()),
            ("task", task.id.as_str()),
            ("pattern", pattern.id.as_str()),
            ("commit", commit),
        ] {
            create_artifact_link(
                &conn,
                &envelope,
                &ArtifactLinkInsert {
                    link_type: "doc_referenced_by_spec",
                    source_kind: "artifact",
                    source_id: &doc.artifact_id,
                    source_version_id: doc.artifact_version_id.as_deref(),
                    source_child_address: None,
                    target_kind,
                    target_id,
                    target_version_id: None,
                    target_child_address: None,
                    created_by_actor_id: &actor_id,
                    created_via_workflow_run_id: None,
                    idempotency_key: None,
                    supersedes_link_id: None,
                },
            )
            .unwrap();
        }

        let linked_doc = get_api_doc(&conn, "docs", &doc.id).unwrap().unwrap();
        for linked_id in [
            spec.artifact_id.as_str(),
            task.id.as_str(),
            pattern.id.as_str(),
            commit,
        ] {
            assert!(
                linked_doc.linked_ids.iter().any(|value| value == linked_id),
                "missing linked id {linked_id}"
            );
            let results = list_api_docs(
                &conn,
                "docs",
                &ApiDocFilters {
                    query: Some(linked_id),
                    ..ApiDocFilters::default()
                },
            )
            .unwrap();
            assert_eq!(results.len(), 1, "query {linked_id}");
            assert_eq!(results[0].id, doc.id);
        }
    }

    // ── Artifact substrate tests (T005) ────────────────────────────────────────

    /// Build a default agent identity for substrate tests.
    fn test_actor_identity<'a>(name: &'a str) -> ArtifactActorIdentity<'a> {
        ArtifactActorIdentity {
            actor_type: "agent",
            agent_system: Some("claude"),
            agent_system_label: None,
            agent_id: Some(name),
            host: Some("test.host"),
            display_name: name,
            runtime_metadata: None,
        }
    }

    fn seed_substrate(conn: &Connection, project: &str) -> (String, String) {
        insert_project(conn, &test_project(project)).unwrap();
        let actor = artifact_actor_upsert(conn, &test_actor_identity("agent-a")).unwrap();
        let artifact = artifact_insert(
            conn,
            &ArtifactInsert {
                project_ident: project,
                kind: "spec",
                subkind: None,
                title: "Substrate test artifact",
                labels: &["test".to_string()],
                created_by_actor_id: &actor,
            },
        )
        .unwrap();
        (actor, artifact)
    }

    #[test]
    fn artifact_actor_upsert_is_idempotent() {
        let conn = test_conn();
        let id1 = artifact_actor_upsert(&conn, &test_actor_identity("agent-x")).unwrap();
        let id2 = artifact_actor_upsert(&conn, &test_actor_identity("agent-x")).unwrap();
        assert_eq!(id1, id2);

        // Different agent_id => different actor.
        let id3 = artifact_actor_upsert(&conn, &test_actor_identity("agent-y")).unwrap();
        assert_ne!(id1, id3);
    }

    #[test]
    fn artifact_schema_apply_preserves_existing_tables() {
        let conn = test_conn();
        // Ensure the historic tables still exist & accept inserts.
        insert_project(&conn, &test_project("legacy")).unwrap();
        let task = insert_task(
            &conn,
            "legacy",
            "legacy task",
            None,
            None,
            &[],
            None,
            "tester",
        )
        .unwrap();
        assert_eq!(task.status, "todo");

        let doc = insert_api_doc(
            &conn,
            "legacy",
            &ApiDocInsert {
                app: "legacy-app",
                title: "Legacy",
                summary: None,
                kind: "agent_context",
                source_format: "agent_context",
                source_ref: None,
                version: None,
                labels: &[],
                content: &serde_json::json!({"purpose": "x"}),
                author: "tester",
            },
        )
        .unwrap();
        assert_eq!(doc.app, "legacy-app");

        // Re-apply schema (simulates upgrade path); both legacy and new
        // artifact tables must coexist.
        apply_schema(&conn).unwrap();
        let row_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                params!["artifact_versions"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(row_exists, 1);
        let task_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(task_count, 1);
    }

    #[test]
    fn artifact_version_body_is_immutable() {
        let conn = test_conn();
        let (actor, artifact) = seed_substrate(&conn, "immut");
        let v_id = artifact_version_insert(
            &conn,
            &ArtifactVersionInsert {
                artifact_id: &artifact,
                version_label: Some("v1"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("original"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "draft",
                idempotency_key: None,
            },
        )
        .unwrap();

        let err = conn
            .execute(
                "UPDATE artifact_versions SET body = 'tampered' WHERE artifact_version_id = ?1",
                params![v_id],
            )
            .unwrap_err();
        assert!(err.to_string().contains("immutable"), "got: {err}");

        // Verify the body did not change.
        let body: Option<String> = conn
            .query_row(
                "SELECT body FROM artifact_versions WHERE artifact_version_id = ?1",
                params![v_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(body.as_deref(), Some("original"));

        // Updating body_format / parent_version_id is also rejected.
        let err = conn
            .execute(
                "UPDATE artifact_versions SET body_format = 'openapi' WHERE artifact_version_id = ?1",
                params![v_id],
            )
            .unwrap_err();
        assert!(err.to_string().contains("immutable"));

        // version_state IS allowed to transition (state model is first-class).
        conn.execute(
            "UPDATE artifact_versions SET version_state = 'under_review' WHERE artifact_version_id = ?1",
            params![v_id],
        )
        .unwrap();

        // Body purge: body_purged_at NULL -> timestamp + body NULL is allowed.
        conn.execute(
            "UPDATE artifact_versions SET body = NULL, body_purged_at = ?2 WHERE artifact_version_id = ?1",
            params![v_id, now_ms()],
        )
        .unwrap();
        // Repopulating the body after purge is rejected.
        let err = conn
            .execute(
                "UPDATE artifact_versions SET body = 'restored' WHERE artifact_version_id = ?1",
                params![v_id],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("cannot be repopulated"),
            "got: {err}"
        );
    }

    #[test]
    fn artifact_current_and_accepted_versions_diverge() {
        let conn = test_conn();
        let (actor, artifact) = seed_substrate(&conn, "diverge");
        let v3 = artifact_version_insert(
            &conn,
            &ArtifactVersionInsert {
                artifact_id: &artifact,
                version_label: Some("v3"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("v3 accepted body"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "accepted",
                idempotency_key: None,
            },
        )
        .unwrap();
        let v4 = artifact_version_insert(
            &conn,
            &ArtifactVersionInsert {
                artifact_id: &artifact,
                version_label: Some("v4"),
                parent_version_id: Some(&v3),
                body_format: "markdown",
                body: Some("v4 draft body"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "draft",
                idempotency_key: None,
            },
        )
        .unwrap();
        // Set both pointers, then diverge them.
        artifact_set_pointers(&conn, &artifact, Some(&v4), Some(&v3)).unwrap();
        let (current, accepted): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT current_version_id, accepted_version_id FROM artifacts WHERE artifact_id = ?1",
                params![artifact],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(current.as_deref(), Some(v4.as_str()));
        assert_eq!(accepted.as_deref(), Some(v3.as_str()));
        assert_ne!(current, accepted);
    }

    #[test]
    fn artifact_pointer_update_rejects_missing_version() {
        let conn = test_conn();
        let (_actor, artifact) = seed_substrate(&conn, "ptr-missing");

        let err = artifact_set_pointers(&conn, &artifact, Some("missing-version"), None)
            .expect_err("missing version pointer must be rejected");
        assert!(
            err.to_string().contains("references missing version"),
            "got: {err}"
        );

        let current: Option<String> = conn
            .query_row(
                "SELECT current_version_id FROM artifacts WHERE artifact_id = ?1",
                params![artifact],
                |r| r.get(0),
            )
            .unwrap();
        assert!(current.is_none());
    }

    #[test]
    fn artifact_pointer_update_rejects_cross_artifact_version() {
        let conn = test_conn();
        let (actor, artifact_a) = seed_substrate(&conn, "ptr-cross");
        let artifact_b = artifact_insert(
            &conn,
            &ArtifactInsert {
                project_ident: "ptr-cross",
                kind: "spec",
                subkind: None,
                title: "other artifact",
                labels: &[],
                created_by_actor_id: &actor,
            },
        )
        .unwrap();
        let version_b = artifact_version_insert(
            &conn,
            &ArtifactVersionInsert {
                artifact_id: &artifact_b,
                version_label: Some("v1"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("other body"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "draft",
                idempotency_key: None,
            },
        )
        .unwrap();

        let err = artifact_set_pointers(&conn, &artifact_a, Some(&version_b), None)
            .expect_err("cross-artifact pointer must be rejected");
        assert!(
            err.to_string().contains("belongs to artifact"),
            "got: {err}"
        );

        let current: Option<String> = conn
            .query_row(
                "SELECT current_version_id FROM artifacts WHERE artifact_id = ?1",
                params![artifact_a],
                |r| r.get(0),
            )
            .unwrap();
        assert!(current.is_none());
    }

    #[test]
    fn artifact_pointer_update_accepts_same_artifact_versions() {
        let conn = test_conn();
        let (actor, artifact) = seed_substrate(&conn, "ptr-valid");
        let accepted = artifact_version_insert(
            &conn,
            &ArtifactVersionInsert {
                artifact_id: &artifact,
                version_label: Some("accepted"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("accepted body"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "accepted",
                idempotency_key: None,
            },
        )
        .unwrap();
        let current = artifact_version_insert(
            &conn,
            &ArtifactVersionInsert {
                artifact_id: &artifact,
                version_label: Some("current"),
                parent_version_id: Some(&accepted),
                body_format: "markdown",
                body: Some("current body"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "draft",
                idempotency_key: None,
            },
        )
        .unwrap();

        artifact_set_pointers(&conn, &artifact, Some(&current), Some(&accepted)).unwrap();

        let (actual_current, actual_accepted): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT current_version_id, accepted_version_id FROM artifacts WHERE artifact_id = ?1",
                params![artifact],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(actual_current.as_deref(), Some(current.as_str()));
        assert_eq!(actual_accepted.as_deref(), Some(accepted.as_str()));
    }

    #[test]
    fn artifact_state_fields_are_independent() {
        let conn = test_conn();
        let (_actor, artifact) = seed_substrate(&conn, "states");
        // spec artifacts default implementation_state to 'not_started'.
        let (lifecycle, review, implementation): (String, String, String) = conn
            .query_row(
                "SELECT lifecycle_state, review_state, implementation_state FROM artifacts WHERE artifact_id = ?1",
                params![artifact],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(lifecycle, "draft");
        assert_eq!(review, "none");
        assert_eq!(implementation, "not_started");

        // Independently transition each.
        conn.execute(
            "UPDATE artifacts SET lifecycle_state = 'active', review_state = 'collecting_reviews',
                                  implementation_state = 'in_progress', updated_at = ?2
             WHERE artifact_id = ?1",
            params![artifact, now_ms()],
        )
        .unwrap();
        let (l2, r2, i2): (String, String, String) = conn
            .query_row(
                "SELECT lifecycle_state, review_state, implementation_state FROM artifacts WHERE artifact_id = ?1",
                params![artifact],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (l2.as_str(), r2.as_str(), i2.as_str()),
            ("active", "collecting_reviews", "in_progress")
        );

        // Invalid value rejected by CHECK constraint.
        let err = conn
            .execute(
                "UPDATE artifacts SET lifecycle_state = 'banana' WHERE artifact_id = ?1",
                params![artifact],
            )
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("check"),
            "got: {err}"
        );
    }

    #[test]
    fn artifact_link_idempotency_is_unique_per_run() {
        let conn = test_conn();
        let (actor, artifact) = seed_substrate(&conn, "links");
        let v1 = artifact_version_insert(
            &conn,
            &ArtifactVersionInsert {
                artifact_id: &artifact,
                version_label: None,
                parent_version_id: None,
                body_format: "markdown",
                body: Some("body"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "accepted",
                idempotency_key: None,
            },
        )
        .unwrap();
        let run = workflow_run_insert(
            &conn,
            &WorkflowRunInsert {
                artifact_id: &artifact,
                workflow_kind: "spec_acceptance",
                phase: None,
                round_id: None,
                coordinator_actor_id: &actor,
                participant_actor_ids: &[],
                source_artifact_version_id: Some(&v1),
                read_set: None,
                idempotency_key: Some("run-key-1"),
                is_resumable: false,
            },
        )
        .unwrap();

        let l1 = artifact_link_insert(
            &conn,
            &ArtifactLinkInsert {
                link_type: "supersedes_version",
                source_kind: "artifact_version",
                source_id: &v1,
                source_version_id: Some(&v1),
                source_child_address: None,
                target_kind: "artifact_version",
                target_id: &v1,
                target_version_id: Some(&v1),
                target_child_address: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: Some(&run),
                idempotency_key: Some("link-key-A"),
                supersedes_link_id: None,
            },
        )
        .unwrap();
        // Duplicate (run, key) is rejected.
        let dup = artifact_link_insert(
            &conn,
            &ArtifactLinkInsert {
                link_type: "supersedes_version",
                source_kind: "artifact_version",
                source_id: &v1,
                source_version_id: Some(&v1),
                source_child_address: None,
                target_kind: "artifact_version",
                target_id: &v1,
                target_version_id: Some(&v1),
                target_child_address: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: Some(&run),
                idempotency_key: Some("link-key-A"),
                supersedes_link_id: None,
            },
        );
        assert!(
            dup.is_err(),
            "expected UNIQUE violation on duplicate (run, key)"
        );
        assert!(!l1.is_empty());
    }

    #[test]
    fn workflow_run_idempotency_is_unique_per_kind() {
        let conn = test_conn();
        let (actor, artifact) = seed_substrate(&conn, "runidemp");
        workflow_run_insert(
            &conn,
            &WorkflowRunInsert {
                artifact_id: &artifact,
                workflow_kind: "doc_publish",
                phase: None,
                round_id: None,
                coordinator_actor_id: &actor,
                participant_actor_ids: &[],
                source_artifact_version_id: None,
                read_set: None,
                idempotency_key: Some("publish-1"),
                is_resumable: true,
            },
        )
        .unwrap();
        let dup = workflow_run_insert(
            &conn,
            &WorkflowRunInsert {
                artifact_id: &artifact,
                workflow_kind: "doc_publish",
                phase: None,
                round_id: None,
                coordinator_actor_id: &actor,
                participant_actor_ids: &[],
                source_artifact_version_id: None,
                read_set: None,
                idempotency_key: Some("publish-1"),
                is_resumable: true,
            },
        );
        assert!(dup.is_err());
    }

    #[test]
    fn artifact_chunk_supersession_preserves_history() {
        let conn = test_conn();
        let (actor, artifact) = seed_substrate(&conn, "chunks");
        let v1 = artifact_version_insert(
            &conn,
            &ArtifactVersionInsert {
                artifact_id: &artifact,
                version_label: Some("v1"),
                parent_version_id: None,
                body_format: "application/agent-context+json",
                body: Some("{}"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "accepted",
                idempotency_key: None,
            },
        )
        .unwrap();
        let v2 = artifact_version_insert(
            &conn,
            &ArtifactVersionInsert {
                artifact_id: &artifact,
                version_label: Some("v2"),
                parent_version_id: Some(&v1),
                body_format: "application/agent-context+json",
                body: Some("{}"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "accepted",
                idempotency_key: None,
            },
        )
        .unwrap();

        let old_chunk = artifact_chunk_insert(
            &conn,
            &ArtifactChunkInsert {
                artifact_id: &artifact,
                artifact_version_id: &v1,
                child_address: "endpoints[0]",
                text: "old chunk text",
                embedding_model: Some("test-embed"),
                embedding_vector: None,
                app: Some("docs"),
                label: Some("api"),
                kind: Some("agent_context"),
                metadata: None,
            },
        )
        .unwrap();
        let new_chunk = artifact_chunk_insert(
            &conn,
            &ArtifactChunkInsert {
                artifact_id: &artifact,
                artifact_version_id: &v2,
                child_address: "endpoints[0]",
                text: "new chunk text",
                embedding_model: Some("test-embed"),
                embedding_vector: None,
                app: Some("docs"),
                label: Some("api"),
                kind: Some("agent_context"),
                metadata: None,
            },
        )
        .unwrap();
        artifact_chunk_mark_superseded(&conn, &old_chunk, &new_chunk).unwrap();

        // Natural-key uniqueness: re-inserting the same (version, child_address)
        // must fail (same chunk_id is the contract; the schema is the gate).
        let dup = artifact_chunk_insert(
            &conn,
            &ArtifactChunkInsert {
                artifact_id: &artifact,
                artifact_version_id: &v2,
                child_address: "endpoints[0]",
                text: "would-be duplicate",
                embedding_model: None,
                embedding_vector: None,
                app: None,
                label: None,
                kind: None,
                metadata: None,
            },
        );
        assert!(dup.is_err());

        // Default retrieval (current only) excludes superseded rows.
        let mut current = conn
            .prepare(
                "SELECT chunk_id, text FROM artifact_chunks
                 WHERE artifact_id = ?1 AND superseded_by_chunk_id IS NULL",
            )
            .unwrap();
        let live: Vec<(String, String)> = current
            .query_map(params![artifact], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].0, new_chunk);

        // History-aware retrieval reconstructs the superseded chunk.
        let mut history = conn
            .prepare(
                "SELECT chunk_id, text, superseded_by_chunk_id, artifact_version_id
                 FROM artifact_chunks WHERE artifact_id = ?1",
            )
            .unwrap();
        let all: Vec<(String, String, Option<String>, String)> = history
            .query_map(params![artifact], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(all.len(), 2);
        // Find the soft-superseded row by its superseded_by pointer; verify it
        // anchors to the older artifact_version and carries the historical text.
        let superseded = all
            .iter()
            .find(|(_, _, sup, _)| sup.as_deref() == Some(new_chunk.as_str()))
            .expect("superseded chunk preserved");
        assert_eq!(superseded.0, old_chunk);
        assert_eq!(superseded.1, "old chunk text");
        assert_eq!(superseded.3, v1);
    }

    #[test]
    fn artifact_comment_anchors_to_manifest_item() {
        let conn = test_conn();
        let (actor, artifact) = seed_substrate(&conn, "comments");
        let v1 = artifact_version_insert(
            &conn,
            &ArtifactVersionInsert {
                artifact_id: &artifact,
                version_label: Some("v1"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("# spec"),
                structured_payload: Some(
                    &serde_json::json!({"manifest_version": "1", "items": []}),
                ),
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "accepted",
                idempotency_key: None,
            },
        )
        .unwrap();
        let manifest_item_id = "T005";
        let comment_id = artifact_comment_insert(
            &conn,
            &ArtifactCommentInsert {
                artifact_id: &artifact,
                target_kind: "artifact_version",
                target_id: &v1,
                child_address: Some(&format!("manifest.items[{manifest_item_id}]")),
                parent_comment_id: None,
                actor_id: &actor,
                body: "needs idempotency callout",
                idempotency_key: Some("comment-key-1"),
            },
        )
        .unwrap();
        let stored: (String, Option<String>) = conn
            .query_row(
                "SELECT target_id, child_address FROM artifact_comments WHERE comment_id = ?1",
                params![comment_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored.0, v1);
        assert_eq!(stored.1.as_deref(), Some("manifest.items[T005]"));

        // child_address only valid when target_kind = artifact_version.
        let bad = artifact_comment_insert(
            &conn,
            &ArtifactCommentInsert {
                artifact_id: &artifact,
                target_kind: "artifact",
                target_id: &artifact,
                child_address: Some("manifest.items[T999]"),
                parent_comment_id: None,
                actor_id: &actor,
                body: "should reject",
                idempotency_key: None,
            },
        );
        assert!(bad.is_err());

        // Idempotency: same (target, actor, key) returns existing on conflict.
        let dup = artifact_comment_insert(
            &conn,
            &ArtifactCommentInsert {
                artifact_id: &artifact,
                target_kind: "artifact_version",
                target_id: &v1,
                child_address: Some(&format!("manifest.items[{manifest_item_id}]")),
                parent_comment_id: None,
                actor_id: &actor,
                body: "duplicate body",
                idempotency_key: Some("comment-key-1"),
            },
        );
        assert!(dup.is_err());
    }

    #[test]
    fn workflow_run_resumable_kind_can_recover_from_failed() {
        let conn = test_conn();
        let (actor, artifact) = seed_substrate(&conn, "resume");
        let run = workflow_run_insert(
            &conn,
            &WorkflowRunInsert {
                artifact_id: &artifact,
                workflow_kind: "spec_task_generation",
                phase: None,
                round_id: None,
                coordinator_actor_id: &actor,
                participant_actor_ids: &[],
                source_artifact_version_id: None,
                read_set: None,
                idempotency_key: Some("gen-1"),
                is_resumable: true,
            },
        )
        .unwrap();
        // started -> failed.
        workflow_run_set_state(&conn, &run, "failed", Some("partial")).unwrap();
        // failed -> succeeded under same idempotency scope: resumable rule.
        workflow_run_set_state(&conn, &run, "succeeded", None).unwrap();
        let state: String = conn
            .query_row(
                "SELECT state FROM workflow_runs WHERE workflow_run_id = ?1",
                params![run],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "succeeded");

        // Non-resumable kind cannot recover from failed.
        let nonresume = workflow_run_insert(
            &conn,
            &WorkflowRunInsert {
                artifact_id: &artifact,
                workflow_kind: "spec_iteration",
                phase: None,
                round_id: None,
                coordinator_actor_id: &actor,
                participant_actor_ids: &[],
                source_artifact_version_id: None,
                read_set: None,
                idempotency_key: Some("iter-1"),
                is_resumable: false,
            },
        )
        .unwrap();
        workflow_run_set_state(&conn, &nonresume, "failed", Some("oops")).unwrap();
        let err = workflow_run_set_state(&conn, &nonresume, "succeeded", None).unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid workflow_run state transition"),
            "got: {err}"
        );
    }

    #[test]
    fn artifact_repository_filters_detail_and_search_related_rows() {
        let conn = test_conn();
        insert_project(&conn, &test_project("repo")).unwrap();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        let actor = resolve_artifact_actor(&conn, &test_actor_identity("repo-agent")).unwrap();
        let artifact = create_artifact(
            &conn,
            &envelope,
            &ArtifactInsert {
                project_ident: "repo",
                kind: "spec",
                subkind: Some("implementation"),
                title: "Cache invalidation spec",
                labels: &["searchable".to_string(), "backend".to_string()],
                created_by_actor_id: &actor.actor_id,
            },
        )
        .unwrap()
        .record;

        let version = create_artifact_version(
            &conn,
            &envelope,
            &ArtifactVersionInsert {
                artifact_id: &artifact.artifact_id,
                version_label: Some("v1"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("body-only phrase: eviction fence"),
                structured_payload: Some(&serde_json::json!({"manifest": "repo"})),
                source_format: None,
                created_by_actor_id: &actor.actor_id,
                created_via_workflow_run_id: None,
                version_state: "draft",
                idempotency_key: None,
            },
        )
        .unwrap()
        .record;
        let contribution = add_artifact_contribution(
            &conn,
            &envelope,
            &ArtifactContributionInsert {
                artifact_id: &artifact.artifact_id,
                target_kind: "artifact_version",
                target_id: &version.artifact_version_id,
                contribution_kind: "review",
                phase: Some("pass_1"),
                role: "reviewer",
                actor_id: &actor.actor_id,
                workflow_run_id: None,
                read_set: None,
                body_format: "markdown",
                body: "contribution-only phrase: monotonic token",
                idempotency_key: Some("contrib-search"),
            },
        )
        .unwrap()
        .record;
        let linked_task = insert_task(
            &conn,
            "repo",
            "linked generated task",
            None,
            None,
            &[],
            None,
            "repo-agent",
        )
        .unwrap();
        let linked_doc = insert_api_doc(
            &conn,
            "repo",
            &ApiDocInsert {
                app: "linked-app",
                title: "Linked API context",
                summary: None,
                kind: "agent_context",
                source_format: "agent_context",
                source_ref: None,
                version: None,
                labels: &[],
                content: &serde_json::json!({"purpose": "linked fixture"}),
                author: "repo-agent",
            },
        )
        .unwrap();
        let linked_pattern = insert_pattern(
            &conn,
            "Linked pattern",
            Some("linked-pattern"),
            None,
            "pattern body",
            &[],
            "latest",
            "active",
            None,
            "repo-agent",
        )
        .unwrap();
        for (target_kind, target_id) in [
            ("task", linked_task.id.as_str()),
            ("external_url", linked_doc.id.as_str()),
            ("pattern", linked_pattern.id.as_str()),
        ] {
            create_artifact_link(
                &conn,
                &envelope,
                &ArtifactLinkInsert {
                    link_type: "comment_references_task",
                    source_kind: "artifact",
                    source_id: &artifact.artifact_id,
                    source_version_id: None,
                    source_child_address: None,
                    target_kind,
                    target_id,
                    target_version_id: None,
                    target_child_address: None,
                    created_by_actor_id: &actor.actor_id,
                    created_via_workflow_run_id: None,
                    idempotency_key: None,
                    supersedes_link_id: None,
                },
            )
            .unwrap();
        }

        let by_label = list_artifacts(
            &conn,
            "repo",
            &ArtifactFilters {
                kind: Some("spec"),
                lifecycle_state: Some("draft"),
                label: Some("backend"),
                actor_id: Some(&actor.actor_id),
                ..ArtifactFilters::default()
            },
        )
        .unwrap();
        assert_eq!(by_label.len(), 1);
        assert_eq!(by_label[0].artifact_id, artifact.artifact_id);

        for query in [
            "eviction fence",
            "monotonic token",
            linked_task.id.as_str(),
            linked_pattern.id.as_str(),
        ] {
            let results = list_artifacts(
                &conn,
                "repo",
                &ArtifactFilters {
                    query: Some(query),
                    ..ArtifactFilters::default()
                },
            )
            .unwrap();
            assert_eq!(results.len(), 1, "query {query}");
            assert_eq!(results[0].artifact_id, artifact.artifact_id);
        }
        let linked_doc_results = list_artifacts(
            &conn,
            "repo",
            &ArtifactFilters {
                query: Some(&linked_doc.id),
                ..ArtifactFilters::default()
            },
        )
        .unwrap();
        assert_eq!(linked_doc_results.len(), 2, "query {}", linked_doc.id);
        assert!(linked_doc_results
            .iter()
            .any(|result| result.artifact_id == artifact.artifact_id));
        assert!(linked_doc_results
            .iter()
            .any(|result| result.artifact_id == linked_doc.artifact_id));

        let detail = get_artifact(&conn, "repo", &artifact.artifact_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            detail.current_version.unwrap().artifact_version_id,
            version.artifact_version_id
        );
        assert_eq!(
            list_artifact_versions(&conn, "repo", &artifact.artifact_id)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_artifact_contributions(&conn, "repo", &artifact.artifact_id).unwrap()[0]
                .contribution_id,
            contribution.contribution_id
        );

        let updated = update_artifact(
            &conn,
            "repo",
            &artifact.artifact_id,
            &ArtifactUpdate {
                lifecycle_state: Some("active"),
                labels: Some(&["searchable".to_string(), "accepted".to_string()]),
                ..ArtifactUpdate::default()
            },
            &envelope,
        )
        .unwrap()
        .unwrap();
        assert_eq!(updated.lifecycle_state, "active");
        assert_eq!(updated.labels, vec!["searchable", "accepted"]);
    }

    #[test]
    fn artifact_repository_idempotent_mutations_and_workflow_updates() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        let (actor, artifact) = seed_substrate(&conn, "repo-idempotent");
        let run_input = WorkflowRunInsert {
            artifact_id: &artifact,
            workflow_kind: "spec_task_generation",
            phase: Some("generation"),
            round_id: Some("round-1"),
            coordinator_actor_id: &actor,
            participant_actor_ids: std::slice::from_ref(&actor),
            source_artifact_version_id: None,
            read_set: Some(&serde_json::json!({"versions": [], "contributions": []})),
            idempotency_key: Some("run-key"),
            is_resumable: true,
        };
        let run = start_workflow_run(&conn, &envelope, &run_input).unwrap();
        let run_replay = start_workflow_run(&conn, &envelope, &run_input).unwrap();
        assert!(!run.replayed);
        assert!(run_replay.replayed);
        assert_eq!(
            run.record.workflow_run_id,
            run_replay.record.workflow_run_id
        );

        let version_input = ArtifactVersionInsert {
            artifact_id: &artifact,
            version_label: Some("v1"),
            parent_version_id: None,
            body_format: "markdown",
            body: Some("idempotent body"),
            structured_payload: None,
            source_format: None,
            created_by_actor_id: &actor,
            created_via_workflow_run_id: Some(&run.record.workflow_run_id),
            version_state: "under_review",
            idempotency_key: Some("version-key"),
        };
        let version = create_artifact_version(&conn, &envelope, &version_input).unwrap();
        let version_replay = create_artifact_version(&conn, &envelope, &version_input).unwrap();
        assert!(version_replay.replayed);
        assert_eq!(
            version.record.artifact_version_id,
            version_replay.record.artifact_version_id
        );

        let contribution_input = ArtifactContributionInsert {
            artifact_id: &artifact,
            target_kind: "artifact_version",
            target_id: &version.record.artifact_version_id,
            contribution_kind: "synthesis",
            phase: Some("synthesis"),
            role: "analyst",
            actor_id: &actor,
            workflow_run_id: Some(&run.record.workflow_run_id),
            read_set: Some(&serde_json::json!({"versions": [version.record.artifact_version_id]})),
            body_format: "markdown",
            body: "synthesized finding",
            idempotency_key: Some("contrib-key"),
        };
        let contribution =
            add_artifact_contribution(&conn, &envelope, &contribution_input).unwrap();
        let contribution_replay =
            add_artifact_contribution(&conn, &envelope, &contribution_input).unwrap();
        assert!(contribution_replay.replayed);
        assert_eq!(
            contribution.record.contribution_id,
            contribution_replay.record.contribution_id
        );

        let link_input = ArtifactLinkInsert {
            link_type: "task_generated_from_spec",
            source_kind: "task",
            source_id: "generated-task-1",
            source_version_id: None,
            source_child_address: None,
            target_kind: "artifact_version",
            target_id: &version.record.artifact_version_id,
            target_version_id: Some(&version.record.artifact_version_id),
            target_child_address: Some("manifest.items[T006]"),
            created_by_actor_id: &actor,
            created_via_workflow_run_id: Some(&run.record.workflow_run_id),
            idempotency_key: Some("link-key"),
            supersedes_link_id: None,
        };
        let link = create_artifact_link(&conn, &envelope, &link_input).unwrap();
        let link_replay = create_artifact_link(&conn, &envelope, &link_input).unwrap();
        assert!(link_replay.replayed);
        assert_eq!(link.record.link_id, link_replay.record.link_id);

        let chunk_input = ArtifactChunkInsert {
            artifact_id: &artifact,
            artifact_version_id: &version.record.artifact_version_id,
            child_address: "manifest.items[T006]",
            text: "chunk text",
            embedding_model: None,
            embedding_vector: None,
            app: Some("spec"),
            label: Some("backend"),
            kind: Some("manifest_item"),
            metadata: Some(&serde_json::json!({"item": "T006"})),
        };
        let chunk = create_artifact_chunk(&conn, &envelope, &chunk_input).unwrap();
        let chunk_replay = create_artifact_chunk(&conn, &envelope, &chunk_input).unwrap();
        assert!(chunk_replay.replayed);
        assert_eq!(chunk.record.chunk_id, chunk_replay.record.chunk_id);

        let updated_run = update_workflow_run(
            &conn,
            &run.record.workflow_run_id,
            &WorkflowRunUpdate {
                state: Some("succeeded"),
                generated_contribution_ids: Some(std::slice::from_ref(
                    &contribution.record.contribution_id,
                )),
                generated_version_ids: Some(std::slice::from_ref(
                    &version.record.artifact_version_id,
                )),
                generated_task_ids: Some(&["generated-task-1".to_string()]),
                generated_link_ids: Some(std::slice::from_ref(&link.record.link_id)),
                generated_chunk_ids: Some(std::slice::from_ref(&chunk.record.chunk_id)),
                ..WorkflowRunUpdate::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(updated_run.state, "succeeded");
        assert_eq!(updated_run.generated_task_ids, vec!["generated-task-1"]);
        assert_eq!(
            list_workflow_runs(&conn, "repo-idempotent", &artifact)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_artifact_links(
                &conn,
                "repo-idempotent",
                &ArtifactLinkFilters {
                    link_type: Some("task_generated_from_spec"),
                    ..ArtifactLinkFilters::default()
                },
            )
            .unwrap()
            .len(),
            1
        );
        assert_eq!(
            list_artifact_chunks(
                &conn,
                "repo-idempotent",
                &artifact,
                &ArtifactChunkFilters {
                    query: Some("chunk text"),
                    ..ArtifactChunkFilters::default()
                },
            )
            .unwrap()
            .len(),
            1
        );
    }

    /// T024 acceptance: partial-failure repair.
    ///
    /// Simulates a crash AFTER the version row was inserted but BEFORE
    /// `artifacts.current_version_id` was set. A subsequent idempotent
    /// replay must backfill the pointer to the existing version so the
    /// artifact is no longer dangling.
    #[test]
    fn create_artifact_version_replay_repairs_null_current_pointer() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        let (actor, artifact) = seed_substrate(&conn, "repo-pointer-repair");

        let input = ArtifactVersionInsert {
            artifact_id: &artifact,
            version_label: Some("v1"),
            parent_version_id: None,
            body_format: "markdown",
            body: Some("v1 body"),
            structured_payload: None,
            source_format: None,
            created_by_actor_id: &actor,
            created_via_workflow_run_id: None,
            version_state: "under_review",
            // Replay key requires a run id; use a synthetic NIL-style run.
            idempotency_key: None,
        };

        // First write must populate current_version_id.
        let first = create_artifact_version(&conn, &envelope, &input).unwrap();
        let detail = get_artifact(&conn, "repo-pointer-repair", &artifact)
            .unwrap()
            .unwrap();
        assert_eq!(
            detail.artifact.current_version_id.as_deref(),
            Some(first.record.artifact_version_id.as_str())
        );

        // Simulate the partial-failure scenario: pointer is NULL but the
        // version row still exists. Build the version+run+key tuple the
        // idempotent replay branch will discover.
        let run = start_workflow_run(
            &conn,
            &envelope,
            &WorkflowRunInsert {
                artifact_id: &artifact,
                workflow_kind: "spec_iteration",
                phase: Some("review"),
                round_id: Some("round-1"),
                coordinator_actor_id: &actor,
                participant_actor_ids: &[],
                source_artifact_version_id: None,
                read_set: None,
                idempotency_key: Some("repair-run-key"),
                is_resumable: true,
            },
        )
        .unwrap()
        .record;

        let keyed_input = ArtifactVersionInsert {
            created_via_workflow_run_id: Some(&run.workflow_run_id),
            idempotency_key: Some("repair-version-key"),
            version_label: Some("v2"),
            body: Some("v2 body"),
            ..input
        };
        let v2 = create_artifact_version(&conn, &envelope, &keyed_input).unwrap();
        assert!(!v2.replayed);

        // Force pointer NULL to simulate the partial failure (skip pointer
        // update happening in the same txn as the insert).
        conn.execute(
            "UPDATE artifacts SET current_version_id = NULL WHERE artifact_id = ?1",
            params![artifact],
        )
        .unwrap();
        let detail = get_artifact(&conn, "repo-pointer-repair", &artifact)
            .unwrap()
            .unwrap();
        assert!(detail.artifact.current_version_id.is_none());

        // Replay with the same key — must backfill the pointer.
        let replay = create_artifact_version(&conn, &envelope, &keyed_input).unwrap();
        assert!(replay.replayed);
        assert_eq!(
            replay.record.artifact_version_id,
            v2.record.artifact_version_id
        );
        let detail = get_artifact(&conn, "repo-pointer-repair", &artifact)
            .unwrap()
            .unwrap();
        assert_eq!(
            detail.artifact.current_version_id.as_deref(),
            Some(v2.record.artifact_version_id.as_str()),
            "partial-failure replay must repair the NULL current_version_id pointer"
        );
    }

    /// T024 acceptance: stale-replay non-regression.
    ///
    /// After a newer version becomes current, a stale replay of an older
    /// idempotency key (e.g. delayed redelivery from an at-least-once
    /// transport) must NOT rewind `current_version_id` to the older
    /// version. The repair predicate only fires when the pointer is NULL
    /// or already equal to the replayed version.
    #[test]
    fn create_artifact_version_stale_replay_does_not_regress_current_pointer() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        let (actor, artifact) = seed_substrate(&conn, "repo-stale-replay");

        let run = start_workflow_run(
            &conn,
            &envelope,
            &WorkflowRunInsert {
                artifact_id: &artifact,
                workflow_kind: "spec_iteration",
                phase: Some("review"),
                round_id: Some("round-1"),
                coordinator_actor_id: &actor,
                participant_actor_ids: &[],
                source_artifact_version_id: None,
                read_set: None,
                idempotency_key: Some("stale-run-key"),
                is_resumable: true,
            },
        )
        .unwrap()
        .record;

        let v1_input = ArtifactVersionInsert {
            artifact_id: &artifact,
            version_label: Some("v1"),
            parent_version_id: None,
            body_format: "markdown",
            body: Some("v1 body"),
            structured_payload: None,
            source_format: None,
            created_by_actor_id: &actor,
            created_via_workflow_run_id: Some(&run.workflow_run_id),
            version_state: "under_review",
            idempotency_key: Some("v1-key"),
        };
        let v1 = create_artifact_version(&conn, &envelope, &v1_input).unwrap();
        assert!(!v1.replayed);

        // A second, newer write under a different key becomes current.
        let v2_input = ArtifactVersionInsert {
            version_label: Some("v2"),
            body: Some("v2 body"),
            parent_version_id: Some(&v1.record.artifact_version_id),
            idempotency_key: Some("v2-key"),
            ..v1_input
        };
        let v2 = create_artifact_version(&conn, &envelope, &v2_input).unwrap();
        assert!(!v2.replayed);
        let detail = get_artifact(&conn, "repo-stale-replay", &artifact)
            .unwrap()
            .unwrap();
        assert_eq!(
            detail.artifact.current_version_id.as_deref(),
            Some(v2.record.artifact_version_id.as_str())
        );

        // Stale replay of v1's input. Without the guard this would clobber
        // current_version_id back to v1; with the guard it must stay v2.
        let stale_replay = create_artifact_version(&conn, &envelope, &v1_input).unwrap();
        assert!(stale_replay.replayed);
        assert_eq!(
            stale_replay.record.artifact_version_id,
            v1.record.artifact_version_id
        );
        let detail = get_artifact(&conn, "repo-stale-replay", &artifact)
            .unwrap()
            .unwrap();
        assert_eq!(
            detail.artifact.current_version_id.as_deref(),
            Some(v2.record.artifact_version_id.as_str()),
            "stale replay of older key must NOT regress current_version_id"
        );
    }

    #[test]
    fn artifact_repository_comment_lifecycle_and_acceptance() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        let (actor, artifact) = seed_substrate(&conn, "repo-comments");
        let version = create_artifact_version(
            &conn,
            &envelope,
            &ArtifactVersionInsert {
                artifact_id: &artifact,
                version_label: Some("v1"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("review me"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "under_review",
                idempotency_key: None,
            },
        )
        .unwrap()
        .record;

        let comment_input = ArtifactCommentInsert {
            artifact_id: &artifact,
            target_kind: "artifact_version",
            target_id: &version.artifact_version_id,
            child_address: Some("manifest.items[T006]"),
            parent_comment_id: None,
            actor_id: &actor,
            body: "please clarify ownership",
            idempotency_key: Some("comment-key"),
        };
        let comment = add_artifact_comment(&conn, &envelope, &comment_input).unwrap();
        let replay = add_artifact_comment(&conn, &envelope, &comment_input).unwrap();
        assert!(replay.replayed);
        assert_eq!(comment.record.comment_id, replay.record.comment_id);

        let resolved = resolve_artifact_comment(
            &conn,
            "repo-comments",
            &artifact,
            &comment.record.comment_id,
            &actor,
            None,
            Some("answered in v1"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(resolved.state, "resolved");
        assert_eq!(
            resolved.resolved_by_actor_id.as_deref(),
            Some(actor.as_str())
        );

        let reopened = reopen_artifact_comment(
            &conn,
            &envelope,
            "repo-comments",
            &artifact,
            &comment.record.comment_id,
            &actor,
            "reopening for another pass",
            Some("reopen-note"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(reopened.state, "open");
        assert_eq!(
            list_artifact_comments(&conn, "repo-comments", &artifact)
                .unwrap()
                .len(),
            2
        );

        let acceptance = accept_artifact_version(
            &conn,
            "repo-comments",
            &artifact,
            &version.artifact_version_id,
            &actor,
            None,
            Some("accept-key"),
        )
        .unwrap();
        assert_eq!(acceptance.contribution_kind, "state_transition");
        let detail = get_artifact(&conn, "repo-comments", &artifact)
            .unwrap()
            .unwrap();
        assert_eq!(
            detail.artifact.accepted_version_id.as_deref(),
            Some(version.artifact_version_id.as_str())
        );
    }

    // ── T016 — operations envelope tests ──────────────────────────────────

    #[test]
    fn operations_envelope_defaults_match_t004() {
        let env = ArtifactOperationsEnvelope::default();
        // Sentinel checks — T004 §1 / §2 production values.
        assert_eq!(env.sizes.artifact_version_body_max_bytes, 1024 * 1024);
        assert_eq!(
            env.sizes.artifact_version_source_body_max_bytes,
            4 * 1024 * 1024
        );
        assert_eq!(env.sizes.contribution_body_max_bytes, 256 * 1024);
        assert_eq!(env.sizes.comment_body_max_bytes, 32 * 1024);
        assert_eq!(env.sizes.chunk_text_max_bytes, 8 * 1024);
        assert_eq!(env.sizes.artifact_labels_max_count, 32);
        assert_eq!(env.sizes.read_set_max_refs, 256);
        assert_eq!(env.quotas.artifact.soft, 5_000);
        assert_eq!(env.quotas.artifact.hard, 10_000);
        assert_eq!(env.quotas.write_rpm.hard, 1_200);
        assert_eq!(env.retention.archive_body_ttl_days, 180);
        assert_eq!(env.retention.workflow_run_stuck_ttl_hours, 24);
        assert_eq!(env.retention.retain_permanent_label, "retain:permanent");
        assert_eq!(env.restore.idempotency_sample_size, 100);
    }

    #[test]
    fn operations_envelope_loads_shrunken_fixture() {
        let fixture = t008_shrunken_artifact_operations_fixture_env();
        let env =
            ArtifactOperationsEnvelope::from_env_with(|k| fixture.get(k).map(|s| (*s).to_string()))
                .expect("fixture parses");
        // Values flow from the env map — not from T004 constants restated
        // in this test. The fixture map IS the contract under test.
        assert_eq!(
            env.sizes.artifact_version_body_max_bytes,
            fixture["ARTIFACT_VERSION_BODY_MAX_BYTES"]
                .parse::<usize>()
                .unwrap()
        );
        assert_eq!(
            env.sizes.chunk_text_max_bytes,
            fixture["CHUNK_TEXT_MAX_BYTES"].parse::<usize>().unwrap()
        );
        assert_eq!(
            env.quotas.artifact.hard,
            fixture["PROJECT_ARTIFACT_HARD"].parse::<u64>().unwrap()
        );
        assert_eq!(
            env.quotas.write_rpm.soft,
            fixture["PROJECT_WRITE_RPM_SOFT"].parse::<u64>().unwrap()
        );
    }

    #[test]
    fn operations_envelope_size_check_emits_typed_error() {
        let env = ArtifactOperationsEnvelope::default();
        let oversize = env.sizes.check(
            SizeLimitKind::CommentBody,
            env.sizes.comment_body_max_bytes + 1,
        );
        match oversize {
            Err(OperationsError::SizeLimit {
                kind,
                limit,
                actual,
            }) => {
                assert_eq!(kind, SizeLimitKind::CommentBody);
                assert_eq!(limit, env.sizes.comment_body_max_bytes);
                assert_eq!(actual, env.sizes.comment_body_max_bytes + 1);
            }
            other => panic!("expected SizeLimit error, got {other:?}"),
        }
        // Equal length is accepted.
        assert!(env
            .sizes
            .check(SizeLimitKind::CommentBody, env.sizes.comment_body_max_bytes)
            .is_ok());
    }

    #[test]
    fn operations_envelope_quota_soft_warning_distinct_from_hard_reject() {
        let env = ArtifactOperationsEnvelope::default();
        // Below soft → no warning.
        assert!(env
            .quotas
            .evaluate(QuotaCounter::Artifact, 0)
            .unwrap()
            .is_none());
        // At soft, below hard → warning, no error.
        let warning = env
            .quotas
            .evaluate(QuotaCounter::Artifact, env.quotas.artifact.soft)
            .unwrap()
            .expect("soft warning expected");
        assert_eq!(warning.counter, QuotaCounter::Artifact);
        assert_eq!(warning.token(), "quota_artifact_soft");
        // At hard → hard reject, never a warning.
        let err = env
            .quotas
            .evaluate(QuotaCounter::Artifact, env.quotas.artifact.hard)
            .unwrap_err();
        match err {
            OperationsError::QuotaHardReject {
                counter,
                limit,
                current,
            } => {
                assert_eq!(counter, QuotaCounter::Artifact);
                assert_eq!(limit, env.quotas.artifact.hard);
                assert_eq!(current, env.quotas.artifact.hard);
            }
            other => panic!("expected QuotaHardReject, got {other:?}"),
        }
    }

    #[test]
    fn operations_envelope_rejects_invalid_env_values() {
        let mut fixture = t008_shrunken_artifact_operations_fixture_env();
        fixture.insert("CONTRIBUTION_BODY_MAX_BYTES", "not-a-number");
        let result =
            ArtifactOperationsEnvelope::from_env_with(|k| fixture.get(k).map(|s| (*s).to_string()));
        assert!(matches!(
            result,
            Err(OperationsError::InvalidEnvValue {
                key: "CONTRIBUTION_BODY_MAX_BYTES",
                ..
            })
        ));

        let mut fixture = t008_shrunken_artifact_operations_fixture_env();
        fixture.insert("CHUNK_TEXT_MAX_BYTES", "0");
        let result =
            ArtifactOperationsEnvelope::from_env_with(|k| fixture.get(k).map(|s| (*s).to_string()));
        assert!(matches!(
            result,
            Err(OperationsError::InvalidEnvValue {
                key: "CHUNK_TEXT_MAX_BYTES",
                ..
            })
        ));
    }

    #[test]
    fn operations_envelope_rejects_inverted_quota_thresholds() {
        let mut fixture = t008_shrunken_artifact_operations_fixture_env();
        fixture.insert("PROJECT_LINK_SOFT", "10");
        fixture.insert("PROJECT_LINK_HARD", "5");
        let err =
            ArtifactOperationsEnvelope::from_env_with(|k| fixture.get(k).map(|s| (*s).to_string()))
                .unwrap_err();
        assert!(matches!(
            err,
            OperationsError::InvalidQuotaThresholds {
                counter: QuotaCounter::Link,
                soft: 10,
                hard: 5,
            }
        ));
    }

    // ── Purge helpers ─────────────────────────────────────────────────────

    /// Build an archived artifact with one version that is older than the
    /// retention TTL. Returns `(artifact_id, version_id)`. The version's
    /// `created_at` is back-dated by `days_old` so a same-day purge is
    /// eligible without sleeping.
    fn seed_archived_version(
        conn: &Connection,
        project: &str,
        days_old: u32,
        labels: &[String],
    ) -> (String, String) {
        insert_project(conn, &test_project(project)).unwrap();
        let actor = artifact_actor_upsert(conn, &test_actor_identity("agent-purge")).unwrap();
        let artifact = artifact_insert(
            conn,
            &ArtifactInsert {
                project_ident: project,
                kind: "documentation",
                subkind: None,
                title: "purge fixture",
                labels,
                created_by_actor_id: &actor,
            },
        )
        .unwrap();
        // Archive the artifact (lifecycle_state = 'archived').
        conn.execute(
            "UPDATE artifacts SET lifecycle_state = 'archived', updated_at = ?2
             WHERE artifact_id = ?1",
            params![artifact, now_ms()],
        )
        .unwrap();
        let version = artifact_version_insert(
            conn,
            &ArtifactVersionInsert {
                artifact_id: &artifact,
                version_label: Some("v1"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("retired body"),
                structured_payload: Some(&serde_json::json!({"k": "v"})),
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "accepted",
                idempotency_key: None,
            },
        )
        .unwrap();
        // Back-date the version so the TTL check sees it as stale.
        let backdated = now_ms() - (days_old as i64) * 24 * 60 * 60 * 1000;
        conn.execute(
            "UPDATE artifact_versions SET created_at = ?2 WHERE artifact_version_id = ?1",
            params![version, backdated],
        )
        .unwrap();
        (artifact, version)
    }

    #[test]
    fn purge_archived_version_body_preserves_audit_metadata() {
        let conn = test_conn();
        let retention = RetentionPolicy::production_defaults();
        let (_artifact, version) = seed_archived_version(&conn, "purge-ok", 200, &[]);

        // Add a comment, link, and workflow_run so we can prove they survive.
        let actor = artifact_actor_upsert(&conn, &test_actor_identity("agent-aux")).unwrap();
        let run = workflow_run_insert(
            &conn,
            &WorkflowRunInsert {
                artifact_id: &_artifact,
                workflow_kind: "doc_publish",
                phase: None,
                round_id: None,
                coordinator_actor_id: &actor,
                participant_actor_ids: &[],
                source_artifact_version_id: Some(&version),
                read_set: None,
                idempotency_key: Some("k1"),
                is_resumable: true,
            },
        )
        .unwrap();
        artifact_comment_insert(
            &conn,
            &ArtifactCommentInsert {
                artifact_id: &_artifact,
                target_kind: "artifact_version",
                target_id: &version,
                child_address: None,
                parent_comment_id: None,
                actor_id: &actor,
                body: "audit note",
                idempotency_key: None,
            },
        )
        .unwrap();

        let outcome = purge_archived_version_body(&conn, &version, &retention, now_ms()).unwrap();
        assert_eq!(outcome, PurgeOutcome::Purged);

        // Body and structured_payload nulled; body_purged_at stamped.
        let (body, payload, purged_at, version_state): (
            Option<String>,
            Option<String>,
            Option<i64>,
            String,
        ) = conn
            .query_row(
                "SELECT body, structured_payload, body_purged_at, version_state
                 FROM artifact_versions WHERE artifact_version_id = ?1",
                params![version],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert!(body.is_none());
        assert!(payload.is_none());
        assert!(purged_at.is_some());
        assert_eq!(version_state, "accepted", "immutable state preserved");

        // Comment, workflow_run, and idempotency mapping survive.
        let comment_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM artifact_comments WHERE target_id = ?1",
                params![version],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(comment_count, 1);
        let run_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM workflow_runs WHERE workflow_run_id = ?1
                 AND idempotency_key = 'k1'",
                params![run],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(run_count, 1);

        // Second purge is a no-op (AlreadyPurged).
        let again = purge_archived_version_body(&conn, &version, &retention, now_ms()).unwrap();
        assert_eq!(again, PurgeOutcome::Skipped(PurgeSkipReason::AlreadyPurged));
    }

    #[test]
    fn purge_archived_version_body_respects_retain_permanent_label() {
        let conn = test_conn();
        let retention = RetentionPolicy::production_defaults();
        let (_artifact, version) =
            seed_archived_version(&conn, "purge-retain", 365, &["retain:permanent".into()]);
        let outcome = purge_archived_version_body(&conn, &version, &retention, now_ms()).unwrap();
        assert_eq!(
            outcome,
            PurgeOutcome::Skipped(PurgeSkipReason::RetainPermanent)
        );
        let body: Option<String> = conn
            .query_row(
                "SELECT body FROM artifact_versions WHERE artifact_version_id = ?1",
                params![version],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(body.as_deref(), Some("retired body"));
    }

    #[test]
    fn purge_archived_version_body_skips_when_age_below_ttl() {
        let conn = test_conn();
        let retention = RetentionPolicy::production_defaults();
        let (_artifact, version) = seed_archived_version(&conn, "purge-fresh", 7, &[]);
        let outcome = purge_archived_version_body(&conn, &version, &retention, now_ms()).unwrap();
        assert_eq!(outcome, PurgeOutcome::Skipped(PurgeSkipReason::AgeBelowTtl));
    }

    #[test]
    fn purge_archived_bodies_due_drives_bulk_run() {
        let conn = test_conn();
        let retention = RetentionPolicy::production_defaults();
        let (_a1, _v1) = seed_archived_version(&conn, "bulk-a", 365, &[]);
        let (_a2, _v2) = seed_archived_version(&conn, "bulk-b", 365, &["retain:permanent".into()]);
        let (_a3, _v3) = seed_archived_version(&conn, "bulk-c", 5, &[]);
        let summary = purge_archived_bodies_due(&conn, &retention, now_ms()).unwrap();
        assert_eq!(summary.purged, 1);
        assert_eq!(summary.skipped_retain_permanent, 1);
        // The recent (5-day-old) row is filtered by the SQL cutoff, not
        // counted as skipped — that's the idempotent property we want.
        assert_eq!(summary.skipped_already_purged, 0);
        // Running again is a clean no-op.
        let second = purge_archived_bodies_due(&conn, &retention, now_ms()).unwrap();
        assert_eq!(second.purged, 0);
    }

    // ── Restore checks ────────────────────────────────────────────────────

    #[test]
    fn restore_finding_collector_propagates_row_mapper_errors() {
        let conn = test_conn();
        let err = RestoreFindingCollector::new("SELECT 'only-column'")
            .collect(
                &conn,
                [],
                |r| r.get::<_, i64>(1),
                |_| -> Result<Vec<RestoreFinding>> {
                    panic!("finding mapper should not run after row mapper failure");
                },
            )
            .unwrap_err();

        assert!(
            err.to_string().contains("Invalid column index"),
            "got: {err}"
        );
    }

    #[test]
    fn restore_check_reports_pointer_mismatch_without_repair() {
        let conn = test_conn();
        let (actor, artifact_a) = seed_substrate(&conn, "restore-ptrs");
        // Create a second artifact owned by the same actor + project.
        let artifact_b = artifact_insert(
            &conn,
            &ArtifactInsert {
                project_ident: "restore-ptrs",
                kind: "spec",
                subkind: None,
                title: "second",
                labels: &[],
                created_by_actor_id: &actor,
            },
        )
        .unwrap();
        // Real version row belongs to artifact_b.
        let version_b = artifact_version_insert(
            &conn,
            &ArtifactVersionInsert {
                artifact_id: &artifact_b,
                version_label: Some("v1"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("body"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "accepted",
                idempotency_key: None,
            },
        )
        .unwrap();
        // Point artifact_a at artifact_b's version — schema FK is satisfied
        // (version row exists), but ownership check fails. This is the
        // pointer-mismatch case the restore helper is supposed to catch.
        conn.execute(
            "UPDATE artifacts SET current_version_id = ?2 WHERE artifact_id = ?1",
            params![artifact_a, version_b],
        )
        .unwrap();
        let findings = restore_check_artifact_pointers(&conn).unwrap();
        assert!(findings.iter().any(|f| f.tag == "restore:pointer_mismatch"
            && f.entity_id == artifact_a
            && f.detail.contains("belongs to artifact")));
        // Helper did NOT auto-clear the bad pointer (T004 §4.3: report only).
        let still_set: Option<String> = conn
            .query_row(
                "SELECT current_version_id FROM artifacts WHERE artifact_id = ?1",
                params![artifact_a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_set.as_deref(), Some(version_b.as_str()));
    }

    #[test]
    fn restore_check_reports_stuck_workflow_run() {
        let conn = test_conn();
        let (actor, artifact) = seed_substrate(&conn, "restore-stuck");
        let run = workflow_run_insert(
            &conn,
            &WorkflowRunInsert {
                artifact_id: &artifact,
                workflow_kind: "doc_publish",
                phase: None,
                round_id: None,
                coordinator_actor_id: &actor,
                participant_actor_ids: &[],
                source_artifact_version_id: None,
                read_set: None,
                idempotency_key: Some("stuck-1"),
                is_resumable: true,
            },
        )
        .unwrap();
        // Back-date the run start well beyond the 24h TTL.
        let backdated = now_ms() - 48 * 60 * 60 * 1000;
        conn.execute(
            "UPDATE workflow_runs SET started_at = ?2 WHERE workflow_run_id = ?1",
            params![run, backdated],
        )
        .unwrap();
        let retention = RetentionPolicy::production_defaults();
        let findings = restore_check_workflow_runs(&conn, &retention, now_ms()).unwrap();
        assert!(findings
            .iter()
            .any(|f| f.tag == "restore:stuck_workflow_run" && f.entity_id == run));
        // Helper did not flip the state.
        let state: String = conn
            .query_row(
                "SELECT state FROM workflow_runs WHERE workflow_run_id = ?1",
                params![run],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "started");
    }

    #[test]
    fn restore_check_report_is_clean_on_pristine_db() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::default();
        let report = run_restore_check(&conn, &envelope, now_ms()).unwrap();
        assert!(report.is_clean());
        assert_eq!(report.total_findings(), 0);
    }

    #[test]
    fn workflow_run_cancelled_is_terminal() {
        let conn = test_conn();
        let (actor, artifact) = seed_substrate(&conn, "cancel");
        let run = workflow_run_insert(
            &conn,
            &WorkflowRunInsert {
                artifact_id: &artifact,
                workflow_kind: "doc_publish",
                phase: None,
                round_id: None,
                coordinator_actor_id: &actor,
                participant_actor_ids: &[],
                source_artifact_version_id: None,
                read_set: None,
                idempotency_key: Some("pub-1"),
                is_resumable: true,
            },
        )
        .unwrap();
        workflow_run_set_state(&conn, &run, "cancelled", None).unwrap();
        let err = workflow_run_set_state(&conn, &run, "succeeded", None).unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid workflow_run state transition"),
            "got: {err}"
        );
    }

    /// T025: project-scoped artifact link visibility predicate.
    ///
    /// Both the link quota counter (`artifact_quota_count` with
    /// `QuotaCounter::Link`) and `list_artifact_links` must go through
    /// `artifact_link_visibility_clause`. This test exercises every visibility
    /// side (source artifact, target artifact, source version, target version)
    /// plus the negative case (external/discovery-only links), and asserts
    /// the two surfaces agree.
    #[test]
    fn artifact_link_visibility_predicate_is_consistent() {
        let conn = test_conn();
        let (actor_a, artifact_a) = seed_substrate(&conn, "proj-a");
        let (actor_b, artifact_b) = seed_substrate(&conn, "proj-b");

        let mk_version = |artifact_id: &str, actor_id: &str| -> String {
            artifact_version_insert(
                &conn,
                &ArtifactVersionInsert {
                    artifact_id,
                    version_label: None,
                    parent_version_id: None,
                    body_format: "markdown",
                    body: Some("body"),
                    structured_payload: None,
                    source_format: None,
                    created_by_actor_id: actor_id,
                    created_via_workflow_run_id: None,
                    version_state: "accepted",
                    idempotency_key: None,
                },
            )
            .unwrap()
        };
        let va = mk_version(&artifact_a, &actor_a);
        let vb = mk_version(&artifact_b, &actor_b);

        // Helper: inject a link directly. `artifact_link_insert` does not
        // enforce visibility, which is exactly what this test needs so it can
        // also construct the external/discovery-only counter-example.
        let mk_link = |link_type: &str,
                       source_kind: &str,
                       source_id: &str,
                       source_version_id: Option<&str>,
                       target_kind: &str,
                       target_id: &str,
                       target_version_id: Option<&str>,
                       actor_id: &str|
         -> String {
            artifact_link_insert(
                &conn,
                &ArtifactLinkInsert {
                    link_type,
                    source_kind,
                    source_id,
                    source_version_id,
                    source_child_address: None,
                    target_kind,
                    target_id,
                    target_version_id,
                    target_child_address: None,
                    created_by_actor_id: actor_id,
                    created_via_workflow_run_id: None,
                    idempotency_key: None,
                    supersedes_link_id: None,
                },
            )
            .unwrap()
        };

        // 1) Source artifact in A → external target.
        let l_src_artifact = mk_link(
            "doc_referenced_by_spec",
            "artifact",
            &artifact_a,
            None,
            "external_url",
            "https://example.com/x",
            None,
            &actor_a,
        );
        // 2) External source → target artifact in A.
        let l_tgt_artifact = mk_link(
            "doc_referenced_by_spec",
            "external_url",
            "https://example.com/y",
            None,
            "artifact",
            &artifact_a,
            None,
            &actor_a,
        );
        // 3) Source version belongs to A; sides themselves are external.
        let l_src_version = mk_link(
            "doc_referenced_by_spec",
            "external_url",
            "https://example.com/z1",
            Some(&va),
            "external_url",
            "https://example.com/z2",
            None,
            &actor_a,
        );
        // 4) Target version belongs to A; sides themselves are external.
        let l_tgt_version = mk_link(
            "doc_referenced_by_spec",
            "external_url",
            "https://example.com/w1",
            None,
            "external_url",
            "https://example.com/w2",
            Some(&va),
            &actor_a,
        );
        // 5) Pure external/discovery link — invisible to every project.
        let l_external = mk_link(
            "doc_referenced_by_spec",
            "external_url",
            "https://example.com/ext1",
            None,
            "external_url",
            "https://example.com/ext2",
            None,
            &actor_a,
        );
        // 6) Cross-project link (artifact A ↔ artifact B) — must surface in both.
        let l_cross = mk_link(
            "doc_referenced_by_spec",
            "artifact",
            &artifact_a,
            None,
            "artifact",
            &artifact_b,
            None,
            &actor_a,
        );
        // 7) Project-B-only link via target version.
        let l_b_only = mk_link(
            "doc_referenced_by_spec",
            "external_url",
            "https://example.com/b1",
            None,
            "external_url",
            "https://example.com/b2",
            Some(&vb),
            &actor_b,
        );

        // ── list_artifact_links: project A sees exactly the five A-side rows.
        let list_a = list_artifact_links(&conn, "proj-a", &ArtifactLinkFilters::default()).unwrap();
        let ids_a: std::collections::HashSet<&str> =
            list_a.iter().map(|l| l.link_id.as_str()).collect();
        assert!(ids_a.contains(l_src_artifact.as_str()), "source artifact");
        assert!(ids_a.contains(l_tgt_artifact.as_str()), "target artifact");
        assert!(ids_a.contains(l_src_version.as_str()), "source version");
        assert!(ids_a.contains(l_tgt_version.as_str()), "target version");
        assert!(ids_a.contains(l_cross.as_str()), "cross-project link in A");
        assert!(
            !ids_a.contains(l_external.as_str()),
            "external/discovery-only link must be excluded from project A"
        );
        assert!(
            !ids_a.contains(l_b_only.as_str()),
            "project-B-only link must not surface in project A"
        );

        // ── list_artifact_links: project B sees only the B-side rows.
        let list_b = list_artifact_links(&conn, "proj-b", &ArtifactLinkFilters::default()).unwrap();
        let ids_b: std::collections::HashSet<&str> =
            list_b.iter().map(|l| l.link_id.as_str()).collect();
        assert!(ids_b.contains(l_cross.as_str()), "cross-project link in B");
        assert!(
            ids_b.contains(l_b_only.as_str()),
            "B-only via target version"
        );
        assert!(
            !ids_b.contains(l_src_artifact.as_str()),
            "A-only source-artifact link must not surface in B"
        );
        assert!(
            !ids_b.contains(l_external.as_str()),
            "external/discovery-only link must be excluded from project B"
        );

        // ── Quota counter and list_artifact_links agree per project.
        let count_a = artifact_quota_count(&conn, "proj-a", QuotaCounter::Link).unwrap();
        let count_b = artifact_quota_count(&conn, "proj-b", QuotaCounter::Link).unwrap();
        assert_eq!(
            count_a as usize,
            list_a.len(),
            "link quota count must equal list_artifact_links length in project A"
        );
        assert_eq!(
            count_b as usize,
            list_b.len(),
            "link quota count must equal list_artifact_links length in project B"
        );

        // ── Pure external/discovery links exist in the table but are
        // invisible to every project — neither surface counts them, even
        // though cross-project links legitimately surface in both.
        let total_links: i64 = conn
            .query_row("SELECT COUNT(*) FROM artifact_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total_links, 7, "test corpus shape");
        assert!(
            !ids_a.contains(l_external.as_str()) && !ids_b.contains(l_external.as_str()),
            "pure external/discovery link must not appear in any project listing"
        );
        // Sanity: project A has the 4 A-side links + cross = 5; project B
        // has b_only + cross = 2. Cross legitimately double-counts across
        // projects; the external link is the only one nobody sees.
        assert_eq!(count_a, 5);
        assert_eq!(count_b, 2);
    }

    // ---------------------------------------------------------------------
    // T028 — target_kind/target_id and read_set reference validation tests.
    // Boundary: repository-level (see module comment near
    // `validate_target_ref`). Tests cover: (1) missing target, (2)
    // cross-artifact target, (3) unresolved read_set ref, (4) valid refs.
    // ---------------------------------------------------------------------

    fn t028_seed_two_artifacts(
        conn: &Connection,
        envelope: &ArtifactOperationsEnvelope,
    ) -> (
        String,
        ArtifactSummary,
        String,
        ArtifactSummary,
        ArtifactVersion,
    ) {
        insert_project(conn, &test_project("t028")).unwrap();
        let actor_a = resolve_artifact_actor(conn, &test_actor_identity("t028-a"))
            .unwrap()
            .actor_id;
        let actor_b = resolve_artifact_actor(conn, &test_actor_identity("t028-b"))
            .unwrap()
            .actor_id;
        let artifact_a = create_artifact(
            conn,
            envelope,
            &ArtifactInsert {
                project_ident: "t028",
                kind: "spec",
                subkind: None,
                title: "T028 artifact A",
                labels: &[],
                created_by_actor_id: &actor_a,
            },
        )
        .unwrap()
        .record;
        let artifact_b = create_artifact(
            conn,
            envelope,
            &ArtifactInsert {
                project_ident: "t028",
                kind: "spec",
                subkind: None,
                title: "T028 artifact B",
                labels: &[],
                created_by_actor_id: &actor_b,
            },
        )
        .unwrap()
        .record;
        let version_b = create_artifact_version(
            conn,
            envelope,
            &ArtifactVersionInsert {
                artifact_id: &artifact_b.artifact_id,
                version_label: Some("v1"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("artifact B v1 body"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor_b,
                created_via_workflow_run_id: None,
                version_state: "draft",
                idempotency_key: None,
            },
        )
        .unwrap()
        .record;
        (actor_a, artifact_a, actor_b, artifact_b, version_b)
    }

    #[test]
    fn t028_contribution_rejects_missing_target() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        let (actor_a, artifact_a, _actor_b, _artifact_b, _version_b) =
            t028_seed_two_artifacts(&conn, &envelope);
        let err = add_artifact_contribution(
            &conn,
            &envelope,
            &ArtifactContributionInsert {
                artifact_id: &artifact_a.artifact_id,
                target_kind: "artifact_version",
                target_id: "does-not-exist",
                contribution_kind: "review",
                phase: None,
                role: "reviewer",
                actor_id: &actor_a,
                workflow_run_id: None,
                read_set: None,
                body_format: "markdown",
                body: "x",
                idempotency_key: None,
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn t028_contribution_rejects_cross_artifact_target() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        let (actor_a, artifact_a, _actor_b, _artifact_b, version_b) =
            t028_seed_two_artifacts(&conn, &envelope);
        // version_b belongs to artifact_b, not artifact_a — must be rejected.
        let err = add_artifact_contribution(
            &conn,
            &envelope,
            &ArtifactContributionInsert {
                artifact_id: &artifact_a.artifact_id,
                target_kind: "artifact_version",
                target_id: &version_b.artifact_version_id,
                contribution_kind: "review",
                phase: None,
                role: "reviewer",
                actor_id: &actor_a,
                workflow_run_id: None,
                read_set: None,
                body_format: "markdown",
                body: "x",
                idempotency_key: None,
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("belongs to artifact"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn t028_comment_rejects_cross_artifact_target() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        let (actor_a, artifact_a, _actor_b, _artifact_b, version_b) =
            t028_seed_two_artifacts(&conn, &envelope);
        let err = add_artifact_comment(
            &conn,
            &envelope,
            &ArtifactCommentInsert {
                artifact_id: &artifact_a.artifact_id,
                target_kind: "artifact_version",
                target_id: &version_b.artifact_version_id,
                child_address: None,
                parent_comment_id: None,
                actor_id: &actor_a,
                body: "x",
                idempotency_key: None,
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("belongs to artifact"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn t029_comment_rejects_cross_artifact_parent_comment() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        let (actor_a, artifact_a, actor_b, artifact_b, version_b) =
            t028_seed_two_artifacts(&conn, &envelope);
        let version_a = create_artifact_version(
            &conn,
            &envelope,
            &ArtifactVersionInsert {
                artifact_id: &artifact_a.artifact_id,
                version_label: Some("v1"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("artifact A v1 body"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor_a,
                created_via_workflow_run_id: None,
                version_state: "draft",
                idempotency_key: None,
            },
        )
        .unwrap()
        .record;
        let parent = add_artifact_comment(
            &conn,
            &envelope,
            &ArtifactCommentInsert {
                artifact_id: &artifact_b.artifact_id,
                target_kind: "artifact_version",
                target_id: &version_b.artifact_version_id,
                child_address: None,
                parent_comment_id: None,
                actor_id: &actor_b,
                body: "parent on artifact B",
                idempotency_key: None,
            },
        )
        .unwrap()
        .record;
        let err = add_artifact_comment(
            &conn,
            &envelope,
            &ArtifactCommentInsert {
                artifact_id: &artifact_a.artifact_id,
                target_kind: "artifact_version",
                target_id: &version_a.artifact_version_id,
                child_address: None,
                parent_comment_id: Some(&parent.comment_id),
                actor_id: &actor_a,
                body: "reply should not cross artifact boundary",
                idempotency_key: None,
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("belongs to artifact"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn t028_contribution_rejects_unresolved_read_set_ref() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        let (actor_a, artifact_a, _actor_b, _artifact_b, _version_b) =
            t028_seed_two_artifacts(&conn, &envelope);
        // Need a same-artifact version target so we hit the read_set check.
        let version_a = create_artifact_version(
            &conn,
            &envelope,
            &ArtifactVersionInsert {
                artifact_id: &artifact_a.artifact_id,
                version_label: Some("v1"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("a v1"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor_a,
                created_via_workflow_run_id: None,
                version_state: "draft",
                idempotency_key: None,
            },
        )
        .unwrap()
        .record;
        let read_set = serde_json::json!({
            "versions": ["bogus-version-id"],
        });
        let err = add_artifact_contribution(
            &conn,
            &envelope,
            &ArtifactContributionInsert {
                artifact_id: &artifact_a.artifact_id,
                target_kind: "artifact_version",
                target_id: &version_a.artifact_version_id,
                contribution_kind: "review",
                phase: None,
                role: "reviewer",
                actor_id: &actor_a,
                workflow_run_id: None,
                read_set: Some(&read_set),
                body_format: "markdown",
                body: "x",
                idempotency_key: None,
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("does not resolve"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn t028_contribution_accepts_valid_target_and_read_set() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        let (actor_a, artifact_a, _actor_b, artifact_b, version_b) =
            t028_seed_two_artifacts(&conn, &envelope);
        let _ = artifact_b; // keep binding for clarity
        let version_a = create_artifact_version(
            &conn,
            &envelope,
            &ArtifactVersionInsert {
                artifact_id: &artifact_a.artifact_id,
                version_label: Some("v1"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("a v1"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &actor_a,
                created_via_workflow_run_id: None,
                version_state: "draft",
                idempotency_key: None,
            },
        )
        .unwrap()
        .record;
        // Cross-artifact version is a perfectly valid *read_set* reference
        // (the analyst can read across artifacts); only `target_id` is
        // constrained to the same artifact.
        let read_set = serde_json::json!({
            "versions": [version_a.artifact_version_id, version_b.artifact_version_id],
            "manifest_items": ["deferred-T011-address"], // deferred kind: skipped
        });
        let contribution = add_artifact_contribution(
            &conn,
            &envelope,
            &ArtifactContributionInsert {
                artifact_id: &artifact_a.artifact_id,
                target_kind: "artifact_version",
                target_id: &version_a.artifact_version_id,
                contribution_kind: "review",
                phase: None,
                role: "reviewer",
                actor_id: &actor_a,
                workflow_run_id: None,
                read_set: Some(&read_set),
                body_format: "markdown",
                body: "valid",
                idempotency_key: None,
            },
        )
        .unwrap()
        .record;
        assert_eq!(contribution.target_id, version_a.artifact_version_id);
    }

    struct DesignReviewFixture {
        actor: String,
        artifact: ArtifactSummary,
        source_version: ArtifactVersion,
        run: WorkflowRun,
    }

    fn seed_design_review_fixture(
        conn: &Connection,
        envelope: &ArtifactOperationsEnvelope,
    ) -> DesignReviewFixture {
        insert_project(conn, &test_project("t010")).unwrap();
        let actor = resolve_artifact_actor(conn, &test_actor_identity("t010-reviewer"))
            .unwrap()
            .actor_id;
        let artifact = create_artifact(
            conn,
            envelope,
            &ArtifactInsert {
                project_ident: "t010",
                kind: "design_review",
                subkind: None,
                title: "T010 design review",
                labels: &["review".to_string()],
                created_by_actor_id: &actor,
            },
        )
        .unwrap()
        .record;
        let source_version = create_artifact_version(
            conn,
            envelope,
            &ArtifactVersionInsert {
                artifact_id: &artifact.artifact_id,
                version_label: Some("source"),
                parent_version_id: None,
                body_format: "markdown",
                body: Some("# Proposal"),
                structured_payload: Some(&serde_json::json!({"workflow": "design_review"})),
                source_format: None,
                created_by_actor_id: &actor,
                created_via_workflow_run_id: None,
                version_state: "under_review",
                idempotency_key: Some("source-version"),
            },
        )
        .unwrap()
        .record;
        let run = start_workflow_run(
            conn,
            envelope,
            &WorkflowRunInsert {
                artifact_id: &artifact.artifact_id,
                workflow_kind: "design_review_round",
                phase: Some("pass_1"),
                round_id: Some("round-1"),
                coordinator_actor_id: &actor,
                participant_actor_ids: &[],
                source_artifact_version_id: Some(&source_version.artifact_version_id),
                read_set: None,
                idempotency_key: Some("round-1"),
                is_resumable: false,
            },
        )
        .unwrap()
        .record;
        DesignReviewFixture {
            actor,
            artifact,
            source_version,
            run,
        }
    }

    #[test]
    fn design_review_two_pass_fixture_filters_provenance() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        let fixture = seed_design_review_fixture(&conn, &envelope);
        let mut pass1_ids = Vec::new();
        for agent in ["codex", "claude", "gemini"] {
            let read_set = serde_json::json!({
                "versions": [fixture.source_version.artifact_version_id.clone()],
            });
            let contribution = add_artifact_contribution(
                &conn,
                &envelope,
                &ArtifactContributionInsert {
                    artifact_id: &fixture.artifact.artifact_id,
                    target_kind: "artifact_version",
                    target_id: &fixture.source_version.artifact_version_id,
                    contribution_kind: "review",
                    phase: Some("pass_1"),
                    role: "reviewer",
                    actor_id: &fixture.actor,
                    workflow_run_id: Some(&fixture.run.workflow_run_id),
                    read_set: Some(&read_set),
                    body_format: "markdown",
                    body: &format!("{agent} pass 1"),
                    idempotency_key: Some(agent),
                },
            )
            .unwrap()
            .record;
            append_workflow_run_outputs(
                &conn,
                &fixture.run.workflow_run_id,
                Some(&contribution.contribution_id),
                None,
            )
            .unwrap();
            pass1_ids.push(contribution.contribution_id);
        }

        for (idx, agent) in ["codex-p2", "claude-p2", "gemini-p2"].iter().enumerate() {
            let read_set = serde_json::json!({
                "versions": [fixture.source_version.artifact_version_id.clone()],
                "contributions": pass1_ids.clone(),
            });
            let contribution = add_artifact_contribution(
                &conn,
                &envelope,
                &ArtifactContributionInsert {
                    artifact_id: &fixture.artifact.artifact_id,
                    target_kind: "artifact_version",
                    target_id: &fixture.source_version.artifact_version_id,
                    contribution_kind: "review",
                    phase: Some("pass_2"),
                    role: "reviewer",
                    actor_id: &fixture.actor,
                    workflow_run_id: Some(&fixture.run.workflow_run_id),
                    read_set: Some(&read_set),
                    body_format: "markdown",
                    body: &format!("{agent} response"),
                    idempotency_key: Some(agent),
                },
            )
            .unwrap()
            .record;
            assert!(contribution
                .read_set
                .as_ref()
                .unwrap()
                .to_string()
                .contains(&pass1_ids[idx]));
        }

        let pass2 = list_design_review_contributions(
            &conn,
            "t010",
            &fixture.artifact.artifact_id,
            &DesignReviewContributionFilters {
                round_id: Some("round-1"),
                phase: Some("pass_2"),
                role: Some("reviewer"),
                reviewed_version_id: Some(&fixture.source_version.artifact_version_id),
                read_set_contains: Some(&pass1_ids[0]),
            },
        )
        .unwrap();
        assert_eq!(pass2.len(), 3);
        assert!(pass2
            .iter()
            .all(|c| c.workflow_run_id.as_deref() == Some(fixture.run.workflow_run_id.as_str())));
    }

    #[test]
    fn design_review_synthesis_version_and_state_preserve_pointers() {
        let conn = test_conn();
        let envelope = ArtifactOperationsEnvelope::production_defaults();
        let fixture = seed_design_review_fixture(&conn, &envelope);
        let accepted_version = create_artifact_version(
            &conn,
            &envelope,
            &ArtifactVersionInsert {
                artifact_id: &fixture.artifact.artifact_id,
                version_label: Some("accepted"),
                parent_version_id: Some(&fixture.source_version.artifact_version_id),
                body_format: "markdown",
                body: Some("# Accepted baseline"),
                structured_payload: None,
                source_format: None,
                created_by_actor_id: &fixture.actor,
                created_via_workflow_run_id: None,
                version_state: "draft",
                idempotency_key: Some("accepted-baseline"),
            },
        )
        .unwrap()
        .record;
        accept_artifact_version(
            &conn,
            "t010",
            &fixture.artifact.artifact_id,
            &accepted_version.artifact_version_id,
            &fixture.actor,
            None,
            Some("accept-baseline"),
        )
        .unwrap();
        let read_set = serde_json::json!({
            "versions": [fixture.source_version.artifact_version_id.clone()],
        });
        let synthesis = add_artifact_contribution(
            &conn,
            &envelope,
            &ArtifactContributionInsert {
                artifact_id: &fixture.artifact.artifact_id,
                target_kind: "artifact_version",
                target_id: &fixture.source_version.artifact_version_id,
                contribution_kind: "synthesis",
                phase: Some("synthesis"),
                role: "analyst",
                actor_id: &fixture.actor,
                workflow_run_id: Some(&fixture.run.workflow_run_id),
                read_set: Some(&read_set),
                body_format: "markdown",
                body: "# Synthesis",
                idempotency_key: Some("synthesis"),
            },
        )
        .unwrap()
        .record;
        let synthesized_version = create_artifact_version(
            &conn,
            &envelope,
            &ArtifactVersionInsert {
                artifact_id: &fixture.artifact.artifact_id,
                version_label: Some("synthesis"),
                parent_version_id: Some(&fixture.source_version.artifact_version_id),
                body_format: "markdown",
                body: Some("# Synthesis"),
                structured_payload: Some(&serde_json::json!({
                    "synthesis_contribution_id": synthesis.contribution_id.clone(),
                    "read_set": read_set.clone(),
                })),
                source_format: None,
                created_by_actor_id: &fixture.actor,
                created_via_workflow_run_id: Some(&fixture.run.workflow_run_id),
                version_state: "draft",
                idempotency_key: Some("synthesis-version"),
            },
        )
        .unwrap()
        .record;
        append_workflow_run_outputs(
            &conn,
            &fixture.run.workflow_run_id,
            Some(&synthesis.contribution_id),
            Some(&synthesized_version.artifact_version_id),
        )
        .unwrap();
        let updated = update_artifact(
            &conn,
            "t010",
            &fixture.artifact.artifact_id,
            &ArtifactUpdate {
                review_state: Some("needs_user_decision"),
                ..Default::default()
            },
            &envelope,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            updated.current_version_id.as_deref(),
            Some(synthesized_version.artifact_version_id.as_str())
        );
        assert_eq!(
            updated.accepted_version_id.as_deref(),
            Some(accepted_version.artifact_version_id.as_str())
        );
        assert_eq!(updated.review_state, "needs_user_decision");
        let run = get_workflow_run(&conn, &fixture.run.workflow_run_id)
            .unwrap()
            .unwrap();
        assert!(run
            .generated_contribution_ids
            .contains(&synthesis.contribution_id));
        assert!(run
            .generated_version_ids
            .contains(&synthesized_version.artifact_version_id));
    }
}
