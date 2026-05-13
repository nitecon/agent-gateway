# T011 - Integrate project documentation artifacts and chunks

**Team:** backend
**Phase:** 3
**Depends on:** T001, T007, T008
**Status:** todo

## Scope

**In:** Implement the documentation workflow chosen by T001, including immutable source references for chunks, search behavior, compatibility with existing API docs, and links to specs/tasks/patterns/repo refs.

**Out:** Full external CLI migration. This task keeps gateway endpoints and behavior compatible for clients.

## Source references

- `gateway-features.md` sections "Feature 3: Project Documentation" and "Search And Retrieval"
- Existing `ApiDoc` DB and route code in `crates/gateway/src/db.rs` and `crates/gateway/src/routes.rs`
- T001 API docs integration decision
- `docs/artifact-operations-rollout.md` from T004
- T005 artifact chunk schema and models

## Deliverables

1. **Gateway docs implementation** following T001.
2. **Chunk source immutability** so chunks point to immutable artifact/docs versions and migrate from existing generated API-doc chunks.
3. **Compatibility behavior** for existing API docs endpoints and agent retrieval.
4. **Tests** for docs search, chunk source references, and links.

## Implementation notes

- The important distinction remains: memory stores distilled reusable lessons; docs store durable project knowledge agents and humans can inspect.
- Queries should say whether they search only current versions or include history.
- T001 chose artifact-backed API docs. Preserve the legacy `api_docs.kind` metadata and `/api-docs?kind=` / `agent-tools docs list --kind` filter semantics separately from artifact `kind = "documentation"` and `subkind = "api_context"`. New responses may add `artifact_id`, `artifact_version_id`, and `subkind`, but must not break legacy fields.
- Reuse the T005 chunk table/model for documentation chunks; do not create a parallel API-doc chunk store.
- Apply T004 documentation-specific operations behavior: API context artifacts retain bodies permanently by default, chunk failures surface as partial retrieval instead of silent success, and restore validation can compare `manifest.chunk_count` to regenerated chunks.

## Acceptance criteria

- [ ] Documentation publishing follows the T001 decision and writes artifact-backed docs with API-doc compatibility behavior.
- [ ] Chunks reference immutable artifact or docs versions and expose whether retrieval searches current versions only or history.
- [ ] Existing generated API-doc chunks migrate to artifact-backed chunks anchored by immutable `artifact_version_id` plus stable `child_address`, with tests for current-version-only and history-aware retrieval.
- [ ] API context documentation artifacts carry the T004 `retain:permanent` behavior unless an explicit operator override is documented.
- [ ] Documentation retrieval exposes exact `chunking_status=partial` behavior for stale or failed chunks and includes `manifest.chunk_count` in structured payloads for restore validation.
- [ ] Docs workflow implements the T004 Phase 3 API-doc/read-source migration and rollback behavior without breaking legacy API-doc reads.
- [ ] Existing API docs endpoints continue working during migration or return documented compatibility redirects/aliases.
- [ ] Legacy `api_docs.kind` remains a response/filter field with current semantics; it is not collapsed into artifact `kind = "documentation"` or `subkind = "api_context"`.
- [ ] Docs can link to specs, tasks, patterns, and source repository refs.
- [ ] Tests cover chunk source immutability, docs search behavior, and compatibility for existing `api-docs` handlers.

## Validation plan

- **Compatibility tests:** Existing API docs route tests still pass or are updated with documented compatibility behavior.
- **Chunk immutability test:** Update a doc and verify old chunks still identify their original immutable source version.
- **Chunk migration test:** Migrate or republish an existing API doc and verify legacy chunks become artifact-backed chunks with stable child addresses.
- **Kind filter test:** Verify `/api-docs?kind=` and `agent-tools docs list --kind` keep legacy filter behavior while artifact fields are also present.
- **Search test:** Verify docs search can find content and linked resource IDs.
- **Operations check:** Verify retained API context artifacts, partial chunk status, and `manifest.chunk_count` behavior match T004.

## Dependencies

- **T001:** documentation integration decision.
- **T007:** generic artifact API.
- **T008:** substrate regression coverage.

## Provides to downstream tasks

- **T012:** documentation browser UI.
- **T013:** published agent API context.
- **T014:** docs migration/rollback validation.
