# T009 - Implement spec artifact workflow and retry-safe task generation

**Team:** backend
**Phase:** 3
**Depends on:** T007, T008
**Status:** todo

## Scope

**In:** Implement spec-specific workflow behavior on top of generic artifacts, including spec import/create/iterate routes, manifests, per-task detail, accepted versions, generated task links, and retry-safe task creation.

**Out:** Updating the external `agent-tools` CLI or `/spec` skill implementation. This task exposes gateway behavior those clients can consume.

## Source references

- `gateway-features.md` section "Feature 2: Spec Artifacts"
- Existing task DB/routes in `crates/gateway/src/db.rs` and `crates/gateway/src/routes.rs`
- Prior memory: spec-to-task creation policy must define who may invoke it, confirmation needs, idempotent mapping, partial failure, and rerun behavior.
- T003 mutation contract

## Deliverables

1. **Spec workflow routes/repository functions** for creating/importing spec artifacts, storing structured manifests, accepting versions, fetching manifest items, and generating or linking tasks.
2. **Concrete route family** for the spec workflow, including create/import, create version/iterate, accept version, get manifest, get manifest item, generate tasks from accepted version, and link existing task.
3. **Task source fields and artifact links** from generated gateway tasks to source spec artifact/version/manifest item; both are required so cold-agent resume and graph/audit queries work.
4. **Tests** for accepted-version task generation, idempotent reruns, manifest-item retrieval, and imported spec-directory compatibility.

## Implementation notes

- The existing task `specification` field remains valuable for focused handoff context, but the canonical planning body belongs to the spec artifact.
- The structured manifest must round-trip current source-adjacent spec directories such as `gateway-features_spec/`: phase, task code, team, title, status, dependencies, labels, touch surface, acceptance criteria, validation plan, gateway task ID, and focused per-task spec body/file path.
- Stable manifest item IDs are required; implementation agents should not infer task identity from headings.
- Define the manifest item stability heuristic before task generation is considered complete: how version N and N+1 decide an item is the same conceptual item, when a new `manifest_item_id` is issued, and how generated task back-links behave when an item is renamed, split, merged, or deleted.
- Partial failure recovery should link existing tasks rather than duplicating them.
- Task generation is a T003 resumable fan-out workflow: reruns on a failed run resume from missing manifest items and transition the same workflow_run to `succeeded` when complete; cancelled runs require a new run.
- Proposed route shape: `/v1/projects/:ident/specs`, `/specs/:artifact_id/versions`, `/specs/:artifact_id/accept`, `/specs/:artifact_id/manifest`, `/specs/:artifact_id/manifest/:manifest_item_id`, `/specs/:artifact_id/generate-tasks`, and `/specs/:artifact_id/link-task`. If implementation chooses generic `/artifacts` routes instead, README/API context must document the equivalent calls.

## Acceptance criteria

- [ ] Spec workflow stores source links, manifest versions, per-task detail, phase/task metadata, touch surfaces, acceptance criteria, validation plans, focused task spec body/file path, gateway task IDs, and stable manifest item IDs in artifact versions.
- [ ] Route/repository API exposes create/import, iterate/version, accept, manifest retrieval, manifest-item retrieval, generate-tasks, and link-existing-task operations.
- [ ] Manifest item ID stability rules cover unchanged, renamed, split, merged, and deleted items across spec versions, including collision/replacement behavior.
- [ ] Task generation requires the authorization/confirmation policy chosen in T003 and records source spec artifact ID, immutable version ID, and manifest item ID in both the gateway task source field and an artifact link.
- [ ] Task generation is idempotent across reruns and partial failures, linking existing gateway task IDs instead of duplicating work.
- [ ] Failed task-generation runs are resumable under T003: retry fills missing manifest items, preserves existing task/link IDs, and moves the same workflow_run to `succeeded` when complete.
- [ ] Workflow records contributions or workflow run outputs for generated tasks and implementation handoff notes.
- [ ] Tests cover accepted-version generation, manifest import from a spec-directory-shaped fixture, manifest-item retrieval by stable ID, rerun behavior, partial failure recovery, and task source/back-links.

## Validation plan

- **Unit/route tests:** Run `cargo test -p gateway`.
- **Rerun test:** Generate tasks from the same accepted spec version twice with the same idempotency key and confirm task IDs are reused.
- **Partial failure test:** Simulate interruption after one task is created and confirm rerun links the existing task and creates only missing tasks.
- **Manifest stability test:** Create a new spec version with renamed and split manifest items; verify retained items preserve IDs where appropriate and changed items get documented replacement/back-link behavior.
- **Import/round-trip test:** Import a fixture matching `gateway-features_spec/manifest.yaml` plus per-task spec bodies; retrieve the manifest and one manifest item by ID and confirm `/implement` has enough data to delegate without reading scratch files.

## Dependencies

- **T007:** generic artifact API.
- **T008:** substrate regression coverage.

## Provides to downstream tasks

- **T012:** spec manifest UI.
- **T013:** agent API context for spec workflows.
- **T014:** migration and rollback validation for spec/task handoff.
