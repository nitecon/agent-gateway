# Gateway Setup (Linux)

## Install

The install script handles everything: creates the `agentic` system user/group, sets up `/opt/agentic`, downloads the latest release, installs the systemd service, and creates a template config.

```bash
curl -fsSL https://raw.githubusercontent.com/nitecon/agent-gateway/main/install-gateway.sh | sudo bash
```

This will:
- Create the `agentic` system user and group
- Add all human users (uid >= 1000) to the `agentic` group
- Set up `/opt/agentic` with correct ownership (`agentic:agentic`)
- Download and install the gateway binary to `/opt/agentic/bin/gateway`
- Create `/opt/agentic/gateway/` for the database
- Install the systemd service and template config

## Configure the environment file

Edit `/etc/agent-gateway/gateway.env` and fill in your values:

```bash
sudo vim /etc/agent-gateway/gateway.env
```

Key settings (a full reference is at `.env.example` in the repository):

```ini
# Discord bot token from https://discord.com/developers/applications
DISCORD_BOT_TOKEN=

# The Guild (server) ID where project channels will be created
DISCORD_GUILD_ID=

# Optional: category channel ID to group project channels under
DISCORD_CATEGORY_ID=

# WhatsApp Cloud API. Leave empty to disable WhatsApp.
# Configure Meta webhooks to call https://<gateway-host>/webhooks/whatsapp.
WHATSAPP_ACCESS_TOKEN=
WHATSAPP_PHONE_NUMBER_ID=
WHATSAPP_WEBHOOK_VERIFY_TOKEN=

# Optional: validates X-Hub-Signature-256 on webhook POSTs.
WHATSAPP_APP_SECRET=

# Optional: override Graph API version/base URL.
WHATSAPP_GRAPH_API_VERSION=v25.0
# WHATSAPP_GRAPH_BASE_URL=https://graph.facebook.com

# Map project idents to WhatsApp recipient wa_id values.
# Example: WHATSAPP_PROJECT_ROOMS=agent-gateway=15551234567,other-project=15550001111
WHATSAPP_PROJECT_ROOMS=

# Optional fallback recipient for single-project deployments.
WHATSAPP_DEFAULT_RECIPIENT=

# Shared secret — MCP clients must send this in Authorization: Bearer <key>
GATEWAY_API_KEY=your-secret-key-here

# HTTP listen config
GATEWAY_HOST=0.0.0.0
GATEWAY_PORT=7913

# Database backend. SQLite is the implemented backend today; postgres/mariadb
# are reserved targets for the adapter and migration work.
DATABASE_BACKEND=sqlite

# SQLite database path
DATABASE_PATH=/opt/agentic/gateway/agent-gateway.db

# Future postgres/mariadb adapters will use DATABASE_URL.
# DATABASE_URL=postgres://gateway:secret@localhost/gateway
# DATABASE_URL=mysql://gateway:secret@localhost/gateway

# Delete acknowledged / agent-authored messages older than N days
MESSAGE_RETENTION_DAYS=30

# Delete bot and webhook messages (alerting, CI) older than N days
BOT_MESSAGE_RETENTION_DAYS=7

# Browser login for the control panel (on|off). Off is for loopback-only hosts.
# First visit to /login creates the first administrator (asks for GATEWAY_API_KEY).
GATEWAY_UI_AUTH=on

# Log level: error | warn | info | debug | trace
RUST_LOG=info
```

## Enable and start the service

```bash
sudo systemctl enable --now gateway
```

## Repository mappings and agent execution

Open **Settings → Agent execution** as an administrator. Set an absolute data
directory and an ordered list of clients and optional models. Claude Code and
Codex are discovered on the gateway service account's `PATH`; install and
authenticate them as that account. An empty gateway list tries Claude then Codex
using their configured default models. Repeating a client with another model
enables model fallback before switching clients.

The supplied systemd unit allows writes under `/opt/agentic` and hides home
directories. Prefer a data directory and service-account client configuration
under `/opt/agentic`. For mappings elsewhere, use a service override with the
required `ReadWritePaths`; adjust `ProtectHome` only if using a home-directory
checkout or client configuration. Ensure the service's `PATH` includes the
installed clients, `agent-tools`, `memory`, Git, and SSH before enabling execution.

