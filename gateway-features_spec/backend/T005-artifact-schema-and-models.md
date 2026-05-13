# T005 - Add artifact schema migrations and Rust models

**Team:** backend
**Phase:** 2
**Depends on:** T002, T003, T004
**Status:** todo

## Scope

**In:** Add SQLite schema and Rust data models for the shared artifact substrate.

**Out:** HTTP route handlers and specialized spec/review/docs workflows. Those use the schema after it exists.

## Source references

- `docs/artifact-substrate-v1.md` from T002
- `docs/workflow-mutation-contract.md` from T003
- `docs/artifact-operations-rollout.md` from T004
- Existing schema patterns in `crates/gateway/src/db.rs` `apply_schema`

## Deliverables

1. **`crates/gateway/src/db.rs` schema migration** creating artifact-related tables.
2. **Contract-to-table traceability matrix** mapping T002/T003/T004 decisions to tables, columns, indexes, route validations, and tests.
3. **Rust structs** for artifact summaries/details, versions, chunks, contributions, comments, links, actors, workflow runs/activities, and insert/update inputs.
4. **DB unit tests** for schema invariants.

## Implementation notes

- Follow the existing style in `apply_schema`: additive migrations guarded by `CREATE TABLE IF NOT EXISTS`, explicit indexes, and compatibility with existing SQLite databases.
- Version bodies must be immutable after creation. Use API and DB constraints where practical.
- Include artifact-backed chunks in the substrate schema so T001/T011 can move existing API-doc chunks without a second storage path. The final T002-approved table name may differ, but the model must store immutable `artifact_version_id`, stable `child_address`/path, chunk text/content, metadata, retrieval filters such as app/label/kind where needed, and indexes for current-version retrieval and search.
- Include `superseded_by_chunk_id` or equivalent soft-supersession on artifact chunks so historical retrieval reconstruction remains possible after re-chunking.
- Include nullable `child_address`/selector support on artifact comments for `artifact_version` targets, matching T002's v1 child-addressed comment contract.
- Idempotency uniqueness should align with T003, not be ad hoc per endpoint.
- Workflow run state constraints must support T003's resumable-run rule: declared resumable workflow kinds can transition `failed` -> `succeeded` when a retry with the same idempotency scope completes missing generated resources; `cancelled` remains terminal.
- The traceability matrix can live in comments near the migration or in `docs/artifact-substrate-v1.md`, but it must be updated with the implementation.

## Acceptance criteria

- [ ] Implementation includes a contract-to-table traceability matrix mapping T002/T003/T004 decisions to tables, columns, indexes, routes, and tests.
- [ ] SQLite migration creates tables for artifacts, artifact_versions, artifact_chunks or the T002-approved chunk table, artifact_contributions, artifact_comments, artifact_links, artifact_actors, and workflow_runs or activities.
- [ ] Schema enforces immutable versions and idempotency uniqueness for workflow mutations.
- [ ] Schema stores current and accepted version references without collapsing them into lifecycle state.
- [ ] Rust structs cover summary/detail models needed by routes and UI without leaking raw SQL rows.
- [ ] Chunk schema and models cover immutable version anchoring, stable child addresses, stale/current-version query semantics, and search/index fields needed by documentation retrieval.
- [ ] Chunk schema includes soft-supersession (`superseded_by_chunk_id` or equivalent) and tests prove historical retrieval can reconstruct superseded chunks.
- [ ] Comment schema supports nullable child addresses/selectors for artifact-version comments, with a test that anchors a comment to `manifest.items[<manifest_item_id>]`.
- [ ] Workflow run schema/tests allow resumable fan-out workflow kinds to recover from `failed` to `succeeded` under the same idempotency scope while keeping `cancelled` terminal.
- [ ] DB unit tests cover insert, version immutability, accepted/current version behavior, link uniqueness, and workflow idempotency.

## Validation plan

- **Unit tests:** Run `cargo test -p gateway db::tests`.
- **Traceability review:** Every required contract field has a storage location, index/constraint where needed, and a test or route validation reference.
- **Migration test:** Open a fresh test DB and an existing fixture-style DB, then apply schema without losing task/API-doc tables.
- **Constraint check:** Add tests that attempt duplicate idempotency keys and mutation of immutable version fields.

## Dependencies

- **T002:** resource contract.
- **T003:** idempotency and permission contract.
- **T004:** operations limits and migration assumptions.

## Provides to downstream tasks

- **T006:** repository functions over the schema.
- **T008:** regression tests extend schema coverage.
