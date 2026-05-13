# T021 - Consolidate restore-check helpers via RestoreFindingCollector

**Team:** backend
**Phase:** 2
**Depends on:** T016
**Status:** todo
**Promoted from:** wave-2 DRY check (refactor + semantic lenses both converged on this as the highest-ROI consolidation)

## Scope

**In:** Replace the five `restore_check_*` helpers (`artifact_pointers`, `audit_links`, `workflow_runs`, `idempotency_mappings`, `chunks`) with a shared `RestoreFindingCollector` (trait, builder, or closure-based helper) that captures the common pattern: prepare statement → query → filter to inconsistent rows → emit `RestoreFinding`. Apply before T006/T014 wire restore-check into routes or rollout validation.

**Out:** Changing the semantics of any specific restore-check pass; adding new restore-check categories; the `manifest.chunk_count` validation deferred to T011.

## Implementation notes

- The peers proposed `RestoreFindingCollector` shape: each helper supplies (a) a SQL query, (b) a row-to-finding mapper, (c) optional config (e.g. retention for workflow runs). The collector drives iteration and `RestoreFinding` construction.
- `run_restore_check` should still be the public driver consumed by T014; it can either iterate over a collection of collectors or call the individual helpers if they remain as thin wrappers.
- Preserve the no-auto-repair invariant — collectors REPORT only.
- `restore_check_workflow_runs` has both stuck-run detection and generated-id integrity; that may justify either two collectors or a slightly richer interface. Pick whichever keeps total LOC lowest while preserving readability.

## Acceptance criteria

- [ ] One shared collector abstraction exists; the five restore-check helpers either delegate to it or are deleted in favor of inline registration.
- [ ] `RestoreCheckReport` shape and `run_restore_check` public signature are unchanged.
- [ ] All T016 restore-check tests (`restore_check_reports_pointer_mismatch_without_repair`, `restore_check_reports_stuck_workflow_run`, `restore_check_report_is_clean_on_pristine_db`) still pass unmodified.
- [ ] Net LOC reduction of at least 60 lines across the five helpers (peer estimate was ~100+).
- [ ] `cargo clippy -p gateway --all-targets -- -D warnings` is clean.
- [ ] PR diff is restricted to `crates/gateway/src/db.rs`.

## Validation plan

- `cargo test -p gateway` — full suite passes.
- Visual diff of `RestoreCheckReport` JSON for a fixture with seeded inconsistencies before/after refactor — must be byte-identical.
- `cargo clippy -p gateway --all-targets -- -D warnings`.

## Provides to downstream tasks

- **T006:** repository functions can add new restore-check passes by registering a collector rather than duplicating the loop.
- **T014:** rollout validation continues to call the unchanged `run_restore_check` driver.
