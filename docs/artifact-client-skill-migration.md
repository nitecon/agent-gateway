# Artifact Client and Skill Migration

This plan moves agent-facing review, spec, implementation, and documentation
handoff from scratch files and task `Specification` blobs to stable gateway
artifact IDs. During migration, scratch files remain compatibility mirrors,
not canonical state.

## Canonical Handoff

The canonical tuple for workflow handoff is:

- `project_ident`
- `artifact_id`
- immutable `artifact_version_id`
- optional `manifest_item_id`, `contribution_id`, `comment_id`, `link_id`, or
  `workflow_run_id`
- generated gateway `task_id` when a spec item has become implementation work

Skills and clients should read that tuple from artifact routes first. Local
scratch paths and task specifications are dual-read fallbacks only when an
artifact pointer is absent or the artifact API is disabled.

## Agent-Tools Command Migration

Required `agent-tools` client work, owned by the agent-tools project:

| Area | Target command | Gateway route family | Validation |
|---|---|---|---|
| Generic artifacts | `agent-tools artifacts list [--kind --status --label --actor --query]` | `GET /v1/projects/:ident/artifacts` | List seeded design-review, spec, and docs artifacts by filters. |
| Generic artifacts | `agent-tools artifacts get <artifact_id>` | `GET /v1/projects/:ident/artifacts/:artifact_id` | Shows current/accepted versions, comments, links, and chunking status. |
| Versions | `agent-tools artifacts versions <artifact_id>` | `GET /versions` | Shows immutable version IDs and state. |
| Diffs | `agent-tools artifacts diff <artifact_id> <from> <to>` | `GET /versions/:version_id/diff?base_version_id=...` | Diff output matches route response. |
| Comments | `agent-tools artifacts comments <artifact_id>` and `comment` | `/comments` | Create/list/resolve/reopen round-trips with actor provenance. |
| Reviews | `agent-tools reviews create/start/contribute/synthesize/state` | `/design-reviews` | Two-pass review stores pass 1, pass 2, synthesis, read-set, and decision state by artifact ID. |
| Specs | `agent-tools specs import/version/accept/manifest/generate-tasks/link-task` | `/specs` | Accepted spec artifact generates idempotent gateway tasks and links existing tasks. |
| Docs | `agent-tools docs export` | docs compatibility + artifact routes | Export accepted docs artifact to source-adjacent files using a manifest and propose-by-default conflict policy. |

Delegated tracking:

- Agent-gateway delegated task `019e23b7-6300-7441-9370-ac19b3302d58`
  targets agent-tools task `019e23b7-62fd-7540-ac79-a5cb4ac6731a` for the
  `artifacts`, `reviews`, and `specs` CLI wrappers above. Scratch-file
  deprecation is blocked until that work, or an equivalent agent-tools command
  surface, is validated.

## Skill Migration

### `/design-review`

Target behavior:

1. Create or update a `design_review` artifact for the source document.
2. Start each round with a `workflow_run_id`.
3. Pass `artifact_id`, `source_artifact_version_id`, `workflow_run_id`, and
   prior `contribution_id` values to peer agents.
4. Store pass 1 and pass 2 outputs as contributions. Pass 2 must preserve the
   read-set of pass 1 contribution IDs.
5. Store synthesis as a contribution and, when appropriate, as a new artifact
   version.

Compatibility:

- Existing scratch review markdown may still be written for human inspection,
  but the gateway contribution/version IDs are canonical.
- If artifact creation fails with `artifact_api_disabled`, the skill may fall
  back to current scratch behavior and must record that fallback in the task
  summary.

### `/spec`

Target behavior:

1. Convert the spec directory manifest and per-task files into a `spec`
   artifact version.
2. Preserve stable `manifest_item_id` values, acceptance criteria, validation
   plans, touch surfaces, and source design references.
3. Accept the version when it becomes the implementation baseline.
4. Iterate by creating new immutable versions instead of mutating a scratch
   directory as the canonical source.

Compatibility:

- Source-adjacent spec directories remain materialized mirrors for review and
  local editing.
- When both a spec directory and artifact ID exist, the accepted artifact
  version wins unless the user explicitly asks to regenerate from files.

### `/implement`

Target behavior:

1. Accept either a source-adjacent spec directory or a spec artifact ID.
2. Prefer the accepted spec artifact version and manifest item IDs.
3. Generate or reuse exact gateway task IDs via
   `POST /v1/projects/:ident/specs/:artifact_id/generate-tasks`.
4. Delegate implementation using `task_id`, `artifact_id`,
   `artifact_version_id`, and `manifest_item_id`, not scratch path alone.
5. Link any pre-existing implementation task with `/link-task` before claiming
   it as generated work.

Compatibility:

- Existing task `Specification` content remains a fallback and should include
  the source artifact tuple when tasks are generated during migration.
- If artifact links are absent, the skill may read the task specification but
  must not treat that as ready for scratch-file deprecation.

## Ownership Map

| Surface | Owner | Location | Blocking for deprecation |
|---|---|---|---|
| Gateway routes and storage | agent-gateway | this repo | Done for artifact/spec/review/docs routes. |
| Agent API context | agent-gateway | `.agent/api/agent-gateway.yaml` | Done; keep published after changes. |
| Agent-tools CLI wrappers | agent-tools project | delegated task `019e1d32-5d07-7002-a98b-27ffefb6e777` | Yes. |
| Local `/design-review` skill | local skill directories synced by skill-sync | `~/.codex`, `~/.claude`, `~/.gemini` skill working copies | Yes. |
| Local `/spec` skill | local skill directories synced by skill-sync | same | Yes. |
| Local `/implement` skill | local skill directories synced by skill-sync | same | Yes. |
| Rollout validation | agent-gateway | `docs/artifact-operations-rollout.md` and T014 | Yes. |

## Release Gates

Scratch-file deprecation is blocked until all of the following are true:

- Agent-tools can list/get/diff/comment artifacts and run review/spec commands
  without raw curl.
- `/design-review` has completed one two-pass review using artifact IDs as the
  peer handoff.
- `/spec` has produced one spec artifact and accepted version from a source
  design document.
- `/implement` has generated or reused gateway tasks from an accepted spec
  artifact and delegated exact task IDs.
- Dual-read fallback has been tested for legacy task specifications and local
  scratch directories.
- `agent-tools docs export` has dry-run or materialized source-adjacent docs
  with an export manifest and propose-by-default conflict handling.
- T014 rollout validation has exercised restore checks, idempotent task links,
  rollback instructions, metrics, and authorization mode signals.

## Dry-Run Handoff Check

Before deprecation, run this end-to-end path:

1. Create a design review artifact for `gateway-features.md`.
2. Store pass 1, pass 2, and synthesis contributions by artifact IDs.
3. Create a spec artifact from the synthesis and accept the spec version.
4. Generate implementation tasks from the accepted spec version.
5. Confirm every generated task has a `task_generated_from_spec` artifact link.
6. Re-run generation with the same idempotency key and confirm task/link IDs
   are reused.
7. Re-run `/implement` using the spec artifact ID and confirm no scratch file is
   required to identify work.
