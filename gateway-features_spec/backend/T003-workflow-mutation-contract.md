# T003 - Define workflow mutation, permissions, and idempotency contract

**Team:** backend
**Phase:** 1
**Depends on:** T002
**Status:** todo

## Scope

**In:** Define the common write contract for artifact workflow mutations, including authorization boundary, actor/provenance requirements, idempotency keys, uniqueness, generated resources, retry behavior, and partial-failure handling.

**Out:** Implementing full project membership or multi-tenant authorization unless explicitly chosen in the contract.

## Source references

- `gateway-features.md` sections "Actor Model", "API Shape", "Rollout"
- Prior memory: permissions, idempotency, and immutable links should be treated as one v1 write/read contract.
- Prior memory: the first vertical slice may be trusted single-user/project, but that boundary must be explicit.

## Deliverables

1. **`docs/workflow-mutation-contract.md`** - endpoint-agnostic mutation contract.
2. **Route validation notes** in the document naming where the checks belong in `crates/gateway/src/routes.rs`.

## Implementation notes

- Existing gateway auth is bearer-token based. If object-level authorization is deferred, name the deferred scopes and make the trusted boundary visible in API, CLI, and UI copy.
- The contract should apply to creating versions, contributing reviews, synthesizing reviews, accepting versions, generating/linking tasks, publishing docs/chunks, and comments.
- Idempotency keys should be scoped enough to prevent accidental cross-project or cross-artifact collisions.

## Acceptance criteria

- [ ] Contract lists required authorization scope for artifact reads, version writes, comments, link writes, accepting versions, and task generation.
- [ ] Contract states the initial trusted single-user/project boundary if full project/object authorization is deferred.
- [ ] Contract defines idempotency key shape and uniqueness rules for mutating workflow endpoints.
- [ ] Contract defines retry and partial-failure behavior for generated versions, contributions, links, chunks, and task records.
- [ ] Contract requires actor/provenance fields on every workflow mutation result.

## Validation plan

- **Scope matrix check:** The document includes a resource/action/scope table.
- **Retry scenario check:** The document covers at least one retry path each for version creation, contribution creation, and spec task generation.
- **Route readiness:** T007 can cite the validation requirements without inventing endpoint-specific rules.

## Dependencies

- **T002:** resource and provenance model must exist first.

## Provides to downstream tasks

- **T004:** operations and rollback plan uses retry and partial-failure semantics.
- **T005/T007:** schema and routes implement idempotency and validation.
- **T009/T010/T011:** specialized workflows share one mutation contract.
