# Artifact Operations Envelope and Rollout Plan (v1)

**Status:** Draft (Phase 1, gateway-features_spec T004)
**Audience:** SRE, gateway backend implementers (T005 schema, T006 repository, T007 generic HTTP API, T008 regression tests, T011 documentation workflow), CLI/skill authors, T014 final-rollout validators
**Depends on:** T002 — `docs/artifact-substrate-v1.md`, T003 — `docs/workflow-mutation-contract.md`
**Consumes (decisions already settled, not re-litigated here):**
- T001: API docs are documentation artifacts (`docs/artifacts-api-docs-integration.md`).
- T002: substrate entities, immutability invariants, child-address rules.
- T003: trusted-boundary scope model, idempotency shape, partial-failure semantics.

## Purpose and non-goals

This document fixes the production operations envelope and the migration /
rollback plan for the artifact substrate's first vertical slice. Downstream
tasks turn these limits and procedures into route validation (T007), regression
fixtures (T008), documentation workflow defaults (T011), and the final
rollout-gate checklist (T014).

**In scope.** Concrete size limits, quotas, retention/archive policy,
backup/restore expectations, metrics surface, stale/failed chunk handling,
migration phases for task specs / API docs / scratch-file workflows / artifact
links, dual-read compatibility windows, deprecation criteria for scratch
files, rollback path back to existing task/docs behavior.

**Out of scope.** Implementation of the metrics system itself (SRE owns
exporter wiring); running the production migration (T014 gates it); per-
workflow specialization (T009/T010/T011); the future per-project
authorization enforcement task (deferred per T003 §1).

## 1. Size limits and configuration keys

All limits are enforced per-write at the route layer (T007) and verified by
T008 size-limit fixtures. Oversize writes are **rejected with HTTP 413**, never
truncated. Limits are configured at process start via environment variables
with the defaults named below; the same names are used by T008 fixtures so a
test environment can shrink them to provoke rejection without code changes.

| Resource | Default | Config key | Rejected with |
|---|---|---|---|
| `artifact_version.body` (markdown / agent-context+json) | 1 MiB | `ARTIFACT_VERSION_BODY_MAX_BYTES` | `HTTP 413 artifact_version_body_too_large` |
| `artifact_version.body` (openapi / swagger source) | 4 MiB | `ARTIFACT_VERSION_SOURCE_BODY_MAX_BYTES` | `HTTP 413 artifact_version_source_body_too_large` |
| `artifact_version.structured_payload` | 512 KiB serialized | `ARTIFACT_VERSION_STRUCTURED_PAYLOAD_MAX_BYTES` | `HTTP 413 structured_payload_too_large` |
| `contribution.body` | 256 KiB | `CONTRIBUTION_BODY_MAX_BYTES` | `HTTP 413 contribution_body_too_large` |
| `comment.body` | 32 KiB | `COMMENT_BODY_MAX_BYTES` | `HTTP 413 comment_body_too_large` |
| `chunk.text` | 8 KiB | `CHUNK_TEXT_MAX_BYTES` | `HTTP 413 chunk_text_too_large` |
| `Idempotency-Key` header | 255 UTF-8 bytes | n/a (fixed by T003 §4.2) | `HTTP 400 idempotency_key_too_long` |
| `artifact.labels` count | 32 labels, each ≤ 64 bytes | `ARTIFACT_LABELS_MAX_COUNT`, `ARTIFACT_LABEL_MAX_BYTES` | `HTTP 400 labels_too_many` / `label_too_long` |
| `read_set` referenced ids | 256 ids | `READ_SET_MAX_REFS` | `HTTP 400 read_set_too_large` |

**Named fixture values for T008.** The same env keys SHOULD be used by T008
regression tests with the shrunk values below so a single fixture document
near the documented boundary can exercise both accept and reject paths
deterministically. T008 owns the exact fixture bodies; this document fixes
the env-key surface they read from.

```env
ARTIFACT_VERSION_BODY_MAX_BYTES=4096
ARTIFACT_VERSION_SOURCE_BODY_MAX_BYTES=8192
ARTIFACT_VERSION_STRUCTURED_PAYLOAD_MAX_BYTES=2048
CONTRIBUTION_BODY_MAX_BYTES=2048
COMMENT_BODY_MAX_BYTES=512
CHUNK_TEXT_MAX_BYTES=512
ARTIFACT_LABELS_MAX_COUNT=4
ARTIFACT_LABEL_MAX_BYTES=32
READ_SET_MAX_REFS=8
PROJECT_ARTIFACT_SOFT=2
PROJECT_ARTIFACT_HARD=3
PROJECT_VERSION_SOFT=3
PROJECT_VERSION_HARD=4
PROJECT_CONTRIBUTION_SOFT=3
PROJECT_CONTRIBUTION_HARD=4
PROJECT_OPEN_COMMENT_SOFT=2
PROJECT_OPEN_COMMENT_HARD=3
PROJECT_LINK_SOFT=3
PROJECT_LINK_HARD=4
PROJECT_CHUNK_SOFT=3
PROJECT_CHUNK_HARD=4
PROJECT_RUNNING_WORKFLOW_SOFT=1
PROJECT_RUNNING_WORKFLOW_HARD=2
PROJECT_WRITE_RPM_SOFT=3
PROJECT_WRITE_RPM_HARD=4
```

