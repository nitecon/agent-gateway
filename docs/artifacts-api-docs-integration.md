# API Docs Integration With The Artifact Substrate

**Status:** Decision recorded (Phase 1, gateway-features_spec T001)
**Audience:** gateway implementers (T002, T005, T011, T013) and `agent-tools docs` maintainers
**Source:** `gateway-features.md` ("Feature 3: Project Documentation", "Relationship To Existing Primitives -> API Docs", "Open Questions"); prior memory IDs `e88038d6`, `13ecbb90`, `06012ebe`, `82b7dbcf`

## Decision

**API docs become a specialized documentation artifact kind on the shared
artifact substrate.** They are not a parallel resource family.

Concretely:

- A published API document is one artifact with `kind = "documentation"` and
  `subkind = "api_context"`.
- Each `POST` or `PATCH` that today produces a new `api_docs` row produces a
  new immutable `artifact_version` on that artifact. The current `version`
  string field (e.g. `"2026-04-28"`) becomes a version label on the
  artifact_version, not a substitute for the version's stable ID.
- Chunks, comments, and links target the immutable `artifact_version_id`
  plus a stable child address (section/block/field path), matching the v1
  artifact contract described in `gateway-features.md` ("Relationship To
  Existing Primitives").
- The existing `/v1/projects/:ident/api-docs/*` HTTP family and
  `agent-tools docs *` CLI remain as a thin, stable compatibility facade
  over the artifact store. They are the canonical agent retrieval path
  during migration; after migration the artifact routes become canonical
  and the api-docs routes remain supported aliases.

This satisfies the design preference for a shared substrate, eliminates a
second documentation store before it grows divergent invariants, and keeps
the docs-first guarantee that canonical agent docs are agent-native
(intent, workflows, auth, safety, schemas, examples) — not OpenAPI-only.

### Why not keep API docs as a separate resource family

The "separate but equivalent" path was the explicit fallback in
`gateway-features.md`. We reject it for v1 because:

1. It duplicates the v1 invariants (immutable versions, version-anchored
   chunks/comments, typed links) in two code paths. Drift is near-certain.
2. The artifact substrate already needs documentation as a first-class
   workflow (Feature 3). Carving out API docs forces every consumer
   (specs, reviews, implement) to learn two link kinds for the same
   conceptual target.
3. Project documentation and API docs share retrieval shape: chunked,
   project-scoped, label-filtered, RAG-served. One index serves both.

We accept the cost of writing a compatibility facade in exchange for one
durable substrate.

## ID, version, chunk, comment, and link mapping

| Today (`api_docs`) | After migration (artifact substrate) |
|---|---|
| `api_doc_id` (uuid) | Stable `artifact_id`. The old id is preserved verbatim so existing references keep resolving. |
| `version` string ("2026-04-28") | Label on `artifact_version`. Latest accepted version is selected by lifecycle state, not by parsing the label. |
| `content` JSON blob | Body of the current `artifact_version`. Body MIME is `application/agent-context+json`; OpenAPI/Swagger bodies are stored with their original `source_format` field surfaced as `artifact_version.source_format`. |
| Implicit "current" doc returned by `GET /api-docs/:id` | Resolved to the artifact's currently accepted version. |
| Generated chunks from `api_doc_chunks` | `artifact_chunks` rows, each anchored to `(artifact_version_id, child_address)`. Child address uses the existing chunk path scheme (`purpose`, `workflows[<n>].steps`, `endpoints[<n>]`, etc.) so chunk identity is stable across re-chunking. |
| Comments (not currently first-class) | Artifact comments anchored at artifact, version, or version+child level per the v1 comment contract (memory `eac21975`). |
| Cross-references between docs and other resources | `artifact_links` with typed source/target kinds and version pinning where appropriate. |

The migration writes a new artifact + initial artifact_version for every
existing `api_docs` row. The artifact id equals the old `api_doc_id` so
external clients that cached ids keep working. The initial artifact_version
is marked accepted; its `created_at` mirrors the original row.

## Canonical agent retrieval path

**During migration (Phase 1 -> Phase 3):**

- Agents continue to call `agent-tools docs search`, `docs list`, `docs get`,
  `docs chunks`, `docs validate`, and `docs publish`. These remain the
  documented and recommended commands.
- The CLI calls the existing `/v1/projects/:ident/api-docs/*` HTTP routes.
- Internally, those routes read from and write to the artifact substrate.
  Chunk responses are produced from `artifact_chunks` joined to the latest
  accepted `artifact_version`.

**After migration (Phase 4+):**

- `agent-tools artifacts` and `agent-tools docs` both resolve documentation
  artifacts. `docs` remains supported indefinitely as the docs-first verb
  set; `artifacts` is the substrate-general verb set.
- Documentation artifacts surface in artifact search alongside specs,
  reviews, and project docs without losing their `subkind = "api_context"`
  filter.
- The canonical retrieval path for an agent that just wants API context for
  app `X` remains `agent-tools docs search "X"` or `agent-tools docs chunks
  --app X --query "..."`. Agents that need the broader artifact graph use
  `agent-tools artifacts ...`.

## Compatibility requirements for `agent-tools docs`

The CLI MUST keep working without behavior changes for every command listed
in `~/.claude/CLAUDE.md` "API Context Docs (gateway-backed)":

- `agent-tools docs search "<api-or-workflow>"`
- `agent-tools docs list [--app APP] [--label LABEL] [--kind KIND] [--query Q]`
- `agent-tools docs get <id>` — resolves by the preserved artifact id
- `agent-tools docs chunks --query "..." [--app APP] [--label LABEL]` — returns chunks anchored to immutable artifact_version ids; chunk payload format is unchanged
- `agent-tools docs validate --file .agent/api/<app>.yaml`
- `agent-tools docs publish --file .agent/api/<app>.yaml` — creates or updates the documentation artifact; each publish creates a new artifact_version

Gateway responses MAY add new fields (`artifact_id`, `artifact_version_id`,
`subkind`) but MUST keep all currently documented fields with their current
semantics. The README API table in §"Agent API docs" stays accurate; the
underlying storage is an implementation detail.

`docs publish` becomes the docs-first wrapper around artifact version
creation. It enforces the docs-first principle: the body MUST express
intent, workflows, auth, safety, and examples — not OpenAPI alone. OpenAPI
input is accepted with `source_format: "openapi" | "swagger"` and stored
verbatim on the artifact_version; a paired agent-context body remains the
canonical retrieval target when present.

## Open questions answered (or explicitly deferred)

From `gateway-features.md` "Open Questions":

- **"Should API docs remain a separate resource family or become one
  documentation artifact kind?"** — Resolved here: documentation artifact
  kind, `subkind = "api_context"`, with a compatibility facade over the
  existing routes/CLI.
- **"Should docs support export back to repository files?"** — Deferred to
  T013 with owner: docs team. Export is in scope for v1 of the docs
  workflow but out of scope for T001. Required shape: a `docs export`
  CLI verb that materializes the current accepted artifact_version of one
  documentation artifact (or a filtered set) to repo paths declared in
  `.agent/api/<app>.yaml` or an equivalent manifest. T013 owns the
  manifest format and conflict policy (overwrite vs. propose).
- **Source-adjacent repository docs** (READMEs, user-facing docs,
  generated API schemas, deployment manifests, docs that must version
  with code) — Resolved: these stay in repositories per
  `gateway-features.md` Feature 3. They MAY be mirrored into the gateway
  for search, but the repository copy remains canonical. T013 defines the
  mirror direction and freshness expectations.

Other documentation-adjacent open questions (block-level commenting depth,
accepted-vs-draft version semantics, body MIME negotiation) are not
re-opened here; they are owned by T002 (artifact contract) and T003
(version state model) and consume this decision unchanged.

## Validation checklist (T001 acceptance)

- [x] Chose one approach: API docs become a documentation artifact kind.
- [x] Mapped existing API doc IDs, versions, chunks, and comments onto the
      artifact/artifact_version/chunk/comment model.
- [x] Stated the canonical agent retrieval path during and after migration
      (continue using `agent-tools docs`; artifact routes become canonical
      post-migration with `docs` remaining supported).
- [x] Listed compatibility requirements for every existing `agent-tools
      docs` command.
- [x] Answered or deferred (with named owner T013) the relevant open
      questions about docs export and source-adjacent repository docs.

## Hand-off

- **T002** (shared artifact contract): consume this mapping as the
  documentation-kind worked example; do not re-litigate the docs vs.
  api-docs split.
- **T005** (chunk model): chunks anchor to `(artifact_version_id,
  child_address)` for both documentation artifacts and any future
  chunked kind; the existing api_docs chunk path scheme is the
  documentation child-address scheme.
- **T011** (project documentation migration): perform the `api_docs ->
  artifact + artifact_version` migration described in "ID, version,
  chunk, comment, and link mapping". Preserve ids. Mark migrated
  versions accepted. Keep the `/api-docs` route family wired to the
  artifact substrate; do not remove it.
- **T013** (canonical agent docs publication path): own `docs publish`,
  `docs export`, and the docs-first body contract. Inherit the
  compatibility requirements listed above unchanged.
