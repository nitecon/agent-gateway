# T004 - Define operations envelope and migration rollback plan

**Team:** infra
**Phase:** 1
**Depends on:** T002, T003
**Status:** todo

## Scope

**In:** Define production limits, observability, retention/archive, backup/restore expectations, migration phases, dual-read compatibility, deprecation criteria, and rollback path.

**Out:** Implementing the metrics system or running production migration. This task produces release criteria and operational requirements.

## Source references

- `gateway-features.md` sections "Rollout" and "Success Criteria"
- Prior memory: rollout needs explicit migration and rollback for task specs, API docs/chunks, scratch-file workflows, and artifact links.
- Prior memory: artifacts need operations envelope before broad rollout.

## Deliverables

1. **`docs/artifact-operations-rollout.md`** - operations and rollout document.
2. **`README.md` note** - links to the rollout document from the relevant feature/API section.

## Implementation notes

- Keep limits concrete enough for route validation and tests.
- Include concrete fixture values or named configuration keys that T008 can reuse in size-limit tests.
- Include visible handling for stale or failed retrieval chunks because documentation artifacts depend on chunk freshness.
- Migration can initially link existing records instead of bulk importing every historical scratch artifact.

## Acceptance criteria

- [ ] Operations document defines maximum artifact body and contribution sizes, quota or warning behavior, and retention/archive policy.
- [ ] Operations document defines backup and restore expectations for artifact tables, bodies, chunks, and links.
- [ ] Operations document defines metrics for writes, diffs, search, chunking, failed chunks, and stale chunks.
- [ ] Migration plan covers importing or linking existing task/spec/doc state where useful.
- [ ] Rollback plan documents how clients return to existing task/docs behavior if artifact endpoints or body schemas need rework.
- [ ] Dual-read/deprecation criteria for scratch files and legacy docs behavior are explicit.

## Validation plan

- **Limit usability check:** T007/T008 can translate size limits and quota behavior into route validation tests.
- **Backup/restore check:** The document names what must be backed up, how restore is verified, and what artifact/chunk/link consistency checks run after restore.
- **Rollback dry read:** Walk the rollback section and confirm legacy tasks/API docs continue to have a named fallback.
- **Deprecation check:** Scratch-file deprecation has objective criteria, not only "when stable."

## Dependencies

- **T002:** artifact/version/link model must exist.
- **T003:** mutation retry behavior informs rollback and migration.

## Provides to downstream tasks

- **T005/T007/T008:** supplies limits, metrics, and migration assumptions.
- **T014:** becomes the checklist for final rollout validation.
