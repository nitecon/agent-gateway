# T023 - Introduce DB-local artifact query builder

**Team:** backend
**Phase:** 2
**Depends on:** T006
**Promoted from:** T006 DRY check

## Scope

**In:** Add a narrow DB-local helper for artifact repository list/search dynamic SQL. Cover optional trimmed exact filters, LIKE filters, placeholder numbering, params collection, and `query_map` collection. Migrate `list_artifacts`, `list_artifact_links`, and `list_artifact_chunks` first.

**Out:** Route-layer API design, a generic ORM abstraction, or changing query semantics.

## Acceptance criteria

- [ ] `list_artifacts`, `list_artifact_links`, and `list_artifact_chunks` use the shared DB-local query helper.
- [ ] Existing filter/search behavior and ordering remain unchanged.
- [ ] `cargo test -p gateway db::tests` passes.
- [ ] `cargo clippy -p gateway --all-targets -- -D warnings` is clean.
- [ ] Diff is restricted to `crates/gateway/src/db.rs` and spec/manifest metadata.

## Validation plan

- `cargo test -p gateway db::tests`
- `cargo test -p gateway`
- `cargo clippy -p gateway --all-targets -- -D warnings`

## Provides to downstream tasks

- **T007:** route query params can compose list filters without multiplying placeholder/bind boilerplate.
