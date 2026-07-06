use anyhow::{Context, Result};
use async_trait::async_trait;
use serenity::{
    all::{
        ChannelId, ChannelType, Context as SerenityCtx, CreateChannel, CreateEmbed,
        CreateEmbedAuthor, CreateMessage, Embed, EventHandler, GatewayIntents, GetMessages,
        GuildChannel, GuildId, Message, MessageId, MessageReference, MessageReferenceKind, Ready,
        Timestamp, UserId,
    },
    Client,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::channel::{ChannelPlugin, InboundMessage, OutboundMessage, PluginEvent};

// ── Embed limits ──────────────────────────────────────────────────────────────

/// Discord embed title limit (per API docs).
const EMBED_TITLE_MAX: usize = 256;
/// Discord embed description limit. We reserve room for the surrounding
/// triple-backtick fence and an ellipsis.
const EMBED_BODY_MAX: usize = 4000;
/// Brand color for agent posts (Discord blurple).
const EMBED_COLOR: u32 = 0x5865F2;

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn build_embed(msg: &OutboundMessage) -> CreateEmbed {
    let title = truncate_chars(&msg.subject, EMBED_TITLE_MAX);
    // Sanitize triple-backticks so user content cannot escape the fence.
    let safe_body = msg.body.replace("```", "``\u{200B}`");
    let body = truncate_chars(&safe_body, EMBED_BODY_MAX);
    let description = format!("```\n{}\n```", body);
    let author_name = format!("{} · {}", msg.agent_id, msg.hostname);

    let mut embed = CreateEmbed::new()
        .title(title)
        .description(description)
        .author(CreateEmbedAuthor::new(author_name))
        .color(EMBED_COLOR);

    if let Ok(ts) = Timestamp::from_unix_timestamp(msg.event_at / 1000) {
        embed = embed.timestamp(ts);
    }
    embed
}

fn is_gateway_authored(author_id: u64, bot_id: Option<u64>) -> bool {
    bot_id == Some(author_id)
}

fn push_trimmed_part(parts: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        parts.push(trimmed.to_string());
    }
}

