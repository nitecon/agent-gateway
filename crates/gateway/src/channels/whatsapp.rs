use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, OnceLock},
};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{
    channel::{ChannelPlugin, InboundMessage, OutboundMessage, PluginEvent},
    AppState,
};

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_GRAPH_BASE_URL: &str = "https://graph.facebook.com";
const DEFAULT_GRAPH_API_VERSION: &str = "v25.0";

#[derive(Debug, Clone)]
pub struct WhatsAppConfig {
    pub access_token: String,
    pub phone_number_id: String,
    pub webhook_verify_token: String,
    pub app_secret: Option<String>,
    pub graph_api_version: String,
    pub graph_base_url: String,
    pub project_rooms: HashMap<String, String>,
    pub default_recipient: Option<String>,
}

impl WhatsAppConfig {
    pub fn from_env() -> Result<Option<Self>> {
        let access_token = env_opt("WHATSAPP_ACCESS_TOKEN");
        let phone_number_id = env_opt("WHATSAPP_PHONE_NUMBER_ID");
        let webhook_verify_token = env_opt("WHATSAPP_WEBHOOK_VERIFY_TOKEN");
        let app_secret = env_opt("WHATSAPP_APP_SECRET");
        let project_rooms_raw = env_opt("WHATSAPP_PROJECT_ROOMS");
        let default_recipient = env_opt("WHATSAPP_DEFAULT_RECIPIENT");

        if access_token.is_none()
            && phone_number_id.is_none()
            && webhook_verify_token.is_none()
            && app_secret.is_none()
            && project_rooms_raw.is_none()
            && default_recipient.is_none()
        {
            return Ok(None);
        }

        let access_token =
            access_token.context("WHATSAPP_ACCESS_TOKEN is required when WhatsApp is enabled")?;
        let phone_number_id = phone_number_id
            .context("WHATSAPP_PHONE_NUMBER_ID is required when WhatsApp is enabled")?;
        let webhook_verify_token = webhook_verify_token
            .context("WHATSAPP_WEBHOOK_VERIFY_TOKEN is required when WhatsApp is enabled")?;
        let project_rooms = parse_project_rooms(project_rooms_raw.as_deref().unwrap_or(""))?;

        Ok(Some(Self {
            access_token,
            phone_number_id,
            webhook_verify_token,
            app_secret,
            graph_api_version: env_opt("WHATSAPP_GRAPH_API_VERSION")
                .unwrap_or_else(|| DEFAULT_GRAPH_API_VERSION.to_string()),
            graph_base_url: env_opt("WHATSAPP_GRAPH_BASE_URL")
                .unwrap_or_else(|| DEFAULT_GRAPH_BASE_URL.to_string()),
            project_rooms,
            default_recipient,
        }))
    }
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_project_rooms(value: &str) -> Result<HashMap<String, String>> {
    let mut rooms = HashMap::new();
    for entry in value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let (project_ident, room_id) = entry
            .split_once('=')
            .with_context(|| format!("invalid WHATSAPP_PROJECT_ROOMS entry '{entry}'"))?;
        let project_ident = project_ident.trim();
        let room_id = room_id.trim();
        if project_ident.is_empty() || room_id.is_empty() {
            bail!("invalid WHATSAPP_PROJECT_ROOMS entry '{entry}'");
        }
        rooms.insert(project_ident.to_string(), room_id.to_string());
    }
    Ok(rooms)
}

pub struct WhatsAppPlugin {
    config: WhatsAppConfig,
    rooms: Mutex<HashMap<String, Option<String>>>,
    seen_message_ids: Mutex<HashSet<String>>,
    tx: OnceLock<mpsc::Sender<PluginEvent>>,
    client: reqwest::Client,
}

impl WhatsAppPlugin {
    pub fn new(config: WhatsAppConfig) -> Self {
        Self {
            config,
            rooms: Mutex::new(HashMap::new()),
            seen_message_ids: Mutex::new(HashSet::new()),
            tx: OnceLock::new(),
            client: reqwest::Client::new(),
        }
    }

    pub fn webhook_verify_token(&self) -> &str {
        &self.config.webhook_verify_token
    }

    fn messages_endpoint(&self) -> String {
        format!(
            "{}/{}/{}/messages",
            self.config.graph_base_url.trim_end_matches('/'),
            self.config.graph_api_version.trim_start_matches('/'),
            self.config.phone_number_id
        )
    }

