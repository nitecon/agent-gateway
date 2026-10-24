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
- A Discord account and server you control only if you want Discord-backed comms. Without Discord credentials, the gateway still starts and serves the API and UI, but channel send/receive is unavailable.

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
WHATSAPP_ACCESS_TOKEN=                # optional; omit to disable WhatsApp
WHATSAPP_PHONE_NUMBER_ID=             # required with WHATSAPP_ACCESS_TOKEN
WHATSAPP_WEBHOOK_VERIFY_TOKEN=        # required for /webhooks/whatsapp verification
WHATSAPP_APP_SECRET=                  # optional; validates X-Hub-Signature-256
WHATSAPP_GRAPH_API_VERSION=v25.0
WHATSAPP_PROJECT_ROOMS=agent-gateway=15551234567
WHATSAPP_DEFAULT_RECIPIENT=
GATEWAY_API_KEY=choose-a-long-random-secret
GATEWAY_HOST=0.0.0.0
GATEWAY_PORT=7913
DATABASE_BACKEND=sqlite
DATABASE_PATH=/opt/agentic/gateway/agent-gateway.db
# DATABASE_URL=postgres://gateway:secret@localhost/gateway
# DATABASE_URL=mysql://gateway:secret@localhost/gateway
MESSAGE_RETENTION_DAYS=30
BOT_MESSAGE_RETENTION_DAYS=7
GATEWAY_UI_AUTH=on
RUST_LOG=info
```

> `GATEWAY_API_KEY` is the shared secret between the gateway and all clients. Use a long random string (e.g. `openssl rand -hex 32`).

> The control-panel pages are protected by per-person accounts. The first visit to
> `/login` creates the first administrator (it asks for the API key as proof you
> operate the gateway); after that people sign in with a username and password and
> administrators manage accounts under Settings. Agents keep using the shared API
> key. Pages never embed the key. Recovery on the host: `gateway user add <name>
> --admin` or `gateway user reset-password <name>` (password from
> `GATEWAY_USER_PASSWORD` or stdin). Set `GATEWAY_UI_AUTH=off` only for
> loopback-only deployments.

> Retention deletes messages older than `MESSAGE_RETENTION_DAYS` once an agent has
> acknowledged them (or they were authored by an agent or the gateway). Messages
> from Discord bots and webhooks (alerting, CI) are classified on ingest and purged
> after `BOT_MESSAGE_RETENTION_DAYS`. Human messages nobody has acknowledged are kept.

When `DISCORD_BOT_TOKEN` and `DISCORD_GUILD_ID` are not both set, the Discord plugin is skipped at startup. Existing pages and non-channel APIs still run; attempts to send through a project whose channel plugin is unavailable return `503 Service Unavailable`.

When WhatsApp is enabled, configure Meta webhooks to call `https://<gateway-host>/webhooks/whatsapp`. `WHATSAPP_PROJECT_ROOMS` maps gateway project idents to WhatsApp recipient `wa_id` values so inbound webhook senders and outbound project messages resolve to the same room.

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
| `POST` | `/v1/projects` | Register a project. Idempotent. Body: `{"ident": "...", "channel": "discord", "repo_url": "github.com/org/repo.git"}`. With `repo_url` the project is a `repo` project: its repository mapping is derived from the remote and its channel room is created immediately. Without it the project is `adhoc`: no room is created until the first message is sent. Re-registering an unmapped project with `repo_url` fills in the mapping. |
| `GET`/`POST` | `/v1/users` | List or create control-panel users (bearer key or admin session). Body: `{username, password, display_name?, role?}`. |
| `PATCH`/`DELETE` | `/v1/users/:id` | Update display name, role, `disabled`, or `password`; or delete. The last active admin is protected. |
| `GET` | `/v1/users/me` | The signed-in browser user. |
| `POST` | `/v1/users/me/password` | Change your own password. Body: `{current_password, new_password}`. |
| `POST` | `/v1/projects/:ident/archive` | Hide a project from default listings (data and URLs stay). |
| `POST` | `/v1/projects/:ident/restore` | Undo archive. |
| `PATCH` | `/v1/projects/:ident/repo` | Correct a project's repository mapping. Body: `{provider, namespace, repo_name}`; namespace and repo_name must be given together, both blank clears the mapping. |
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

