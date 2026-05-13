# T028 - Define contribution/comment target and read_set validation boundary

**Team:** backend
**Phase:** 2
**Depends on:** T006
**Promoted from:** T006 semantic review

## Scope

**In:** Define and implement or explicitly delegate validation for contribution/comment `target_kind` + `target_id` and `read_set` references. T003 requires these references to resolve. Either add repository validation helpers used by `add_artifact_contribution`, `add_artifact_comment`, and workflows, or document that T007 route validation owns this and add route-negative-test requirements before public exposure.

**Out:** Full workflow implementation for specs/reviews/docs.

## Acceptance criteria

- [ ] Boundary is explicit in code/spec comments: repository-level validators or route-level validation contract.
- [ ] Missing and cross-artifact `target_kind` / `target_id` references cannot be inserted through the chosen public write path.
- [ ] `read_set` reference resolution is validated for artifact version, manifest item/chunk/comment/workflow-run references that T006/T007 can reasonably resolve.
- [ ] Deferred reference kinds are documented with owner task.
- [ ] Tests cover missing target, cross-artifact target, unresolved `read_set` ref, and valid refs.
- [ ] `cargo test -p gateway db::tests` or route tests pass, depending on chosen layer.
- [ ] `cargo clippy -p gateway --all-targets -- -D warnings` is clean.

## Validation plan

- `cargo test -p gateway db::tests`
- `cargo test -p gateway`
- `cargo clippy -p gateway --all-targets -- -D warnings`

## Provides to downstream tasks

- **T007/T009/T010:** public routes and workflow writers share a clear validation boundary for provenance and comments.
