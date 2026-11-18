//! Discord webhook mode — the official alternative to the user-token
//! (self-bot) path.
//!
//! A webhook is bound to one channel, needs no bot invite and no
//! gateway, and sends with `POST /api/webhooks/{id}/{token}`. The URL
//! token IS the credential: there is no Authorization header, the full
//! URL must never be logged or displayed, and it is stored encrypted
//! like the user token (see crypto.rs / store.rs).
//!
//! What webhooks cannot do (by Discord's design, not a Nicord
//! limitation): prefix/slash commands, typing indicators, reading
//! messages, or picking a channel at send time. The engine rejects
//! command nodes in webhook mode instead of pretending they work.

use crate::discord::{
    discord_error_from_status, reqwest_error, AllowedMentions, AttachedFile, DiscordClient,
    DiscordError, SendOptions, SentMessage,
};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

/// A parsed Discord webhook URL. The `token` is a SECRET: this struct
/// has a manual Debug impl that never shows it, and no Display impl.
/// Use `masked_url()` for anything shown to the user or written to logs.
#[derive(Clone)]
pub struct WebhookUrl {
    /// Numeric snowflake, safe to display.
    pub id: String,
    /// Last up-to-4 chars of the token (char-safe), for masked display.
    /// The full token lives only inside `base_url` / `execute_url`,
    /// which are never displayed or logged.
    token_tail: String,
    /// "discord.com" or "discordapp.com", lowercased.
    pub host: String,
    /// `https://{host}/api/webhooks/{id}/{token}` (no query). Used for GET.
    base_url: String,
    /// `base_url + "?wait=true"`. Used for POST (returns the message).
    execute_url: String,
}

impl std::fmt::Debug for WebhookUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookUrl")
            .field("id", &self.id)
            .field("host", &self.host)
            .field("token", &"****")
            .finish()
    }
}

impl WebhookUrl {
    /// Display form: host + id + last 4 chars of the token only.
    /// e.g. "https://discord.com/api/webhooks/123456789/****abcd"
    /// (Kept as the single safe display API; covered by unit tests.)
    #[allow(dead_code)]
    pub fn masked_url(&self) -> String {
        format!(
            "https://{}/api/webhooks/{}/****{}",
            self.host, self.id, self.token_tail
        )
    }
}

/// Parse + validate a user-pasted webhook URL.
///
/// Strict on purpose: only HTTPS, only Discord's own hosts, exactly
/// `/api/webhooks/{numeric id}/{token}`. A lax parser would let a
/// lookalike host receive message content. Error messages NEVER echo
/// the input, because the input contains the token.
///
/// No `url` crate is used (it is not a dependency): the checks below
/// are deliberately explicit and covered by unit tests.
pub fn parse_webhook_url(raw: &str) -> Result<WebhookUrl, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("webhook URL is empty".into());
    }
    if s.len() > 2048 {
        return Err("webhook URL is too long".into());
    }
    if s.contains('#') || s.chars().any(|c| c.is_ascii_whitespace()) {
        return Err(
            "webhook URL must look like https://discord.com/api/webhooks/{id}/{token}".into(),
        );
    }
    // Scheme: "https://" (case-insensitive for the scheme only; the
    // token keeps its original case). Compare bytes, not a str slice:
    // `s[..8]` panics when byte 8 falls inside a multi-byte char
    // (e.g. pasted "€€€€..."). The prefix is pure ASCII, so byte
    // comparison is exact, and slicing at 8 is safe once it matched.
    let prefix_ok = s.len() >= 8 && s.as_bytes()[..8].eq_ignore_ascii_case(b"https://");
    if !prefix_ok {
        return Err("webhook URL must start with https://discord.com/api/webhooks/".into());
    }
    let rest = &s[8..];
    let slash = rest
        .find('/')
        .ok_or("webhook URL must start with https://discord.com/api/webhooks/")?;
    let host_raw = &rest[..slash];
    let path_and_query = &rest[slash..];
    // Reject userinfo / ports / query-in-host tricks outright.
    if host_raw.is_empty()
        || host_raw.contains('@')
        || host_raw.contains(':')
        || host_raw.contains('?')
    {
        return Err("webhook URL host must be discord.com".into());
    }
    let host = host_raw.to_ascii_lowercase();
    if host != "discord.com" && host != "discordapp.com" {
        return Err("webhook URL host must be discord.com".into());
    }
    // Path: ignore any query string (we rebuild ?wait=true ourselves).
    let path = path_and_query.split('?').next().unwrap_or("");
    let path = path.trim_end_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    // ["", "api", "webhooks", id, token]
    if parts.len() != 5 || !parts[0].is_empty() || parts[1] != "api" || parts[2] != "webhooks" {
        return Err(
            "webhook URL must look like https://discord.com/api/webhooks/{id}/{token}".into(),
        );
    }
    let id = parts[3];
    let token = parts[4];
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("webhook ID must be numeric".into());
    }
    if token.is_empty() {
        return Err("webhook URL is missing its token".into());
    }
    let base_url = format!("https://{host}/api/webhooks/{id}/{token}");
    let execute_url = format!("{base_url}?wait=true");
    let token_tail: String = token
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Ok(WebhookUrl {
        id: id.to_string(),
        token_tail,
        host,
        base_url,
        execute_url,
    })
}