    fn text_payload(&self, to: &str, content: &str, context_message_id: Option<&str>) -> Value {
        let mut payload = json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": to,
            "type": "text",
            "text": {
                "preview_url": false,
                "body": content
            }
        });

        if let Some(message_id) = context_message_id {
            payload["context"] = json!({ "message_id": message_id });
        }

        payload
    }

    async fn send_text(
        &self,
        to: &str,
        content: &str,
        context_message_id: Option<&str>,
    ) -> Result<String> {
        let response = self
            .client
            .post(self.messages_endpoint())
            .bearer_auth(&self.config.access_token)
            .json(&self.text_payload(to, content, context_message_id))
            .send()
            .await
            .context("send WhatsApp message request")?;

        let status = response.status();
        let body = response.text().await.context("read WhatsApp response")?;
        if !status.is_success() {
            bail!("send WhatsApp message failed: status={status} body={body}");
        }

        let parsed: WhatsAppSendResponse =
            serde_json::from_str(&body).context("parse WhatsApp send response")?;
        parsed
            .messages
            .into_iter()
            .next()
            .map(|message| message.id)
            .context("WhatsApp send response did not include a message id")
    }

    fn verify_signature(&self, headers: &HeaderMap, body: &[u8]) -> Result<()> {
        let Some(app_secret) = &self.config.app_secret else {
            return Ok(());
        };

        let raw_signature = headers
            .get("x-hub-signature-256")
            .and_then(|value| value.to_str().ok())
            .context("missing X-Hub-Signature-256")?;
        let signature = raw_signature
            .strip_prefix("sha256=")
            .context("X-Hub-Signature-256 must start with sha256=")?;
        let signature_bytes = hex::decode(signature).context("decode X-Hub-Signature-256")?;

        let mut mac = HmacSha256::new_from_slice(app_secret.as_bytes())
            .context("create WhatsApp webhook HMAC")?;
        mac.update(body);
        mac.verify_slice(&signature_bytes)
            .context("invalid WhatsApp webhook signature")
    }

    async fn handle_webhook(&self, payload: WhatsAppWebhookPayload) -> Result<usize> {
        let tx = self
            .tx
            .get()
            .context("WhatsAppPlugin::start() must run before webhooks are accepted")?
            .clone();
        let mut sent = 0usize;

        for event in payload.into_events() {
            if !self.mark_seen(&event.id) {
                continue;
            }
            tx.send(PluginEvent::Message {
                channel_name: self.name().to_string(),
                room_id: event.room_id,
                message: InboundMessage {
                    id: event.id,
                    content: event.content,
                    sender: event.sender,
                },
            })
            .await
            .context("forward WhatsApp webhook message to inbound processor")?;
            sent += 1;
        }

        Ok(sent)
    }

    fn mark_seen(&self, message_id: &str) -> bool {
        self.seen_message_ids
            .lock()
            .unwrap()
            .insert(message_id.to_string())
    }
}

#[async_trait]
impl ChannelPlugin for WhatsAppPlugin {
    fn name(&self) -> &str {
        "whatsapp"
    }

    fn register_room(&self, room_id: &str, last_msg_id: Option<&str>) {
        self.rooms
            .lock()
            .unwrap()
            .entry(room_id.to_string())
            .or_insert_with(|| last_msg_id.map(String::from));
    }

    async fn start(&self, tx: mpsc::Sender<PluginEvent>) -> Result<()> {
        if self.tx.set(tx).is_err() {
            warn!("WhatsApp plugin start called more than once; keeping original webhook sender");
        }
        Ok(())
    }

    async fn ensure_room(&self, project_ident: &str) -> Result<String> {
        let room_id = self
            .config
            .project_rooms
            .get(project_ident)
            .or(self.config.default_recipient.as_ref())
            .with_context(|| {
                format!(
                    "no WhatsApp room configured for project '{project_ident}'; set WHATSAPP_PROJECT_ROOMS={project_ident}=<recipient-wa-id>"
                )
            })?
            .clone();
        self.register_room(&room_id, None);
        Ok(room_id)
    }

    async fn send(&self, room_id: &str, content: &str) -> Result<String> {
        self.send_text(room_id, content, None).await
    }

