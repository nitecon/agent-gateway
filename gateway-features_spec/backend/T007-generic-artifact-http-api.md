# T007 - Expose generic artifact HTTP API

**Team:** backend
**Phase:** 2
**Depends on:** T006
**Status:** todo

## Scope

**In:** Add generic artifact HTTP endpoints and request validation for list/create/get/version/diff/comment/contribution/link operations.

**Out:** Specialized review/spec/docs workflow endpoints, except where thin generic routes are required for them later.

## Source references

- Existing route patterns in `crates/gateway/src/routes.rs`
- Existing app route wiring in `crates/gateway/src/main.rs`
- `docs/workflow-mutation-contract.md` from T003
- `docs/artifact-operations-rollout.md` from T004

## Deliverables

1. **Route handlers in `crates/gateway/src/routes.rs`** for generic artifact operations under the chosen route family.
2. **Route wiring in `crates/gateway/src/main.rs`** under `/v1/projects/:ident/artifacts`.
3. **README API reference update** documenting endpoints and contracts.
4. **Route tests** for validation, idempotency, and happy paths.

## Implementation notes

- Existing endpoints require bearer auth at the app layer; artifact handlers should still validate project and actor/provenance fields per T003.
- Preferred route shape is `/v1/projects/:ident/artifacts` with nested `/versions`, `/comments`, `/contributions`, and `/links`; any deviation needs to be justified in the README/API context.
- Diff can be generated on read from full version bodies for v1.
- Mutation routes must honor T003's resumable-run rule: retrying a resumable failed workflow run with the same idempotency scope can complete missing sub-steps and return a successful provenance envelope; cancelled runs remain non-recoverable.
- Size limit and quota errors should be stable and testable via T016 helpers and T004 fixture values.
- Read responses should expose chunk freshness and `chunking_status`, including partial status when chunks are stale or failed.
- Keep rollback feature flags explicit so clients can return to legacy task/docs behavior during rollout.

## Acceptance criteria

- [ ] Route family is explicitly named, starting from `/v1/projects/:ident/artifacts` with nested version, comment, contribution, and link routes unless T002/T003 justify a different shape.
- [ ] Routes expose artifact list, create, get, versions, version create, diff, comments, contributions, and links operations.
- [ ] Every mutating endpoint validates authorization boundary, idempotency key requirements, actor fields, immutable source references, size limits, and T004 soft/hard quota behavior.
- [ ] Routes allow retrying declared resumable mutations on a failed workflow_run and reject retries on cancelled or non-resumable failed runs according to T003.
- [ ] Responses and read models expose T004 freshness/chunking status fields, including partial chunk status where chunks failed or are stale.
- [ ] Routes emit the T004 metrics surface for writes, diffs, search, chunking, failed chunks, stale chunks, quota warnings, and quota rejects.
- [ ] Rollback feature flags can disable artifact endpoints or body-schema-dependent behavior while preserving documented legacy task/docs fallback behavior.
- [ ] Responses include stable artifact, version, contribution, comment, link, actor, and workflow run IDs where applicable.
- [ ] Route tests cover validation failures, happy paths, idempotent retry, and missing immutable source version failures.
- [ ] README API reference includes the generic artifact endpoints and v1 contract summary.

## Validation plan

- **Route tests:** Run `cargo test -p gateway routes::tests`.
- **Full gateway tests:** Run `cargo test -p gateway`.
- **Manual API smoke:** Use curl against a local gateway to create an artifact, add a version, add a comment, and retrieve a diff.
- **Operations check:** Verify size/quota responses, metrics labels, chunk freshness fields, and rollback flags map back to T004.

## Dependencies

- **T006:** repository functions.

## Provides to downstream tasks

- **T008:** regression tests finalize substrate coverage.
- **T009/T010/T011:** specialized workflows build on these APIs.
- **T012/T013:** UI and agent docs describe these APIs.