## 2. Quotas (per project)

Quotas are evaluated on every mutating endpoint after the size check. Two
thresholds per counter: a **soft warning** (response carries
`provenance.warnings[]` containing `quota_<name>_soft`) and a **hard limit**
(`HTTP 429 quota_<name>_exceeded`, no row written). T003 §2.2 already
mandates the `provenance` envelope; warnings ride inside it.

| Counter | Soft warning | Hard limit | Config keys |
|---|---|---|---|
| Active artifacts (`lifecycle_state != "archived"`) | 5,000 | 10,000 | `PROJECT_ARTIFACT_SOFT`, `PROJECT_ARTIFACT_HARD` |
| Artifact versions across all artifacts | 50,000 | 100,000 | `PROJECT_VERSION_SOFT`, `PROJECT_VERSION_HARD` |
| Contributions across all artifacts | 250,000 | 500,000 | `PROJECT_CONTRIBUTION_SOFT`, `PROJECT_CONTRIBUTION_HARD` |
| Open comments | 5,000 | 10,000 | `PROJECT_OPEN_COMMENT_SOFT`, `PROJECT_OPEN_COMMENT_HARD` |
| Audit-path links | 200,000 | 400,000 | `PROJECT_LINK_SOFT`, `PROJECT_LINK_HARD` |
| Chunks (live, non-superseded) | 100,000 | 200,000 | `PROJECT_CHUNK_SOFT`, `PROJECT_CHUNK_HARD` |
| Workflow runs in `started` state (concurrent) | 50 | 100 | `PROJECT_RUNNING_WORKFLOW_SOFT`, `PROJECT_RUNNING_WORKFLOW_HARD` |
| Writes per minute (per project) | 600 | 1,200 | `PROJECT_WRITE_RPM_SOFT`, `PROJECT_WRITE_RPM_HARD` |

**Project-administrator override.** A caller holding `project.administer`
(reserved per T003 §3) MAY bypass soft warnings; hard limits remain
enforced. v1 trusted mode treats all callers as scope-holders, but the
override is still recorded in `provenance.warnings[]` so growth is visible.

**No global gateway quota in v1.** SRE monitors absolute disk and chunk row
counts via the metrics in §5; capacity planning lives in the dashboard, not
in a hard cross-project ceiling.

## 3. Retention and archive policy

Artifact data has four retention tiers. The tier is derived from
`artifact.lifecycle_state` (T002 §"State model") and `artifact_version.version_state`.

| Tier | Source state | Body retained | Structured payload | Chunks | Links | Comments |
|---|---|---|---|---|---|---|
| **Hot** | `lifecycle_state ∈ {draft, active}` | yes | yes | yes (live) | yes | yes |
| **Cold-accepted** | `lifecycle_state = active`, `version_state = superseded` | yes | yes | superseded (queryable with `include_history=true`) | yes | yes |
| **Cold-archived** | `lifecycle_state = archived`, age < `ARCHIVE_BODY_TTL_DAYS` (default 180) | yes | yes | superseded | yes | yes |
| **Cold-archived (purged body)** | `lifecycle_state = archived`, age ≥ `ARCHIVE_BODY_TTL_DAYS` | **no** (replaced with `null` + `body_purged_at` timestamp) | no (replaced with `null`) | retained (chunks remain queryable) | yes | yes |

**What never expires.** `artifact_id`, `artifact_version_id`, `version_state`,
`version_label`, `parent_version_id`, `source_format`, `created_by_actor_id`,
`created_via_workflow_run_id`, all audit-path links, all comments and their
resolution provenance, all workflow_run rows, all idempotency mappings.
The audit graph is preserved indefinitely so historical handoffs and
decision traces remain reconstructible after the body bytes are purged.

**Purge mechanism.** A nightly maintenance job (`gateway artifacts purge`,
owned by SRE) scans archived artifacts past `ARCHIVE_BODY_TTL_DAYS` and
nulls body/structured_payload bytes. The job is idempotent and records
counts to the metrics surface (`gateway_artifact_purge_total`,
labels `tier`, `reason`).

**Override.** A label `retain:permanent` on an artifact suppresses body
purge regardless of state. Documentation artifacts with
`subkind=api_context` carry this label by default in v1 because their
canonical retrieval body is the durable contract for downstream agents.

## 4. Backup and restore expectations

### 4.1 What is backed up

The substrate ships inside the existing gateway SQLite backup envelope.
The backup MUST include:

- All artifact tables: `artifacts`, `artifact_versions`, `contributions`,
  `comments`, `links`, `workflow_runs`, `actors`.
