# Gateway Features Vision

## Summary

agent-gateway is evolving from a communication hub into the shared workspace
for distributed AI agents and the user working with them. The next feature
layer should make design reviews, implementation specs, and project
documentation first-class gateway objects instead of temporary markdown files
passed through local scratch directories.

The goal is not just persistence. The goal is confluence: Claude, Codex,
Gemini, future agents, and the user should be able to contribute to the same
project-scoped bodies of work, with stable IDs, comments, version history,
task links, documentation links, and enough structure for skills to delegate
precisely.

## Current Pain

The newer skills for design review, spec generation, and implementation create
a much clearer workflow:

- design review gathers independent agent perspectives and a research analyst
  synthesizes the result
- spec turns a reviewed design into a structured implementation plan
- implement executes tasks from that plan through specialized agents

Those workflows are valuable, but their handoff artifacts are still brittle.
Markdown files in `/tmp` are easy to lose, hard to discover, awkward to
version, and require agents to infer which file or section is authoritative.
They also make user engagement clumsy because comments and decisions do not
live next to the artifact being discussed.

The gateway already has the right surrounding primitives:

- project identity
- agent identity
- task management
- comments on tasks and patterns
- agent-native API documentation
- persistent comms
- skill and agent distribution

The missing primitive is a durable project artifact that can be reviewed,
versioned, commented on, searched, linked to tasks, and consumed directly by
skills.

## Product Thesis

Design reviews, specs, and project documentation are variations of one product
shape:

- a project-scoped artifact
- one or more structured versions
- contributions from multiple actors
- comments and decisions
- links to tasks, docs, patterns, commits, memories, and related artifacts
- agent-readable IDs for precise delegation

The gateway should provide a shared artifact substrate and expose specialized
workflows on top of it:

- `design_review` artifacts for multi-agent review and synthesis
- `spec` artifacts for major feature planning and implementation handoff
- `doc` artifacts for living project documentation and agent-native context

This keeps the mental model small while allowing the gateway to grow into a
collaboration layer rather than a collection of unrelated stores.

## Core Concepts

### Artifact

An artifact is a durable, project-scoped body of work.

Examples:

- "Gateway Artifact System Design Review"
- "Eventic Build Status Spec"
- "Agent Tools Task API Documentation"
- "Skill Sync Canonical Transform Decision"

Artifacts should have stable IDs that can be passed to skills and agents
directly. A skill should be able to say "review artifact `art_123`" or
"implement spec `spec_456` task `task_789`" instead of telling an agent to
search through scratch files.

### Artifact Version

An artifact version captures a specific state of the artifact.

Versions allow:

- review of changes between iterations
- rollback or reference to prior reasoning
- preserving what agents saw when they produced feedback
- separating draft, reviewed, accepted, and implemented states

The system does not need to start with complex document editing. A version can
initially be a markdown or structured JSON body with metadata. The important
requirement is that version history is explicit and queryable.

### Contribution

A contribution is an actor's input to an artifact or version.

Examples:

- Codex pass 1 review
- Gemini pass 2 response after reading other reviewers
- Claude architecture critique
- research analyst synthesis
- user decision note
- implementation agent completion note linked back to the spec

Contributions should preserve provenance. The gateway should know who produced
the content, from which host or agent system, in what role, and during which
workflow phase.

For review workflows, contribution provenance should also preserve the review
round or workflow run, the phase, the artifact version reviewed, and for pass
2 or synthesis contributions the prior contribution IDs or deterministic
read-set rule used. This keeps later agents from mixing feedback across
rounds or source versions when reconstructing what a peer saw.

The v1 model should include a small workflow run or activity contract. A
workflow run records the actor role, phase, source artifact version,
deterministic read set, idempotency key, and generated outputs such as
contributions, syntheses, artifact versions, task links, or task records.
This can stay relational, but should borrow the PROV-O distinction between
agents, activities, and entities so provenance does not become a set of
one-off contribution fields.

### Comment

Comments support user and agent discussion on artifacts, versions,
contributions, or specific blocks when block-level targeting exists.

The first version should make the supported target contract explicit.
Artifact-level, version-level, and contribution-level comments are stable
enough for the initial workflow. Section, block, or text-range comments need
stable block IDs or selector metadata tied to the version/state being
annotated, and can be deferred until the body model supports that safely.

If anchored comments are added later, they should use selector type,
selector payload, selector state, and immutable artifact version ID rather
than headings or raw offsets alone. W3C Web Annotation Selectors and States
are useful prior art for that shape without requiring the gateway to adopt
RDF or JSON-LD.