/// A webhook object (`GET /api/webhooks/{id}/{token}`): name + channel
/// for validation and display. All fields tolerate absence.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WebhookInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Guild id from GET (display only; may be absent).
    #[serde(default)]
    pub guild_id: Option<String>,
}

impl WebhookInfo {
    pub fn display_name(&self) -> String {
        self.name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "Webhook".to_string())
    }
}

/// Webhook sender. No Authorization header is ever sent: the URL token
/// IS the credential.
#[derive(Debug, Clone)]
pub struct WebhookClient {
    url: WebhookUrl,
    http: Client,
    /// Optional per-send `username` override (empty = the webhook's own name).
    /// Limited to 80 chars at the UI as a precaution (the docs only
    /// state that limit for the webhook's name at creation time).
    username_override: String,
    /// Optional per-send `avatar_url` override (empty = webhook avatar).
    avatar_url_override: String,
    /// Optional `thread_id` query param (empty = the webhook's channel).
    /// The thread must belong to the webhook's channel (verified in
    /// Discord's docs); locked-thread behaviour is unverified.
    thread_id: String,
}

impl WebhookClient {
    /// None when the HTTP client cannot be built (TLS backend or
    /// resolver init failure) — the caller treats the destination
    /// as unusable instead of crashing. The build used to `expect`,
    /// which panicked on the engine thread.
    pub fn new(url: WebhookUrl, username_override: &str) -> Option<Self> {
        let http = Client::builder()
            .user_agent("Nicord/0.1.0 (personal use)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .ok()?;
        Some(Self {
            url,
            http,
            username_override: username_override.trim().to_string(),
            avatar_url_override: String::new(),
            thread_id: String::new(),
        })
    }

    pub fn with_avatar_url(mut self, url: &str) -> Self {
        self.avatar_url_override = url.trim().to_string();
        self
    }

    pub fn with_thread_id(mut self, thread_id: &str) -> Self {
        self.thread_id = thread_id.trim().to_string();
        self
    }

    /// Thread id safe to put in a URL: digits-only. Anything else is
    /// ignored (defense in depth — save-time validation already
    /// requires digits-only, but a crafted local database or import
    /// could bypass it and inject extra query parameters).
    fn safe_thread_id(&self) -> Option<&str> {
        if !self.thread_id.is_empty() && self.thread_id.bytes().all(|b| b.is_ascii_digit()) {
            Some(self.thread_id.as_str())
        } else {
            None
        }
    }

    /// Execute URL with the optional thread param appended.
    fn execute_url(&self) -> String {
        if let Some(thread_id) = self.safe_thread_id() {
            format!("{}&thread_id={thread_id}", self.url.execute_url)
        } else {
            self.url.execute_url.clone()
        }
    }

    /// Execute with optional embeds (Q8). Discord requires at least
    /// one of content/embeds; the caller guarantees that.
    pub async fn send_message_with_embeds(
        &self,
        content: &str,
        embeds: &[crate::embed::Embed],
        opts: &SendOptions,
        poll: Option<&crate::poll::Poll>,
    ) -> Result<SentMessage, DiscordError> {
        if content.chars().count() > 2000 {
            return Err(DiscordError::Other(
                "message is longer than Discord's 2000-character limit".into(),
            ));
        }
        let body = ExecuteBody {
            content,
            username: if self.username_override.is_empty() {
                None
            } else {
                Some(&self.username_override)
            },
            avatar_url: if self.avatar_url_override.is_empty() {
                None
            } else {
                Some(&self.avatar_url_override)
            },
            embeds: embeds.iter().map(|e| e.to_json()).collect(),
            allowed_mentions: opts.allowed_mentions(),
            flags: opts.flags(),
            poll: poll.map(|p| p.to_json()),
        };
        let resp = self
            .http
            .post(self.execute_url())
            .json(&body)
            .send()
            .await
            .map_err(|e| reqwest_error("network error", e))?;
        match resp.status() {
            StatusCode::OK | StatusCode::CREATED => resp
                .json()
                .await
                .map_err(|e| reqwest_error("bad webhook send response", e)),
            // 204 should not happen with ?wait=true, but handle it:
            // sent, no message id to report.
            StatusCode::NO_CONTENT => Ok(SentMessage {
                id: String::new(),
                channel_id: String::new(),
            }),
            s => Err(discord_error_from_status(
                s,
                &resp.text().await.unwrap_or_default(),
            )),
        }
    }

    /// Execute with file attachments (idea #4): the same JSON body
    /// as `send_message_with_embeds`, carried as the `payload_json`
    /// part of a multipart request.
    pub async fn send_message_with_files(
        &self,
        content: &str,
        embeds: &[crate::embed::Embed],
        opts: &SendOptions,
        poll: Option<&crate::poll::Poll>,
        files: &[AttachedFile],
    ) -> Result<SentMessage, DiscordError> {
        if content.chars().count() > 2000 {
            return Err(DiscordError::Other(
                "message is longer than Discord's 2000-character limit".into(),
            ));
        }
        let body = ExecuteBody {
            content,
            username: if self.username_override.is_empty() {
                None
            } else {
                Some(&self.username_override)
            },
            avatar_url: if self.avatar_url_override.is_empty() {
                None
            } else {
                Some(&self.avatar_url_override)
            },
            embeds: embeds.iter().map(|e| e.to_json()).collect(),
            allowed_mentions: opts.allowed_mentions(),
            flags: opts.flags(),
            poll: poll.map(|p| p.to_json()),
        };
        let payload_json =
            serde_json::to_string(&body).map_err(|e| DiscordError::Other(e.to_string()))?;
        let form = crate::discord::build_multipart_form(&payload_json, files);
        let resp = self
            .http
            .post(self.execute_url())
            .multipart(form)
            .send()
            .await
            .map_err(|e| reqwest_error("network error", e))?;
        match resp.status() {
            StatusCode::OK | StatusCode::CREATED => resp
                .json()
                .await
                .map_err(|e| reqwest_error("bad webhook send response", e)),
            StatusCode::NO_CONTENT => Ok(SentMessage {
                id: String::new(),
                channel_id: String::new(),
            }),
            s => Err(discord_error_from_status(
                s,
                &resp.text().await.unwrap_or_default(),
            )),
        }
    }

    /// Delete one message sent through this webhook (Q13, manual
    /// only — the Logs screen calls this after explicit confirmation;
    /// nothing automatic ever calls it). Discord returns 204 and
    /// honours `thread_id` for thread messages. A 404 means the
    /// message is already gone (or never existed under this webhook)
    /// and surfaces as `NotFound` so the UI can say so honestly.
    /// The id must be digits-only (a snowflake): it goes into the
    /// URL path, so anything else is rejected before any request.
    pub async fn delete_message(&self, message_id: &str) -> Result<(), DiscordError> {
        let id = message_id.trim();
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(DiscordError::Other(
                "message id must be a numeric Discord snowflake".into(),
            ));
        }
        let mut url = format!("{}/messages/{id}", self.url.base_url);
        if let Some(thread_id) = self.safe_thread_id() {
            url.push_str(&format!("?thread_id={thread_id}"));
        }
        let resp = self
            .http
            .delete(url)
            .send()
            .await
            .map_err(|e| reqwest_error("network error", e))?;
        match resp.status() {
            StatusCode::NO_CONTENT | StatusCode::OK => Ok(()),
            s => Err(discord_error_from_status(
                s,
                &resp.text().await.unwrap_or_default(),
            )),
        }
    }

