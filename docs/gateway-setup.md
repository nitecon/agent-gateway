# Gateway Setup (Linux)

## Setup

To set up claude-mail on linux we first have to create the user for the service to run as use the following:

```bash
useradd -m -d /etc/cmail --shell /bin/false cmail
mkdir -p /etc/cmail && vim /etc/cmail/gateway.env
chown -R cmail:cmail /etc/cmail
mkdir -p /var/lib/claude-mail
chown -R cmail:cmail /var/lib/claude-mail
```

As seen above we also create the var lib directory for app storage.

## Copy the downloaded binaries

Under the releases directory you will now need to download the latest release of claude-mail from the github repository like so:

```bash
wget https://github.com/nitecon/claude-mail/releases/download/v0.1.1/claude-mail-v0.1.1-x86_64-unknown-linux-gnu.tar.gz
tar xf claude-mail-v0.1.1-x86_64-unknown-linux-gnu.tar.gz
sudo cp -f claude-mail-v0.1.1-x86_64-unknown-linux-gnu/claude-mail /usr/local/bin/
sudo cp -f claude-mail-v0.1.1-x86_64-unknown-linux-gnu/claude-mail-gateway /usr/local/bin/
```

## System-D Setup

Now we have to setup the systemd service file with:

```bash
sudo vim /etc/systemd/system/claude-mail-gateway.service
```

Paste the following in to make sure we have a proper service file (adjust to handle your changes...)

```ini
[Unit]
Description=claude-mail Gateway
Documentation=https://github.com/nitecon/claude-mail
After=network.target

[Service]
Type=simple
User=cmail
Group=cmail

EnvironmentFile=/etc/cmail/gateway.env

ExecStart=/usr/local/bin/claude-mail-gateway

Restart=on-failure
RestartSec=5
TimeoutStopSec=10

# Logging goes to the system journal: journalctl -u claude-mail-gateway -f
StandardOutput=journal
StandardError=journal
SyslogIdentifier=claude-mail-gateway

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/claude-mail

[Install]
WantedBy=multi-user.target
```

## Configure systemd

Now we will reload and install it to restart on boot

```bash
sudo systemctl daemon-reload
sudo systemctl enable claude-mail-gateway.service
sudo systemctl restart claude-mail-gateway
```

## Troubleshoot

To validate it is working and to look at the logs run the following:

```bash
journalctl -fu claude-mail-gateway
```
