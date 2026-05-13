# T025 - Extract project-scoped artifact link visibility predicate

**Team:** backend
**Phase:** 2
**Depends on:** T006
**Promoted from:** T006 DRY check

## Scope

**In:** Extract one canonical DB-local predicate/helper for project-scoped artifact link visibility and membership. Use it for link quotas, `list_artifact_links`, and downstream authorization surfaces.

**Behavior:** A link is visible in a project when a source or target artifact belongs to the project, or a source/target version belongs to an artifact in the project. Pure external/discovery links with no artifact/version side are excluded.

**Out:** Full project authorization policy; that remains T017.

## Acceptance criteria

- [ ] Link quota counting and `list_artifact_links` use the same predicate/helper.
- [ ] Tests cover source artifact, target artifact, source version, target version, and external/discovery-only cases.
- [ ] Helper is suitable for T007 link routes and T017 authorization checks.
- [ ] `cargo test -p gateway db::tests` passes.
- [ ] `cargo clippy -p gateway --all-targets -- -D warnings` is clean.

## Validation plan

- `cargo test -p gateway db::tests`
- `cargo test -p gateway`
- `cargo clippy -p gateway --all-targets -- -D warnings`

## Provides to downstream tasks

- **T007:** link routes use one visibility predicate.
- **T017:** authorization checks build on the same membership semantics.