For each project, open **Settings → Configure execution and view runs**:

1. Set an absolute local repository-root path for an existing checkout. This
   takes precedence over automatic checkout, including when checkout is prohibited.
2. Alternatively, explicitly allow checkout and provide an HTTPS or SSH clone
   URL. Unmapped repositories are cloned into the gateway data directory. A
   project without this permission is never cloned. An invalid local mapping
   fails execution rather than falling back to a clone.
3. Enable agent execution and select the task-receive trigger, a review cadence
   in seconds (60 minimum), or both. Project client/model lists override gateway
   defaults. Checkout and execution are disabled by default.

Execution runs on the gateway host in the resolved repository directory. Git
credentials, client credentials, `agent-tools`, and `memory` must be available to
the service account. Managed repositories retain local changes between attempts;
the gateway does not reset or automatically pull them. Moving the data directory
does not move existing clones.

The durable queue checks for work every five seconds while idle. Enabling the
task trigger also picks up existing unclaimed normal tasks, including delegated
target tasks and subtasks. Each task receives one automatic run. Cadence performs
a task-board review, starts immediately when first enabled, and skips projects
with queued or running work. One worker serializes runs across all repositories.
Archived projects and disabled triggers are skipped before execution.

Base instructions are visible on the execution settings page. They require the
agent to read repository rules, evaluate scope and feasibility, claim work,
verify changes, and close completed tickets. Agents use local project context
through `agent-tools` and `memory`; the prompt also supplies authoritative gateway
connection environment-variable names for direct API access when CLI configuration
differs. Credentials are supplied through the environment, never command arguments.

Each candidate gets up to 30 minutes. Missing executables, spawn failures,
nonzero exits, reported Claude errors, and timeouts advance to the next configured
client/model. Attempts retain bounded output in the run history. A successful
client exit does not itself close a ticket: an open task is recorded as
`needs_attention`. Exhausted fallback is `failed`; runs interrupted by a gateway
restart are `interrupted` and are not automatically replayed. Inspect the task and
repository before continuing manually or through cadence. Run one gateway process
per database, under the supplied systemd service so service shutdown also stops
child processes.

Codex uses its workspace-write sandbox with network access and noninteractive
approvals. Claude uses noninteractive permissions with Bash, Read, Edit, Write,
Glob, and Grep allowed. Repository instructions still apply; enabling execution
authorizes those clients to work in the selected checkout.

Task detail pages include **Subtasks and reviews**. Create a same-project subtask
or select another registered project for security review, performance testing,
or other specialist work. The target project's own execution policy controls
whether it runs automatically. Parent/child links and child statuses persist;
the agent should wait for required reviews before closing the parent.

API routes (bearer authentication; execution configuration/history requires an
administrator when using browser sessions):

- `GET/PUT /v1/execution/settings`: `data_directory`, ordered `candidates`.
- `GET /v1/execution/clients`: discovered executable paths or null.
- `GET /v1/execution/templates`: task-receive and cadence base instructions.
- `GET/PUT /v1/projects/:ident/execution`: `local_path`, `clone_url`,
  `allow_checkout`, `enabled`, `on_task_received`, `cadence_seconds`, `candidates`.
- `GET /v1/projects/:ident/execution/runs`: latest 100 runs and their attempts.
- `GET/POST /v1/projects/:ident/tasks/:id/subtasks`: list children or create one
  with `title`, optional `description`, `specification`, `labels`, and
  `target_project_ident` (defaults to the parent project).

The settings `PUT` routes replace the full configuration; omitted fields reset
to disabled/empty defaults. A candidate is `{"client":"claude","model":"MODEL"}`
or `{"client":"codex","model":null}`. Model names are passed through to the
client without maintaining a stale gateway model catalog.

Verification: `cargo test -p gateway`, then `cargo build -p gateway` and
`python3 crates/gateway/tests/execution_smoke.py`. The smoke test uses fake clients
and a local Git fixture under `target/`; it does not invoke paid models.

## Troubleshoot

Check logs:

```bash
journalctl -fu gateway
```