Comments should support at least:

- open and resolved states
- author identity
- target artifact, version, or contribution
- timestamps
- links to task comments or decisions when relevant

This makes the gateway the place where user engagement happens, instead of
splitting discussion across Discord messages, task comments, and temporary
files.

## Feature 1: Design Review Artifacts

Design review should become a gateway-backed workflow.

The design review skill should create or update a `design_review` artifact,
then invite multiple agents to contribute directly to that artifact. Each peer
review pass becomes a contribution with a stable ID. The research analyst can
then fetch the artifact and its contributions directly from the gateway.

Target workflow:

1. User asks to review a design document or existing artifact.
2. Gateway creates a design review artifact or adds a new review round to an
   existing artifact.
3. Claude, Codex, Gemini, and future peers write independent pass 1
   contributions.
4. Peers read each other's pass 1 contributions and write pass 2 responses.
5. Research analyst writes a synthesis contribution and may create a new
   artifact version.
6. User comments, resolves questions, or asks for another iteration.

Useful states:

- draft
- collecting_reviews
- synthesizing
- needs_user_decision
- accepted
- superseded

The immediate win is reliability. The larger win is that review history becomes
queryable project history rather than disposable process output.

## Feature 2: Spec Artifacts

Specs should also become first-class artifacts.

A major feature spec can hold:

- the source design or review artifact link
- the implementation manifest
- task breakdown
- task dependencies
- acceptance criteria
- validation plans
- links to gateway task IDs
- comments and user decisions
- version history across iterations

The spec skill should create a `spec` artifact and produce versioned spec
content directly in the gateway. Accepted or task-generating spec versions
must include a structured manifest with stable item IDs, dependency IDs,
acceptance criteria, validation plans, and source fields for generated work,
even if the human-authored body is markdown. The implement skill should
accept a spec ID, fetch the manifest and task records, and delegate exact
work by task ID plus the source spec version and stable manifest item,
section, or block address.

Task generation from a spec should be a retry-safe workflow mutation. The
design should define who may invoke it, whether user confirmation is
required, how manifest items map idempotently to existing or new gateway
task IDs, and what happens on partial failure or rerun.

Target workflow:

1. User asks to spec a design artifact.
2. Gateway creates a spec artifact linked to the source design review or doc.
3. Spec generation creates a first version with a manifest and per-task
   detail.
4. Peer agents comment or contribute revisions.
5. User or analyst accepts a version as ready for implementation.
6. Gateway tasks are created or linked from the spec.
7. Implement agents receive exact spec IDs and task IDs.
8. Implementation results and validation notes link back to the spec.

This should reduce agent waste. Agents should not have to search for the right
spec file, infer task ownership, or reconstruct dependency order from prose.

## Feature 3: Project Documentation

Project documentation should become the third workflow on the same substrate.

The gateway already has agent-native API docs and RAG-ready chunks. That
direction should broaden into a project documentation section that can hold
engineering context, workflow docs, API docs, decision records, and operational
notes.

This does not mean removing all docs from repositories. Repositories should
still keep source-adjacent and externally consumed documents:

- README files
- user-facing documentation
- generated API schemas when needed
- deployment manifests
- docs that must version exactly with code

But internal planning docs, TODO documents, design drafts, living engineering
notes, and agent workflow context can move into gateway artifacts where they
are searchable, commentable, versioned, and linked to tasks/specs.

Target workflow:

1. Agent or user publishes project documentation to the gateway.
2. Documentation is chunked for retrieval.
3. Agents query gateway docs before searching code for API or workflow intent.
4. Comments and versions capture corrections.
5. Specs and tasks link to relevant docs.
6. Docs that become canonical source-adjacent files can still be exported or
   mirrored into the repo when appropriate.

The important distinction is that memory stores distilled reusable lessons,
while docs store durable project knowledge that agents and humans can inspect.

## Actor Model

The artifact system needs first-class actors.

At minimum, an actor should capture:

- actor type: user, agent, system
- agent system: codex, claude, gemini, other
- stable agent ID when available
- host or machine identity when available
- display name
- optional model or runtime metadata

Role should be captured on the contribution, workflow participation, or
activity record rather than treated as a durable actor attribute. The same
stable agent may be a reviewer, implementer, analyst, or coordinator in
different workflows. An actor may expose a default display label for UI
convenience, but contribution role, phase, and responsibility need to be
provenance on the work itself.

This is more important than it first appears. The platform is not just
coordinating anonymous outputs. It is coordinating different agent systems with
different strengths, tool access, memory stores, and review lenses. Provenance
lets the gateway present a true multi-agent conversation instead of a flat pile
of markdown.

