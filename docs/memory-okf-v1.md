# Memory OKF v1 exchange

## Contract

The existing project/global memory routes accept optional `concept_hash` and
`okf` fields alongside all legacy fields. `content_hash` remains the SHA-256 of
content bytes, and legacy conflict/tombstone behavior is unchanged. WorkingContext
is still excluded. Authentication uses the existing gateway bearer API key;
project IDs, client links, queries, and mutations retain their existing scope.

`okf` is an object with integer `version: 1`, string `format: okf-markdown`,
positive integer `revision`, lowercase SHA-256 hex `semantic_hash`, and string
`document`. When supplied, `concept_hash` must be a lowercase SHA-256 hex digest
and agree with `okf.semantic_hash`. Either optional field may be absent/null.
The gateway does not compute the concept hash: it cannot interpret arbitrary
client concept semantics. The legacy body hash is still verified independently.
Unknown JSON fields and their values, including large numbers, are preserved.
Document text is preserved exactly; JSON whitespace/key order is not significant.
Documents, URLs, YAML, and computation metadata are data only: no parsing into
files, evaluation, execution, or external fetching occurs.

## Capability and rollout

Authenticated `GET /v1/memories/capabilities` returns `version: 1` and a
`capabilities` array with `id: okf-markdown-v1`, version, format, and limits.
To receive envelopes on list/pull, send
`X-Memory-Capabilities: okf-markdown-v1` (a comma-separated token list is accepted).
Without this explicit opt-in, both optional fields are omitted, including for
concept-aware stored records. Push responses, conflicts, and tombstones retain
the existing wire shape. Push accepts valid optional fields without the header.

Deploy the gateway first. Until the agent-memory client negotiates this capability,
its explicit `AGENT_MEMORY_GATEWAY_OKF` / `MEMORY_GATEWAY_OKF` flag still controls
outbound envelopes. A client must also opt in on list/pull to receive them; this
header is a new gateway contract, not functionality implied by the old client flag.
Legacy clients can coexist in the same project without enabling the capability.

## Writes and conflicts

A supplied envelope replaces the stored envelope as a whole. Clients must retain
unknown fields from pulls when preparing subsequent writes. An identical envelope
is a no-op/link and does not advance the server revision. Changed optional fields
require the current `base_gateway_revision`, even when the legacy body is identical.
A same-body duplicate with different optional metadata returns a conflict; pull
the returned ID and retry with the current base revision to upgrade that record.

Omitting both fields preserves existing metadata on same-body writes (including
legacy tag edits/no-ops). A legacy body edit clears both optional fields because
the stored document/hash would otherwise describe the old body. Supplying only
one optional field on a changed envelope clears the other; null is omission, not
an explicit deletion command. Tombstoning retains stored metadata for audit, while
the tombstone response continues to use the original legacy fields.

## Limits and rejection

Each envelope is limited to 1,048,576 serialized JSON bytes, document text to
524,288 UTF-8 bytes, nesting to 16 levels (root is level 1), and total JSON values
to 16,384. NULs in keys/string values are rejected. Unsupported versions/formats,
invalid required types, and mismatched hashes return per-item `rejected` results
without writes. Malformed request JSON/types and requests exceeding the existing
HTTP body limit are rejected at the transport layer. The existing 500-item batch
limit and HTTP body limit also apply, including across mixed legacy/OKF batches.

## Migration and downgrade

Startup adds nullable `concept_hash TEXT` and `okf_json TEXT` columns to
`gateway_memories` idempotently. Existing records remain NULL, with no hash,
revision, content, or link rewrite. No files are created for envelopes.

For a binary downgrade, stop the service, back up the SQLite database, and leave
the additive columns intact. Older binaries ignore them, but cannot keep concept
metadata current during edits; do not expose that stale metadata on a later
upgrade without restoring a consistent backup or explicitly clearing it first.
If permanently abandoning the optional metadata, the offline
[memory-okf-v1-rollback.sql](memory-okf-v1-rollback.sql) drops only the two new
columns and preserves legacy content/revisions/client links. This removes OKF
metadata irreversibly; retain the backup. A later new binary recreates empty
columns. Do not run rollback SQL against a running service.

## Verification

`cargo test --workspace` covers storage, no-op/conflict/tombstone compatibility,
validation, and migration/rollback. Build the gateway and run
`python3 crates/gateway/tests/memory_okf_smoke.py /path/to/gateway` for authenticated
HTTP, legacy projection, project isolation, mixed records, precise unknown values,
and persistence across an actual server restart. All smoke data stays in an
isolated temporary database under `target/`.
