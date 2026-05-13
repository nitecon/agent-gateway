# Artifact Substrate v1 Contract

**Status:** Draft contract (Phase 1, gateway-features_spec T002)
**Audience:** gateway backend implementers (T003, T005, T006, T007, T009, T010, T011), CLI/skill implementers (downstream tasks), reviewers
**Source:** `gateway-features.md` (Core Concepts, Actor Model, Relationship To Existing Primitives, Versioning And Diffs, Search And Retrieval, Rollout), `docs/artifacts-api-docs-integration.md` (T001 decision)
**Consumes (decisions already settled, not re-litigated here):**
- T001: API docs are a documentation artifact (kind=`documentation`, subkind=`api_context`) on this substrate, with a compatibility facade. See `docs/artifacts-api-docs-integration.md`.

## Purpose and non-goals

This document fixes the v1 product and data contract for the shared artifact
substrate. Downstream tasks turn this contract into schema (T005), repository
functions (T006), generic HTTP API (T007), workflow mutation endpoints (T003),
and per-workflow specializations (T009 specs, T010 design reviews, T011
documentation).

**In scope:** entity shapes, identity, immutability invariants, state
ownership, comment lifecycle, link contract, actor model, workflow-run /
activity contract, child-reference rules, body model choice and migration
path.

**Out of scope:** SQL DDL, route handlers, wire encoding details beyond named
fields, authorization policy specifics (the contract names the scopes that
must exist; T003 owns enforcement), UI views.

## Top-level invariants

These are the rules every downstream task is required to preserve. Violations
are bugs, not tradeoffs.

1. **Artifact vs. version split.** An *artifact* is a mutable container with a
   stable id. An *artifact_version* is an immutable snapshot of body and
   structured payload with a stable id. Once written, an artifact_version's
   body, structured payload, manifest items, child addresses, and
   `source_format` MUST NOT change. Corrections produce a new version.
2. **Audit and handoff target immutable versions.** Any link, chunk, comment,
   contribution, workflow input/output, or generated task that exists to
   reconstruct what an agent saw, what a reviewer reviewed, or what a task was
   generated from MUST reference an `artifact_version_id` (plus a stable
   child address where applicable). Links to the mutable `artifact_id` are
   reserved for discovery and navigation surfaces (lists, search results,
   "latest" lookups). T003 / T005 / T006 enforce this at the API and schema
   level; T007 surfaces both shapes distinctly.
3. **State separation.** Artifact lifecycle, current version pointer,
   accepted version pointer, review state, and implementation state are
   distinct fields with distinct owners. They do not collapse into one enum.
   See §"State model".
4. **Provenance ≠ authorization.** Actor and workflow_run fields record *who
   did what when*. They are not access-control decisions. T003 / T007 own
   the project-scoped authorization layer that gates reads and mutations
   (named scopes listed in §"Authorization scopes (named, not enforced
   here)").
5. **Idempotency on workflow mutation.** Every mutating workflow endpoint
   (version creation, contribution write, comment open/resolve, task
   generation, link creation, chunk emission) accepts an idempotency key and
   the substrate stores the resulting `(workflow_run_id, idempotency_key) ->
   produced resource ids` mapping so retries are safe. T003 owns the write
   contract; this document fixes the field shape on the substrate side.
6. **Body model is markdown for v1, with a structured-payload escape hatch.**
   See §"Body model and migration path". The contract preserves stable
   child addresses for spec manifest items, doc chunks, and generated tasks
   *before* block-level body structure exists, so block-level commenting can
   be added later without rewriting handoff links.

## Entities

The substrate defines eight entity kinds. Downstream schema (T005) MAY merge
or split tables as long as the conceptual contract is preserved.

### 1. Actor

Represents the identity of a contributing party — a human user, an agent
process, or the gateway system itself.

| Field | Type | Notes |
|---|---|---|
| `actor_id` | stable id | Globally unique. Stable across sessions. |
| `actor_type` | enum: `user`, `agent`, `system` | Required. |
| `agent_system` | nullable enum: `claude`, `codex`, `gemini`, `other` | Set when `actor_type = agent`. `other` carries a free-text `agent_system_label`. |
| `agent_id` | nullable string | The host-persistent agent id (e.g. `willsm4max-05bc2f77`) when known. |
| `host` | nullable string | Machine identity when known (e.g. `willsm4max.nitecon.net`). |
| `display_name` | string | UI label. May change; not used for identity. |
| `runtime_metadata` | nullable json | Optional model name, version, runtime — informational only. |
| `created_at`, `updated_at` | timestamps | |