    /// Edit a message sent through this webhook (idea #2).
    /// PATCH carries content/embeds/allowed_mentions/flags only —
    /// no username/avatar (execute-only) and no poll (not
    /// editable). Honours `thread_id` for thread messages, like
    /// delete. A 404 means the message is gone and surfaces as
    /// `NotFound` so the caller can fall back to a fresh send (it
    /// must NOT pause the destination: the webhook itself may be
    /// fine). The id must be digits-only (URL path).
    pub async fn edit_message(
        &self,
        message_id: &str,
        content: &str,
        embeds: &[crate::embed::Embed],
        opts: &SendOptions,
    ) -> Result<SentMessage, DiscordError> {
        let id = message_id.trim();
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(DiscordError::Other(
                "message id must be a numeric Discord snowflake".into(),
            ));
        }
        if content.chars().count() > 2000 {
            return Err(DiscordError::Other(
                "message is longer than Discord's 2000-character limit".into(),
            ));
        }
        let mut url = format!("{}/messages/{id}", self.url.base_url);
        if let Some(thread_id) = self.safe_thread_id() {
            url.push_str(&format!("?thread_id={thread_id}"));
        }
        let body = EditBody {
            content,
            embeds: embeds.iter().map(|e| e.to_json()).collect(),
            allowed_mentions: opts.allowed_mentions(),
            flags: opts.flags(),
        };
        let resp = self
            .http
            .patch(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| reqwest_error("network error", e))?;
        match resp.status() {
            StatusCode::OK | StatusCode::CREATED => resp
                .json()
                .await
                .map_err(|e| reqwest_error("bad webhook edit response", e)),
            // 204 should not happen for edits, but handle it: the
            // edit went through, no message body to report.
            StatusCode::NO_CONTENT => Ok(SentMessage {
                id: String::new(),
                channel_id: String::new(),
            }),
            s => Err(discord_error_from_status(
                s,
                &resp.text().await.unwrap_or_default(),
            )),
        }
    }

    /// GET the webhook object (validation + display: name, channel).
    pub async fn get_webhook(&self) -> Result<WebhookInfo, DiscordError> {
        let resp = self
            .http
            .get(&self.url.base_url)
            .send()
            .await
            .map_err(|e| reqwest_error("network error", e))?;
        match resp.status() {
            StatusCode::OK => resp
                .json()
                .await
                .map_err(|e| reqwest_error("bad webhook response", e)),
            s => Err(discord_error_from_status(
                s,
                &resp.text().await.unwrap_or_default(),
            )),
        }
    }
}

