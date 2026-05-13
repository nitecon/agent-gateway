# agent-gateway

Communication hub for AI agents. Provides standardized APIs for Discord, Slack, email, and other channels.

AI agents (Claude, Gemini, Codex, etc.) often run unattended for extended periods. agent-gateway gives them a persistent communication layer -- each project gets its own channel, messages are persisted in SQLite, and multiple agent sessions across different machines share the same channel.

## How it works

```
AI Agent (Claude Code, etc.)
    |  HTTP + Bearer token
    v
Gateway service             <-- one persistent service on your network
    |  Discord WebSocket / REST (+ future: Slack, email, etc.)
    v
Communication channel       <-- one channel per project (e.g. #bruce)
    |
    v
You (the user)
```

1. A client calls `POST /v1/projects` with the project's git remote URL or directory name.
2. The gateway ensures a channel exists for that project (creates it if not).
3. The client calls `POST /v1/projects/:ident/messages` -- the message appears in the channel as a rich embed (Discord) or a formatted markdown block (other plugins), with the agent ID and hostname in the byline and the body in a fenced code block.
4. You reply in the channel.
5. The client calls `GET /v1/projects/:ident/messages/unread` -- it receives everything since its last check.

Multiple machines running agents on the same project share the same channel and message history.

## Project layout

```
agent-gateway/
├── crates/
│   ├── gateway/        # Persistent HTTP service + channel plugins (Discord, etc.)
│   └── updater/        # Self-update library (GitHub releases)
```