fn render_embed(embed: &Embed) -> Option<String> {
    let mut parts = Vec::new();

    if let Some(author) = &embed.author {
        push_trimmed_part(&mut parts, &author.name);
    }
    if let Some(title) = &embed.title {
        push_trimmed_part(&mut parts, title);
    }
    if let Some(description) = &embed.description {
        push_trimmed_part(&mut parts, description);
    }

    for field in &embed.fields {
        let name = field.name.trim();
        let value = field.value.trim();
        match (name.is_empty(), value.is_empty()) {
            (true, true) => {}
            (true, false) => parts.push(value.to_string()),
            (false, true) => parts.push(name.to_string()),
            (false, false) => parts.push(format!("{name}: {value}")),
        }
    }

    if let Some(footer) = &embed.footer {
        push_trimmed_part(&mut parts, &footer.text);
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn discord_message_content_from_parts(content: &str, embeds: &[Embed]) -> String {
    let mut parts = Vec::new();
    push_trimmed_part(&mut parts, content);
    for embed in embeds {
        if let Some(rendered) = render_embed(embed) {
            parts.push(rendered);
        }
    }
    parts.join("\n\n")
}

fn discord_message_content(msg: &Message) -> String {
    discord_message_content_from_parts(&msg.content, &msg.embeds)
}

// ── Config ────────────────────────────────────────────────────────────────────

pub struct DiscordConfig {
    pub token: String,
    pub guild_id: u64,
    pub category_id: Option<u64>,
}

// ── Plugin ────────────────────────────────────────────────────────────────────

/// Rooms known to this plugin: Discord channel ID (u64) → last seen message ID.
type RoomMap = Arc<Mutex<HashMap<u64, Option<String>>>>;

pub struct DiscordPlugin {
    config: DiscordConfig,
    /// channel_id → Option<last_msg_id>  (populated via register_room, persisted in DB)
    rooms: RoomMap,
    /// Set after start() is called; used for REST operations.
    http: OnceLock<Arc<serenity::http::Http>>,
    /// Set by the gateway ready event; used to avoid forwarding our own posts.
    bot_id: Arc<OnceLock<UserId>>,
}

impl DiscordPlugin {
    pub fn new(config: DiscordConfig) -> Self {
        Self {
            config,
            rooms: Arc::new(Mutex::new(HashMap::new())),
            http: OnceLock::new(),
            bot_id: Arc::new(OnceLock::new()),
        }
    }

    fn guild(&self) -> GuildId {
        GuildId::new(self.config.guild_id)
    }

    fn category(&self) -> Option<ChannelId> {
        self.config.category_id.map(ChannelId::new)
    }

    fn http(&self) -> &Arc<serenity::http::Http> {
        self.http
            .get()
            .expect("DiscordPlugin::start() must be called before HTTP operations")
    }
}

// ── ChannelPlugin impl ────────────────────────────────────────────────────────

#[async_trait]
impl ChannelPlugin for DiscordPlugin {
    fn name(&self) -> &str {
        "discord"
    }

    fn register_room(&self, room_id: &str, last_msg_id: Option<&str>) {
        if let Ok(id) = room_id.parse::<u64>() {
            self.rooms
                .lock()
                .unwrap()
                .entry(id)
                .or_insert_with(|| last_msg_id.map(String::from));
        }
    }

    async fn start(&self, tx: mpsc::Sender<PluginEvent>) -> Result<()> {
        let handler = DiscordHandler {
            rooms: self.rooms.clone(),
            tx,
            bot_id: self.bot_id.clone(),
        };

        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT
            | GatewayIntents::GUILDS;

        let mut client = Client::builder(&self.config.token, intents)
            .event_handler(handler)
            .await
            .context("build Discord client")?;

        // Store HTTP handle for REST calls from other trait methods.
        let _ = self.http.set(client.http.clone());

        tokio::spawn(async move {
            if let Err(e) = client.start().await {
                error!("Discord gateway error: {e}");
            }
        });

        Ok(())
    }

    async fn ensure_room(&self, project_ident: &str) -> Result<String> {
        let http = self.http();
        let guild = self.guild();

        // Check if channel already exists.
        let channels: HashMap<ChannelId, GuildChannel> =
            guild.channels(http).await.context("fetch guild channels")?;

        if let Some(ch) = channels.values().find(|c| c.name == project_ident) {
            let room_id = ch.id.get().to_string();
            self.register_room(&room_id, None);
            return Ok(room_id);
        }

        // Create it.
        let mut builder = CreateChannel::new(project_ident).kind(ChannelType::Text);
        if let Some(cat) = self.category() {
            builder = builder.category(cat);
        }

        let channel = guild.create_channel(http, builder).await.map_err(|e| {
            error!("Discord channel creation failed for '{project_ident}': {e}");
            anyhow::anyhow!("create Discord channel: {e}")
        })?;

        let room_id = channel.id.get().to_string();
        self.register_room(&room_id, None);
        info!("Created Discord channel #{project_ident} (id={room_id})");
        Ok(room_id)
    }

    async fn send(&self, room_id: &str, content: &str) -> Result<String> {
        let id: u64 = room_id.parse().context("parse Discord channel id")?;
        let ch = ChannelId::new(id);
        let msg = ch
            .send_message(self.http(), CreateMessage::new().content(content))
            .await
            .context("send Discord message")?;
        Ok(msg.id.to_string())
    }

    async fn reply(
        &self,
        room_id: &str,
        reply_to_external_id: &str,
        content: &str,
    ) -> Result<String> {
        let id: u64 = room_id.parse().context("parse Discord channel id")?;
        let reply_to: u64 = reply_to_external_id
            .parse()
            .context("parse reply-to message id")?;
        let ch = ChannelId::new(id);
        let msg = ch
            .send_message(
                self.http(),
                CreateMessage::new()
                    .content(content)
                    .reference_message((ch, MessageId::new(reply_to))),
            )
            .await
            .context("send Discord reply")?;
        Ok(msg.id.to_string())
    }

    async fn send_structured(&self, room_id: &str, msg: &OutboundMessage) -> Result<String> {
        let id: u64 = room_id.parse().context("parse Discord channel id")?;
        let ch = ChannelId::new(id);
        let sent = ch
            .send_message(self.http(), CreateMessage::new().embed(build_embed(msg)))
            .await
            .context("send Discord embed")?;
        Ok(sent.id.to_string())
    }

    async fn reply_structured(
        &self,
        room_id: &str,
        reply_to_external_id: &str,
        msg: &OutboundMessage,
    ) -> Result<String> {
        let id: u64 = room_id.parse().context("parse Discord channel id")?;
        let reply_to: u64 = reply_to_external_id
            .parse()
            .context("parse reply-to message id")?;
        let ch = ChannelId::new(id);
        let reference = MessageReference::new(MessageReferenceKind::Default, ch)
            .message_id(MessageId::new(reply_to));
        let sent = ch
            .send_message(
                self.http(),
                CreateMessage::new()
                    .embed(build_embed(msg))
                    .reference_message(reference),
            )
            .await
            .context("send Discord embed reply")?;
        Ok(sent.id.to_string())
    }

    async fn fetch_since(
        &self,
        room_id: &str,
        after_id: Option<&str>,
    ) -> Result<Vec<InboundMessage>> {
        let id: u64 = room_id.parse().context("parse Discord channel id")?;
        let ch = ChannelId::new(id);

        let builder = match after_id {
            Some(after) => {
                let snowflake: u64 = after.parse().context("parse after_id snowflake")?;
                GetMessages::new()
                    .after(MessageId::new(snowflake))
                    .limit(100)
            }
            None => GetMessages::new().limit(100),
        };

        let msgs = ch
            .messages(self.http(), builder)
            .await
            .context("fetch Discord messages")?;

        Ok(msgs
            .into_iter()
            .filter(|m| {
                !is_gateway_authored(
                    m.author.id.get(),
                    self.bot_id.get().map(|bot_id| bot_id.get()),
                )
            })
            .map(|m| {
                let content = discord_message_content(&m);
                InboundMessage {
                    id: m.id.to_string(),
                    content,
                    sender: m.author.name,
                }
            })
            .collect())
    }
}

// ── Event handler ─────────────────────────────────────────────────────────────

struct DiscordHandler {
    /// Shared room map for this gateway session.
    rooms: RoomMap,
    tx: mpsc::Sender<PluginEvent>,
    bot_id: Arc<OnceLock<UserId>>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, ctx: SerenityCtx, ready: Ready) {
        let _ = self.bot_id.set(ready.user.id);
        info!("Discord bot connected as {}", ready.user.name);

        // Clone room map before any await so we never hold the guard across one.
        let rooms: Vec<(u64, Option<String>)> =
            self.rooms.lock().unwrap().clone().into_iter().collect();

        let bot_id = self.bot_id.get().map(|id| id.get());

        // ── Backfill missed messages ──────────────────────────────────────────
        for (channel_id, last_msg_id) in &rooms {
            let after = match last_msg_id.as_deref().and_then(|s| s.parse::<u64>().ok()) {
                Some(id) => id,
                None => continue,
            };

            let ch = ChannelId::new(*channel_id);
            match ch
                .messages(
                    &ctx.http,
                    GetMessages::new().after(MessageId::new(after)).limit(100),
                )
                .await
            {
                Ok(msgs) => {
                    for msg in msgs
                        .into_iter()
                        .filter(|m| !is_gateway_authored(m.author.id.get(), bot_id))
                    {
                        let content = discord_message_content(&msg);
                        let event = PluginEvent::Message {
                            channel_name: "discord".into(),
                            room_id: channel_id.to_string(),
                            message: InboundMessage {
                                id: msg.id.to_string(),
                                content,
                                sender: msg.author.name,
                            },
                        };
                        if self.tx.send(event).await.is_err() {
                            return; // gateway shut down
                        }
                    }
                }
                Err(e) => warn!("backfill error for channel {channel_id}: {e}"),
            }
        }
    }

    async fn message(&self, ctx: SerenityCtx, msg: Message) {
        if is_gateway_authored(
            msg.author.id.get(),
            self.bot_id.get().map(|bot_id| bot_id.get()),
        ) {
            return;
        }

        let channel_id = msg.channel_id.get();

        // Drop the guard before any await.
        let is_known = self.rooms.lock().unwrap().contains_key(&channel_id);
        if !is_known {
            // If the bot is @mentioned in a non-project channel, tell the user.
            let bot_mentioned = self
                .bot_id
                .get()
                .map(|id| msg.mentions.iter().any(|u| u.id == *id))
                .unwrap_or(false);
            if bot_mentioned {
                if let Err(e) = msg
                    .channel_id
                    .send_message(
                        &ctx.http,
                        CreateMessage::new()
                            .content("This is not a project channel — I don't forward mail here."),
                    )
                    .await
                {
                    warn!("failed to send non-project reply: {e}");
                }
            }
            return;
        }

        let content = discord_message_content(&msg);
        let event = PluginEvent::Message {
            channel_name: "discord".into(),
            room_id: channel_id.to_string(),
            message: InboundMessage {
                id: msg.id.to_string(),
                content,
                sender: msg.author.name,
            },
        };

        if let Err(e) = self.tx.send(event).await {
            error!("failed to forward Discord message to inbound processor: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gateway_authored_filter_only_matches_self() {
        assert!(is_gateway_authored(42, Some(42)));
        assert!(!is_gateway_authored(7, Some(42)));
        assert!(!is_gateway_authored(42, None));
    }

    #[test]
    fn discord_message_content_uses_embed_when_body_is_empty() {
        let embed: Embed = serde_json::from_value(json!({
            "author": {"name": "Alertmanager"},
            "title": "FIRING KubeDaemonSetMisScheduled",
            "description": "1 Pods of DaemonSet rook-ceph/nodeplugin are running where they are not supposed to run.",
            "fields": [
                {"name": "severity", "value": "warning", "inline": true}
            ],
            "footer": {"text": "cluster monitoring"}
        }))
        .unwrap();

        assert_eq!(
            discord_message_content_from_parts("", &[embed]),
            "Alertmanager\nFIRING KubeDaemonSetMisScheduled\n1 Pods of DaemonSet rook-ceph/nodeplugin are running where they are not supposed to run.\nseverity: warning\ncluster monitoring"
        );
    }

    #[test]
    fn discord_message_content_preserves_body_and_embed() {
        let embed: Embed = serde_json::from_value(json!({
            "title": "Alert details",
            "description": "failure rate is 8.333%"
        }))
        .unwrap();

        assert_eq!(
            discord_message_content_from_parts("[FIRING]", &[embed]),
            "[FIRING]\n\nAlert details\nfailure rate is 8.333%"
        );
    }
}
