# T022 - Propagate restore-check collector row errors

**Team:** backend
**Phase:** 2
**Depends on:** T021
**Promoted from:** T021 DRY check

## Scope

**In:** Change `RestoreFindingCollector::collect` in `crates/gateway/src/db.rs` so `rusqlite` row-mapping errors from `query_map` propagate instead of being silently dropped by `filter_map(|r| r.ok())`.

**Out:** Broadening `RestoreFindingCollector` into a repository abstraction; changing restore-check report shape; changing restore-check domain semantics.

## Implementation notes

- Prefer collecting `query_map` output into a `rusqlite::Result<Vec<R>>` or equivalent fail-fast loop.
- Keep the collector private and restore-check-local.
- `T006` should continue to use existing `row_to_*` fail-fast mapper style for repository row mappers, not this collector.

## Acceptance criteria

- [ ] `RestoreFindingCollector::collect` propagates row mapper errors.
- [ ] `RestoreCheckReport` shape and `run_restore_check` signature are unchanged.
- [ ] Existing restore-check tests pass unmodified.
- [ ] Add or adjust one focused DB test if practical to prove row mapper errors are not swallowed; if impractical, explain why in the task comment.
- [ ] `cargo test -p gateway` passes.
- [ ] `cargo clippy -p gateway --all-targets -- -D warnings` is clean.
- [ ] PR diff is restricted to `crates/gateway/src/db.rs` and this spec/manifest metadata.

## Validation plan

- `cargo test -p gateway`
- `cargo clippy -p gateway --all-targets -- -D warnings`

## Provides to downstream tasks

- **T006:** preserves the fail-fast row-mapping precedent before artifact repository functions add more mapper surface.
