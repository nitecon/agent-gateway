# T027 - Split or harden ArtifactUpdate pointer fields

**Team:** backend
**Phase:** 2
**Depends on:** T006
**Promoted from:** T006 semantic review

## Scope

**In:** Resolve the unsafe generic pointer update surface in `ArtifactUpdate` / `update_artifact` before T007 exposes update routes. Preferred shape: public metadata/state `ArtifactUpdate` excludes `current_version_id` and `accepted_version_id`; dedicated pointer helpers validate version membership and state-transition semantics. Alternative: keep fields but validate supplied version IDs belong to the same artifact/project before update.

**Out:** HTTP route implementation.

## Acceptance criteria

- [ ] Generic artifact metadata/state updates cannot create dangling or cross-artifact `current_version_id` / `accepted_version_id` pointers.
- [ ] Dedicated pointer mutation path validates version membership and keeps existing create/accept semantics intact.
- [ ] Tests cover rejected missing/cross-artifact pointer updates and successful valid pointer updates through the dedicated path if one remains public.
- [ ] `cargo test -p gateway db::tests` passes.
- [ ] `cargo clippy -p gateway --all-targets -- -D warnings` is clean.

## Validation plan

- `cargo test -p gateway db::tests`
- `cargo test -p gateway`
- `cargo clippy -p gateway --all-targets -- -D warnings`

## Provides to downstream tasks

- **T007:** generic artifact update routes can expose only safe metadata/state mutation semantics.
