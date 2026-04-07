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

# Shared secret — MCP clients must send this in Authorization: Bearer <key>
GATEWAY_API_KEY=your-secret-key-here

# HTTP listen config
GATEWAY_HOST=0.0.0.0
GATEWAY_PORT=7913

# SQLite database path
DATABASE_PATH=/opt/agentic/gateway/agent-gateway.db

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