Provenance is not authorization. Before artifacts become a multi-human or
multi-project production surface, the design needs a project-scoped
permission model for reading artifacts, creating versions, commenting,
resolving comments, accepting versions, generating tasks, linking related
resources, and administering project membership. If the first vertical slice
assumes a trusted single-user or single-project boundary, that boundary should
be explicit in the API, CLI, and UI so it is not mistaken for general
workspace readiness.

## Relationship To Existing Primitives

A shared invariant should apply across the substrate: artifacts are mutable
containers, artifact versions are immutable review and retrieval targets, and
workflow contributions, generated tasks, comments, links, and chunks should
link to the immutable version plus a stable child identifier where applicable.

Artifact links need a v1 contract, not just foreign keys. Each link should
define source and target kinds, link type, actor/provenance fields, optional
source and target version IDs, optional stable child references, uniqueness
or idempotency rules, and deletion or supersession behavior. Links used for
audit, handoff, generated tasks, chunks, comments, and provenance should
target immutable versions plus child IDs where applicable; links to mutable
artifact containers are acceptable for discovery and navigation.

### Tasks

Tasks remain the execution surface.

Artifacts should link to tasks, and specs should be able to create or update
tasks. The task `Specification` field remains valuable for focused handoff
context, but large planning bodies belong in spec artifacts. A task should
point back to the exact spec artifact, immutable spec version, and stable
manifest item, section, or block address that created it.

### Memory

Memory remains the distilled learning surface.

The gateway should not copy every review, decision, or version into memory.
Instead, agents should store memories only when a reusable lesson emerges:

- a durable user preference
- a non-obvious implementation constraint
- a recurring failure mode
- a project convention that future agents need before acting

Artifacts hold the detailed record. Memory helps cold agents find the right
record and avoid repeating mistakes.

### Patterns

Patterns remain reusable organization-wide guidance.

Artifacts can link to patterns, and review/spec workflows can discover and
apply patterns. When a project artifact produces a reusable approach, the
gateway can help promote it into a pattern, but the two concepts should stay
separate.

### API Docs

The existing API docs capability should become a specialized documentation
kind or be integrated under the broader project documentation model. This
choice should be made before artifact schema or client work creates parallel
documentation stores. If API docs remain a separate resource family, they
should still share the same immutable version, comment, chunk, and link
invariants or define equivalent guarantees, including how API doc IDs map to
artifact IDs, how chunks reference immutable source versions, and which
endpoint family is canonical for agent retrieval.

The current docs-first principle still holds: many owned apps do not have
OpenAPI first, so the canonical agent document should capture intent,
workflows, auth expectations, safety constraints, schemas, and copyable
examples.

## CLI And Skill Experience

The CLI should support stable, copyable operations:

```bash
agent-tools artifacts list
agent-tools artifacts get <id>
agent-tools artifacts versions <id>
agent-tools artifacts diff <id> <from-version> <to-version>
agent-tools artifacts comments <id>

agent-tools reviews create --title "..."
agent-tools reviews contribute <review-id> --phase pass_1 --file ...
agent-tools reviews synthesize <review-id> --file ...

agent-tools specs create --from-artifact <id>
agent-tools specs get <spec-id>
agent-tools specs tasks <spec-id>

agent-tools docs publish --file ...
agent-tools docs chunks --query "..."
```

The exact command names can change, but the skill experience should be stable:

- `/design-review` should pass artifact IDs to peer agents.
- `/spec` should produce and iterate spec artifacts.
- `/implement` should accept a spec ID and delegate exact task IDs.
- Research analysts should fetch contributions directly from the gateway.
- Agents should stop relying on `/tmp` files for canonical handoff state.

## UI Experience

The UI should make the gateway feel like a shared project workspace.

Useful views:

- project artifact list with filters by kind, status, label, and actor
- artifact detail with current version, contribution timeline, comments, and
  linked tasks/docs/patterns
- version history with diff between versions
- review round view showing pass 1, pass 2, and synthesis
- spec manifest view showing tasks, dependencies, status, and acceptance
  criteria
- documentation browser with search and chunks

The UI should prioritize dense, operational clarity. This is a work surface for
agents and the user, not a marketing page.

## API Shape

The API should expose the shared substrate and allow specialized routes where
they reduce client complexity.

Possible generic resources:

- artifacts
- artifact versions
- artifact contributions
- artifact comments
- artifact links
- actors

Possible specialized resource families:

- reviews
- specs
- docs

