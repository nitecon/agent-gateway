# T019 - Extract artifact test fixture helpers

**Team:** qa
**Phase:** 2 (cleanup)
**Depends on:** T007, T008
**Status:** todo

## Scope

**In:** Refactor repeated fixture/setup patterns across artifact-substrate tests in `crates/gateway/src/db.rs` (and `routes.rs` if T007/T008 added route-level tests with similar setup) into reusable helpers. Touch surface: test modules only; no production code changes.

**Out:** New test cases. New assertions. Coverage changes. This is a pure DRY/maintainability refactor; if a test currently passes, it must still pass with the same semantics after this task.

## Rationale (from wave-1 DRY check)

The DRY peers noted that 11+ test functions in T005 repeat 3-step setup boilerplate (`test_conn`, `test_project`, then per-entity insertion). Once T006 (repository CRUD), T007 (HTTP routes), and T008 (regression coverage) land, the duplication will multiply and obscure what each test actually exercises. Extracting helpers like `seed_artifact_with_version`, `seed_workflow_run`, and `seed_chunk_chain` after the surface stabilizes (post-T007) is cheaper and lower-risk than doing it during active development.

## Deliverables

1. Identify the top 3-5 most-repeated setup sequences across the artifact-substrate test surface (db + routes).
2. Extract each into a named helper function in a `mod test_fixtures` (or equivalent) inside the test module(s). Helpers should:
   - Take only the parameters the call site genuinely varies.
   - Return the inserted row's id (or full struct when callers need it).
   - Be documented with a one-line `///` comment naming the invariant they establish.
3. Replace call sites with the new helpers. Aim for >50% reduction in the number of raw `INSERT INTO artifact_*` statements inside test bodies.

## Acceptance criteria

- [ ] All artifact-substrate tests still pass: `cargo test -p gateway` returns the same pass count as immediately before T019.
- [ ] At least 3 named test fixture helpers exist and are used by at least 2 tests each.
- [ ] No test body contains a 3+ line setup block that has an exact duplicate elsewhere.
- [ ] `cargo clippy -p gateway --all-targets -- -D warnings` is clean.
- [ ] PR diff is restricted to `#[cfg(test)]` modules; no production code touched.

## Validation plan

- `cargo test -p gateway` — pass count unchanged.
- Manual diff review: confirm the helpers genuinely simplify call sites rather than just relocating boilerplate.

## Provides to downstream tasks

- Lower-friction test additions for T009 (spec workflow), T010 (design review), T011 (docs), T014 (rollout validation).
