# T015 - Coordinate client and skill migration to artifact IDs

**Team:** docs
**Phase:** 4
**Depends on:** T009, T010, T011, T013
**Status:** todo

## Scope

**In:** Define and track the client and skill migration needed for `agent-tools` commands and `/design-review`, `/spec`, and `/implement` to use gateway artifact IDs as canonical handoff state.

**Out:** Implementing every external client change directly in this repo unless the implementation owner decides those changes belong here. This task must create delegated work or release-blocking checklist items for external surfaces.

## Source references

- `gateway-features.md` section "CLI And Skill Experience"
- `gateway-features.md` section "Success Criteria"
- Peer review convergence: the gateway API/UI work alone does not satisfy scratch-file replacement unless clients and skills consume stable artifact/spec IDs.

## Deliverables

1. **`docs/artifact-client-skill-migration.md`** - migration plan for CLI commands and skills.
2. **Delegated task records or release checklist entries** for agent-tools and local skill changes if they are outside this repo.
3. **README/API-context updates** that state which handoff path is canonical during migration.

## Implementation notes

- The design doc names copyable CLI operations like `agent-tools artifacts list`, `agent-tools reviews create`, and `agent-tools specs get`.
- `/design-review` should pass artifact IDs to peer agents.
- `/spec` should produce and iterate spec artifacts.
- `/implement` should accept a spec ID and delegate exact task IDs plus source spec version and manifest item address.
- During transition, dual-read compatibility may still read existing scratch files and task `Specification` fields, but those should not remain canonical after artifact-ID handoff is validated.

## Acceptance criteria

- [ ] Migration plan defines agent-tools CLI commands or delegated tasks for `artifacts`, `reviews`, and `specs` operations.
- [ ] Migration plan defines how `/design-review`, `/spec`, and `/implement` stop using scratch-file handoff as canonical state and pass stable artifact/spec/task IDs instead.
- [ ] Plan defines dual-read compatibility and fallback behavior while existing scratch-file and task-specification workflows still exist.
- [ ] Plan identifies whether required client/skill updates live in this repo, the agent-tools project, local skill directories, or delegated gateway tasks.
- [ ] Release checklist blocks scratch-file deprecation until artifact-ID handoff is validated end to end.

## Validation plan

- **Traceability check:** Every CLI/skill experience named in `gateway-features.md` has an owner, target command or skill, and validation method.
- **Delegation check:** External work has gateway task IDs or a release-blocking checklist entry.
- **Handoff check:** A dry-run workflow proves a design review artifact can become a spec artifact and implementation task handoff without relying on scratch files as canonical state.

## Dependencies

- **T009:** spec artifact workflow.
- **T010:** design review artifact workflow.
- **T011:** documentation artifact workflow.
- **T013:** agent API context.

## Provides to downstream tasks

- **T014:** end-to-end migration and rollback validation.
