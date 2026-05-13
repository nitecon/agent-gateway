# T024 - Consolidate artifact idempotent write envelopes

**Team:** backend
**Phase:** 2
**Depends on:** T006
**Promoted from:** T006 DRY and semantic review

## Scope

**In:** Consolidate artifact repository idempotent write envelope behavior while defining guarded replay side effects. Cover `create_artifact_version` `current_version_id` repair semantics, replay warning behavior, per-operation validation hooks, insert/reload result shape, and stale replay regression tests.

**Out:** HTTP route handlers and workflow orchestration.

## Acceptance criteria

- [ ] Idempotent replay returns the original generated resource without rerunning aggregate side effects unless a guarded repair predicate is satisfied.
- [ ] `create_artifact_version` preserves T003 partial-failure repair intent but cannot regress `current_version_id` after a newer version exists.
- [ ] Repeated `ArtifactWriteResult` replay/insert/reload boilerplate is reduced for version, contribution, comment, link, chunk, and workflow writes where practical.
- [ ] Tests cover partial pointer repair and stale replay non-regression.
- [ ] Replay warning behavior is explicit and tested or documented.
- [ ] `cargo test -p gateway db::tests` passes.
- [ ] `cargo clippy -p gateway --all-targets -- -D warnings` is clean.

## Validation plan

- `cargo test -p gateway db::tests`
- `cargo test -p gateway`
- `cargo clippy -p gateway --all-targets -- -D warnings`

## Provides to downstream tasks

- **T007/T009/T010/T011:** mutation routes and workflows can rely on retry-safe repository write envelopes instead of cloning replay logic.
