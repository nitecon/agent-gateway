# T031 - Centralize T004 shrunken artifact operations test fixture

**Team:** qa
**Phase:** 2
**Depends on:** T008
**Status:** todo

## Scope

**In:** Define the shrunken T004 artifact operations limits and quotas once for tests and reuse them from DB envelope tests and T008 route regression tests.

**Out:** Broad route request/assertion helper extraction, unless a later edit reveals additional meaningful duplication.

## Source references

- `docs/artifact-operations-rollout.md` from T004
- `crates/gateway/src/db.rs` T004 operations envelope tests
- `crates/gateway/src/routes.rs` T008 route regression tests
- DRY review tasks:
  - `019e22ab-f9ac-7193-9c7a-45e3ed74ff10`
  - `019e22ab-f9af-7ae3-aff6-9b7b6127b562`

## Deliverables

1. **Shared test fixture source** for the T004 shrunken artifact operations envelope values.
2. **DB test reuse** while preserving `ArtifactOperationsEnvelope::from_env_with` parsing coverage.
3. **Route test reuse** while preserving HTTP-level assertions for stable 413 errors, soft quota warnings, hard quota rejects, and stale/partial chunking status.

## Implementation notes

- Keep endpoint-specific request construction and assertions local to the route tests.
- The goal is preventing fixture drift between DB and route tests, not changing the behavioral coverage layering.
- This task is non-blocking; T009, T010, and T011 can proceed on the current T008 substrate tests.

## Acceptance criteria

- [ ] DB envelope tests and route regression tests derive shrunken T004 limits/quotas from one shared test-only source.
- [ ] DB tests still prove env-style key/value parsing through `from_env_with`.
- [ ] Route tests still assert stable 413 size-limit errors, quota soft warning vs hard reject behavior, and stale/partial chunking status.
- [ ] No broad route helper extraction obscures acceptance-criteria readability.
- [ ] `cargo test -p gateway` passes.
- [ ] `cargo clippy -p gateway --all-targets -- -D warnings` is clean.

## Validation plan

- **Automated:** Run `cargo test -p gateway`.
- **Lint:** Run `cargo clippy -p gateway --all-targets -- -D warnings`.
- **Review:** Confirm the fixture values appear in one canonical test-only definition and endpoint-specific assertions remain readable.

## Dependencies

- **T008:** regression tests introduce the duplicated fixture values.

## Provides to downstream tasks

- Lower drift risk for future operations-envelope and artifact route regression work.
