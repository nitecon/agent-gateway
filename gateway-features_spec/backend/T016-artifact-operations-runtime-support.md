# T016 - Wire artifact operations envelope runtime support

**Team:** backend
**Phase:** 2
**Depends on:** T004, T005
**Status:** todo

## Scope

**In:** Centralize the runtime support needed to enforce the operations envelope from T004 after the artifact schema exists.

**Out:** HTTP endpoint implementation, specialized workflow behavior, and final rollout validation.

## Source references

- `docs/artifact-operations-rollout.md` from T004
- `docs/artifact-substrate-v1.md` from T002
- `docs/workflow-mutation-contract.md` from T003
- T005 schema and model implementation

## Deliverables

1. **Typed operations envelope** for size, quota, retention, purge, and restore-check configuration.
2. **Shared error and warning helpers** for size rejects, quota warnings, and quota rejects.
3. **Repository/runtime helpers** for archived body purge and restore verification queries.
4. **Unit tests** for defaults and T008 shrunken fixture values.

## Implementation notes

- Load and normalize the T004 environment keys once. Route handlers and tests should consume typed values instead of copying constants.
- Keep soft quota warnings distinct from hard quota rejects so T007 can return provenance warnings without writing rows after hard-limit failures.
- Purge helpers should preserve immutable version metadata, audit-path links, comments, workflow runs, and idempotency mappings exactly as T004 requires.
- Restore-check helpers should report inconsistencies without silently repairing them.

## Acceptance criteria

- [ ] Runtime loads and normalizes the T004 size, quota, retention, purge, and restore-check configuration keys once through a typed operations envelope.
- [ ] Shared helpers expose stable typed errors for size-limit and quota failures, including soft quota warnings and hard quota rejects.
- [ ] Repository/runtime layer exposes archived body and structured-payload purge entry points matching the T004 retention policy.
- [ ] Repository/runtime layer exposes restore-check helper queries for artifact pointer consistency, audit-path links, workflow run consistency, idempotency mappings, and chunk regeneration validation.
- [ ] Unit tests cover default values and shrunken T008 fixture values without duplicating T004 constants in route code.

## Validation plan

- **Unit tests:** Run the focused gateway tests for operations-envelope parsing and helper behavior.
- **Fixture check:** Set the T008 shrunken fixture environment values and verify parsed limits and quotas match T004.
- **Purge dry test:** Insert archived artifact versions and verify body bytes are purged while audit metadata remains intact.
- **Restore helper test:** Insert inconsistent pointers/links/workflow rows and verify helpers report stable, actionable findings.

## Dependencies

- **T004:** defines the operations envelope.
- **T005:** provides artifact schema and Rust models.

## Provides to downstream tasks

- **T006:** repository functions use shared helpers instead of reimplementing operations policy.
- **T007:** routes map typed errors and warnings to stable HTTP responses and provenance.
- **T008:** tests reuse fixture values and helper behavior.
- **T014:** rollout validation consumes restore-check helpers.
