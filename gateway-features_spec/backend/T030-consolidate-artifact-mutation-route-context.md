# T030 - Consolidate artifact mutation route context

**Origin**: DRY follow-up from wave 6 (T007/T029), with agreement between refactor-proposals and semantic-duplication lenses.

## Problem

T007 introduced the generic artifact API and necessarily added several mutating route handlers at once. Those handlers now repeat the same outer route mechanics:

- artifact API and body-schema feature gating
- mutation context parsing from actor and idempotency headers
- actor resolution
- `spawn_blocking` execution and DB error mapping
- provenance response construction
- replay-aware HTTP status handling
- T004 metric result labels
- workflow-run retryability checks, with link creation carrying a project-scoped inline variant

The duplication is manageable for the generic route slice, but T009/T010/T011 will add specialized spec, review, and docs workflows on top of the same artifact substrate. Leaving the route context shape implicit will make those tasks copy partial variants.

## Scope

**In:**

1. Extract a route-local artifact mutation execution/context helper near `ArtifactMutationContext`.
2. Make actor/idempotency parsing the canonical artifact mutation context boundary for artifact routes.
3. Consolidate workflow-run retryability validation so project-scoped link creation and artifact-scoped mutations share cancelled, non-resumable failed, and not-found semantics while preserving their different ownership contexts.
4. Keep endpoint-specific request validation and DB write calls local to each handler.
5. Preserve existing response bodies, provenance fields, replay status behavior, metrics, and tests.

**Out:**

- Forcing legacy non-artifact routes that only need `X-Agent-Id` to adopt the stricter artifact mutation header contract.
- Moving DB repository behavior into route helpers.
- Reworking README route tables or generating docs from the router.

## Acceptance Criteria

- [ ] A shared route-local helper handles common artifact mutation scaffolding without hiding endpoint-specific validation or DB write semantics.
- [ ] Artifact mutation handlers no longer duplicate feature-gate, mutation-context parse, actor-resolution, spawn-blocking/error-map, provenance, replay-status, and metric-result scaffolding.
- [ ] `create_artifact_link_handler` no longer open-codes workflow-run retryability checks; link workflow-run validation shares canonical cancelled/non-resumable/not-found semantics with artifact-scoped mutations.
- [ ] Existing T007 route tests pass unchanged or with only assertion updates needed by helper extraction.
- [ ] At least one focused regression proves replayed mutations still return the expected HTTP status, `provenance.replayed`, generated resource IDs, and metric result behavior after consolidation.
- [ ] `cargo test -p gateway routes::tests` passes.
- [ ] `cargo test -p gateway` passes.
- [ ] `cargo clippy -p gateway --all-targets -- -D warnings` is clean.

## Touch Surface

- `crates/gateway/src/routes.rs`

## Notes

Keep this as a route-local consolidation. The peers explicitly warned against a broad generic artifact abstraction; the value is a stable mutation envelope before specialized workflow routes land.