### Tasks

Per-project kanban task board. Tasks move through three statuses — `todo`,
`in_progress`, and `done` — and back. Identity is resolved from the
`X-Agent-Id` header (falling back to `"user"` when absent or `_default`) and
can be overridden per request with an explicit `reporter`/`author` field.

Session-origin builds also accept `X-Agent-Session-Id` (UUID),
`X-Agent-Instance-Id` (UUID), `X-Agent-Provider` (`codex`/`claude`) and
`X-Agent-OS` (`linux`/`windows`/`macos`) together. Partial or malformed headers
return 400. Agent task creation (including delegated tasks and subtasks) requires
these origin headers; browser-created tasks without an agent remain unassigned.
An agent-created task is assigned to the submitting session in its initial
INSERT, including its first WebSocket snapshot. It remains `todo` with no start
time. `reporter` and `X-Agent-Id` display labels do not override that assignment.
Attributed claims persist `owner_origin`; status or owner changes then require
the exact claiming session and instance (409 otherwise), including requests
sharing the same `X-Agent-Id` and unattributed callers. Release/reopen to `todo`
and stale reclaim clear session ownership.

Lifecycle events and comments expose optional `origin` with `session_id`,
`instance_id`, `provider` and `os`. Completion events identify the completing
actor independently of the latest comment author. Automatic delegated source
result comments and completion retain the target worker's origin and commit
atomically with target completion. The protocol-v1 broadcast and receipt
contract is unchanged; see [task_events.rs](crates/gateway/src/task_events.rs)
for the wire contract.

Inactivity expiry runs every 30 seconds and also on task list/detail reads:

- **Stale reclaim (15 minutes).** An assigned `todo` or ordinary `in_progress`
  task with no comment or task action for 15 minutes becomes unclaimed `todo`.
  Any comment or task update resets the deadline through `updated_at`.
  The owner and start time are cleared, an audit comment is stored, and a durable
  `task_unclaimed` WebSocket event contains the unclaimed task snapshot. No board
  read is needed: expiry is delivered within the next 30-second maintenance tick.
  Repeated sweeps do not emit duplicate expiry events. Explicit releases also
  emit `task_unclaimed`. Credential-bound dispatch tasks retain their separate
  receipt/claim/start/handoff protocol and do not automatically transfer work.