/// The engine's send transport: either a user-token client or a
/// webhook client. The engine calls `send_text` and never knows
/// Edit-webhook-message request body: content/embeds/mentions/flags
/// only. No username/avatar (execute-only), no poll (not editable).
/// Module-level so its serialization is unit-testable without a network.
#[derive(Serialize)]
struct EditBody<'a> {
    content: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    embeds: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_mentions: Option<AllowedMentions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flags: Option<u32>,
}

/// Execute-webhook request body. Module-level (not local to the
/// send fn) so its serialization is unit-testable without a network.
#[derive(Serialize)]
struct ExecuteBody<'a> {
    content: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    embeds: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_mentions: Option<AllowedMentions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flags: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    poll: Option<serde_json::Value>,
}

/// which one it is — except for features webhooks cannot do
/// (commands, typing, delete), which check `is_webhook()`.
#[derive(Debug, Clone)]
pub enum MessageSender {
    User(DiscordClient),
    Webhook(WebhookClient),
}

impl MessageSender {
    pub fn is_webhook(&self) -> bool {
        matches!(self, MessageSender::Webhook(_))
    }

    /// Send text with optional embeds (Q8). Empty content is allowed
    /// when at least one embed is present. `opts` carries the
    /// scenario's mention/silent settings (idea #7).
    pub async fn send_with_embeds(
        &self,
        channel_id: &str,
        content: &str,
        embeds: &[crate::embed::Embed],
        opts: &SendOptions,
        poll: Option<&crate::poll::Poll>,
    ) -> Result<SentMessage, DiscordError> {
        match self {
            MessageSender::User(c) => {
                c.send_message_with_embeds(channel_id, content, embeds, opts, poll)
                    .await
            }
            MessageSender::Webhook(c) => {
                c.send_message_with_embeds(content, embeds, opts, poll)
                    .await
            }
        }
    }

