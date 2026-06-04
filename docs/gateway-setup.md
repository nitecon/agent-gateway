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

# Delete messages older than N days that are behind the read cursor
MESSAGE_RETENTION_DAYS=30

# Log level: error | warn | info | debug | trace
RUST_LOG=info
```

## Enable and start the service

```bash
sudo systemctl enable --now gateway
```

## Troubleshoot

Check logs:

```bash
journalctl -fu gateway
```