- **Done fade (7d).** `done` tasks are hidden from list results once their
  `done_at` is older than 7 days. Pass `include_stale=true` to include them.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/projects/:ident/tasks` | List task summaries. `status` is a comma-separated filter (`todo,in_progress`); defaults to `todo,in_progress` when omitted. `include_stale=true` includes faded `done` tasks. Returns `[{id, title, status, rank, labels, owner_agent_id, hostname, reporter, comment_count, created_at, updated_at, kind, delegated_to_project_ident, delegated_to_task_id}]`. |
| `POST` | `/v1/projects/:ident/tasks` | Create a task. Body: `title` (required), optional `goal`, `description`, `specification` (alias `details`), `labels`, `hostname`, `reporter`. Returns the created task flattened with `specification` and a `hint` field. |
| `GET` | `/v1/projects/:ident/tasks/:id` | Fetch one task with its `comments[]` and a derived `actions[]` array (see below). |
| `PATCH` | `/v1/projects/:ident/tasks/:id` | Update a task. Body fields: `status`, `rank`, `title`, `labels`, plus nullable `goal`, `owner_agent_id`, `description`, `specification` (alias `details`), `hostname`. For the nullable fields, JSON `null` clears the column, a string sets it, and an absent key leaves it unchanged. |
| `POST` | `/v1/projects/:ident/tasks/:id/reassign` | Reassign an ordinary active task to a live registered session. Any authenticated calling agent may call it; the current owner is not required. Body: `{target: {origin: {session_id, instance_id, provider, os}, generation}}`. Returns the updated flat task and stores an audit comment. |
| `POST` | `/v1/projects/:ident/tasks/:id/comments` | Add a comment. Body: `content` (required), optional `author`, and `author_type` (`agent` or `user`; defaults from whether `X-Agent-Id` is present). |

#### Reassignment

Call `POST /v1/projects/:ident/tasks/:id/reassign` with the usual gateway bearer
and complete calling-agent origin headers. It is a regular authenticated API;
the caller can reassign another agent's task as well as its own. Choose an exact
live target from `GET /v1/projects/:ident/task-targets`. Ordinary target presence
uses the existing gateway connection and native session identity; it needs no
additional machine/session keys or operator provisioning. Expired, replaced,
unregistered, and other-project targets return 409. Existing credential-bound
dispatch registrations remain compatible with reassignment.
For example:

```json
{"target":{"origin":{"session_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","instance_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","provider":"codex","os":"linux"},"generation":"cccccccc-cccc-4ccc-8ccc-cccccccccccc"}}
```

Reassignment atomically sets `owner_origin` and `owner_agent_id` (the target's
session UUID), returns the task to `todo`, clears its start time, records the
calling agent in an audit comment, and resets inactivity expiry. The new owner
can then claim the task. Ordinary PATCH/status ownership checks continue to
prevent peers from accidentally claiming assigned work. Finished tasks must be
reopened first; dispatch-managed tasks use their existing dispatch handoff API.

#### Desktop routing and available agents

`POST /v1/projects/:ident/task-targets` advertises an actual native agent through
the usual gateway bearer connection and complete native origin headers. The
body is `{"generation":"<executor UUID>","canonical_remote":"<repository remote>"}`.
There are no additional credentials. The server checks that the remote matches
the project's registered canonical repository. Heartbeat at least every 60 seconds;
availability expires after 120 seconds. A new generation replaces the old one
for the same session/instance. Availability expiry does not release task ownership;
the separate 15-minute task activity deadline still applies.

The heartbeat returns `{target:{origin,generation},seen_at,expires_at,canonical_remote}`.
`GET` on the same endpoint requires only the usual gateway bearer and returns
`{targets:[{target,seen_at,expires_at}],canonical_remote}`. Times are Unix milliseconds;
no machine/session secrets are returned. Advertise real observed native identities;
do not derive them from labels, OS process discovery or a selected recipient.

Task creation accepts optional `routing`:

| Routing | Creation and notification behavior |
| --- | --- |
| omitted or `{"mode":"auto"}` | Existing behavior: agent submission reserves its own session; human submission creates an unclaimed task for ordinary project routing. |
| `{"mode":"unassigned"}` | One unclaimed TODO; no immediate task-created nudge or agent delivery. |
| `{"mode":"broadcast"}` | One unclaimed TODO; notify all matching workspace agents without claiming on their behalf. |
| `{"mode":"session","target":<directory target>}` | One TODO reserved for that exact live agent in the initial INSERT; no start time. |

Explicit non-auto modes require `canonical_remote` in the creation body, matching
the server project. A supplied remote is also checked for auto mode. A stale,
replaced or cross-project session target returns 409 before any task/event is
inserted. Human creation sends no agent origin headers; its event `origin` stays
null even when its `task.owner_origin` names the selected agent.

Task creation/detail/list responses and durable event task snapshots include
`routing`; existing tasks default to `{"mode":"auto"}`. Consumers apply this
notification intent to `task_created`, always enforcing repository and goal
matching. Queue-only events stay on the board; broadcast fans out locally to
matching agents. The existing WebSocket transports one event per task, not a
separate task/claim per recipient. Clients must implement these delivery modes;
older clients that ignore `routing` cannot provide queue-only/broadcast semantics.
Later events use current ownership/status: a `task_unclaimed` event after release
or inactivity can route normally regardless of the original session recipient.
Task claims retain the existing single-owner arbitration and 15-minute expiry.
The server does not schedule or start an executor.

#### Goal queues

Tasks can carry an optional `goal`, such as `release` or `authentication-refresh`.
Set it on creation or PATCH an existing task; strings are trimmed, and an empty
string or PATCH `null` removes the association. Names are case-sensitive, allow
at most 128 UTF-8 bytes, and cannot contain control characters. Task summaries, details, and
lifecycle event snapshots include `goal` (null for existing ungrouped tasks).
The board offers a goal filter, and creation/edit forms let operators set it.
Same-project subtasks inherit their parent's goal; cross-project subtasks start
ungrouped.

Agent task lists (requests with `X-Agent-Id` or session-origin headers) default
to ungrouped tasks. `?goal=release` selects that goal, `?goal=` selects ungrouped
tasks, and `?all_goals=true` explicitly includes every goal. Human requests
continue to list all goals by default. Status and stale-done filters still apply.

Agents can send `X-Task-Goal: release` as their queue context. It supplies the
list filter when `goal` is absent and the creation goal when the body omits it.
An explicit creation `goal: null` or empty string creates ungrouped work.
Claiming a goal task (PATCH or reorder into `in_progress`, or assigning an
owner) requires the matching context header; changing an existing goal also
requires that header. A mismatch returns 409. Existing exact session claim
ownership checks continue to apply. Goals provide explicit opt-in isolation;
they do not reserve an entire goal for one agent.

Goal tasks do not create project-wide "Task created" nudges. The task stream
still broadcasts durable events, including `task.goal`; terminal clients must
route goal events only to sessions that selected that goal. CLI/hook goal
selection and terminal delivery integration are maintained in their respective
client repositories.

#### Status transitions and `actions[]`

The task detail response includes a server-derived `actions[]` array alongside
`comments[]`. Each entry is `{verb, style, target_status}` where `style` is
`primary` or `secondary`. The available transitions per current status are:

| Current status | Actions (`verb` → `target_status`) |
|----------------|-------------------------------------|
| `todo` | `Claim` (primary) → `in_progress`; `Done` (primary) → `done` |
| `in_progress` | `Release` (secondary) → `todo`; `Done` (primary) → `done` |
| `done` | `Reopen` (secondary) → `todo` |

To perform a transition, `PATCH` the task with the chosen `target_status`.

#### Operator recovery of an orphaned claim

A crashed session or a change from legacy to native actor registration can
leave a claim whose exact owner can no longer release it. Comments from a
replacement session count as activity, so they delay automatic stale reclaim.
The gateway does not infer that an owner died from missing CMUX membership or
a disconnected client. Ordinary PATCH/status changes retain session ownership
checks; the explicit reassignment API above allows any authenticated calling
agent to choose a live replacement.

An authorized local operator with access to the gateway database can inspect
and recover a confirmed orphan. This uses the same local administration
boundary as `gateway user`; the shared bearer API does not expose recovery.

```bash
gateway task claim-info starxapi <task-id>
gateway task recover starxapi <task-id> \
  --expected-claim <claim_token-from-inspection> \
  --reason "Owner confirmed that the previous session is no longer available"