    async fn reply(
        &self,
        room_id: &str,
        reply_to_external_id: &str,
        content: &str,
    ) -> Result<String> {
        self.send_text(room_id, content, Some(reply_to_external_id))
            .await
    }

    async fn send_structured(&self, room_id: &str, msg: &OutboundMessage) -> Result<String> {
        self.send(room_id, &msg.render_markdown()).await
    }

    async fn reply_structured(
        &self,
        room_id: &str,
        reply_to_external_id: &str,
        msg: &OutboundMessage,
    ) -> Result<String> {
        self.reply(room_id, reply_to_external_id, &msg.render_markdown())
            .await
    }

    async fn fetch_since(
        &self,
        room_id: &str,
        after_id: Option<&str>,
    ) -> Result<Vec<InboundMessage>> {
        let _ = (room_id, after_id);
        Ok(Vec::new())
    }
}

pub async fn verify_webhook(
    State(state): State<AppState>,
    Query(query): Query<WhatsAppVerifyQuery>,
) -> Response {
    let Some(plugin) = state.whatsapp.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "WhatsApp plugin is not configured",
        )
            .into_response();
    };

    if query.mode.as_deref() == Some("subscribe")
        && query.verify_token.as_deref() == Some(plugin.webhook_verify_token())
    {
        return query.challenge.unwrap_or_default().into_response();
    }

    (
        StatusCode::FORBIDDEN,
        "invalid WhatsApp webhook verification token",
    )
        .into_response()
}