The design question is whether specialized routes are thin wrappers over
generic artifacts or separate first-class models that share lower-level tables.
The product model favors a shared substrate either way.

Mutating workflow endpoints should share a v1 write contract. Creating
versions, contributing reviews, synthesizing reviews, accepting versions,
generating or linking tasks, publishing docs or chunks, and creating comments
should specify the required authorization scope, idempotency key shape,
uniqueness constraints, immutable source and target IDs, actor/provenance
fields, generated resource IDs, and retry behavior.

## Versioning And Diffs

Versioning should be simple at first.

Initial versioning can store full bodies per version. Diffing can be generated
on read. If structured blocks are introduced later, the gateway can support
block-level comments and more precise diffs.

Important behaviors:

- each version has an immutable body
- current version is explicit
- accepted version is explicit when different from current
- contributions can reference the version they reviewed
- task generation records the source spec version
- diffs between versions are available to agents and users

Artifact lifecycle state, current version, accepted version, review state,
and implementation state should not collapse into one enum. An artifact can
have an accepted version while a later draft version is under review, and an
implemented spec can still receive documentation edits. State ownership
should make those cases explicit.

## Search And Retrieval

Artifacts should be searchable by:

- project
- kind
- status
- title
- labels
- actor
- body text
- contribution text
- linked task ID
- linked pattern ID
- linked doc ID

Docs should remain chunkable for retrieval. Retrieval chunks should reference
the immutable artifact version or equivalent immutable docs version they were
generated from, and queries should make clear whether they search only current
versions or include history. Specs and design reviews may also benefit from
chunks, but the initial requirement is exact retrieval by ID. Search helps
discovery; IDs make delegation precise.

## Rollout

A conservative rollout path:

1. Decide whether API docs are artifact-backed or remain a separate family
   with equivalent immutable version, chunk, comment, and link guarantees.
2. Add generic artifact, version, contribution, comment, and link primitives.
3. Add the shared workflow mutation contract for authorization, provenance,
   idempotency, immutable source references, generated resource IDs, and retry
   behavior.
4. Add CLI/API support for creating and reading artifacts by ID.
5. Move spec artifacts onto the substrate first because they connect directly
   to implementation tasks.
6. Move design review workflows onto the substrate next.
7. Broaden API docs into project documentation artifacts or attach the
   equivalent compatibility layer.
8. Update `/design-review`, `/spec`, and `/implement` skills to use gateway
   IDs instead of `/tmp` handoff files.
9. Add UI views once the API and CLI workflows are stable enough to validate.

The rollout should include migration and rollback phases: import or backfill
existing task/spec/doc state where useful, provide dual-read compatibility
while skills transition, create artifact links to existing task IDs
idempotently, define deprecation criteria for scratch files, and document how
clients return to existing task/docs behavior if artifact endpoints or body
schemas need rework.

The first production slice should also define an operations envelope: maximum
body and contribution sizes, project quotas or warnings, retention and archive
rules, backup and restore expectations, metrics for artifact writes, diffs,
search, and chunking, and visible handling for stale or failed retrieval
chunks.

Specs may be the best first vertical slice because they exercise artifacts,
versions, links, tasks, comments, and implementation handoff.

## Open Questions

- Should API docs remain a separate resource family or become one
  documentation artifact kind?
- Should artifact bodies be markdown, structured JSON blocks, or both?
- How much block-level commenting is needed in the first version?
- What is the minimum actor identity that works across Claude, Codex, Gemini,
  and future systems?
- Should review rounds be explicit child records or modeled as artifact
  versions plus contribution phases?
- How should accepted versions differ from current draft versions?
- Should docs support export back to repository files?
- Should task creation from specs be automatic, user-confirmed, or workflow
  dependent?
- What artifact states are shared across all kinds, and which states are
  kind-specific?
- Is the first artifact vertical slice limited to a trusted single-user or
  single-project boundary, or must project/object authorization block initial
  production use?

## Success Criteria

This feature set is successful when:

- agents can fetch canonical design review, spec, and documentation state by
  gateway ID
- peer review no longer depends on scratch markdown files
- research analyst synthesis reads structured contributions from the gateway
- specs create or link exact task IDs
- implement agents receive precise spec and task references
- users can comment on and review artifact changes between versions
- project documentation is searchable and useful enough to replace most
  internal planning docs in repositories
- memory remains focused on distilled reusable learning instead of becoming a
  dumping ground for full documents

The end state is a gateway that acts as the durable collaboration layer for a
distributed agent platform: tasks for execution, memory for distilled lessons,
patterns for reusable guidance, and artifacts for evolving project knowledge.
