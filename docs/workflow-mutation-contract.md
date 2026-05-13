# Workflow Mutation Contract (v1)

**Status:** Draft contract (Phase 1, gateway-features_spec T003)
**Audience:** gateway backend implementers (T004 ops/rollback, T005 schema,
T006 repository, T007 generic HTTP API, T009 spec workflow, T010 design-review
workflow, T011 documentation workflow), SRE, CLI/skill authors, UI
**Depends on:** T002 — `docs/artifact-substrate-v1.md`
**Consumes (decisions already settled, not re-litigated here):**
- T001: API docs are documentation artifacts on the substrate
  (`docs/artifacts-api-docs-integration.md`).
- T002: substrate entities, immutability invariants, named authorization
  scopes, and child-address rules (`docs/artifact-substrate-v1.md`).

## Purpose and non-goals

This document fixes the single v1 write contract for every mutating workflow
on the artifact substrate. Per the gateway-features Pass-2 convergence,
permissions, idempotency, and immutable typed links are one contract — not
three. Downstream tasks implement endpoints, schema constraints, and
specialized workflows on top of these rules.

**In scope.** Authorization scope binding per resource/action; the v1
trusted-boundary statement; idempotency key shape, scoping, and uniqueness;
actor and provenance requirements on every mutation result; retry and
partial-failure semantics for version creation, contribution writes, link
emission, chunk regeneration, comment lifecycle, and spec task generation;
the audit-path link-type registry; route-validation anchor notes for
`crates/gateway/src/routes.rs`.

**Out of scope.** SQL DDL (T005). Repository function signatures (T006).
Endpoint paths, request bodies, and status codes (T007 — though this
document fixes what those endpoints must validate). Per-workflow specifics
beyond what every workflow shares (T009/T010/T011). Full project membership
or multi-tenant authorization — see §"Trusted boundary (v1)".

## 1. Trusted boundary (v1)

The first vertical slice runs under the existing gateway authentication
model: a single shared API key carried as `Authorization: Bearer <token>`
and an `X-Agent-Id` header naming the calling agent. Within that boundary,
**every authenticated caller is treated as if it holds every scope listed in
§3 for every project on this gateway instance.** There is no per-project
membership check, no per-object ACL, and no scope-token system in v1.

This is a deliberate v1 simplification, **not** a stable design. It MUST be
surfaced explicitly so it is not mistaken for general workspace readiness:

- **API.** Every artifact, version, contribution, comment, link, chunk, and
  workflow-run response object MUST include
  `authorization: { boundary: "trusted-single-tenant", required_scopes: [...] }`
  on every mutating endpoint result. T007 owns surface; T003 fixes the
  field name and shape.
- **CLI.** `agent-tools artifacts` (and the compatibility facade
  `agent-tools docs`) MUST print a single-line banner the first time a
  mutating verb runs in a session: `gateway: trusted single-tenant mode —
  all callers share full artifact authority`. The banner is suppressed by
  `--quiet` but the underlying field is always present in JSON output.
- **UI.** Any view that exposes a mutating action (create version, accept,
  resolve comment, generate tasks, publish doc) MUST render a persistent
  badge that links to this contract section. The badge text is "trusted
  single-tenant" verbatim.

**Deferred work (owned by T004 / a follow-up auth task, not T003).**

- Per-project membership records and an enforcement layer that maps the
  current actor onto a per-project scope set.
- Scope tokens that narrow a caller below the full scope set (e.g. a
  read-only retrieval client).
- Cross-project artifact reads when artifacts deliberately span projects.

The substrate already carries the fields needed to add enforcement without a
contract revision: every mutation result names its `required_scopes`, every
actor record carries a stable `actor_id`, and project scope is on the
artifact. A later task swaps the trusted-mode check for a real scope check
without changing wire shapes.

## 2. Actor and provenance requirements

Every mutation request and every mutation result MUST carry the fields below.
Missing fields are validation errors (HTTP 400), not silent defaults.

### 2.1 Request fields