pub async fn receive_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(plugin) = state.whatsapp.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "WhatsApp plugin is not configured"})),
        )
            .into_response();
    };

    if let Err(err) = plugin.verify_signature(&headers, &body) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": format!("{err:#}")})),
        )
            .into_response();
    }

    let payload: WhatsAppWebhookPayload = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid WhatsApp webhook payload: {err}")})),
            )
                .into_response()
        }
    };

    match plugin.handle_webhook(payload).await {
        Ok(count) => {
            if count > 0 {
                info!("Accepted {count} WhatsApp webhook message(s)");
            }
            (
                StatusCode::OK,
                Json(json!({"received": true, "messages": count})),
            )
                .into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{err:#}")})),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppVerifyQuery {
    #[serde(rename = "hub.mode")]
    mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    challenge: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppSendResponse {
    #[serde(default)]
    messages: Vec<WhatsAppSentMessage>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppSentMessage {
    id: String,
}

#[derive(Debug, Deserialize)]
struct WhatsAppWebhookPayload {
    #[serde(default)]
    entry: Vec<WhatsAppEntry>,
}

impl WhatsAppWebhookPayload {
    fn into_events(self) -> Vec<WhatsAppInboundEvent> {
        let mut events = Vec::new();
        for entry in self.entry {
            for change in entry.changes {
                if change.field.as_deref() != Some("messages") {
                    continue;
                }
                let contact_names = change.value.contact_names();
                for message in change.value.messages {
                    let Some(content) = message.content() else {
                        warn!(
                            "Ignoring unsupported WhatsApp message type '{}' from {}",
                            message.message_type, message.from
                        );
                        continue;
                    };
                    let sender = contact_names
                        .get(&message.from)
                        .cloned()
                        .unwrap_or_else(|| message.from.clone());
                    events.push(WhatsAppInboundEvent {
                        id: message.id,
                        room_id: message.from,
                        sender,
                        content,
                    });
                }
            }
        }
        events
    }
}

#[derive(Debug, Deserialize)]
struct WhatsAppEntry {
    #[serde(default)]
    changes: Vec<WhatsAppChange>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppChange {
    field: Option<String>,
    #[serde(default)]
    value: WhatsAppChangeValue,
}

#[derive(Debug, Default, Deserialize)]
struct WhatsAppChangeValue {
    #[serde(default)]
    contacts: Vec<WhatsAppContact>,
    #[serde(default)]
    messages: Vec<WhatsAppMessage>,
}

impl WhatsAppChangeValue {
    fn contact_names(&self) -> HashMap<String, String> {
        self.contacts
            .iter()
            .map(|contact| {
                (
                    contact.wa_id.clone(),
                    contact
                        .profile
                        .as_ref()
                        .map(|profile| profile.name.clone())
                        .unwrap_or_else(|| contact.wa_id.clone()),
                )
            })
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct WhatsAppContact {
    wa_id: String,
    profile: Option<WhatsAppProfile>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppProfile {
    name: String,
}

#[derive(Debug, Deserialize)]
struct WhatsAppMessage {
    id: String,
    from: String,
    #[serde(rename = "type")]
    message_type: String,
    text: Option<WhatsAppText>,
    button: Option<WhatsAppButton>,
    interactive: Option<WhatsAppInteractive>,
}

impl WhatsAppMessage {
    fn content(&self) -> Option<String> {
        match self.message_type.as_str() {
            "text" => self.text.as_ref().map(|text| text.body.clone()),
            "button" => self.button.as_ref().map(|button| button.text.clone()),
            "interactive" => self
                .interactive
                .as_ref()
                .and_then(WhatsAppInteractive::content),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct WhatsAppText {
    body: String,
}

#[derive(Debug, Deserialize)]
struct WhatsAppButton {
    text: String,
}

#[derive(Debug, Deserialize)]
struct WhatsAppInteractive {
    #[serde(rename = "type")]
    interactive_type: String,
    button_reply: Option<WhatsAppInteractiveReply>,
    list_reply: Option<WhatsAppInteractiveReply>,
}

impl WhatsAppInteractive {
    fn content(&self) -> Option<String> {
        match self.interactive_type.as_str() {
            "button_reply" => self.button_reply.as_ref().map(|reply| reply.title.clone()),
            "list_reply" => self.list_reply.as_ref().map(|reply| reply.title.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct WhatsAppInteractiveReply {
    title: String,
}

#[derive(Debug, PartialEq, Eq)]
struct WhatsAppInboundEvent {
    id: String,
    room_id: String,
    sender: String,
    content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> WhatsAppConfig {
        WhatsAppConfig {
            access_token: "token".to_string(),
            phone_number_id: "phone-id".to_string(),
            webhook_verify_token: "verify".to_string(),
            app_secret: Some("secret".to_string()),
            graph_api_version: "v25.0".to_string(),
            graph_base_url: "https://graph.facebook.test".to_string(),
            project_rooms: HashMap::new(),
            default_recipient: None,
        }
    }

    #[test]
    fn project_rooms_parse_comma_separated_mapping() {
        let rooms = parse_project_rooms("agent-gateway=15551234567, demo = 15550001111").unwrap();

        assert_eq!(
            rooms.get("agent-gateway").map(String::as_str),
            Some("15551234567")
        );
        assert_eq!(rooms.get("demo").map(String::as_str), Some("15550001111"));
    }

    #[test]
    fn text_payload_adds_reply_context_when_present() {
        let plugin = WhatsAppPlugin::new(test_config());
        let payload = plugin.text_payload("15551234567", "hello", Some("wamid.reply"));

        assert_eq!(payload["messaging_product"], "whatsapp");
        assert_eq!(payload["to"], "15551234567");
        assert_eq!(payload["text"]["body"], "hello");
        assert_eq!(payload["context"]["message_id"], "wamid.reply");
    }

    #[test]
    fn webhook_payload_extracts_text_messages_with_contact_names() {
        let payload: WhatsAppWebhookPayload = serde_json::from_value(json!({
            "entry": [{
                "changes": [{
                    "field": "messages",
                    "value": {
                        "contacts": [{
                            "wa_id": "15551234567",
                            "profile": {"name": "Will"}
                        }],
                        "messages": [{
                            "from": "15551234567",
                            "id": "wamid.1",
                            "timestamp": "1770000000",
                            "type": "text",
                            "text": {"body": "ship it"}
                        }]
                    }
                }]
            }]
        }))
        .unwrap();

        assert_eq!(
            payload.into_events(),
            vec![WhatsAppInboundEvent {
                id: "wamid.1".to_string(),
                room_id: "15551234567".to_string(),
                sender: "Will".to_string(),
                content: "ship it".to_string(),
            }]
        );
    }

    #[test]
    fn signature_validation_accepts_meta_hmac_header() {
        let plugin = WhatsAppPlugin::new(test_config());
        let body = br#"{"entry":[]}"#;
        let mut mac = HmacSha256::new_from_slice(b"secret").unwrap();
        mac.update(body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let mut headers = HeaderMap::new();
        headers.insert("x-hub-signature-256", signature.parse().unwrap());

        plugin.verify_signature(&headers, body).unwrap();
    }
}