**Role is NOT an actor attribute.** The same actor can be a reviewer in one
contribution and an implementer in another. Role lives on `contribution` and
`workflow_run` (see below). The `display_name` is the only attribute UI may
treat as a default label.

### 2. Artifact

A mutable, project-scoped container.

| Field | Type | Notes |
|---|---|---|
| `artifact_id` | stable id | Globally unique. Surfaced to skills/agents. |
| `project_ident` | string | Project scope. Existing project identity primitive. |
| `kind` | enum: `design_review`, `spec`, `documentation`, future kinds reserved | Required. Drives workflow specialization (T009/T010/T011). |
| `subkind` | nullable string | e.g. `api_context` for documentation artifacts (T001). |
| `title` | string | Human-readable. May be edited. |
| `labels` | string array | Free-form tags. |
| `lifecycle_state` | enum (see §"State model") | Artifact-level lifecycle. |
| `current_version_id` | nullable `artifact_version_id` | The version surfaced as "latest". |
| `accepted_version_id` | nullable `artifact_version_id` | The version blessed for downstream consumption (handoff to implement, chunk retrieval, etc.). MAY differ from `current_version_id`. |
| `created_by_actor_id` | `actor_id` | Provenance only. |
| `created_at`, `updated_at` | timestamps | |

The artifact id is the durable handle for skills: `/spec <artifact_id>`,
`/implement <artifact_id>`, `agent-tools docs get <artifact_id>` (per T001
compatibility facade).

### 3. Artifact version

An immutable snapshot. Created by contributions; never edited in place.

| Field | Type | Notes |
|---|---|---|
| `artifact_version_id` | stable id | Globally unique. |
| `artifact_id` | `artifact_id` | Owning container. |
| `version_label` | nullable string | Human label (e.g. `"2026-04-28"`, `"draft-3"`). Not an identity. |
| `parent_version_id` | nullable `artifact_version_id` | The version this one was derived from. Null only for the first version. |
| `body_format` | enum: `markdown`, `application/agent-context+json`, `openapi`, `swagger`, future formats reserved | See §"Body model". |
| `body` | bytes / text | Immutable. |
| `structured_payload` | nullable json | Used when the artifact kind requires structured handoff (spec manifest, doc chunks-source, review round metadata). Immutable. See §"Structured payload". |
| `source_format` | nullable string | Original input format when different from `body_format` (e.g. body is agent-context+json, source was openapi). Preserved verbatim from T001. |
| `created_by_actor_id` | `actor_id` | Provenance. |
| `created_via_workflow_run_id` | nullable `workflow_run_id` | Set when the version was produced inside a workflow run (review synthesis, spec acceptance, doc publish). |
| `version_state` | enum (see §"State model") | Per-version state — draft / under_review / accepted / superseded / rejected. |
| `created_at` | timestamp | Immutable. |

