# T013 - Publish agent-native API context for artifact workflows

**Team:** docs
**Phase:** 4
**Depends on:** T007, T009, T010, T011
**Status:** todo

## Scope

**In:** Update or publish gateway API context for agents covering generic artifacts and specialized review/spec/docs workflows, including the docs export behavior deferred by T001.

**Out:** Rewriting the full README API reference beyond links and excerpts needed to point agents at the canonical context.

## Source references

- `gateway-features.md` sections "CLI And Skill Experience", "API Shape", and "Success Criteria"
- Current peer lookup found an existing `agent-gateway` API context via broader docs listing, while artifact-workflow-specific search/chunks were not discoverable.

## Deliverables

1. **Updated existing agent-gateway API context or `.agent/api/gateway.yaml`** - agent-native API context for artifact workflows, without duplicate app/context entries.
2. **Docs export contract** covering the `agent-tools docs export` CLI/API behavior, manifest/source-path format, and conflict policy for materializing current accepted documentation artifact versions back to repository files.
3. **README update** pointing agents to the context docs.
4. **Validation/publish command record** in task notes or release checklist if publishing cannot run in the implementation environment.

## Implementation notes

- Include auth expectations, idempotency examples, provenance fields, safety constraints, schemas, route family names, and copyable curl examples.
- The docs should tell agents to retrieve by stable artifact/spec/review IDs before searching code.
- Explain both route families during migration: docs-first commands remain preferred for API context, while artifact routes expose the broader graph.
- Define the export conflict policy explicitly: whether `docs export` overwrites repository files, writes proposed changes, or requires confirmation, and how the manifest records source paths.
- If publish cannot run because a gateway is unavailable, leave a release-blocking checklist item with the exact command.

## Acceptance criteria

- [ ] Existing agent-gateway API context is updated or intentionally replaced without creating a duplicate app/context entry.
- [ ] `.agent/api/gateway.yaml` or equivalent agent API context document describes artifact, review, spec, and docs workflows.
- [ ] Docs export behavior is specified, including `agent-tools docs export`, manifest/source path format, accepted-version selection, and overwrite-vs-propose conflict policy.
- [ ] `agent-tools docs validate --file .agent/api/gateway.yaml` or the chosen existing-context file passes.
- [ ] `agent-tools docs publish --file .agent/api/gateway.yaml` or the chosen existing-context file has been run or is captured as a release-blocking manual step.
- [ ] Published context includes auth expectations, idempotency examples, provenance fields, safety constraints, schemas, and copyable curl examples.
- [ ] `agent-tools docs search` and `agent-tools docs chunks` can retrieve artifact workflow context after publish.
- [ ] README points agents to gateway API docs before code search for artifact behavior.

## Validation plan

- **Schema validation:** Run `agent-tools docs validate --file .agent/api/gateway.yaml` or the chosen existing-context file.
- **Publish:** Run `agent-tools docs publish --file .agent/api/gateway.yaml` or the chosen existing-context file, or record the exact blocked reason.
- **Retrieval:** Run `agent-tools docs search "artifact workflow"` and `agent-tools docs chunks --query "artifact workflow"` and verify the context is discoverable.

## Dependencies

- **T007:** generic artifact API.
- **T009:** spec workflow.
- **T010:** design review workflow.
- **T011:** documentation workflow.

## Provides to downstream tasks

- **T014:** final release readiness check.
- Future `/design-review`, `/spec`, and `/implement` updates can consume this API context.
