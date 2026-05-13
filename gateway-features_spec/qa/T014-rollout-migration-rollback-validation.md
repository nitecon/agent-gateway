# T014 - Validate rollout, migration, and rollback readiness

**Team:** qa
**Phase:** 4
**Depends on:** T009, T010, T011, T012, T013, T015
**Status:** todo

## Scope

**In:** Validate the full feature set against rollout, migration, rollback, observability, and compatibility requirements.

**Out:** Implementing new behavior discovered during validation unless it is a small test fixture correction. File follow-up tasks for missing functionality.

## Source references

- `gateway-features.md` sections "Rollout" and "Success Criteria"
- `docs/artifact-operations-rollout.md` from T004
- T009, T010, T011, T012, and T013 deliverables

## Deliverables

1. **Final validation notes** appended to `docs/artifact-operations-rollout.md` or a linked release checklist.
2. **Automated tests** for migration/compatibility paths where practical.
3. **Manual smoke checklist** for UI and API workflows.
4. **Client/skill handoff validation** proving stable artifact/spec/task IDs are the canonical workflow surface.

## Implementation notes

- This is the task that proves scratch-file workflow replacement is safe enough to roll forward.
- Do not mark scratch-file deprecation ready until T015 has owners or delegated tasks for every external client/skill surface.
- Confirm rollback behavior before deprecating legacy task specification or API docs paths.
- Metrics/logging can be validated by emitted log lines or counters depending on what exists by implementation time.
- Treat T004 section 4.3 restore verification and section 9 deprecation gates as mandatory checklist inputs, not background reference material.

## Acceptance criteria

- [ ] Validation plan exercises importing or linking existing task/spec/doc state and verifies idempotent artifact links to existing task IDs.
- [ ] Dual-read compatibility is tested for legacy task specification and docs behavior during transition.
- [ ] End-to-end validation proves agent-tools and `/design-review`, `/spec`, and `/implement` can use stable artifact/spec/task IDs instead of scratch files as canonical handoff.
- [ ] Rollback procedure is tested or dry-run documented for disabling artifact endpoints or reverting body schema assumptions.
- [ ] T004 section 4.3 restore verification is exercised or dry-run documented, including pointer consistency, audit-path links, workflow runs, idempotency mappings, and chunk regeneration.
- [ ] T004 section 9 deprecation gates are checked explicitly before scratch files, legacy docs behavior, legacy skill modes, or feature flags are removed.
- [ ] Metrics/logging checks cover artifact writes, diffs, search, chunking, failed chunks, and stale chunks.
- [ ] `cargo test -p gateway` passes after the full feature set lands.

## Validation plan

- **Automated:** Run `cargo test -p gateway`.
- **Migration dry run:** Use a database containing existing tasks and API docs, then apply artifact migrations and verify legacy reads still work.
- **Client/skill dry run:** Execute or simulate design-review -> spec -> implement handoff using gateway IDs and confirm scratch files are not canonical state.
- **Rollback dry run:** Follow the documented rollback path and confirm clients can return to existing tasks/API docs behavior.
- **Restore dry run:** Follow the T004 restore checklist and record pass/fail for each consistency check.
- **Deprecation gate check:** Verify every T004 objective gate has evidence before marking legacy behavior removable.
- **UI smoke:** Exercise artifact list/detail, version diff, review round, spec manifest, and docs browser views.

## Dependencies

- **T009:** spec workflow.
- **T010:** design review workflow.
- **T011:** docs workflow.
- **T012:** artifact UI.
- **T013:** agent API context.
- **T015:** client and skill migration plan.

## Provides to downstream tasks

- Release readiness signal for implementation of `gateway-features.md`.
