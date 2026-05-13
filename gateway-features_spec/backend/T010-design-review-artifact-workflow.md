# T010 - Implement design review artifact workflow

**Team:** backend
**Phase:** 3
**Depends on:** T007, T008
**Status:** todo

## Scope

**In:** Implement design-review-specific behavior for two-pass peer reviews, synthesis, review rounds, contribution provenance, and analyst retrieval.

**Out:** Rewriting the `/design-review` skill. This task creates gateway primitives and endpoints that the skill can later use.

## Source references

- `gateway-features.md` section "Feature 1: Design Review Artifacts"
- Prior memory: minimal review provenance contract should decide whether explicit workflow_run/review_round records are required.
- Prior memory: workflow run/activity should carry run identity, phase, source artifact version, deterministic read set, idempotency key, and generated outputs.

## Deliverables

1. **Design review workflow routes/repository functions** for creating review artifacts, adding rounds, recording pass contributions, and adding synthesis.
2. **Read APIs** for analysts to fetch contributions by artifact, round, phase, actor role, reviewed version, and read set.
3. **Tests** for two-pass and synthesis provenance.

## Implementation notes

- If review rounds are explicit child records, keep them aligned with the workflow run/activity contract from T002/T003.
- Pass 2 contributions must make clear what pass 1 outputs were read. Use prior contribution IDs or a deterministic read-set rule.
- States like collecting_reviews and synthesizing should not overwrite current/accepted artifact version semantics.

## Acceptance criteria

- [ ] Design review workflow can create review artifacts, record review rounds, pass 1 contributions, pass 2 contributions, and synthesis contributions.
- [ ] Pass 2 and synthesis records preserve deterministic read-set or prior contribution IDs.
- [ ] Workflow state supports draft, collecting reviews, synthesizing, needs user decision, accepted, and superseded without corrupting artifact version state.
- [ ] Research analyst retrieval can fetch all contributions by artifact ID, round, phase, actor role, and reviewed version.
- [ ] Tests cover two-pass contribution provenance and synthesis creating a new artifact version.

## Validation plan

- **Two-pass fixture:** Create one review with three pass 1 contributions, three pass 2 contributions, and one synthesis; verify read-set/provenance queries.
- **State test:** Move through review states and confirm current/accepted versions remain independently queryable.
- **Full tests:** Run `cargo test -p gateway`.

## Dependencies

- **T007:** generic artifact API.
- **T008:** substrate regression coverage.

## Provides to downstream tasks

- **T012:** review round UI.
- **T013:** agent API context for review workflows.
- **T014:** migration validation for design-review scratch-file replacement.
