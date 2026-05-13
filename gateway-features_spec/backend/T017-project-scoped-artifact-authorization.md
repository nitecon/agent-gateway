# T017 - Implement project-scoped artifact authorization and administer overrides

**Team:** backend
**Phase:** 5
**Depends on:** T007, T012
**Status:** todo

## Scope

**In:** Replace the trusted-single-tenant artifact authorization placeholder with project-scoped membership and scope checks.

**Out:** Changing the v1 trusted-boundary contract retroactively. This task is deferred hardening and should not block the first artifact rollout.

## Source references

- `docs/workflow-mutation-contract.md` from T003
- `docs/artifact-operations-rollout.md` from T004
- Generic artifact API from T007
- Artifact UI behavior from T012

## Deliverables

1. **Authorization enforcement** for artifact reads, version writes, comments, links, accepting versions, task generation, and workflow actions.
2. **Administrator quota override behavior** for T004 soft-warning bypasses.
3. **Stable authorization errors** matching T003 response and provenance shape.
4. **CLI/UI trusted-boundary signals** updated to reflect real project authorization.
5. **Tests** for allowed, forbidden, and administrator-override paths.

## Implementation notes

- Preserve T003 response shape so clients do not need a second error contract for the hardened mode.
- Keep actor/provenance fields present on rejected mutating requests where the actor can be identified safely.
- Do not weaken hard quota limits. Administrators may bypass soft warnings only.
- Treat this task as a blocker for any future multi-user or workspace-ready claim, not for the v1 trusted-single-tenant artifact slice.

## Acceptance criteria

- [ ] Trusted-single-tenant no-op authorization is replaced with project membership and scope checks for artifact reads, writes, comments, links, accepting versions, task generation, and workflow actions.
- [ ] `project.administer` or the chosen equivalent scope is required for T004 quota override behavior.
- [ ] Authorization failures preserve the T003 response shape, actor/provenance fields, and auditability expectations.
- [ ] CLI and UI surfaces expose clear trusted-boundary or authorization-state signals during migration.
- [ ] Tests cover authorized access, forbidden access, quota override allowed for administrators, and quota override rejected for non-administrators.

## Validation plan

- **Route tests:** Exercise allowed and forbidden artifact operations for representative scopes.
- **Quota override tests:** Verify administrators can bypass soft warnings and non-administrators cannot.
- **Compatibility check:** Existing trusted-mode tests either remain valid behind a flag or are updated with documented migration behavior.
- **UI/API context check:** Confirm user-facing and agent-facing docs no longer imply the trusted boundary is the hardened model.

## Dependencies

- **T007:** generic artifact routes and response shapes exist.
- **T012:** UI surfaces exist for trusted-boundary or authorization-state signals.

## Provides to downstream tasks

- Future multi-user or workspace-ready artifact rollout claims.