- All idempotency-mapping rows (no separate table — the unique indexes
  on each resource are the mappings per T003 §4.3).
- The full text of every `artifact_version.body`, `structured_payload`,
  `contribution.body`, and `comment.body` that has not been purged per §3.

The backup MAY exclude:

- `chunks` table rows (text + embedding vectors). Chunks are regeneratable
  from their owning `artifact_version` via the `doc_publish` workflow
  (T011) at restore time. Excluding chunks keeps backups small; including
  them is acceptable when storage is cheap.

If chunks are excluded, the restore procedure (§4.3) MUST trigger
`doc_publish` re-chunking for every documentation artifact whose
`accepted_version_id` is set.

### 4.2 Backup cadence and retention

Inherits the existing gateway backup cadence (currently nightly via the
SRE-managed backup job; the cadence itself is not redefined here). Backup
retention SHOULD provide at least:

- 7 daily snapshots
- 4 weekly snapshots
- 12 monthly snapshots

Substrate-specific backup metadata stored alongside each snapshot:
`backup_schema_version`, `artifact_count`, `artifact_version_count`,
`link_count`, `chunk_count_or_excluded`.

### 4.3 Restore verification

Restore is verified by an automated post-restore checklist before the
gateway accepts mutating traffic:

1. **Schema check.** SQLite `PRAGMA integrity_check` returns `ok`. The
   migration version recorded in the backup matches the running binary's
   expected migration head; if not, the gateway refuses to start and
   surfaces the mismatch in the operator log.
2. **Artifact pointer consistency.** For every artifact with
   `current_version_id IS NOT NULL`, the referenced `artifact_version` row
   exists and belongs to the same `artifact_id`. Same check for
   `accepted_version_id`. Mismatches are logged and the artifact is
   flagged with a warning label `restore:pointer_mismatch` rather than
   silently corrected.
3. **Audit-path link integrity.** For every link whose `link_type` appears
   in the T003 §7 audit-path registry, both `source_version_id` and
   `target_version_id` (when required by the registry) resolve to existing
   rows. Missing references are logged and the link row is marked
   `restore:dangling` for human review.
4. **Workflow_run consistency.** Every `workflow_run` row with state
   `succeeded` has its declared `generated_*` ids resolvable. Runs with
   state `started` older than `WORKFLOW_RUN_STUCK_TTL_HOURS` (default 24)
   are transitioned to `failed` with
   `failure_reason="state lost across restore"`.
5. **Idempotency mapping spot-check.** A configurable sample (default
   100) of `(workflow_run_id, idempotency_key)` pairs is re-queried; each
   MUST return the expected `generated_resources` payload from §5.1 of
   T003.
