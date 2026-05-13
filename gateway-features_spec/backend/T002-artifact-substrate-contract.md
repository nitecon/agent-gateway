# T002 - Define artifact substrate v1 contract

**Team:** backend
**Phase:** 1
**Depends on:** T001
**Status:** todo

## Scope

**In:** Define the v1 product and data contract for artifacts, immutable versions, contributions, comments, links, actors, and workflow runs or activities.

**Out:** Writing database migrations or route handlers. This task produces the contract that implementation tasks follow.

## Source references

- `gateway-features.md` sections "Core Concepts", "Actor Model", "Relationship To Existing Primitives", "Versioning And Diffs"
- Prior memory: use immutable version links and stable child identifiers for audit and handoff.
- Prior memory: workflow run/activity should be a v1 contract, not only contribution metadata.

## Deliverables

1. **`docs/artifact-substrate-v1.md`** - v1 contract covering:
   - artifact kinds and lifecycle fields
   - v1 body model choice: markdown, structured JSON blocks, or both
   - migration path for stable block IDs and future selector anchoring
   - artifact version immutability
   - contribution provenance
   - actor identity and role placement
   - workflow run/activity fields
   - comment target support, open/resolved states, and resolution provenance
   - link source/target contract
   - stable child references for specs and docs

## Implementation notes

- Treat artifacts as mutable containers and artifact versions as immutable review/retrieval targets.
- Do not collapse lifecycle state, current version, accepted version, review state, and implementation state into one enum.
- V1 comments can target artifacts, versions, and contributions. Block/range comments can be deferred, but selector requirements should be captured.
- If structured blocks are deferred, the contract still needs stable child-reference rules for spec manifest items, generated tasks, chunks, and future block-level comments.
- Borrow the PROV-O distinction between agents, activities, and entities without adopting RDF or JSON-LD.

## Acceptance criteria

- [ ] Contract defines artifacts, immutable artifact versions, contributions, comments, links, actors, and workflow runs/activities.
- [ ] Contract chooses the v1 body model: markdown, structured JSON blocks, or both, and names the migration path for future stable block IDs.
- [ ] Contract preserves the invariant that audit and handoff links target immutable versions plus stable child identifiers where applicable.
- [ ] Contract separates artifact lifecycle, current version, accepted version, review state, and implementation state.
- [ ] Contract includes supported v1 comment targets, open/resolved state transitions, resolution provenance, and defers block/range comments with selector requirements.
- [ ] Contract includes stable manifest item or child-address fields for spec-to-task handoff.

## Validation plan

- **Traceability check:** Every core concept in `gateway-features.md` maps to a contract section.
- **Invariant check:** Search the contract for mutable artifact links used in audit/handoff paths; any such case must be justified as discovery/navigation only.
- **Comment lifecycle check:** The contract shows how a comment is opened, resolved, and traced to the actor/workflow that resolved it.
- **Downstream readiness:** T005 can turn the contract into tables without adding new conceptual entities.

## Dependencies

- **T001:** API docs relationship must be decided before the docs artifact model is fixed.

## Provides to downstream tasks

- **T003:** defines the resources mutation endpoints operate on.
- **T005/T006/T007:** supplies schema, repository, and API contract.
- **T009/T010/T011:** supplies workflow-specific substrate constraints.