**Immutability:** all fields except `version_state` are write-once.
`version_state` transitions are append-only via the state model (§"State
model") and recorded as state-transition contributions or workflow
activities, never silently overwritten.

### 4. Contribution

An actor's input to an artifact or specific version.

| Field | Type | Notes |
|---|---|---|
| `contribution_id` | stable id | |
| `artifact_id` | `artifact_id` | |
| `target_kind` | enum: `artifact`, `artifact_version`, `contribution` | What the contribution is attached to. Contributions targeting other contributions support response/synthesis threads. |
| `target_id` | id matching `target_kind` | |
| `contribution_kind` | enum: `review`, `synthesis`, `decision`, `note`, `completion`, `state_transition`, future kinds reserved | |
| `phase` | nullable string | e.g. `pass_1`, `pass_2`, `synthesis`. Free-form within a workflow_run. |
| `role` | enum: `author`, `reviewer`, `analyst`, `implementer`, `coordinator`, `user` | Role IS on the contribution, not the actor. |
| `actor_id` | `actor_id` | |
| `workflow_run_id` | nullable `workflow_run_id` | Set when the contribution was produced inside a workflow run. |
| `read_set` | nullable json | Deterministic record of prior `contribution_id`s and `artifact_version_id`s the actor read before producing this contribution. Required for pass-2 / synthesis contributions; optional otherwise. |
| `body_format` | enum (same set as version) | |
| `body` | bytes / text | Immutable. |
| `created_at` | timestamp | Immutable. |

Contributions are immutable once written. Corrections produce a new
contribution (optionally targeting the prior one with `contribution_kind =
note`). The `read_set` field gives the research analyst and audit views a
deterministic basis for reconstructing what each peer saw.

### 5. Comment

User and agent discussion attached to substrate entities.

| Field | Type | Notes |
|---|---|---|
| `comment_id` | stable id | |
| `target_kind` | enum (v1): `artifact`, `artifact_version`, `contribution` | Block/range targets deferred (see §"Comment lifecycle"). |
| `target_id` | id matching `target_kind` | |
| `artifact_id` | `artifact_id` | Denormalized for project-scoped queries. |
| `child_address` | nullable string | Only valid when `target_kind = artifact_version`. Anchors the comment to a stable manifest item, chunk path, or other v1 child address inside the immutable version. |
| `parent_comment_id` | nullable `comment_id` | Threading. |
| `actor_id` | `actor_id` | Author. |
| `body` | text | Free-form markdown. |
| `state` | enum: `open`, `resolved` | See §"Comment lifecycle". |
| `resolved_by_actor_id` | nullable `actor_id` | Set when `state = resolved`. |
| `resolved_by_workflow_run_id` | nullable `workflow_run_id` | Set when resolution happened inside a workflow. |
| `resolved_at` | nullable timestamp | |
| `resolution_note` | nullable text | Why the comment was resolved. |
| `created_at`, `updated_at` | timestamps | |

### 6. Link

A typed relationship from one substrate-or-external resource to another.

| Field | Type | Notes |
|---|---|---|
| `link_id` | stable id | |
| `link_type` | string | E.g. `spec_implements_design`, `task_generated_from_spec`, `chunk_of_version`, `doc_referenced_by_spec`, `comment_references_task`. T003 / T009 / T010 / T011 each declare the link types they emit; the substrate stores the type but does not enumerate it. |
| `source_kind` | enum: `artifact`, `artifact_version`, `contribution`, `comment`, `task`, `chunk`, `pattern`, `memory`, `commit`, `external_url` | |
| `source_id` | id matching `source_kind` | |
| `source_version_id` | nullable `artifact_version_id` | Required when source is an artifact and the link is in an audit/handoff path (see invariant 2). |
| `source_child_address` | nullable string | Stable child address within the source version. See §"Child references". |
| `target_kind` | same enum as `source_kind` | |
| `target_id` | id matching `target_kind` | |
| `target_version_id` | nullable `artifact_version_id` | Same rule as source. |
| `target_child_address` | nullable string | |
| `created_by_actor_id` | `actor_id` | |
| `created_via_workflow_run_id` | nullable `workflow_run_id` | |
| `idempotency_key` | nullable string | When set, `(workflow_run_id, idempotency_key)` is unique. |
| `supersedes_link_id` | nullable `link_id` | Soft deletion / replacement. Links are not hard-deleted in audit paths. |
| `created_at` | timestamp | |

**Audit/handoff path enforcement:** the substrate stores
`source_version_id` / `target_version_id` whenever the link participates in
an audit, handoff, chunk, comment-anchor, or provenance path. T003 declares
which `link_type` values are audit-path; T005 / T006 reject links that
violate the rule. Discovery/navigation links (e.g. "show me the latest
version of this doc") MAY omit version ids.

### 7. Chunk

Retrieval payload generated from an artifact version. Documentation kinds
chunk by default; specs and reviews chunk optionally.

| Field | Type | Notes |
|---|---|---|
| `chunk_id` | stable id | |
| `artifact_id` | `artifact_id` | Denormalized for filter convenience. |
| `artifact_version_id` | `artifact_version_id` | **Required.** Chunks always anchor to an immutable version. |
| `child_address` | string | Stable address inside the version body / structured payload (see §"Child references"). |
| `embedding_model` | string | |
| `embedding_vector` | bytes / vector | |
| `text` | text | The chunk text presented to retrieval clients. |
| `created_at` | timestamp | |
| `superseded_by_chunk_id` | nullable `chunk_id` | When a re-chunk replaces this row, the old row is soft-superseded so historical retrieval reconstruction stays possible. |

When an artifact version is superseded, its chunks remain queryable (with
the appropriate version filter). The current-version-only filter is the
default for agent retrieval; history-aware queries opt in explicitly
(consumes `gateway-features.md` "Search And Retrieval").

### 8. Workflow run / activity

A first-class record of a workflow execution, borrowing the PROV-O
distinction between agents (`actor`), activities (`workflow_run`), and
entities (`artifact_version`, `contribution`, `task`, `link`, `chunk`) —
**without** adopting RDF or JSON-LD.

| Field | Type | Notes |
|---|---|---|
| `workflow_run_id` | stable id | |
| `artifact_id` | `artifact_id` | The artifact this run operates on. |
| `workflow_kind` | enum: `design_review_round`, `spec_iteration`, `spec_acceptance`, `spec_task_generation`, `doc_publish`, future kinds reserved | T009 / T010 / T011 declare the kinds they emit. |
| `phase` | nullable string | E.g. `pass_1`, `pass_2`, `synthesis`, `acceptance`. |
| `round_id` | nullable string | Stable handle for multi-pass rounds (review pass1/pass2/synthesis share a `round_id`). |
| `coordinator_actor_id` | `actor_id` | The actor that initiated the run. |
| `participant_actor_ids` | actor_id array | All actors that contributed to the run. |
| `source_artifact_version_id` | nullable `artifact_version_id` | The version this run reviewed/transformed. Required for audit-path workflows. |
| `read_set` | nullable json | Deterministic record of prior contribution / version ids the run was authorized to read. |
| `idempotency_key` | nullable string | Together with `workflow_run_id` enables retry-safe writes. |
| `started_at`, `ended_at` | timestamps | |
| `state` | enum: `started`, `succeeded`, `failed`, `cancelled` | |
| `generated_contribution_ids` | contribution_id array | Filled as contributions are written. |
| `generated_version_ids` | artifact_version_id array | Versions created by the run. |
| `generated_task_ids` | external task id array | Gateway task ids created/linked by the run (T009 spec_task_generation). |
| `generated_link_ids` | link_id array | |
| `generated_chunk_ids` | chunk_id array | |
| `failure_reason` | nullable text | |

The workflow_run record is what makes the substrate retry-safe and
audit-reconstructible. A failed spec_task_generation run leaves its
`workflow_run_id` recorded with `state = failed` and partial
`generated_task_ids`; a retry with the same `(workflow_run_id,
idempotency_key)` either continues idempotently or starts a new run that
references the failed one by id. T003 owns the exact retry semantics; the
substrate guarantees the fields exist to support them.

## Body model and migration path

**v1 body model:** markdown is the default human-authored body. Structured
JSON payloads ride alongside the markdown body in `artifact_version.structured_payload`
when the artifact kind requires machine-precise handoff.

Specifically:

- **design_review artifacts:** body = markdown. `structured_payload` MAY hold
  per-round metadata (phase, peer roster, idempotency keys) but is not
  required for v1. Contributions carry the structured fields the analyst
  needs.
- **spec artifacts:** body = markdown. `structured_payload` is **required**
  on any version that is accepted or that downstream tasks consume. It
  carries the manifest (see §"Structured payload — spec manifest"). T009
  owns the schema.
- **documentation artifacts (incl. api_context):** `body` is the canonical
  agent retrieval body. For `subkind = "api_context"` it MUST use
  `body_format = application/agent-context+json` per T001 docs-first
  principle. When the version was derived from OpenAPI or Swagger,
  `source_format` records `openapi` or `swagger` and the raw source bytes
  live in `structured_payload.source`, a linked source artifact/version, or
  another explicitly named source field chosen by T011/T013. Raw OpenAPI or
  Swagger MUST NOT replace the canonical agent-context body unless T011 also
  defines where the canonical retrieval body is materialized.

**Why not structured JSON blocks for everything in v1?** A typed block
model would let block-level comments land cleanly, but block schemas are
not yet stable across the three workflow kinds and forcing one risks
churning the substrate before it carries production load. Markdown +
structured_payload + child-address rules give us the migration path
without locking in block schemas prematurely.

**Migration path to block-level structure:**

1. v1: markdown body + stable child addresses for the entities that need
   precise handoff today (spec manifest items, doc chunks, future
   selectors).
2. v1.5: introduce per-kind block parsers that derive stable block ids
   from markdown sections (heading paths + content hash) and persist
   them in `structured_payload.block_index`. Existing child addresses
   (manifest item ids, chunk paths) are preserved by the parser so no
   stored link breaks.
3. v2: optional rich body format
   (`application/structured-blocks+json`) per artifact kind. Block ids
   become first-class fields. Block-level comments use W3C Web
   Annotation-shaped selectors (selector type, selector payload,
   selector state, immutable artifact_version_id) without adopting RDF
   or JSON-LD. Comment target enum gains `version_block`.
4. v2+: per-kind selector strategies (range selectors for prose, path
   selectors for structured JSON) layered on the same comment-target
   contract.

The block-index migration is additive. v1 links and comments do not need
to be rewritten — they continue to anchor on `(artifact_version_id,
child_address)`.

## Structured payload — spec manifest (worked example for T009)

When `artifact.kind = "spec"` and a version is accepted, the
`structured_payload` field MUST conform to:

```
{
  "manifest_version": "1",
  "items": [
    {
      "manifest_item_id": "<stable id, unique within version>",
      "task_code": "<e.g. T002>",
      "phase_id": "<optional phase identifier>",
      "team": "<backend|frontend|qa|docs|...>",
      "title": "<string>",
      "status": "<todo|in_progress|blocked|done|skip>",
      "depends_on_manifest_item_ids": ["<id>", ...],
      "labels": ["<string>", ...],
      "touch_surface": ["<repo path or logical surface>", ...],
      "spec_body": "<focused per-task handoff body or null>",
      "spec_file_path": "<source-adjacent path when imported from a spec directory, or null>",
      "acceptance_criteria": ["<string>", ...],
      "validation_plan": {
        "commands": ["<command>", "..."],
        "checks": ["<manual or automated check>", "..."]
      },
      "source_section_path": "<markdown heading path or null>",
      "generated_task_ids": ["<gateway task id>", ...]
    }
  ]
}
```

`manifest_item_id` is the stable child address used by:

- `task.source` references back to the spec (`spec_artifact_id`,
  `spec_artifact_version_id`, `manifest_item_id`) — invariant 2.
- spec_task_generation workflow_run records
  (`generated_task_ids` indexed by `manifest_item_id` in the run's
  generated-resources mapping).
- comments anchored to manifest items (target = `artifact_version` +
  `child_address = "manifest.items[<manifest_item_id>]"`).

T009 owns evolution of this schema; this contract fixes the minimum
fields required for downstream tasks (T005 schema, T006 repo, T003
mutation, implement-skill consumption).

The manifest schema MUST be able to represent both gateway-native specs and
source-adjacent spec directories like `gateway-features_spec/`. Imported
spec-directory tasks preserve their focused task file body in `spec_body`
and `spec_file_path`, but the accepted artifact version is the canonical
planning state once imported.

## State model

State is split across multiple owned fields so audit and downstream
behavior remain unambiguous. No collapse into a single status enum.

| Field | Owned by | Values | Semantics |
|---|---|---|---|
| `artifact.lifecycle_state` | artifact | `draft`, `active`, `superseded`, `archived` | Coarse lifecycle of the *container*. `superseded` means a replacement artifact exists (link with `link_type = "supersedes_artifact"`). |
| `artifact.current_version_id` | artifact | pointer | The version surfaced as "latest" in UI/CLI. Updated when a new version is created. |
| `artifact.accepted_version_id` | artifact | pointer | The version downstream consumers (implement, chunk retrieval, doc serve) target. MAY lag `current_version_id` (draft under review) or lead it (later edits not yet accepted). MAY equal it. |
| `artifact_version.version_state` | version | `draft`, `under_review`, `accepted`, `superseded`, `rejected` | Per-version state. Transitions are append-only via workflow_run / contribution of kind `state_transition`. |
| `artifact.review_state` | artifact | `none`, `collecting_reviews`, `synthesizing`, `needs_user_decision`, `accepted`, `rejected` | Kind-aware: design_review uses the full enum; spec uses `none` + `collecting_reviews` + `accepted`; documentation uses `none` only by default. |
| `artifact.implementation_state` | artifact | `not_applicable`, `not_started`, `in_progress`, `complete`, `blocked` | Only meaningful for `kind = "spec"`. Derived but persisted for fast filtering. Updated by spec_task_generation workflow + task-completion contributions. |

**Why five fields, not one:** a spec can be `lifecycle_state=active`,
`accepted_version_id=v3` (downstream tasks consume v3), `current_version_id=v4`
(a later draft is under review), `review_state=collecting_reviews` (peers
are reviewing v4), `implementation_state=in_progress` (tasks from v3 are
running). Collapsing this into one enum loses information that audit and UI
both need.

**Transition recording:** every state change is recorded as either a
`contribution` of `contribution_kind = "state_transition"` (when an actor
explicitly decides) or as a `workflow_run` activity (when a workflow
produces the change). Silent writes are a contract violation.

## Comment lifecycle

### v1 supported targets

- `artifact` — discussion attached to the container ("does this artifact
  still belong on this project?").
- `artifact_version` — discussion attached to a specific version ("v3
  introduces a regression").
- `contribution` — discussion attached to a peer's review or synthesis
  contribution ("Codex's pass-1 missed the cache invalidation case").

### Deferred to v2 (block / range)

Block-level and text-range comments are **not** v1 targets. They are
deferred until the block-index migration (§"Migration path") lands. The
contract still captures the requirement so v2 work is additive:

- v2 adds `target_kind = "version_block"` with `child_address` set to
  the stable block id (from the v1.5 block_index migration).
- v2 selectors follow W3C Web Annotation Selectors and States shape
  (selector type, selector payload, selector state, immutable
  `artifact_version_id`) — not raw heading text and not byte offsets
  against mutable bodies.
- v1 child addresses already in use (manifest item ids, chunk paths)
  are valid block ids in v2 without rewriting stored comments / links.

### State transitions

`open` and `resolved` are the only states. Transitions:

- `open -> resolved`: requires `resolved_by_actor_id` and `resolved_at`.
  SHOULD set `resolution_note`. If resolution happened inside a
  workflow, SHOULD set `resolved_by_workflow_run_id`.
- `resolved -> open`: allowed (a user or analyst can re-open). Resets
  `resolved_by_*` and `resolved_at` to null. The re-open itself is
  recorded as a comment of kind `note` on the same thread so the
  resolution history is reconstructible.

**Resolution provenance trace.** Given a `resolved` comment, the audit
path is:

1. `comment.resolved_by_actor_id` -> actor identity.
2. `comment.resolved_by_workflow_run_id` -> workflow run record ->
   `coordinator_actor_id`, `phase`, `read_set`, `generated_*` arrays.
3. Sibling comments on the same `target_id` and any contributions of
   kind `decision` linked via `parent_comment_id` or via `link`
   records with `link_type = "decision_resolves_comment"`.

This satisfies the T002 validation-plan "comment lifecycle check".

## Link contract

The link contract is enumerated in §"Entities -> Link" above. Additional
rules:

- **Audit-path links** (any link whose `link_type` participates in
  reconstructing what an agent saw or what downstream resource a task
  derived from) MUST set `source_version_id` and/or `target_version_id`
  whenever the source/target is an `artifact`. T003 declares the
  audit-path link types; T005 / T006 enforce via constraint.
- **Discovery / navigation links** (UI surfacing, "latest" lookups,
  search filters) MAY omit version ids. They are not audit-path.
- **Idempotency.** When a workflow emits links, it MUST set
  `idempotency_key`. `(workflow_run_id, idempotency_key)` is unique.
  Retries of the same workflow run produce the same link id.
- **Soft supersession.** Links in audit paths are never hard-deleted.
  Replacement uses `supersedes_link_id`. Discovery links MAY be hard
  deleted by their owning workflow.

### v1 link types (initial registry)

The registry below names the link types each downstream task is
expected to emit. New types MAY be added by later tasks without
revising this contract.

| `link_type` | Source kind | Target kind | Audit-path? | Emitted by |
|---|---|---|---|---|
| `spec_implements_design` | artifact (spec) | artifact (design_review) | yes | T009 |
| `task_generated_from_spec` | task | artifact_version (spec) | yes | T009 |
| `chunk_of_version` | chunk | artifact_version (documentation) | yes | T011 |
| `doc_referenced_by_spec` | artifact (spec) | artifact (documentation) | no (discovery) | T009 |
| `comment_references_task` | comment | task | no | T010 |
| `decision_resolves_comment` | contribution (decision) | comment | yes | T010 |
| `supersedes_artifact` | artifact | artifact | yes | T003 |
| `supersedes_version` | artifact_version | artifact_version | yes | T003 |

## Actor model details

See §"Entities -> Actor". Key contract points repeated for emphasis:

- Identity is `actor_id` (stable). `display_name` is UI label only.
- `agent_id` + `host` are the bridge to existing
  `agent-tools comms whoami` shape so existing project identity stays
  consistent.
- **Role is not a durable actor attribute.** It lives on `contribution`
  and on `workflow_run.participant_actor_ids` (where each participant
  may declare its role in the run).
- Minimum identity shape that works across Claude, Codex, Gemini, and
  future agents: `(actor_type, agent_system, agent_id?, host?)`. The
  substrate accepts agents that report only `agent_system` (no stable
  `agent_id`) but downgrades them in audit views to "best-effort
  identity" so downstream consumers can decide whether to require
  stable ids.

## Workflow run / activity contract

See §"Entities -> Workflow run / activity" above. This section names the
v1 workflow kinds and the substrate guarantees they consume.

| Workflow kind | Owner | Substrate guarantees consumed |
|---|---|---|
| `design_review_round` | T010 | Multiple contributions per run, with `phase` and `read_set`; workflow_run as the durable round_id holder. |
| `spec_iteration` | T009 | Contributions + new artifact_version possibly; structured_payload schema preserved. |
| `spec_acceptance` | T009 | `version_state` -> `accepted` transition recorded as `state_transition` contribution; `artifact.accepted_version_id` updated atomically with the run. |
| `spec_task_generation` | T009 | Idempotent task creation keyed by `(workflow_run_id, manifest_item_id)`; tasks back-link to `(artifact_id, artifact_version_id, manifest_item_id)`. |
| `doc_publish` | T011 | New `artifact_version` per publish; chunks regenerated and old chunks soft-superseded; api_context subkind preserved. |

Each workflow MUST declare its `idempotency_key` shape in its T009 /
T010 / T011 spec so retries are reproducible.

## Child references (stable handoff addresses)

The substrate carries `child_address` on links, comments, and chunks even
though v1 bodies are markdown. The address schemes per artifact kind are:

| Artifact kind | Child address scheme | Notes |
|---|---|---|
| `spec` | `manifest.items[<manifest_item_id>]` for manifest items; `manifest.items[<id>].acceptance_criteria[<index>]` for AC; `body.section[<heading-path-slug>]` for raw markdown sections | Manifest ids are stable; section paths are best-effort and MAY shift across versions (use the v1.5 block_index to stabilize). |
| `design_review` | `body.section[<slug>]`; future block ids land via the v2 migration | |
| `documentation` (incl. api_context) | Existing api_docs chunk path scheme (`purpose`, `workflows[<n>].steps[<m>]`, `endpoints[<n>]`, etc.) per T001 | Already stable. Chunks anchor on these addresses. |

**Stable manifest item id requirement (T002 acceptance criterion).**
Spec manifests MUST carry `manifest_item_id` on every item. The id is
unique within the artifact version and SHOULD remain stable across
versions when the item is the "same" item conceptually (T009 owns the
stability heuristic). Tasks generated from manifest items store
`(spec_artifact_id, spec_artifact_version_id, manifest_item_id)` so
implement-skill can fetch the originating manifest item even after the
spec advances to a new version.

## Authorization scopes (named, not enforced here)

T003 owns enforcement. The substrate guarantees that every mutating
endpoint declares which of these scopes it requires:

- `artifact.read`, `artifact.write`, `artifact.administer`
- `artifact_version.create`, `artifact_version.accept`
- `contribution.write`
- `comment.write`, `comment.resolve`
- `link.write`
- `chunk.write`, `chunk.read`
- `workflow_run.start`, `workflow_run.complete`
- `task.generate_from_spec`
- `project.administer` (membership and quota)

The v1 vertical slice MAY implement these as a trusted single-user / single-
project boundary, but the API, CLI, and UI MUST surface the boundary
explicitly per `gateway-features.md` "Actor Model" so the limit is not
mistaken for general workspace readiness.

## Relationship to existing primitives

- **Tasks.** Tasks remain the execution surface. A task generated from a
  spec stores `(spec_artifact_id, spec_artifact_version_id,
  manifest_item_id)` on its `source` field so the immutable spec version
  and the stable manifest item id are both reachable. The task
  `Specification` field still holds the focused per-task spec body for
  cold-agent resume; the spec artifact holds the planning context.
- **Memory.** Memory remains the distilled-learning surface. The
  substrate does **not** auto-write memories. Skills decide when an
  artifact contribution implies a reusable memory and call `memory
  store` separately.
- **Patterns.** Patterns remain organization-wide guidance. Links of
  type `pattern_applied_to_artifact_version` are allowed (registered by
  T003) so spec/review workflows can record which patterns were
  consulted. Pattern bodies stay in the patterns store; only the link
  lives on the substrate.
- **API docs.** Per T001 decision, API docs are `kind=documentation`,
  `subkind=api_context` artifacts. Old `api_doc_id` values are
  preserved as `artifact_id`. The `agent-tools docs` CLI remains the
  canonical agent retrieval verb during migration; `agent-tools
  artifacts` is the substrate-general verb set.

## Operations envelope (named, owned downstream)

Per `gateway-features.md` "Rollout" the first production slice has an
operations envelope. The substrate names the fields it needs to surface
so T007 / T011 / SRE can enforce:

- **Sizes.** `artifact_version.body` and `contribution.body` each have a
  per-project maximum size (default named in T011). Oversize writes are
  rejected, not truncated.
- **Quotas.** Per-project counts for artifacts, versions, contributions,
  comments, links, chunks. Soft warning + hard limit per project.
- **Retention.** Lifecycle archives (after `lifecycle_state =
  archived`) MAY drop body bytes after N days; ids, version_state, and
  audit links persist. T011 owns the retention policy.
- **Backups.** Substrate is in the gateway backup envelope; chunks /
  embeddings are regeneratable from versions and MAY be excluded.
- **Metrics.** Counters for artifact write rate, version creation rate,
  contribution write rate, comment open/resolve rate, link emit rate,
  chunk regeneration rate, search-by-id hit rate, retrieval staleness
  (chunks pointing to non-current accepted versions).
- **Stale chunk handling.** Retrieval responses MUST surface whether a
  returned chunk anchors to the currently accepted version or to an
  older / superseded version so agents can decide whether to refetch.

## Validation against T002 acceptance criteria

- [x] **Defines artifacts, immutable artifact versions, contributions,
      comments, links, actors, and workflow runs/activities.** §"Entities"
      defines all eight (chunks included).
- [x] **Chooses v1 body model and names the migration path.** §"Body
      model and migration path" — markdown body + optional
      `structured_payload`, migration via block_index + W3C Web
      Annotation-shaped selectors.
- [x] **Audit and handoff links target immutable versions plus stable
      child identifiers.** Invariant 2 + §"Link contract" +
      §"Child references".
- [x] **Separates artifact lifecycle, current version, accepted version,
      review state, implementation state.** §"State model".
- [x] **v1 comment targets, open/resolved transitions, resolution
      provenance, deferred block/range selectors.** §"Comment lifecycle".
- [x] **Stable manifest item / child-address fields for spec-to-task
      handoff.** §"Structured payload — spec manifest" + §"Child
      references" + T009 task source-field rule.

## Validation plan results

- **Traceability check.** Every Core Concept section in
  `gateway-features.md` maps to a contract section:
  Artifact -> §Entities/Artifact; Artifact Version -> §Entities/Version
  + §Body model; Contribution -> §Entities/Contribution; Comment ->
  §Entities/Comment + §Comment lifecycle; Actor Model -> §Entities/Actor
  + §Actor model details; Relationship To Existing Primitives ->
  §Relationship to existing primitives + §Link contract; Versioning
  And Diffs -> §State model + §Body model; Search And Retrieval ->
  §Entities/Chunk + §Operations envelope; Workflow run/activity ->
  §Entities/Workflow run + §Workflow run contract.
- **Invariant check.** §"Top-level invariants" rule 2 forbids mutable
  artifact links in audit/handoff paths; §"Link contract" enumerates
  audit-path link types; discovery-only links are explicitly carved
  out.
- **Comment lifecycle check.** §"Comment lifecycle -> Resolution
  provenance trace" walks the open -> resolved -> trace path through
  actor, workflow_run, and sibling decision contributions.
- **Downstream readiness.** T005 can turn each entity into one or more
  tables without inventing new conceptual entities. The state model
  decomposes cleanly into a small number of nullable pointer columns
  + per-version state. The structured_payload spec-manifest schema is
  fixed enough that T009 can implement without reopening this
  contract.

## Open questions surfaced for downstream tasks

These are explicitly handed to the named owners. They are not blockers
for T005 / T006 / T007.

1. **(T009)** Spec manifest stability heuristic: when is a v_n
   `manifest_item_id` the "same item" as a v_{n+1} `manifest_item_id`?
   Required so tasks generated against v_n stay valid after v_{n+1}.
2. **(T010)** Review round definition: is a "round" one
   `workflow_run` per phase (one for pass_1, one for pass_2, one for
   synthesis) or one `workflow_run` with phase transitions? The
   substrate supports either; T010 picks.
3. **(T011)** Documentation export back to repository (T013-owned per
   T001) — not re-opened here, but T011 needs to confirm that
   chunk regeneration on doc_publish is the correct invalidation
   boundary for retrieval freshness.
4. **(T003)** Idempotency key shape: free-form opaque token vs.
   structured `(workflow_run_id, sub-step)` namespacing. The substrate
   stores whatever T003 picks; T003 decides.
5. **(T003 / SRE)** Authorization scope binding: which scopes are
   project-membership-bound vs. project-administrator-bound. The
   substrate names the scopes; T003 binds them.

## Hand-off

- **T003** (workflow mutation contract): consumes every workflow_run
  field, idempotency rules, and named authorization scopes. Owns the
  audit-path link-type registry.
- **T005** (schema): turns entities into tables. May merge actor /
  workflow_run into the same logical store as long as ids and state
  fields are distinct. Adds `artifact_chunks` per T001 + this
  contract.
- **T006** (repository functions): exposes the substrate operations
  named here without leaking SQL specifics into T007.
- **T007** (generic HTTP API): surfaces the entities as resources.
  Surface name choices are T007's; identity and immutability rules are
  this contract's.
- **T009** (spec workflow): consumes §"Structured payload — spec
  manifest", workflow kinds `spec_iteration`, `spec_acceptance`,
  `spec_task_generation`.
- **T010** (design_review workflow): consumes `design_review_round`,
  contribution phase + read_set, workflow run as round_id holder.
- **T011** (documentation): consumes T001 mapping, chunks anchored to
  immutable versions, `doc_publish` workflow.