6. **Chunk regeneration (when chunks excluded).** Trigger `doc_publish`
   for every documentation artifact's `accepted_version_id`. Chunk count
   after regeneration MUST match the version's `manifest.chunk_count`
   (recorded on the version's structured_payload by T011). Mismatch is
   surfaced as `restore:chunk_count_mismatch`.

The post-restore checklist is owned by SRE; T014 includes it in the final
rollout-gate validation.

### 4.4 RPO / RTO targets

- **RPO** (recovery point objective): ≤ 24 hours. The substrate uses the
  existing gateway nightly backup cadence.
- **RTO** (recovery time objective): ≤ 2 hours for the substrate-specific
  restore checks above, on top of the underlying SQLite restore time.
  Chunk regeneration may run in the background after the gateway accepts
  reads; mutating traffic resumes once steps 1–5 pass.

## 5. Metrics surface

All metrics use the existing gateway Prometheus exporter conventions
(`gateway_*` prefix, snake_case, project label `project=<ident>` where
project-scoped, artifact-kind label `kind=<kind>` where artifact-scoped).
SRE wires the exporter; this document fixes the names and labels so T007
emits them consistently.

### 5.1 Write rates

- `gateway_artifact_writes_total{project,kind,result}` — counter; result
  ∈ `created|replayed|rejected_size|rejected_quota|rejected_validation`.
- `gateway_artifact_version_writes_total{project,kind,result}` — counter,
  same result enum.
- `gateway_contribution_writes_total{project,kind,phase,result}` — counter,
  `phase` from contribution metadata (`pass_1`, `pass_2`, `synthesis`, etc.).
- `gateway_link_writes_total{project,link_type,result}` — counter.
- `gateway_comment_writes_total{project,target_kind,result}` — counter.

### 5.2 Diff and version

- `gateway_artifact_version_body_bytes{project,kind}` — histogram. Buckets
  `[1KiB, 4KiB, 16KiB, 64KiB, 256KiB, 1MiB]`.
- `gateway_artifact_version_diff_bytes{project,kind}` — histogram of the
  byte delta against `parent_version_id`. Buckets shared with body bytes.

### 5.3 Search and retrieval

- `gateway_artifact_search_requests_total{project,by}` — counter; `by`
  ∈ `id|query|chunks|history`.
- `gateway_artifact_search_latency_seconds{project,by}` — histogram.
- `gateway_artifact_chunk_lookups_total{project,result}` — counter;
  result ∈ `current|stale|superseded|not_found`.

### 5.4 Chunking lifecycle

- `gateway_artifact_chunks_generated_total{project,kind,result}` —
  counter; result ∈ `created|replayed|failed`.
- `gateway_artifact_chunks_failed_total{project,kind,reason}` — counter;
  `reason` ∈ `embedding_error|oversize|source_format_unsupported|other`.
- `gateway_artifact_chunks_stale_total{project,kind}` — gauge. A chunk
  is **stale** when its `artifact_version_id != artifact.accepted_version_id`
  for the owning artifact and it has not been soft-superseded. This is the
  retrieval-freshness signal that T011 retries against.
- `gateway_artifact_chunks_superseded_total{project,kind}` — gauge of
  rows with `superseded_by_chunk_id IS NOT NULL`.

### 5.5 Workflow runs

- `gateway_workflow_runs_total{project,kind,state}` — counter.
- `gateway_workflow_run_duration_seconds{project,kind}` — histogram
  (ended_at − started_at).
- `gateway_workflow_run_retries_total{project,kind}` — counter; bumped
  on every `Idempotency-Key` replay against an existing run.
- `gateway_workflow_run_stuck{project,kind}` — gauge of runs in
  `started` for longer than `WORKFLOW_RUN_STUCK_TTL_HOURS`.

### 5.6 Quotas

- `gateway_artifact_quota_warnings_total{project,counter}` — counter,
  bumped on every soft-warning emission.
- `gateway_artifact_quota_rejects_total{project,counter}` — counter,
  bumped on every hard-limit rejection.

### 5.7 Health

- `gateway_artifact_db_open_connections` — gauge.
- `gateway_artifact_purge_total{tier,reason}` — counter, emitted by the
  nightly purge job (§3).

## 6. Stale and failed chunk handling

Per T002 §"Operations envelope" the retrieval response MUST surface chunk
freshness. T011 owns the wire shape; T004 fixes the visible behaviors.

### 6.1 Stale chunk surfacing

Every `agent-tools docs chunks` (and underlying
`/v1/projects/:ident/api-docs/chunks` plus future
`/v1/projects/:ident/artifacts/:id/chunks`) response item MUST include:

- `artifact_version_id` — the version the chunk anchors to.
- `accepted_version_id` — the artifact's current accepted version.
- `freshness` — enum `current | stale | superseded_history`.
  - `current`: `artifact_version_id == accepted_version_id`.
  - `stale`: `artifact_version_id != accepted_version_id` AND the chunk
    is not soft-superseded (a re-chunk against the accepted version has
    not yet completed).
  - `superseded_history`: returned only when the caller passed
    `include_history=true`; the chunk's row has
    `superseded_by_chunk_id IS NOT NULL`.

Agents SHOULD treat `freshness = stale` as a soft signal that the underlying
artifact has moved on; clients MAY re-issue the same query after a short
delay or trigger a re-chunk by calling `doc_publish` against the accepted
version. Default `agent-tools docs chunks` callers DO NOT see
`superseded_history` chunks; opt-in is explicit per T002 §"Search and
retrieval".

### 6.2 Failed chunk surfacing

When a `doc_publish` run fails to chunk a version (T003 §5.1 chunk row),
the failure is durable:

- The `workflow_run` row records `state=failed`,
  `failure_reason="chunking failed at child_address=<addr>: <reason>"`,
  and `generated_chunk_ids` lists the chunks that did land.
- A retry with the same `(workflow_run_id, idempotency_key)` resumes from
  the first missing child_address (the `(artifact_version_id, child_address)`
  natural key makes resume safe; T003 §5.1).
- The next `chunks` query against the artifact surfaces a top-level
  envelope field `chunking_status` = `partial` with a `failed_addresses`
  list naming the unfinished child addresses. Agents that need complete
  coverage MUST either trigger a `doc_publish` retry or fall back to
  fetching the full version body via `artifact_version` get.
- The `gateway_artifact_chunks_failed_total` metric bumps on every
  failure with `reason` set per T011's failure taxonomy.

**Rate-limit.** SRE alerts when
`rate(gateway_artifact_chunks_failed_total[15m]) > 0.1 per project`. The
alert routes to the doc-workflow on-call; sustained failures pause the
`doc_publish` retry loop to avoid embedding-cost runaway.

## 7. Migration plan

The migration runs in phases tied to the rollout order in
`gateway-features.md` §"Rollout". Each phase has a dual-read window where
the legacy surface and the substrate surface BOTH work; the phase
"completes" only when the deprecation criteria in §9 are met.

### Phase 0 — Pre-migration (no behavior change)

- Deploy T005 schema, T006 repository, T007 generic API behind a feature
  flag `GATEWAY_ARTIFACT_API_ENABLED` (default `false`).
- Backfill `actors` table from the existing agent identities observed in
  the messages, tasks, and api_docs tables. The backfill is idempotent
  on `(actor_type, agent_system, agent_id, host)`.
- No legacy reads or writes change.

### Phase 1 — Spec artifacts as first vertical slice

Per `gateway-features.md` §"Rollout" step 5 (specs first) and the prior
gateway-features memory ("artifacts need operations envelope before broad
rollout") this is the slice that proves the substrate carries production
load.

- `GATEWAY_ARTIFACT_API_ENABLED=true` in a staging environment.
- `/spec` skill writes accepted spec versions to the substrate; legacy
  `<doc>_spec/` directories are linked (not bulk-imported) via a
  `spec_artifact_imports_directory` link type so the spec artifact
  carries the source directory path without copying the file bodies.
- `agent-tools tasks add-delegated --target-project ...` and
  `--specification ...` are unchanged; tasks generated from spec
  artifacts gain a `source` field per T009 referencing
  `(spec_artifact_id, spec_artifact_version_id, manifest_item_id)`.
- Existing tasks without a spec artifact source are untouched. They
  remain valid; no implicit backfill.
- Migration is reversible by toggling the feature flag off (see §8).

### Phase 2 — Design review artifacts

- `/design-review` writes review rounds to the substrate (T010).
- Legacy `/tmp/design-review-*` scratch files keep working in parallel
  for one full release cycle. Skills emit a `dual_write` warning on
  every scratch write so the deprecation is visible.

### Phase 3 — Documentation artifacts (API docs migration)

Per T001 the existing `api_docs` rows become documentation artifacts.
This phase is the one that touches durable agent retrieval, so the
ordering matters: it runs **after** specs and reviews because spec/review
workflows do not depend on doc retrieval, while doc retrieval is hit by
every agent session.

- One-shot migration writes a new `artifact` + initial `artifact_version`
  per existing `api_docs` row. The artifact id equals the old
  `api_doc_id`. The initial version is marked `accepted`.
- Chunks are regenerated against the new `artifact_version_id` via
  `doc_publish` (T011). Old `api_doc_chunks` rows are kept for one
  release cycle as a fallback read path.
- `agent-tools docs *` and `/v1/projects/:ident/api-docs/*` remain
  canonical agent retrieval per T001; internally they read the
  substrate.

### Phase 4 — Project documentation (non-API docs)

- T011 owns this phase. New artifact kind subkinds (e.g.
  `runbook`, `onboarding`, `architecture`) are introduced on the
  substrate. Source-adjacent repository docs (READMEs, etc.) remain in
  the repo per `gateway-features.md` Feature 3; they MAY be mirrored
  into the substrate for search but the repo copy stays canonical.

### Phase 5 — Skill migration (T015)

- `/design-review`, `/spec`, `/implement` skills consume artifact ids
  instead of `/tmp` scratch files. Skill-level dual-write banners are
  removed; the substrate becomes the canonical handoff surface.

### Phase 6 — Tear-down

- Legacy scratch files (`/tmp/design-review-*`, `<doc>_spec/` raw
  imports, `api_doc_chunks` fallback rows) are removed only after the
  deprecation criteria in §9 are met.
- Feature flag `GATEWAY_ARTIFACT_API_ENABLED` becomes unconditional; the
  flag is removed in the next release.

### What the migration links rather than imports

Per the T004 implementation note ("Migration can initially link existing
records instead of bulk importing every historical scratch artifact"),
the following are linked, not copied:

- Historical `<doc>_spec/` task files: a `spec_artifact_imports_directory`
  link points the spec artifact at the source directory path. Per-task
  files remain readable in the repo; the artifact version's
  `structured_payload.items[].spec_file_path` and `spec_body` carry the
  imported content T002 already names.
- Historical task records: existing gateway tasks without a spec source
  stay untouched. New tasks generated from a spec carry the source
  field. There is no bulk backfill of historical tasks onto spec
  artifacts.
- Historical messages / comms: untouched. The substrate does not own
  message history.
- Historical memories: untouched. Memories remain the distilled-learning
  surface per T002 §"Relationship to existing primitives".

## 8. Rollback path

Rollback restores the existing task / docs behavior without data loss.
The path differs by phase.

### 8.1 Pre-acceptance rollback (Phase 0–1 in staging)

- **Trigger.** A blocker is found before any production workload writes
  to the substrate.
- **Procedure.** Set `GATEWAY_ARTIFACT_API_ENABLED=false`. Substrate
  tables remain in the database but no new writes land. Legacy surfaces
  continue unchanged.
- **Data treatment.** Substrate rows written during staging are kept for
  forensic review and dropped manually once root cause is found.

### 8.2 Phase 1 rollback (spec artifacts in production)

- **Trigger.** Spec artifact behavior is found broken (e.g. T009 manifest
  schema regresses, task generation duplicates rows).
- **Procedure.**
  1. Set `GATEWAY_ARTIFACT_API_ENABLED=false` for write paths only.
     Reads remain enabled so existing spec-artifact references resolve.
  2. `/spec` skill falls back to its prior `<doc>_spec/` scratch
     directory behavior. The skill code keeps the legacy path
     available behind a `--legacy` flag for one full release cycle to
     make this fallback explicit and testable.
  3. Newly generated tasks omit the `source` field referring to the
     spec artifact; they revert to the prior plain `--specification`
     workflow.
- **Existing tasks.** Tasks already generated with
  `(spec_artifact_id, spec_artifact_version_id, manifest_item_id)`
  source fields remain valid: the substrate is read-only, so the
  references resolve. No task surgery is required.
- **Comments and links.** Resolved comments on substrate artifacts stay
  resolved. Links remain readable. No deletes.

### 8.3 Phase 2 rollback (design reviews)

- **Trigger.** Review-round mutation contract is found broken.
- **Procedure.** Same flag-flip; `/design-review` falls back to its
  scratch-file behavior. Existing review-artifact comments and
  contributions remain queryable as audit history.

### 8.4 Phase 3 rollback (API docs migration)

This is the most sensitive rollback because API doc retrieval is hit by
every agent session.

- **Trigger.** Documentation artifact retrieval regresses (chunk
  mismatch, body format mismatch, freshness signal broken) after
  migration.
- **Procedure.**
  1. Flip a separate flag `GATEWAY_API_DOCS_READ_SOURCE` from
     `artifact` back to `legacy`. The legacy `api_docs` /
     `api_doc_chunks` tables are still present (kept for one release
     cycle per §7 Phase 3); reads resume against them.
  2. The artifact rows for migrated API docs are left in place. New
     `agent-tools docs publish` calls land in BOTH legacy and
     substrate during the rollback window, controlled by
     `GATEWAY_API_DOCS_DUAL_WRITE=true`, so a subsequent fix-forward
     does not lose data published during the rollback.
  3. Once the substrate fix lands, flip the read source back to
     `artifact`. Re-chunk via `doc_publish` to bring any rollback-window
     publishes into the substrate's chunk index.
- **No id breakage.** Per T001 the `api_doc_id` is preserved as
  `artifact_id`. Existing skill references to doc ids continue to
  resolve in both modes.

### 8.5 Named legacy fallbacks (per surface)

| Substrate surface | Legacy fallback | Where the fallback lives |
|---|---|---|
| Spec artifacts (T009) | `<doc>_spec/` directories + plain `agent-tools tasks add --specification` | `/spec` skill `--legacy` mode |
| Design-review artifacts (T010) | `/tmp/design-review-*` markdown + research-analyst scratch | `/design-review` skill `--legacy` mode |
| Documentation artifacts — API docs (T011) | Legacy `api_docs` + `api_doc_chunks` tables | `GATEWAY_API_DOCS_READ_SOURCE=legacy` flag |
| Documentation artifacts — project docs (T011) | Repo markdown + `agent-tools patterns` for org-wide guidance | n/a (repo never went away) |
| Generic artifact retrieval by id | Per-surface legacy fallbacks above; no generic fallback | n/a |

Each `/spec`, `/design-review`, `/implement` skill MUST land its
`--legacy` mode in the same release that switches its substrate write
on so the fallback is provably exercised before production traffic
moves.

## 9. Deprecation criteria

Deprecation is **objective**, not "when stable". A surface MAY be
removed only when ALL of its criteria below hold for at least one
release cycle.

### 9.1 Scratch-file deprecation (Phases 1–2)

The `/tmp/design-review-*` scratch directory and
`<doc>_spec/` directory dual-read are removed when:

- [ ] ≥ 95% of `/design-review` and `/spec` invocations in the last 30
      days targeted artifact ids (measured via skill telemetry; the
      remaining 5% allowance covers user-triggered `--legacy` runs).
- [ ] 0 substrate-side `failure_reason` entries in the last 30 days
      naming "spec manifest schema mismatch" or "review round
      contract mismatch".
- [ ] All `dual_write` warnings emitted by the skills in the last 30
      days resolved to clean artifact-only writes on retry (no
      scratch-only success paths).
- [ ] T014 final-rollout validation has signed off.

### 9.2 Legacy `api_docs` table deprecation (Phase 3)

The `api_docs` and `api_doc_chunks` fallback tables are removed when:

- [ ] One full release cycle has elapsed since `GATEWAY_API_DOCS_READ_SOURCE`
      switched to `artifact` with no rollback.
- [ ] Substrate chunk freshness (§6.1) has shown `current` ≥ 99% across
      all projects for 30 days (measured via
      `gateway_artifact_chunk_lookups_total{result="current"}` ratio).
- [ ] 0 documentation artifacts have `chunking_status=partial` older
      than 24 hours.
- [ ] Every project has at least one successful `doc_publish` against
      the substrate in the last 30 days (or has been formally archived).

### 9.3 `--legacy` skill mode removal (Phase 5)

Skill `--legacy` flags are removed when:

- [ ] No `--legacy` invocation in the last 60 days.
- [ ] No open ticket / task referencing the substrate path of the
      affected workflow as blocked.
- [ ] T015 client migration task is done.

### 9.4 Feature flag removal (Phase 6)

`GATEWAY_ARTIFACT_API_ENABLED` and `GATEWAY_API_DOCS_READ_SOURCE` are
removed when:

- [ ] All phase deprecation criteria above are met.
- [ ] One release cycle has passed since both flags were unconditional
      in production.

T014 owns final sign-off on each deprecation. Until sign-off, the
fallback surface MUST remain operable.

## 10. Validation plan results

This document was self-checked against the T004 validation plan:

- **Limit usability check.** §1 names every size limit, names its env
  key, names its HTTP rejection code, and names the T008 shrunk
  fixture values. T007 and T008 can read both production and test
  limits from the same env-key surface. **Pass.**
- **Backup/restore check.** §4 names what is backed up, names a
  6-step post-restore consistency checklist (schema, artifact
  pointers, audit-path links, workflow_runs, idempotency mappings,
  chunk regeneration), names the RPO/RTO, and names which results
  are surfaced as warning labels vs. hard failures. **Pass.**
- **Rollback dry read.** §8 walks each phase's rollback procedure
  end-to-end and the §8.5 fallback table names the legacy surface,
  fallback location, and skill flag for every substrate surface.
  No "TBD" entries. **Pass.**
- **Deprecation check.** §9 lists objective criteria — usage
  percentages, error-count thresholds, time windows — for every
  scratch-file and legacy-surface removal. No "when stable"
  criteria. **Pass.**

## 11. Acceptance criteria — self-check

- [x] Operations document defines maximum artifact body and contribution
      sizes, quota or warning behavior, and retention/archive policy.
      §1, §2, §3.
- [x] Operations document defines backup and restore expectations for
      artifact tables, bodies, chunks, and links. §4 (incl. §4.3
      restore verification of pointers, links, runs, idempotency,
      chunks).
- [x] Operations document defines metrics for writes, diffs, search,
      chunking, failed chunks, and stale chunks. §5 (5.1 writes, 5.2
      diff, 5.3 search, 5.4 chunking incl. stale + failed).
- [x] Migration plan covers importing or linking existing task/spec/doc
      state where useful. §7 (spec dirs and historical tasks are
      linked, not bulk-imported; api_docs is migrated by writing one
      artifact + version per row with id preservation).
- [x] Rollback plan documents how clients return to existing task/docs
      behavior if artifact endpoints or body schemas need rework. §8
      (per phase) and §8.5 (named legacy fallback table).
- [x] Dual-read / deprecation criteria for scratch files and legacy
      docs behavior are explicit. §7 (dual-read windows per phase)
      and §9 (objective deprecation criteria).

## 12. Open questions surfaced for downstream tasks

These are explicitly handed to the named owners. They are not blockers
for T005/T006/T007/T008/T011/T014.

1. **(T011)** Exact `chunking_status=partial` wire shape on the
   `chunks` response envelope. §6.2 names the field and behavior;
   T011 fixes the JSON shape.
2. **(T011)** `manifest.chunk_count` field on the documentation
   artifact's `structured_payload`. §4.3 step 6 requires it for
   restore verification; T011 owns the schema.
3. **(SRE)** Whether to expose `gateway_workflow_run_replay_count` per
   T003 §11. Not required by the contract; surfaced here for
   completeness of the metrics list (§5.5 covers retries via
   `gateway_workflow_run_retries_total`).
4. **(T014)** Final-rollout gate text per phase. This document supplies
   the criteria; T014 owns the operator-facing checklist UI / runbook.
5. **(Future auth task)** When `project.administer` becomes
   enforceable (vs. v1 trusted mode), §2's quota-override behavior
   needs a real scope check. The fields are already in place; only
   the gate switches.

## 13. Hand-off

- **T005 (schema):** turns §1 size limits into `CHECK` constraints
  where SQLite supports them, otherwise into T006-layer rejections;
  adds the columns required by §3 retention (`body_purged_at`),
  §4.3 restore checks (none new; reuses T002 fields), and §6.2
  chunking status (none new on chunks themselves; the response
  envelope is computed).
- **T006 (repository):** exposes the per-resource validations from
  T003 §6 plus the §1/§2 envelope checks as typed errors so T007
  maps them to HTTP. Implements the §3 nightly purge job entry
  point.
- **T007 (generic HTTP API):** reads the env keys in §1/§2,
  emits the §5 metrics, returns the §6.1 `freshness` field on every
  chunk response, returns the §6.2 `chunking_status` envelope field
  on every chunks-listing endpoint. Wires the rollback feature flags
  in §8.
- **T008 (regression tests):** uses the §1 shrunk-fixture env keys
  to write size-limit accept/reject pairs; uses §2 quota envs to
  write quota-warning and quota-reject pairs; uses §6 fixtures for
  stale + partial-chunk surfacing; uses §4.3 to validate post-restore
  consistency in CI.
- **T011 (documentation workflow):** owns the §6.2 wire shape,
  the §3 default `retain:permanent` label on api_context artifacts,
  and the §4.3 step 6 `manifest.chunk_count` schema field. Drives
  Phase 3 of §7 and Phase 4 (project docs) end-to-end.
- **T014 (rollout / migration / rollback validation):** consumes
  every §9 criterion as a checklist gate; runs the §4.3 post-restore
  checklist in a staging restore drill before each phase advances.

## 14. T014 final validation notes

Recorded during gateway-features_spec T014 on 2026-05-13.

### 14.1 Migration and compatibility evidence

- Existing source-adjacent spec directories can be imported into spec
  artifacts, accepted, and used to generate gateway tasks. The route test
  `spec_routes_import_manifest_round_trip_and_fetch_stable_item` verifies
  stable manifest item retrieval.
- Existing unlinked implementation tasks can be linked to accepted spec
  manifest items without duplication. The route test
  `spec_generate_tasks_recovers_existing_unlinked_task_after_partial_failure`
  verifies idempotent recovery and link creation.
- Explicit task linking remains available for migration. The route test
  `spec_link_existing_task_creates_back_link` verifies artifact-to-task link
  creation for a pre-existing task.
- Legacy API docs remain readable through the `/api-docs` compatibility
  surface while docs are mirrored into documentation artifacts and chunks. The
  route and repository tests for API-doc artifact-backed chunks verify the
  dual-read path.
- Client and skill migration ownership is documented in
  `docs/artifact-client-skill-migration.md`, including delegated agent-tools
  work `019e23b7-6300-7441-9370-ac19b3302d58` ->
  `019e23b7-62fd-7540-ac79-a5cb4ac6731a`.

### 14.2 Stable-ID workflow evidence

- Design review workflow routes persist review rounds, pass 1, pass 2,
  synthesis, read-set provenance, and decision state by artifact IDs.
- Spec workflow routes persist imported spec versions, accepted versions,
  manifest item IDs, generated task IDs, and `task_generated_from_spec` links.
- `/implement` migration is release-gated on accepting a spec artifact ID as
  canonical input and delegating exact task IDs plus
  `(artifact_id, artifact_version_id, manifest_item_id)`.
- Scratch files and task `Specification` bodies are compatibility mirrors until
  all release gates in `docs/artifact-client-skill-migration.md` pass.

### 14.3 Rollback dry run

- `GATEWAY_ARTIFACT_API_ENABLED=false` disables artifact API routes with
  `503 artifact_api_disabled`; legacy task and API-doc workflows remain
  available.
- `GATEWAY_ARTIFACT_BODY_SCHEMA_ENABLED=false` disables structured body-schema
  writes while preserving markdown writes and legacy fallbacks.
- In rollback, clients should stop following artifact links as canonical state,
  resume task-specification/API-doc compatibility reads, and leave existing
  artifact rows intact for later replay or audit.

### 14.4 Restore checklist dry run

T004 §4.3 restore verification is covered by automated repository checks:

- Pointer consistency: `restore_check_reports_pointer_mismatch_without_repair`.
- Audit-path links: `artifact_link_visibility_predicate_is_consistent` plus
  restore audit-link checks in `run_restore_check`.
- Workflow runs: `restore_check_reports_stuck_workflow_run`.
- Idempotency mappings:
  `artifact_repository_idempotent_mutations_and_workflow_updates`,
  `artifact_link_idempotency_is_unique_per_run`, and spec generation rerun
  tests.
- Chunk regeneration/freshness:
  `artifact_routes_expose_stale_and_partial_chunking_status` and API-doc chunk
  tests.

### 14.5 Deprecation gates

No scratch-file, legacy API-doc, legacy skill-mode, or feature-flag removal is
approved by this validation. §9 gates remain open until:

- agent-tools wrapper commands land and pass stable-ID handoff validation;
- `/design-review`, `/spec`, and `/implement` use artifact IDs as canonical
  handoff;
- the T014 validation suite is rerun against that client surface;
- one release cycle has passed with fallback metrics below §9 thresholds.

### 14.6 Metrics and UI/API smoke

- Route tests and browser smoke cover artifact list/detail pages, version
  history/diff, review contribution views, spec manifest views, docs browser
  rows, stale chunks, and failed chunks.
- Log-emitted metric calls cover writes, version writes, diffs, search,
  comments, contributions, links, and workflow runs.
- Hardened authorization mode is exposed by README/API context and artifact UI
  status signals. `GATEWAY_ARTIFACT_AUTH_ENFORCED=true` requires
  `X-Agent-Project` and `X-Agent-Scopes`; quota override requests require
  `project.administer`.

### 14.7 Validation commands

- `cargo test -p gateway`
- `cargo clippy -p gateway --all-targets -- -D warnings`
- `agent-tools docs validate --file .agent/api/agent-gateway.yaml`
- `agent-tools docs publish --file .agent/api/agent-gateway.yaml`
- Browser smoke: `/artifacts`, project artifact workspace, and artifact detail.
