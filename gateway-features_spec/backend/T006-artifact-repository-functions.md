# T006 - Implement artifact repository functions

**Team:** backend
**Phase:** 2
**Depends on:** T016
**Status:** todo

## Scope

**In:** Add DB-layer functions for creating, retrieving, searching, linking, commenting, and recording workflow activity for artifacts.

**Out:** HTTP request/response validation and UI rendering. Keep this task focused on repository behavior.

## Source references

- `crates/gateway/src/db.rs` existing task, pattern, and API-doc repository functions
- `docs/artifact-substrate-v1.md` from T002
- `docs/workflow-mutation-contract.md` from T003
- `docs/artifact-operations-rollout.md` from T004
- T016 operations-envelope runtime helpers

## Deliverables

1. **DB functions in `crates/gateway/src/db.rs`** for artifact CRUD/read workflows.
2. **Row mapping helpers** equivalent to existing `row_to_task`, `row_to_pattern`, and `row_to_api_doc` patterns.
3. **Repository tests** for filtering, search, provenance, and idempotency behavior.

## Implementation notes

- Prefer explicit typed insert/update structs over route-layer SQL construction.
- Keep search behavior predictable. At minimum, search title/body/contribution text and linked resource IDs.
- Link creation should follow uniqueness and supersession rules from T002/T003.
- Use T016 typed operations-envelope helpers for quota errors, purge entry points, and restore-check queries instead of duplicating T004 policy inside repository functions.

## Acceptance criteria

- [ ] DB layer can create/list/get artifacts by project, kind, status, label, actor, and search query.
- [ ] DB layer can create immutable versions and retrieve version history and version bodies.
- [ ] DB layer can add contributions, comments, links, and workflow run records with actor provenance.
- [ ] DB layer exposes search over artifact body, contribution text, linked task ID, linked pattern ID, and linked doc ID.
- [ ] DB layer consumes typed operations-envelope errors and helper queries from T016 rather than reimplementing T004 limit, quota, purge, or restore-check logic ad hoc.
- [ ] Repository tests cover all public artifact DB functions and at least one retry/idempotency path.

## Validation plan

- **Unit tests:** Run `cargo test -p gateway db::tests`.
- **Filter matrix:** Tests should exercise every list filter independently and in at least one combination.
- **Search source check:** Insert records where only a contribution or link matches and verify search returns the parent artifact.

## Dependencies

- **T005:** schema and structs.
- **T016:** operations-envelope runtime helpers.

## Provides to downstream tasks

- **T007:** route handlers call these functions.
- **T009/T010/T011:** workflow handlers compose these functions.
- **T012:** UI depends on list/detail query shape.