```

Use the server's `DATABASE_PATH`/database configuration when running these
commands. The token is an opaque snapshot guard, not an identity or permission.
If the claim or task activity changed since inspection, recovery refuses to
proceed; inspect and assess the current owner again. Recovery requires a reason
and an `in_progress` task, atomically clears its owner/session/start time,
returns it to `todo`, and adds a system audit comment with the previous claim.
It preserves specifications, previous comments, and delegation links. The
replacement session then uses ordinary `agent-tools tasks claim`. No old UUIDs
or launch/configuration identity overrides are needed.

### Generic artifacts

Gateway-backed API and agent documentation are retired. The upgrade deletes
legacy documentation and its artifact versions, chunks, links, and publication
runs in one transaction, preserving specs, reviews, tasks, patterns, actors, and
memory. Documentation routes are removed. Deploy the new binary before applying
the startup migration; older binaries depend on the legacy table. The migration
is idempotent and rejects references from retained records instead of deleting
them. Historical feature plans below do not reinstate documentation support.

The artifact substrate exposes immutable-version resources under
`/v1/projects/:ident/artifacts`. The artifact API supports `spec` and `design_review` containers with markdown bodies. The artifact API is always enabled.
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

### Project registry

Projects are identified by a short ident (the repository or directory name).
When a client registers with `repo_url`, the gateway derives `repo_provider`
(`github`, `gitlab`, `bitbucket`, or the bare host for self-hosted remotes),
`repo_namespace`, `repo_name`, and `repo_full_name`, and records the normalized
`canonical_remote`. Projects registered from a bare directory are `adhoc`, get
no channel room until they first send a message, and can be archived from
`/settings`. There is no CI integration; link out to your own CI from the
project instead.

### Control panel

All HTML pages require a signed-in user. `/login` creates the first
administrator on a fresh gateway (API key required) and then accepts username
and password; sessions are HttpOnly cookies bound to the account, so a password
change or disable signs that user out everywhere. Browser actions (replies,
resolves, comments) are attributed to the signed-in user, while agents keep the
shared API key. `GATEWAY_UI_AUTH=off` disables the login for loopback-only
hosts. Pages never embed the API key.

| Page | What it is for |
|------|----------------|
| `/` | Home inbox: what needs a human (latest agent updates, unacknowledged questions, stalled tasks, artifacts awaiting a decision), agents at work, recent activity. Every row has an Open and, where it applies, a Resolve action. |
| `/activity` | Agents seen in the last 24 hours with their current task, plus the latest events across projects. |
| `/projects` | Project registry: kind (repo/adhoc), channel room, open inbox items, tasks, last activity, archive/restore. Filter by state and kind. |
| `/projects/:ident` | Project overview: inbox counts, in-progress tasks, recent artifacts, links, agents, activity. |
| `/projects/:ident/inbox` | Threaded conversation view: filter by author kind (conversation, humans, agents, alerts, system) and state; read a thread; reply from the browser (posted to the channel and to every agent's unread queue); resolve one or many threads. |
| `/projects/:ident/tasks` | Kanban board (todo / in progress / done, drag-and-drop). `/tasks` shows the same board with a project switcher, and `/projects/:ident/tasks/:id` is the task detail. `/task-link/:prefix` redirects to the detail page. |
| `/projects/:ident/artifacts`, `/projects/:ident/memories` | Artifact and memory views inside the project tab strip. |
| `/projects/:ident/settings` | Identity, repository mapping, channel room, external links (CI, runbooks), archive. |
| `/patterns`, `/skills`, `/commands`, `/agents` | Library pages (unchanged). |
| `/settings` | Gateway-level: your account and password, user management (admins), version, channel plugins, retention windows, theme. |
| `/task-stream` (also `/execution`) | Administrator task lifecycle timeline across projects, comments and outcome notes, delivery receipts, and connected clients. Filter by project/task; updates every three seconds. Previous executions remain visible as read-only history. |

Task creation, comments and completion are published across projects through one
native bearer-authenticated WebSocket at `/v1/tasks/stream`. Events have durable
ordered IDs and resumable consumer cursors. Clients acknowledge receipt separately
from terminal delivery; neither acknowledgment claims nor completes a task.
Add progress/outcome comments and mark the canonical task done through agent-tools.

cmux owns repository matching and optional direct injection into active attached
Claude/Codex terminals, behind a global experimental approval. Per-project
execution settings, gateway-host launching, cadence, fallback clients, and native
session hook requirements are removed. Existing execution records and summaries
are preserved; unfinished legacy executions become `retired` without resuming
work. cmux clients must implement the new stream subscription protocol; the old
`/v1/execution/connect` assignment protocol is removed.

See [task_events.rs](crates/gateway/src/task_events.rs) for the wire contract and
`cargo build -p gateway && python3 crates/gateway/tests/task_stream_smoke.py` for an
isolated real-WebSocket lifecycle, replay, authentication and migration smoke.
The smoke runs in CI and launches no real agents.

Keyboard: `g h/a/p/t/s` jumps between sections, `j`/`k` move the selection,
Enter opens it, `r` focuses the reply box, `e` resolves, `?` shows the help
overlay. Home, Activity, and the Inbox refresh themselves while idle.

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