| Field | Source | Notes |
|---|---|---|
| `Authorization: Bearer <token>` | header | Existing gateway auth. Trusted-boundary gate. |
| `X-Agent-Id` | header | Host-persistent agent id (e.g. `willsm4max-05bc2f77`). Required on every mutating call. Anonymous `_default` is **not** allowed for mutations; reads MAY fall back to `_default`. |
| `X-Actor-Type` | header, optional | `user` \| `agent` \| `system`. Defaults to `agent` when absent. |
| `X-Agent-System` | header, optional | `claude` \| `codex` \| `gemini` \| `other`. Required when `X-Actor-Type=agent`. |
| `X-Host` | header, optional | Machine identity (e.g. `willsm4max.nitecon.net`). Recorded when present. |
| `Idempotency-Key` | header | See §4. Required on every mutating workflow endpoint. |
| `X-Workflow-Run-Id` | header, optional | Provided when the call belongs to an existing run. Absent when the call starts a new run. See §4.2. |
| body `actor_display_name` | json, optional | Free-form UI label; not used for identity. |
| body `read_set` | json, conditional | Required on pass-2 / synthesis contributions and on any mutation whose audit story depends on prior contributions or versions (see §6.2). |

Headers are the canonical carriers so they survive proxy/log redaction
policies and so existing axum extractors (`HeaderMap`) work without body
schema changes.

### 2.2 Result fields (every mutation response)

Every mutation result MUST embed a `provenance` envelope:

```
"provenance": {
  "actor": {
    "actor_id": "<resolved>",
    "actor_type": "agent",
    "agent_system": "claude",
    "agent_id": "willsm4max-05bc2f77",
    "host": "willsm4max.nitecon.net",
    "display_name": "Will"
  },
  "workflow_run_id": "<id or null>",
  "idempotency_key": "<echoed from request>",
  "request_id": "<server-assigned>",
  "created_at": "<ISO-8601>",
  "authorization": {
    "boundary": "trusted-single-tenant",
    "required_scopes": ["artifact_version.create", "link.write", ...]
  },
  "generated_resources": { /* see §5 */ }
}
```

The envelope is non-negotiable: it is the single hand-off point that the
research-analyst, audit views, retry callers, and downstream tasks all read.
T007 surface conventions (snake_case JSON, etc.) override field naming but
the contents MUST be present.

### 2.3 Resolving the actor

The repository layer (T006) MUST upsert an `actor` row keyed on
`(actor_type, agent_system, agent_id, host)` so the same agent on the same
host always resolves to the same `actor_id`. The `display_name` is updated
on each call; identity fields are write-once for a given `actor_id`.

