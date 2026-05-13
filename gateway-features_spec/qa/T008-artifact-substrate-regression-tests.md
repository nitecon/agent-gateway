# T008 - Add artifact substrate regression tests

**Team:** qa
**Phase:** 2
**Depends on:** T004, T005, T006, T007
**Status:** todo

## Scope

**In:** Add and organize regression coverage for core artifact behavior across DB and route layers.

**Out:** Specialized workflow acceptance tests for spec/design-review/docs, except shared substrate behaviors they depend on.

## Source references

- Existing tests in `crates/gateway/src/db.rs` and `crates/gateway/src/routes.rs`
- T005, T006, and T007 implementations
- `docs/artifact-operations-rollout.md` from T004

## Deliverables

1. **DB tests** for schema/repository invariants.
2. **Route tests** for API validation and response shape.
3. **Migration compatibility tests** that keep existing tasks and API docs behavior intact.

## Implementation notes

- The repo currently uses Rust unit tests in the same source files. Match that pattern unless a broader test harness already exists by implementation time.
- Include negative tests for limits, quotas, stale/partial chunks, and immutable source references; these are the most likely regression surfaces.
- Use the concrete limits, quotas, and retention semantics documented in T004 instead of inventing independent thresholds in tests.

## Acceptance criteria

- [ ] `cargo test -p gateway` includes coverage for artifact lifecycle, immutable versions, accepted/current version divergence, comments, links, and workflow provenance.
- [ ] Tests verify body and contribution size limit failures return stable client errors using fixture values from T004.
- [ ] Tests verify T004 quota soft warnings and hard rejects using the documented shrunken fixture values.
- [ ] Tests verify stale chunks and partial chunking failures surface stable status fields for route and documentation retrieval consumers.
- [ ] Tests verify retrying a mutation with the same idempotency key returns or references the original generated resource.
- [ ] Tests verify search and retrieval can find artifacts by body text, contribution text, and linked task/doc/pattern IDs.
- [ ] Tests include at least one migration compatibility check for existing task and API doc tables.

## Validation plan

- **Automated:** Run `cargo test -p gateway`.
- **Operations fixture check:** Tests cite or load the same limit values documented by T004.
- **Quota fixture check:** Tests exercise both soft warning and hard reject fixture paths from T004.
- **Chunk freshness check:** Tests cover stale and partial chunking states exposed by T007/T011.
- **Coverage review:** Compare tests against the acceptance list and T002/T003 invariants.
- **Regression target:** Confirm task and API-doc tests that existed before artifact work still pass.

## Dependencies

- **T005:** schema and models.
- **T006:** repository functions.
- **T007:** generic HTTP API.

## Provides to downstream tasks

- **T009/T010/T011:** confidence that specialized workflows are building on stable substrate behavior.
- **T014:** baseline for final rollout validation.