> **Note:** Client tools (MCP server, sync CLI) have moved to [agent-tools](https://github.com/nitecon/agent-tools).

---

## Installation

### Install script (Linux)

The install script detects your platform, downloads the latest release, creates the systemd service, and sets up directories.

```bash
curl -fsSL https://raw.githubusercontent.com/nitecon/agent-gateway/main/install-gateway.sh | sudo bash
```

### Build from source

```bash
git clone https://github.com/nitecon/agent-gateway
cd agent-gateway
cargo build --release
```

Binary: `target/release/gateway`

---

## Prerequisites

- The gateway reachable from all machines running agent clients (LAN, VPN, or public host)
- A Discord account and server you control only if you want Discord-backed comms. Without Discord credentials, the gateway still starts and serves API/UI/Eventic routes, but channel send/receive is unavailable.

---

## Getting started

### 1. Create a Discord bot (optional)

1. Go to [discord.com/developers/applications](https://discord.com/developers/applications) and click **New Application**.
2. Under **Bot**, click **Add Bot**.
3. Copy the **Bot Token** -- you will need it for `DISCORD_BOT_TOKEN`.
4. Under **Privileged Gateway Intents**, enable:
   - **Server Members Intent** (optional but recommended)
   - **Message Content Intent** -- **required** for the bot to read message text
5. Under **OAuth2 -> URL Generator**, select scopes: `bot`. Select bot permissions:
   - `Manage Channels`
   - `Read Messages / View Channels`
   - `Send Messages`
   - `Read Message History`
6. Open the generated URL and add the bot to your Discord server.

---

### 2. Get your server and category IDs

Enable **Developer Mode** in Discord (Settings -> Advanced -> Developer Mode), then:

- **Guild ID**: Right-click your server name -> **Copy Server ID**.
- **Category ID** (optional): Right-click a category -> **Copy Category ID**. If set, all project channels are created inside this category.

---

### 3. Configure the gateway

Copy the example env file and fill in your values:

```bash
cp crates/gateway/.env.example crates/gateway/.env
```

```env
DISCORD_BOT_TOKEN=your-bot-token-here      # optional; omit to disable Discord
DISCORD_GUILD_ID=123456789012345678       # optional; required only with DISCORD_BOT_TOKEN
DISCORD_CATEGORY_ID=                  # optional -- leave blank for top-level channels
GATEWAY_API_KEY=choose-a-long-random-secret
GATEWAY_HOST=0.0.0.0
GATEWAY_PORT=7913
DATABASE_PATH=/opt/agentic/gateway/agent-gateway.db
MESSAGE_RETENTION_DAYS=30
RUST_LOG=info
```

> `GATEWAY_API_KEY` is the shared secret between the gateway and all clients. Use a long random string (e.g. `openssl rand -hex 32`).

When `DISCORD_BOT_TOKEN` and `DISCORD_GUILD_ID` are not both set, the Discord plugin is skipped at startup. Existing pages and non-channel APIs still run; attempts to send through a project whose channel plugin is unavailable return `503 Service Unavailable`.

---

### 4. Start the gateway

```bash
./gateway
# or from source: cargo run --release -p gateway
```

You should see:

```
INFO gateway: SQLite database opened at /opt/agentic/gateway/agent-gateway.db
INFO gateway: Gateway listening on http://0.0.0.0:7913
INFO gateway::discord: Discord bot connected as YourBotName#1234
```

The dashboard is available at `http://localhost:7913/` -- shows projects, message counts, and skills.

#### Systemd (Linux)

For a full production setup on Linux (dedicated service user, hardened unit file, journald logging) see **[docs/gateway-setup.md](docs/gateway-setup.md)**.

---

## Gateway API reference

All endpoints require `Authorization: Bearer <GATEWAY_API_KEY>`.

Agent-facing workflow context lives in `.agent/api/agent-gateway.yaml`. Before searching code for artifact, review, spec, docs, task, or pattern behavior, agents should run:

```bash
agent-tools docs search "agent-gateway artifact workflow"
agent-tools docs chunks --query "spec artifact generate tasks"
```

The API context is the canonical agent handoff surface for artifact workflows. It covers generic artifacts, design review rounds, spec manifests and task generation, artifact-backed API docs, idempotency/provenance headers, and the proposed `agent-tools docs export` contract.

### Messaging

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/projects` | Register a project. Creates the channel if needed. Idempotent. Body: `{"ident": "...", "channel": "discord"}` |
| `POST` | `/v1/projects/:ident/messages` | Send an agent message. Body: message envelope (see below). |
| `GET` | `/v1/projects/:ident/messages/unread` | Get unread messages for this agent. |
| `POST` | `/v1/projects/:ident/messages/:id/confirm` | Confirm (acknowledge) a message for this agent. |
| `POST` | `/v1/projects/:ident/messages/:id/reply` | Reply to a specific message (threaded). Body: message envelope (see below). |
| `POST` | `/v1/projects/:ident/messages/:id/action` | Post an action notice on a message. Body: message envelope (see below). |

#### Message envelope

The three sending endpoints (`messages`, `messages/:id/reply`, `messages/:id/action`) accept the same JSON envelope:

```json
{
  "body": "the message text (required)",
  "subject": "optional headline; auto-derived from the first line of body when omitted",
  "hostname": "optional origin host; defaults to the agent ID",
  "event_at": 1714000000000
}
```

- `body` is required. An empty or whitespace-only body returns `400 Bad Request`.
- `subject` is rendered as the embed title (Discord) or bold heading (markdown fallback). On `/action`, an unsupplied subject is auto-prefixed with `[ACTION]` so action posts stay visually distinct.
- `hostname` is shown in the byline alongside the agent ID. Defaults to the agent ID when omitted.
- `event_at` is the agent-claimed event time in epoch milliseconds. Distinct from the gateway-receive time stored in `sent_at`. Defaults to `now()`.

For backward compatibility the legacy field names `content` (on `/messages` and `/reply`) and `message` (on `/action`) are still accepted as aliases for `body`. If both are supplied, `body` wins.

#### Multi-agent support

All messaging endpoints accept an optional `X-Agent-Id` header. When provided, each agent gets its own unread queue — messages confirmed by one agent remain unread for others. If omitted, the agent identity defaults to `_default`. The agent identity (and the supplied or defaulted hostname) is shown in the byline of each outbound message — as the embed author on Discord, and as an italic `agent · hostname · timestamp` line on markdown-fallback channels.

### Skills

| Method | Path | Description |
|--------|------|-------------|
| `PUT` | `/v1/skills/:name` | Upload or replace a skill (raw zip bytes, `Content-Type: application/zip`). |
| `GET` | `/v1/skills` | List all skills. Returns `[{name, size, checksum, uploaded_at}]`. |
| `POST` | `/v1/skills` | Create or update a markdown command/agent with JSON `{name, kind, content}`. Intended for the control-panel editor. |
| `POST` | `/v1/skills/:name` | Create or update a markdown command/agent with JSON `{kind, content}`. |
| `GET` | `/v1/skills/:name` | Download a skill as a zip file. |
| `GET` | `/v1/skills/:name/content` | Fetch markdown content for a command/agent. |
| `DELETE` | `/v1/skills/:name` | Delete a skill. |

### Agent API docs

Project-scoped registry for agent-native API context. This is designed for
apps that often do not have OpenAPI/Swagger yet: publish docs-first structured
context directly, or import generated specs later as one source format.

> **Compatibility note.** API docs are moving onto the shared artifact
> substrate as a documentation artifact kind (`subkind = "api_context"`).
> The routes and `agent-tools docs *` commands documented below remain the
> canonical agent retrieval path and keep their current request/response
> shape; the underlying storage is becoming artifact + immutable
> artifact_version with version-anchored chunks, comments, and links.
> Existing API doc IDs are preserved. See
> [`docs/artifacts-api-docs-integration.md`](docs/artifacts-api-docs-integration.md)
> for the full mapping, and
> [`docs/artifact-operations-rollout.md`](docs/artifact-operations-rollout.md)
> for the production size limits, quotas, retention, backup/restore,
> metrics, migration phases, and rollback path that govern this
> transition.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/projects/:ident/api-docs` | List API context summaries. Supports `q`, `app`, `label`, and `kind` query filters. |
| `POST` | `/v1/projects/:ident/api-docs` | Publish a new API context document. Body includes `app`, `title`, `content`, optional `summary`, `kind`, `source_format`, `source_ref`, `version`, `labels`, and `author`. |
| `GET` | `/v1/projects/:ident/api-docs/chunks` | Return RAG-ready chunks generated from stored context. Supports the same filters as list. |
| `GET` | `/v1/projects/:ident/api-docs/:id` | Fetch one API context document with full JSON content. |
| `PATCH` | `/v1/projects/:ident/api-docs/:id` | Update API context metadata or content. Nullable fields such as `summary`, `source_ref`, and `version` can be set to `null`. |
| `DELETE` | `/v1/projects/:ident/api-docs/:id` | Delete one API context document. |

API doc list/get responses keep the legacy `kind` field and `kind` filter
semantics (`agent_context`, etc.). They also include additive artifact
compatibility fields: `artifact_id`, `artifact_version_id`, `subkind`,
`manifest_chunk_count`, `chunking_status`, and `linked_ids`.

`GET /v1/projects/:ident/api-docs/chunks` still returns the legacy JSON array
by default. Each chunk now also identifies its immutable source with
`artifact_id`, `artifact_version_id`, `accepted_version_id`, `child_address`,
`freshness` (`current`, `stale`, or `superseded_history`), and
`retrieval_scope`. Default retrieval is current-version-only and excludes
soft-superseded history. Pass `include_history=true` to include historical
chunks, and pass `envelope=true` to receive `{chunks, chunking_status,
retrieval_scope, include_history}` for restore/readiness checks. A partial or
stale chunking state is surfaced explicitly instead of silently succeeding.

Published API context versions store `structured_payload.manifest.chunk_count`
so restore validation can compare regenerated `artifact_chunks` against the
accepted immutable version. API context documentation artifacts carry
`retain:permanent` by default.

Docs can link through the artifact link model to specs (`target_kind:
"artifact"` for spec artifacts), tasks (`"task"`), patterns (`"pattern"`), and
repository refs. Source repository refs use `target_kind: "commit"` for commit
SHAs or `target_kind: "external_url"` for repository paths/URLs until a
first-class repository-path target kind exists.

The canonical payload is agent context, not OpenAPI. OpenAPI/Swagger should be
stored as `source_format: "openapi"` or `source_format: "swagger"` when it is
available, but agents should prefer enriched fields that capture intent,
workflows, auth expectations, safety constraints, cross-app relationships, and
copyable examples.

Example docs-first publish body:

```json
{
  "app": "billing-api",
  "title": "Billing API agent context",
  "summary": "System of record for invoices and customer billing state.",
  "kind": "agent_context",
  "source_format": "agent_context",
  "source_ref": ".agent/api/billing.yaml",
  "version": "2026-04-28",
  "labels": ["billing", "invoices"],
  "content": {
    "purpose": "Owns invoice state, invoice line items, and billing status.",
    "workflows": [
      {
        "name": "Create invoice",
        "steps": [
          "Look up the customer by external_id",
          "Create a draft invoice",
          "Add line items",
          "Finalize only after explicit user confirmation"
        ]
      }
    ],
    "auth": {
      "type": "bearer",
      "token_source": "project secret store"
    },
    "safety": {
      "destructive_operations": ["void invoice", "finalize invoice"]
    },
    "relationships": [
      {
        "app": "crm",
        "relationship": "CRM owns customer profile; billing owns invoice state."
      }
    ],
    "endpoints": [
      {
        "method": "POST",
        "path": "/v1/invoices",
        "intent": "Create a draft invoice for an existing customer."
      }
    ]
  }
}
```

### Generic artifacts

The artifact substrate exposes immutable-version resources under
`/v1/projects/:ident/artifacts`. It is guarded by
`GATEWAY_ARTIFACT_API_ENABLED`; when the flag is false the routes return
`503 artifact_api_disabled` and legacy task/docs workflows continue to work.
`GATEWAY_ARTIFACT_BODY_SCHEMA_ENABLED=false` can additionally disable
structured-payload/body-format writes while preserving markdown writes and
legacy fallbacks.

All mutating artifact endpoints require `X-Agent-Id`, `X-Agent-System`, and
`Idempotency-Key`. They return a `provenance` envelope with the resolved actor,
workflow run id when supplied, idempotency key, generated resource ids, replay
status, quota warnings, and
`authorization: {boundary: "trusted-single-tenant", required_scopes: [...]}`.
Trusted deployments keep this boundary by default. Set
`GATEWAY_ARTIFACT_AUTH_ENFORCED=true` to require `X-Agent-Project` to match the
route project and `X-Agent-Scopes` to include every declared scope; hardened
responses report `authorization.boundary = "project-scoped"`. Clients may use
exact scopes, comma/space separated scope lists, `<prefix>.*`, or `*`.
Requests with `X-Artifact-Quota-Override: true` additionally require
`project.administer`; hard quota limits are never bypassed. Size and quota
errors use stable codes from
`docs/artifact-operations-rollout.md`, for example
`artifact_version_body_too_large` and `quota_version_exceeded`.

Client and skill migration from scratch-file handoff to stable artifact IDs is
tracked in [`docs/artifact-client-skill-migration.md`](docs/artifact-client-skill-migration.md).
During migration, scratch files and task specifications are compatibility
mirrors; accepted artifact versions and manifest item IDs are canonical.

Read responses include `chunking_status` with `status`, current/stale/
superseded counts, and `failed_addresses`. Diffs are generated from stored full
version bodies for v1.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/projects/:ident/artifacts` | List artifacts. Supports `kind`, `subkind`, `lifecycle_state`, `label`, `actor_id`, and `q`. |
| `POST` | `/v1/projects/:ident/artifacts` | Create an artifact container. Body: `kind`, optional `subkind`, `title`, optional `labels`. |
| `GET` | `/v1/projects/:ident/artifacts/:artifact_id` | Fetch artifact detail with current/accepted versions and chunking freshness. |
| `GET` | `/v1/projects/:ident/artifacts/:artifact_id/versions` | List immutable versions. |
| `POST` | `/v1/projects/:ident/artifacts/:artifact_id/versions` | Create an immutable version. Optional `X-Workflow-Run-Id`; `parent_version_id` must reference the same artifact. |
| `GET` | `/v1/projects/:ident/artifacts/:artifact_id/versions/:version_id` | Fetch one version. |
| `GET` | `/v1/projects/:ident/artifacts/:artifact_id/versions/:version_id/diff` | Return a unified diff against `base_version_id` or the version parent. |
| `GET` | `/v1/projects/:ident/artifacts/:artifact_id/comments` | List comments for an artifact. |
| `POST` | `/v1/projects/:ident/artifacts/:artifact_id/comments` | Add a comment to an artifact, version, or contribution target. |
| `POST` | `/v1/projects/:ident/artifacts/:artifact_id/comments/:comment_id/resolve` | Resolve a comment. |
| `POST` | `/v1/projects/:ident/artifacts/:artifact_id/comments/:comment_id/reopen` | Reopen a resolved comment and add a note. |
| `GET` | `/v1/projects/:ident/artifacts/:artifact_id/contributions` | List contributions. |
| `POST` | `/v1/projects/:ident/artifacts/:artifact_id/contributions` | Add an immutable contribution. Synthesis/pass-2 contributions require `read_set`. |
| `GET` | `/v1/projects/:ident/artifacts/links` | List project-visible links. Supports link/source/target filters. |
| `POST` | `/v1/projects/:ident/artifacts/links` | Create a typed link. Audit-path link types must include required immutable version refs. |
| `GET` | `/v1/projects/:ident/artifacts/:artifact_id/workflow-runs` | List workflow runs for an artifact. |
| `POST` | `/v1/projects/:ident/artifacts/:artifact_id/workflow-runs` | Start a workflow run for idempotent multi-step mutations. |
| `GET` | `/v1/projects/:ident/artifacts/:artifact_id/workflow-runs/:workflow_run_id` | Fetch one workflow run. |
| `PATCH` | `/v1/projects/:ident/artifacts/:artifact_id/workflow-runs/:workflow_run_id` | Complete a workflow run as `succeeded`, `failed`, or `cancelled`. Failed resumable runs can be retried; cancelled runs are terminal. |

### Patterns

Global markdown pattern library for organization-wide practices. Patterns are
not project-scoped. They carry topical `labels` for search plus lifecycle
metadata: `version` (`draft`, `latest`, or `superseded`) and required `state`
(`active` or `archived`). Superseded patterns can point at their replacement
with `superseded_by`.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/patterns` | List pattern summaries. Supports `q`, `label`, `version`, `state`, and `superseded_by` query filters. |
| `POST` | `/v1/patterns` | Create a pattern. Body includes `title`, `body`, `labels`, `version`, `state`, optional `slug`/`summary`/`author`/`superseded_by`. |
| `GET` | `/v1/patterns/:id` | Fetch one pattern by id or slug. Returns markdown and metadata, intentionally without comments. |
| `PATCH` | `/v1/patterns/:id` | Update pattern metadata or markdown body. |
| `DELETE` | `/v1/patterns/:id` | Delete a pattern and its comments. |
| `GET` | `/v1/patterns/:id/comments` | Fetch the comment thread for one pattern. |
| `POST` | `/v1/patterns/:id/comments` | Add a comment. Body: `{"content":"...","author":"...","author_type":"agent|user|system"}`. |

Pattern comments are intentionally opt-in. Normal pattern pulls should use
`GET /v1/patterns/:id`; comments are collaboration/review state and should only
be fetched when a user asks an agent to address comments on that pattern.

### Eventic build status

Gateway can proxy client-local Eventic build information when Eventic web
consoles are configured under `/settings`. Projects keep their existing short
gateway identity (for example `eventic`) and can also store provider-aware repo
metadata (`github`, `gitlab`, `bitbucket`, etc. plus `namespace/repo`) for
matching Eventic's `/projects` output.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/eventic/servers` | List configured Eventic server entries. |
| `POST` | `/v1/eventic/servers` | Add a server. Body: `{name, base_url, enabled}`. Defaults usually point at `http://127.0.0.1:16384`. |
| `PUT` | `/v1/eventic/servers` | Replace the full server list. |
| `PATCH` | `/v1/eventic/servers/:id` | Update one configured server. |
| `DELETE` | `/v1/eventic/servers/:id` | Remove one configured server. |
| `GET` | `/v1/eventic/projects` | Aggregate `/projects` from enabled Eventic servers. |
| `PATCH` | `/v1/projects/:ident/repo` | Set a project's repo mapping. Body: `{provider, namespace, repo_name}`. |
| `POST` | `/v1/projects/repo-mappings/bulk` | Fill unmapped legacy projects with one provider/namespace and `repo_name = ident`. |
| `GET` | `/v1/projects/:ident/eventic` | Return the mapped project's current Eventic status or an actionable hint when mapping/server config is missing. |

### Dashboard

`GET /` -- no auth required. HTML page showing project counts, message stats, skill inventory, and build-status links for repo-mapped projects.

`GET /settings` -- no auth required. HTML settings page for Eventic server configuration and bulk repository mapping.

`GET /projects/:ident/build` -- no auth required. HTML build-status page backed by Eventic project output.

---

## Multi-machine setup

Point all client instances at the same gateway with the same project identity. They share:
- The same communication channel
- The same message history
- Per-agent unread queues (each agent ID tracks its own read state independently)

Skills on the gateway are also accessible from all machines.

---

## Client setup

The gateway is the server-side component. To connect AI agents to it, install [agent-tools](https://github.com/nitecon/agent-tools) on each dev machine:

```bash
curl -fsSL https://raw.githubusercontent.com/nitecon/agent-tools/refs/heads/main/install.sh | sudo bash
```

Then configure the gateway connection:

```bash
agent-tools init
```

This prompts for your gateway URL and API key, writing the config to `~/.agentic/config.toml`. Once configured:

- **MCP server** (`agent-tools-mcp`) — exposes code exploration tools AND communication tools (`set_identity`, `send_message`, `get_messages`, `confirm_read`) in a single MCP server. Register it once:
  ```bash
  claude mcp add -s user agent-tools -- /opt/agentic/bin/agent-tools-mcp
  ```

- **Sync CLI** (`agent-sync`) — push/pull skills, commands, and agent definitions to/from the gateway:
  ```bash
  agent-sync skills push ./my-skill/
  agent-sync sync --dir .
  ```

Both tools read the gateway connection from `~/.agentic/config.toml`, environment variables (`GATEWAY_URL`, `GATEWAY_API_KEY`), or CLI flags.

---

## Troubleshooting

**Bot doesn't receive user messages**
Ensure **Message Content Intent** is enabled in the Discord Developer Portal under your bot's settings. Without it, `message.content` is always empty.

**`get_messages` returns old messages repeatedly**
Each agent has its own unread queue keyed by the `X-Agent-Id` header (default `_default`). Confirm messages via `POST /v1/projects/:ident/messages/:id/confirm` to mark them as read for your agent. Different agents can read and confirm independently.

**Gateway can't create channels**
The bot needs **Manage Channels** permission in your Discord server. Re-invite it using the OAuth2 URL Generator with that permission checked.

**Client can't reach the gateway**
Check that `GATEWAY_URL` is correct and the gateway's port (default `7913`) is reachable from the machine running the client. On LAN, ensure your firewall allows the connection.