For `actor_type=user`, the `agent_id` field carries the human user id
provided by the auth layer once user identity exists. In v1 trusted mode,
the gateway has no user identity; user-typed actors MUST therefore supply a
stable `X-Agent-Id` (the operator's chosen identifier). Best-effort
identities (agent system known, no stable `agent_id`) are accepted but
downgraded in audit views per §"Actor model details" in T002.

## 3. Authorization scope matrix

The v1 scope set is named in T002 §"Authorization scopes". T003 binds each
resource/action pair to its required scope(s). In trusted mode every
authenticated caller holds every scope; the matrix is the durable contract
for the future enforcement task. T007 endpoints MUST declare their required
scope set; clients SHOULD treat the declared set as the future enforcement
point.

| Resource | Action | Required scope(s) | Notes |
|---|---|---|---|
| artifact | list / get / search | `artifact.read` | Discovery surface. |
| artifact | create | `artifact.write` | Creates the mutable container only. |
| artifact | update title/labels | `artifact.write` | Lifecycle pointer changes use the version-state actions below. |
| artifact | set `lifecycle_state` to `archived` | `artifact.administer` | Soft archive. |
| artifact_version | list / get | `artifact.read` | |
| artifact_version | create (new draft / synthesis output) | `artifact_version.create` | Body immutability rule from T002 invariant 1. |
| artifact_version | set `version_state` → `accepted` | `artifact_version.accept` + `artifact.write` | Also moves `artifact.accepted_version_id` atomically. |
| artifact_version | set `version_state` → `superseded` / `rejected` | `artifact_version.create` | Recorded as state-transition contribution. |
| contribution | create (`review`, `synthesis`, `decision`, `note`, `completion`, `state_transition`) | `contribution.write` | `state_transition` additionally requires the scope of the transition (e.g. accept). |
| comment | create | `comment.write` | |
| comment | set `state` → `resolved` | `comment.resolve` | |
| comment | re-open (`resolved` → `open`) | `comment.write` | Re-open also writes a `note` comment per T002. |
| link | create (audit-path types) | `link.write` + the scope of the action that emitted it (e.g. `artifact_version.accept` for `supersedes_version`) | Audit-path types are enumerated in §7. |
| link | create (discovery-only types) | `link.write` | |
| link | supersede (set `supersedes_link_id`) | same as create | Hard deletion is reserved for discovery links and requires `artifact.administer`. |
| chunk | write / regenerate | `chunk.write` + `artifact_version.create` (when emitted by a `doc_publish` run) | Chunks are produced inside a workflow_run. |
| chunk | read | `chunk.read` (alias of `artifact.read` in v1) | |
| workflow_run | start | `workflow_run.start` | The starter is the `coordinator_actor_id`. |
| workflow_run | complete (`succeeded` / `failed` / `cancelled`) | `workflow_run.complete` | Only the coordinator MAY complete; in v1 trusted mode this is enforced by ownership check only. |
| task | generate from spec | `task.generate_from_spec` + `workflow_run.start` | The run is `workflow_kind=spec_task_generation`. |
| task | link from contribution / comment | `link.write` | Tasks themselves remain owned by the task surface; the substrate only writes link rows. |
| project | administer (membership, quota) | `project.administer` | Reserved; no v1 endpoints consume it. |

**Aggregate rule.** When an action requires multiple scopes, the caller
MUST hold all of them. T007 endpoints SHOULD reject with HTTP 403 (after
the enforcement task lands) and SHOULD include the missing scopes in the
error body.

## 4. Idempotency

### 4.1 Why one shape, applied everywhere

Per gateway-features Pass-2 convergence and the substrate's workflow-run
contract, every mutating workflow endpoint MUST be retry-safe. The pattern
is: client supplies an idempotency key, server stores
`(workflow_run_id, idempotency_key) → produced resource ids`, retries with
the same pair return the original result. This makes the substrate
operationally indistinguishable from an append-only event log without
forcing event-log storage in v1 (memory id `a91b8332` is closed in favor of
"conventional tables plus unique request keys" for v1).

### 4.2 Key shape and scoping

The `Idempotency-Key` header is an opaque string, ≤ 255 UTF-8 bytes, chosen
by the client. The substrate stores it verbatim. To prevent accidental
cross-project / cross-artifact collisions, **the uniqueness scope is**
**always the `(workflow_run_id, idempotency_key)` pair** — never the key
alone. The `workflow_run_id` carries the artifact and project scope
implicitly (workflow_run.artifact_id → artifact.project_ident), so two
different artifacts cannot collide on the same key.

Calls that do not yet have a `workflow_run_id`:

- **Workflow-starting calls** (e.g. "start a new design_review_round").
  The server allocates the `workflow_run_id`. The client's
  `Idempotency-Key` is paired with a deterministic
  `workflow_run_id = NIL` value (the UUID nil sentinel) **scoped to the
  caller's `actor_id` and the target `artifact_id`** for the uniqueness
  check, then rewritten to the real `workflow_run_id` once allocated. In
  effect, retries of "start this run" on the same artifact by the same
  actor with the same key return the same `workflow_run_id`.
- **One-shot mutations outside any run** (rare in v1; comments outside a
  workflow are the main case). The substrate synthesizes a degenerate
  `workflow_run_id` per mutation type — `comment.write:<artifact_id>` —
  so `(synthetic_run, key)` still scopes uniqueness to the artifact.
  These calls do not create a workflow_run row; the synthetic id is a
  uniqueness namespace only.

**Recommended client shape.** Clients SHOULD construct keys as a UUIDv7 or
as a deterministic hash of the logical operation
(`hash(workflow_run_id || sub_step || stable_inputs)`) when retrying the
exact same logical operation matters. T009 spec_task_generation uses the
deterministic shape: `idempotency_key = hash(manifest_item_id || spec_version_id)`
so retrying the same generation pass against the same manifest produces
the same task ids. The substrate does not parse the key; it only stores
and matches.

### 4.3 Uniqueness rules (per resource)

T005 enforces these as unique indexes; T006 surfaces "already exists →
return existing" as a normal repository result rather than an error.

| Resource | Unique index | Semantics on conflict |
|---|---|---|
| artifact_version | `(artifact_id, created_via_workflow_run_id, idempotency_key)` when key is set | Return the existing version; do not create a new one. |
| contribution | `(workflow_run_id, idempotency_key)` when run is set; `(artifact_id, actor_id, idempotency_key)` for run-less contributions | Return existing. |
| comment | `(target_kind, target_id, actor_id, idempotency_key)` | Return existing. Re-open is a new comment of kind `note`, not an idempotent duplicate. |
| link | `(workflow_run_id, idempotency_key)` (REQUIRED on workflow-emitted links per T002) | Return existing link_id. |
| chunk | `(artifact_version_id, child_address)` (natural key — idempotency falls out of the address) | Return existing chunk_id. Re-chunk of the same version with the same address produces the same id. |
| task (link only) | `(workflow_run_id, idempotency_key)` on the `task_generated_from_spec` link row | Return existing link; the gateway task itself is created by the task surface. |
| workflow_run | `(coordinator_actor_id, artifact_id, workflow_kind, idempotency_key)` when starting | Return existing run id. |

### 4.4 Idempotency window

There is **no expiry** on stored idempotency mappings in v1. The pair lives
as long as the underlying resource lives. Soft-superseded resources keep
their mapping so retries continue to return the same id; hard-deleted
discovery rows (rare, `artifact.administer` only) drop the mapping with
the row. T004 owns whether a future expiry policy is needed for storage
hygiene; the substrate does not require one.

## 5. Generated resources, retry, and partial-failure

Every mutation result names what it produced. The `provenance.generated_resources`
envelope (§2.2) MUST include the keys present in the table below for the
calling action. Retries return the same `generated_resources` payload.

| Action | `generated_resources` keys |
|---|---|
| Create artifact | `artifact_id` |
| Create version (any) | `artifact_version_id`, optional `workflow_run_id` |
| Accept version | `artifact_version_id`, `artifact.accepted_version_id`, contribution_id of the `state_transition`, `workflow_run_id` |
| Write contribution | `contribution_id`, optional `workflow_run_id` |
| Write comment | `comment_id` |
| Resolve comment | `comment_id`, `resolution_contribution_id` (when resolution was a workflow decision), `workflow_run_id` |
| Create link | `link_id` |
| Write chunk(s) | `chunk_ids`, `superseded_chunk_ids`, `workflow_run_id` |
| Start workflow_run | `workflow_run_id` |
| Complete workflow_run | `workflow_run_id`, `state`, full `generated_*` arrays from §"workflow_run" entity |
| Generate tasks from spec | `link_ids` (the `task_generated_from_spec` rows), `task_ids` (echoed from task surface), `workflow_run_id`, `skipped_manifest_item_ids` (already-generated items) |

### 5.1 Retry semantics by resource

The general rule: **a retry returns the original generated_resources** when
`(workflow_run_id, idempotency_key)` matches, including the original
`request_id` in audit views as a `replayed_from_request_id` field. The
substrate distinguishes "I produced this in a prior call" (HTTP 200 with the
existing payload) from "I produced this now" (HTTP 201) via a
`provenance.replay = true|false` flag. T007 may choose to always return 200
to simplify clients; the flag is the durable signal.

| Resource | First-call success | Retry with same key | Retry after partial failure |
|---|---|---|---|
| **artifact_version** | 201, new id | 200, same id, `replay=true` | If the prior call failed before the row landed: new id is created and indexed; the failed attempt is recorded against the workflow_run's `failure_reason`. If the prior call landed the row but failed to update `current_version_id`: the retry completes the pointer update and returns the same `artifact_version_id`. |
| **contribution** | 201, new id | 200, same id | If the contribution row landed but linked entities (e.g. `read_set` references) failed validation: the contribution row is rolled back (single-row transaction). Retry creates the row. If the workflow_run failed after multiple contributions: only the missing contributions are retried; the rest return as replays. |
| **link** | 201, new id | 200, same id | Audit-path links are emitted in the same transaction as the resource they describe (version, accept, chunk write). If the resource transaction rolls back, the link does too. Retry recreates both. Discovery links are best-effort; failure does not roll back the parent action. |
| **chunk** | 201, ids list, possibly `superseded_chunk_ids` | 200, identical lists | Chunk writes are batched per version. If batch fails partway, the substrate marks the workflow_run `failed` with `failure_reason` and the partial `generated_chunk_ids`; retry resumes from the first missing `child_address`. The `(artifact_version_id, child_address)` natural key makes resume safe without a separate cursor. |
| **task (link row)** | 201 per manifest item, `skipped_manifest_item_ids=[]` | 200, same `link_ids`, full `skipped_manifest_item_ids` list | If task surface created some tasks but the run failed before all links landed: retry calls task surface with the same logical `(manifest_item_id, spec_version_id)`, the surface returns the existing `task_id` (its own idempotency), and the substrate writes the missing link rows. `skipped_manifest_item_ids` lists items whose link rows were already complete. |
| **comment.resolve** | 200, comment moves to `resolved` | 200, same state | If the resolution decision contribution landed but the comment state update failed: retry completes the state update. Re-opens before retry are blocked by the run's idempotency check (same key on a closed run returns the original). |
| **workflow_run** | 201 with `state=started` | 200, same `workflow_run_id`, current state | If start landed but no contributions ever wrote: completion call may still succeed (run with empty generated arrays). For declared resumable fan-out workflow kinds (`spec_task_generation`, `doc_publish`, and any future kind that explicitly opts in), `failed` is terminal for the failed attempt but recoverable for the logical run: a retry with the same run/key may append missing generated resources and transition the run to `succeeded`. `cancelled` is non-recoverable and requires a new run. Non-resumable workflow kinds require a successor run that references the failed run. |

### 5.2 Worked retry scenarios

The validation plan requires at least one scenario each for version
creation, contribution creation, and spec task generation.

**Version creation retry.**
1. Client calls `POST /artifacts/{id}/versions` with
   `Idempotency-Key: v3-attempt-1` and body bytes B.
2. Server begins txn: inserts `artifact_version` row, then attempts to
   update `artifact.current_version_id`. Pointer update fails (e.g.
   constraint conflict because a concurrent acceptance bumped the row).
3. Txn aborts; nothing is persisted. Workflow_run row (if any) is updated
   with `state=failed`, `failure_reason="current_version_id update lost
   race"`, no `generated_version_ids`.
4. Client retries with the same `Idempotency-Key`.
5. Server begins new txn, inserts the version row (no prior row exists),
   updates the pointer successfully, commits, returns 201 with
   `artifact_version_id=AV1`, `provenance.replay=false`.
6. Third retry with the same key: returns 200 with `AV1`,
   `provenance.replay=true`.

**Contribution creation retry inside a multi-pass run.**
1. Client (analyst) calls `POST /workflow_runs/{run}/contributions` with
   `Idempotency-Key: synth-1` for the synthesis contribution. The body
   includes `read_set` referencing five pass-2 contribution ids.
2. Server validates `read_set`: one referenced contribution_id is missing
   (peer never wrote it). Response: 400 with
   `error="read_set references missing contribution X"`. Nothing
   persisted.
3. The peer writes the missing contribution.
4. Analyst retries with the same `Idempotency-Key: synth-1`. Server now
   validates successfully, inserts the synthesis contribution, returns
   201 with `contribution_id=C99`.
5. A duplicate retry of step 4 returns 200 with `C99`, `replay=true`.

**Spec task generation retry.**
1. Coordinator calls `POST /workflow_runs` to start a run with
   `workflow_kind=spec_task_generation`, `artifact_id=A`, `source_artifact_version_id=AV5`,
   `Idempotency-Key: gen-2026-05-12-1`. Server returns `workflow_run_id=R1`.
2. Coordinator calls `POST /workflow_runs/R1/generate_tasks` (T007 path is
   illustrative). Server iterates the accepted manifest, calling task
   surface per manifest item with deterministic key
   `hash(manifest_item_id || AV5)`. Two items succeed (tasks T_a, T_b
   created, links L_a, L_b inserted). On the third item the task surface
   returns 5xx.
3. Server commits the partial state, marks R1 `state=failed`,
   `failure_reason="task surface unavailable at item M3"`,
   `generated_task_ids=[T_a, T_b]`, `generated_link_ids=[L_a, L_b]`.
   Response: 503 with the partial `generated_resources` payload.
4. Coordinator retries the same `POST /workflow_runs/R1/generate_tasks`
   with the same call-level `Idempotency-Key`. Server iterates the
   manifest again. For items M1/M2 the task surface returns the existing
   T_a/T_b (its own idempotency); the substrate sees the existing link
   rows under the deterministic per-item keys and returns them in
   `replay`. For item M3 the task surface now succeeds; substrate
   inserts L_c. Run state moves to `succeeded`. Response: 200 with
   `generated_task_ids=[T_a, T_b, T_c]`,
   `skipped_manifest_item_ids=[M1, M2]`, `replay=false` (the run as a
   whole produced new work).

In all three scenarios the `(workflow_run_id, idempotency_key)` pair —
combined with deterministic sub-step keys for fan-out work — keeps the
operation crash-safe and replay-safe without an event-log table.

**Resumable-run rule.** Fan-out workflows that may produce partial durable
outputs MUST declare whether they are resumable. For resumable kinds, a
`failed` run records the failed attempt and partial generated resources; a
retry with the same `(workflow_run_id, idempotency_key)` resumes from missing
sub-steps and may transition the same run to `succeeded` once complete. This
is the v1 rule for `spec_task_generation` and `doc_publish`. `cancelled`
remains terminal because it represents an operator decision, not a transient
failure. Non-resumable workflow kinds retry by starting a successor
workflow_run that links to the failed run in its read set or metadata.

## 6. Per-mutation validation requirements

These are the validations T007 endpoints MUST perform on every workflow
mutation. T005 enforces structural constraints at the schema layer; T006
exposes the rejections as typed repository errors; T007 maps them to HTTP.

### 6.1 Common preflight (every mutating endpoint)

1. **Authenticated.** Bearer token matches the gateway api key
   (existing `bearer_auth` middleware in `crates/gateway/src/main.rs`
   line ≈ 57). No change in v1.
2. **Actor present.** `X-Agent-Id` header set and non-empty; reject 400
   if absent. (Extends the existing `extract_agent_id` helper in
   `crates/gateway/src/routes.rs:43-49` — see §8 for the route-validation
   anchor.)
3. **Idempotency-Key present.** Reject 400 if absent on any mutating
   path.
4. **Project scope resolvable.** The targeted artifact (or workflow_run)
   resolves to a known `project_ident`. Reject 404 otherwise. In v1
   trusted mode there is no further membership check.
5. **Authorization scopes declared.** The handler MUST attach the
   required scope set from §3 to the response envelope before returning.
   The actual scope check is a no-op in v1 (trusted mode); the
   declaration is the durable contract.

### 6.2 Resource-specific validations

| Mutation | Additional validations |
|---|---|
| Create artifact_version | `parent_version_id` (when set) belongs to the same `artifact_id`; body size ≤ project max (T004/T011 envelope); `body_format` matches what the artifact `kind` accepts (T002 §"Body model"); `structured_payload` present and valid when `kind=spec` and the version is intended for acceptance (T009 schema). |
| Accept version | Caller is allowed by §3; the version belongs to the artifact; the version's current `version_state` ∈ {`draft`, `under_review`}; atomic state transition + `artifact.accepted_version_id` update + `state_transition` contribution write in the same txn. |
| Write contribution | `target_kind`/`target_id` resolves; `role` is in the allowed set for the workflow_kind (when run is set); `read_set` references resolve to existing contribution/version ids; `read_set` REQUIRED on contributions with `phase ∈ {pass_2, synthesis}`. |
| Write comment | `target_kind` ∈ v1 set (`artifact`, `artifact_version`, `contribution`); `child_address` rejected unless `target_kind=artifact_version`; `parent_comment_id` (when set) targets a comment with the same `artifact_id`. |
| Resolve comment | Comment exists and is `open`; `resolved_by_actor_id` set to the calling actor; when called inside a workflow, `resolved_by_workflow_run_id` set. |
| Re-open comment | Comment exists and is `resolved`; creates a paired `note` comment in the same txn per T002 §"Comment lifecycle". |
| Create link | `source_kind`/`target_kind` ∈ the substrate kind set; for audit-path link types (§7) `source_version_id` and/or `target_version_id` set per the type's row in the registry; `idempotency_key` REQUIRED when `created_via_workflow_run_id` is set; `(workflow_run_id, idempotency_key)` not already used for a different shape. |
| Write chunk(s) | Each chunk's `artifact_version_id` belongs to the run's artifact; `child_address` matches the version's address scheme (T002 §"Child references"); replacing chunks soft-supersedes the prior rows (set `superseded_by_chunk_id`). |
| Start workflow_run | `workflow_kind` ∈ v1 set (T002 §"Workflow run / activity"); `source_artifact_version_id` REQUIRED when the kind is audit-path (every v1 kind except open-ended notes); `coordinator_actor_id` = calling actor. |
| Complete workflow_run | Caller is the `coordinator_actor_id`; new `state` ∈ {`succeeded`, `failed`, `cancelled`}; on `failed`, `failure_reason` set. |
| Generate tasks from spec | The run is `workflow_kind=spec_task_generation`; `source_artifact_version_id` exists and has `version_state=accepted`; per-item deterministic key shape is `hash(manifest_item_id || source_artifact_version_id)` (§4.2). |

## 7. Audit-path link-type registry

T003 owns the audit-path declaration. The registry below extends the v1
list in T002 §"v1 link types" and fixes which `link_type` values
participate in audit/handoff paths (T002 invariant 2). Audit-path links
MUST carry `source_version_id` and/or `target_version_id` when the
endpoint references an artifact. T005 enforces via constraint; T007
rejects 400 on violations.

| `link_type` | Audit-path? | Emitter | Required version refs |
|---|---|---|---|
| `spec_implements_design` | yes | T009 | both source & target version ids |
| `task_generated_from_spec` | yes | T009 | target version id (the spec version) |
| `chunk_of_version` | yes | T011 | target version id |
| `decision_resolves_comment` | yes | T010 | source contribution belongs to a version-scoped run; comment may be artifact- or version-scoped |
| `supersedes_artifact` | yes | T003 | n/a (artifact-to-artifact) but both artifacts MUST belong to the same project |
| `supersedes_version` | yes | T003 | both version ids |
| `pattern_applied_to_artifact_version` | yes | T009/T010 | target version id |
| `doc_referenced_by_spec` | no (discovery) | T009 | optional |
| `comment_references_task` | no (discovery) | T010 | optional |

New link types added by later tasks MUST declare their audit-path status
in this registry before T005/T006 will accept them.

## 8. Route validation notes (anchors for `crates/gateway/src/routes.rs`)

T007 implements the endpoint surface. These notes name where the checks
above belong in `crates/gateway/src/routes.rs` so T007 lands consistently
with the existing patterns. No code change is required as part of T003.

- **Auth.** `bearer_auth` middleware in `crates/gateway/src/main.rs:57-69`
  is the single auth gate in v1. Mutation handlers do not re-check the
  bearer token; they trust the middleware. The per-mutation
  authorization scope declaration (§3) is added to the response body, not
  enforced.
- **Actor extraction.** Extend the existing helper
  `extract_agent_id` (`crates/gateway/src/routes.rs:43-49`) into an
  `ActorEnvelope` extractor that reads `X-Agent-Id`, `X-Actor-Type`,
  `X-Agent-System`, `X-Host`, and `X-Workflow-Run-Id`. The extractor
  rejects 400 when `X-Agent-Id` is missing on a mutating path. Use the
  existing `AppError` shape (`crates/gateway/src/routes.rs:22-36`) for
  the rejection.
- **Idempotency.** Add a new extractor `IdempotencyKey(String)` that
  pulls the `Idempotency-Key` header, rejects 400 when absent on a
  mutating path, and is composed into every mutation handler signature.
- **Request validation.** Per-mutation validations from §6 belong
  inside the per-handler function bodies alongside existing patterns
  like `validate_api_doc_input`
  (`crates/gateway/src/routes.rs:570-595`). Validation errors return
  `AppError(StatusCode::BAD_REQUEST, "...")` so the existing
  `IntoResponse` mapping holds.
- **Repository call.** Each handler invokes a single T006 repository
  function inside `spawn_blocking` (mirroring `get_theme` /
  `set_theme` at `crates/gateway/src/routes.rs:115-144`). The
  repository function returns either `Created(resource, generated_resources)`
  or `Replayed(resource, generated_resources)`; the handler maps to
  HTTP 201 / 200 accordingly and sets `provenance.replay`.
- **Response envelope.** A single helper builds the `provenance`
  envelope (§2.2) from the `ActorEnvelope`, `IdempotencyKey`, run state,
  and required-scope set. Every mutation handler returns this envelope;
  the helper lives next to `build_outbound`
  (`crates/gateway/src/routes.rs:78-101`) in style.
- **Trusted-mode banner.** The CLI banner from §1 is the CLI side. On
  the server side, the `authorization.boundary` field is the durable
  signal; no banner is rendered server-side.

T007 may freely choose endpoint paths and request bodies as long as the
extractors, validations, and response envelope above are consistent.

## 9. Acceptance criteria — self-check

- [x] Authorization scope for artifact reads, version writes, comments,
      link writes, accepting versions, and task generation. §3.
- [x] Initial trusted single-user/project boundary, with API/CLI/UI
      surfacing requirements. §1.
- [x] Idempotency key shape and uniqueness rules for mutating workflow
      endpoints. §4.
- [x] Retry and partial-failure behavior for generated versions,
      contributions, links, chunks, and task records. §5.
- [x] Actor/provenance fields required on every workflow mutation
      result. §2.2 (envelope) and §6.1 step 2/5.

## 10. Validation plan — results

- **Scope matrix check.** §3 provides the resource/action/scope table.
- **Retry scenario check.** §5.2 walks one full retry path each for
  version creation, contribution creation, and spec task generation.
- **Route readiness.** §8 cites the validation requirements against
  existing anchors in `crates/gateway/src/routes.rs` (lines 22-36,
  43-49, 78-101, 115-144, 570-595) and
  `crates/gateway/src/main.rs:57-69`. T007 can implement without
  inventing endpoint-specific rules.

## 11. Open questions surfaced for downstream tasks

- **(T004 / future auth task)** Timing for the switch from trusted mode
  to per-project enforcement. The substrate is ready; the question is
  product readiness and how to mint scope tokens.
- **(T009)** Whether `spec_task_generation` should also surface
  `manifest_item_id → task_id` as a structured field in addition to
  link rows. The substrate supports both; T009 picks the canonical
  representation.
- **(T011)** Whether `doc_publish` retries should re-embed chunks on
  unchanged versions or skip embedding for cost reasons. The substrate
  treats `(artifact_version_id, child_address)` as the natural key
  regardless; this is a runtime cost question, not a contract question.
- **(SRE)** Whether to expose a per-workflow-run `replay_count` metric
  for retry-storm detection. Not required by the contract; surfaced for
  operability.

## 12. Hand-off

- **T004 (operations & rollback):** consumes §5 partial-failure
  semantics for the rollback playbook; consumes §4.4 (no expiry) for
  storage growth modeling.
- **T005 (schema):** turns §4.3 uniqueness rules into unique indexes;
  turns §7 audit-path registry into check constraints; turns §2.2 into
  required NOT NULL columns where applicable.
- **T006 (repository):** exposes `Created` / `Replayed` return shapes;
  upserts actors per §2.3; runs the per-resource validations from §6.2.
- **T007 (generic HTTP API):** implements extractors and handlers per
  §8; surfaces the provenance envelope per §2.2; declares scopes per
  §3.
- **T009 / T010 / T011 (specialized workflows):** consume the single
  mutation contract; declare their specific `workflow_kind` keys,
  `link_type` registry entries, and per-item idempotency key shapes
  inside this contract's rules.