    /// Send text with file attachments (idea #4). Empty `files`
    /// behaves like a plain send (callers branch on that).
    pub async fn send_with_files(
        &self,
        channel_id: &str,
        content: &str,
        embeds: &[crate::embed::Embed],
        opts: &SendOptions,
        poll: Option<&crate::poll::Poll>,
        files: &[AttachedFile],
    ) -> Result<SentMessage, DiscordError> {
        match self {
            MessageSender::User(c) => {
                c.send_message_with_files(channel_id, content, embeds, opts, poll, files)
                    .await
            }
            MessageSender::Webhook(c) => {
                c.send_message_with_files(content, embeds, opts, poll, files)
                    .await
            }
        }
    }

    /// Edit a previously sent message (idea #2). No poll: polls
    /// cannot be changed after sending. A 404 surfaces as
    /// `NotFound` so the caller can fall back to a fresh send.
    pub async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
        embeds: &[crate::embed::Embed],
        opts: &SendOptions,
    ) -> Result<SentMessage, DiscordError> {
        match self {
            MessageSender::User(c) => {
                c.edit_message(channel_id, message_id, content, embeds, opts)
                    .await
            }
            MessageSender::Webhook(c) => c.edit_message(message_id, content, embeds, opts).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str =
        "https://discord.com/api/webhooks/123456789/AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";

    #[test]
    fn edit_body_carries_no_username_avatar_or_poll() {
        let opts = SendOptions {
            allow_mass_mentions: false,
            silent: true,
        };
        let body = EditBody {
            content: "hello",
            embeds: vec![],
            allowed_mentions: opts.allowed_mentions(),
            flags: opts.flags(),
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"content\":\"hello\""), "json: {json}");
        assert!(json.contains("allowed_mentions"), "json: {json}");
        assert!(json.contains("\"flags\":4096"), "json: {json}");
        assert!(!json.contains("username"), "json: {json}");
        assert!(!json.contains("avatar"), "json: {json}");
        assert!(!json.contains("poll"), "json: {json}");
        assert!(!json.contains("embeds"), "json: {json}");
    }

    #[test]
    fn parse_accepts_valid_discord_url() {
        let u = parse_webhook_url(GOOD).unwrap();
        assert_eq!(u.id, "123456789");
        assert_eq!(u.host, "discord.com");
        assert!(u.masked_url().contains("123456789"));
        assert!(u.masked_url().ends_with("****6789"));
        // Masked form never contains the full token.
        assert!(!u.masked_url().contains("AbCdEfGh"));
        // Debug form never contains the token either.
        let dbg = format!("{u:?}");
        assert!(dbg.contains("123456789"));
        assert!(!dbg.contains("AbCdEfGh"));
    }

    #[test]
    fn parse_accepts_legacy_host_trailing_slash_and_query() {
        let u =
            parse_webhook_url("https://discordapp.com/api/webhooks/42/tok123/?wait=false").unwrap();
        assert_eq!(u.host, "discordapp.com");
        assert_eq!(u.id, "42");
        let u2 = parse_webhook_url("https://discord.com/api/webhooks/42/tok123/").unwrap();
        assert_eq!(u2.id, "42");
    }

    #[test]
    fn parse_accepts_uppercase_scheme_and_host() {
        let u = parse_webhook_url("HTTPS://DISCORD.COM/api/webhooks/42/TokEn123").unwrap();
        assert_eq!(u.host, "discord.com");
        assert!(u.masked_url().ends_with("****n123"));
    }

    #[test]
    fn parse_rejects_http_wrong_host_and_tricks() {
        assert!(parse_webhook_url("http://discord.com/api/webhooks/1/tok").is_err());
        assert!(parse_webhook_url("https://discord.com.evil.com/api/webhooks/1/tok").is_err());
        assert!(parse_webhook_url("https://discord.com@evil.com/api/webhooks/1/tok").is_err());
        assert!(parse_webhook_url("https://discord.com:8443/api/webhooks/1/tok").is_err());
        assert!(parse_webhook_url("https://evil.com/api/webhooks/1/tok").is_err());
    }

    #[test]
    fn parse_rejects_bad_shape_and_empty() {
        assert!(parse_webhook_url("").is_err());
        assert!(parse_webhook_url("   ").is_err());
        assert!(parse_webhook_url("https://discord.com/api/webhooks/1").is_err());
        assert!(parse_webhook_url("https://discord.com/api/webhooks/abc/tok").is_err());
        assert!(parse_webhook_url("https://discord.com/api/webhooks/1/tok/extra").is_err());
        assert!(parse_webhook_url("https://discord.com/api/channels/1/messages").is_err());
        assert!(parse_webhook_url(&format!("{GOOD}#frag")).is_err());
        // Error messages never echo the token.
        let err = parse_webhook_url("https://evil.com/api/webhooks/1/SECRETTOKEN").unwrap_err();
        assert!(!err.contains("SECRETTOKEN"));
    }

    #[test]
    fn webhook_info_parses_and_names() {
        let v: WebhookInfo =
            serde_json::from_str(r#"{"id":"1","name":"Announce","channel_id":"9"}"#).unwrap();
        assert_eq!(v.display_name(), "Announce");
        assert_eq!(v.channel_id.as_deref(), Some("9"));
        let empty: WebhookInfo = serde_json::from_str(r#"{"id":"1"}"#).unwrap();
        assert_eq!(empty.display_name(), "Webhook");
    }

    #[tokio::test]
    async fn reqwest_error_strips_the_url() {
        // A request to a closed localhost port fails with an error
        // that carries the URL (reqwest appends it in Display). The
        // formatted error must not contain it: webhook URLs hold the
        // token. The first assertion pins the premise — if reqwest
        // ever stops attaching URLs, this test says so loudly
        // instead of passing vacuously.
        let err = Client::new()
            .get("http://127.0.0.1:9/nicord-test")
            .send()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("127.0.0.1"));
        let msg = reqwest_error("network error", err).to_string();
        assert!(msg.contains("network error"));
        assert!(!msg.contains("127.0.0.1"));
        assert!(!msg.contains("nicord-test"));
    }

    #[test]
    fn parse_rejects_multibyte_prefix_without_panicking() {
        // Byte 8 falls inside a multi-byte char: the old str slice
        // panicked here instead of returning an error.
        assert!(parse_webhook_url("€€€€xhttps://discord.com/api/webhooks/1/abc").is_err());
        assert!(parse_webhook_url("€€€€€€€€").is_err());
        // Multi-byte chars later in the string must not panic either.
        let _ = parse_webhook_url("https://discord.com/api/webhooks/123/€€€");
    }

    #[test]
    fn thread_id_must_be_digits_to_reach_the_url() {
        let url = parse_webhook_url(GOOD).unwrap();
        let client = WebhookClient::new(url.clone(), "")
            .unwrap()
            .with_thread_id("12&evil=1");
        assert!(!client.execute_url().contains("thread_id"));
        let client = WebhookClient::new(url, "")
            .unwrap()
            .with_thread_id("987654321");
        assert!(client.execute_url().ends_with("&thread_id=987654321"));
    }

    #[test]
    fn execute_body_carries_mention_and_silent_options() {
        let opts = SendOptions {
            allow_mass_mentions: false,
            silent: true,
        };
        let body = ExecuteBody {
            content: "hi @everyone",
            username: None,
            avatar_url: None,
            embeds: vec![],
            allowed_mentions: opts.allowed_mentions(),
            flags: opts.flags(),
            poll: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(
            json["allowed_mentions"]["parse"],
            serde_json::json!(["users"])
        );
        assert_eq!(json["flags"], 4096);
        assert!(json.get("embeds").is_none());
        let opts = SendOptions {
            allow_mass_mentions: true,
            silent: false,
        };
        let body = ExecuteBody {
            content: "hi",
            username: None,
            avatar_url: None,
            embeds: vec![],
            allowed_mentions: opts.allowed_mentions(),
            flags: opts.flags(),
            poll: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("allowed_mentions").is_none());
        assert!(json.get("flags").is_none());
    }

    #[test]
    fn execute_body_carries_poll_when_present() {
        let poll = crate::poll::Poll {
            question: "Q?".to_string(),
            answers: vec!["A".to_string(), "B".to_string()],
            duration_hours: 48,
            allow_multiselect: false,
        };
        let body = ExecuteBody {
            content: "",
            username: None,
            avatar_url: None,
            embeds: vec![],
            allowed_mentions: None,
            flags: None,
            poll: Some(poll.to_json()),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["poll"]["question"]["text"], "Q?");
        assert_eq!(json["poll"]["answers"].as_array().unwrap().len(), 2);
        assert_eq!(json["poll"]["duration"], 48);
    }
}
