# T001 - Resolve API docs integration strategy

**Team:** docs
**Phase:** 1
**Depends on:** (none)
**Status:** todo

## Scope

**In:** Decide how the existing agent-native API docs registry relates to the new artifact substrate. The decision must cover IDs, chunks, comments, versions, links, CLI compatibility, and the canonical agent retrieval path.

**Out:** Implementing schema migrations, HTTP routes, chunk generation, or UI changes. Those follow after the contract is stable.

## Source references

- `gateway-features.md` section "Feature 3: Project Documentation"
- `gateway-features.md` section "Relationship To Existing Primitives" -> "API Docs"
- `gateway-features.md` section "Open Questions"
- Prior memory: API-docs integration should be decided before artifact schema/client work hardens.

## Deliverables

1. **`docs/artifacts-api-docs-integration.md`** - decision document that chooses one approach:
   - API docs become a documentation artifact kind, or
   - API docs remain separate with equivalent immutable version, chunk, comment, and link guarantees.
2. **`README.md` update** - short note in the API docs/reference area explaining the chosen compatibility path.

## Implementation notes

- Existing `api-docs` handlers and DB functions live in `crates/gateway/src/routes.rs` and `crates/gateway/src/db.rs`.
- The design favors a shared substrate, but allows specialized routes where they reduce client complexity.
- Preserve the docs-first principle: canonical agent docs capture intent, workflows, auth expectations, safety constraints, schemas, and examples, not only OpenAPI.

## Acceptance criteria

- [ ] Decision document states whether current API docs become artifact-backed docs or remain a separate resource family with equivalent guarantees.
- [ ] Decision explicitly maps current API doc IDs, chunks, comments, and versions to the chosen artifact/docs model.
- [ ] Decision identifies the canonical agent retrieval path for docs during and after migration.
- [ ] Decision lists compatibility requirements for existing `agent-tools docs` commands.
- [ ] Open questions from `gateway-features.md` about docs export and source-adjacent repository docs are answered or deferred with owners.

## Validation plan

- **Decision completeness:** Review `docs/artifacts-api-docs-integration.md` against every bullet in acceptance criteria.
- **Compatibility check:** Confirm the document names existing `api-docs` route behavior and `agent-tools docs` behavior explicitly.
- **Implementation dependency check:** Confirm T005/T011 can reference this decision without reopening the product question.

## Dependencies

(none - Phase 1 entry task)

## Provides to downstream tasks

- **T002:** supplies the docs relationship that shapes the shared artifact contract.
- **T011:** supplies the migration and compatibility behavior for project documentation.
- **T013:** supplies the canonical agent docs publication path.
