//! Discord REST client — user-account (self-bot) mode (Spec §1, §4).
//!
//! ⚠️  Automating a normal user account ("self-bot") is forbidden by
//! Discord's Terms of Service and can result in permanent account
//! termination. This module exists because Nico asked for a personal-use
//! tool; it must never be presented as safe or official. Use a spare account.
//!
//! Only REST is used (no gateway): POST /channels/{id}/messages for
//! scheduled sending, plus GET endpoints to list guilds/channels for the
//! channel picker. No email/password login anywhere — the user pastes their
//! own token, which is stored encrypted (see crypto.rs).

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

pub const DISCORD_API: &str = "https://discord.com/api/v10";

/// Maximum files per message (Discord limit).
pub const MAX_ATTACHMENTS: usize = 10;
/// Maximum bytes per file: 8 MiB, Discord's base limit. Boosted
/// servers allow more, but Nicord never assumes a boost — a file
/// over this cap is skipped with a warning, never sent.
pub const MAX_ATTACHMENT_BYTES: u64 = 8 * 1024 * 1024;

/// One file ready to upload (idea #4). The `filename` is the bare
/// file name (no directories) — local paths never leave the
/// machine. Bytes are read at send time; only paths are stored.
#[derive(Debug, Clone)]
pub struct AttachedFile {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// Guess a file's MIME type from its extension (lowercased).
/// Unknown extensions fall back to `application/octet-stream`,
/// which Discord accepts for any file.
pub fn guess_content_type(filename: &str) -> &'static str {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "json" => "application/json",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Build the multipart body Discord expects for a message with
/// files: a `payload_json` part (the message JSON, as
/// `application/json`) plus one `files[i]` part per file.
pub fn build_multipart_form(
    payload_json: &str,
    files: &[AttachedFile],
) -> reqwest::multipart::Form {
    let json_part = reqwest::multipart::Part::text(payload_json.to_string())
        .mime_str("application/json")
        .unwrap_or_else(|_| reqwest::multipart::Part::text(payload_json.to_string()));
    let mut form = reqwest::multipart::Form::new().part("payload_json", json_part);
    for (i, file) in files.iter().enumerate() {
        let part = reqwest::multipart::Part::bytes(file.bytes.clone())
            .file_name(file.filename.clone())
            .mime_str(&file.content_type)
            .unwrap_or_else(|_| {
                reqwest::multipart::Part::bytes(file.bytes.clone()).file_name(file.filename.clone())
            });
        form = form.part(format!("files[{i}]"), part);
    }
    form
}

#[derive(Debug, Clone, Serialize)]
pub struct SendMessageBody {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embeds: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_mentions: Option<AllowedMentions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll: Option<serde_json::Value>,
}

/// The `allowed_mentions` object of a Discord message: which
/// mention types Discord parses (and pings) in the content.
#[derive(Debug, Clone, Serialize)]
pub struct AllowedMentions {
    pub parse: Vec<&'static str>,
}

/// Per-send message options, taken from the scenario (idea #7).
/// The same options apply in both send modes: the webhook
/// execute body and the create-message body accept the same
/// `allowed_mentions` / `flags` fields.
#[derive(Debug, Clone, Copy, Default)]
pub struct SendOptions {
    /// When false (default), mass pings are blocked.
    pub allow_mass_mentions: bool,
    /// When true, SUPPRESS_NOTIFICATIONS is set.
    pub silent: bool,
}

impl SendOptions {
    /// Message flag bit: suppress the push notification.
    pub const SUPPRESS_NOTIFICATIONS: u32 = 1 << 12;

    /// `allowed_mentions` for these options: `{"parse": ["users"]}`
    /// when mass pings are blocked (individual user mentions still
    /// ping; @everyone/@here/roles render as plain text), or None
    /// to omit the field (Discord's default: everything parses).
    pub fn allowed_mentions(&self) -> Option<AllowedMentions> {
        if self.allow_mass_mentions {
            None
        } else {
            Some(AllowedMentions {
                parse: vec!["users"],
            })
        }
    }

    /// Message `flags` for these options: SUPPRESS_NOTIFICATIONS
    /// when silent, else None (field omitted).
    pub fn flags(&self) -> Option<u32> {
        if self.silent {
            Some(Self::SUPPRESS_NOTIFICATIONS)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SentMessage {
    pub id: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub channel_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Guild {
    pub id: String,
    pub name: String,
}

/// The account behind the token (`GET /users/@me`).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Me {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    /// Snowflake user id.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub verified: bool,
    /// Two-factor auth enabled.
    #[serde(default)]
    pub mfa_enabled: bool,
    /// 0 = none, 1 = Nitro Classic, 2 = Nitro, 3 = Nitro Basic.
    #[serde(default)]
    pub premium_type: u32,
}

impl Me {
    /// Display name for `{{username}}` — global name first, else username.
    pub fn display_name(&self) -> String {
        match &self.global_name {
            Some(g) if !g.trim().is_empty() => g.clone(),
            _ => {
                if self.username.trim().is_empty() {
                    "you".to_string()
                } else {
                    self.username.clone()
                }
            }
        }
    }

    /// Any paid Nitro tier.
    #[allow(dead_code)]
    pub fn has_nitro(&self) -> bool {
        self.premium_type > 0
    }

    /// Human-readable Nitro tier.
    pub fn nitro_label(&self) -> &'static str {
        match self.premium_type {
            1 => "Nitro Classic",
            2 => "Nitro",
            3 => "Nitro Basic",
            _ => "Không có",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Channel {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(default)]
    pub guild_id: Option<String>,
}

impl Channel {
    /// 0 = text channel, 5 = announcement channel.
    #[allow(dead_code)]
    pub fn is_text(&self) -> bool {
        self.kind == 0 || self.kind == 5
    }
}

#[derive(Debug)]
pub enum DiscordError {
    /// HTTP 429 — caller must wait `retry_after_secs` (anti-ban layer 3).
    RateLimited {
        retry_after_secs: u64,
        global: bool,
    },
    /// 401 / 403 — bad token or no permission. Stop immediately.
    Unauthorized(String),
    Forbidden(String),
    /// 404 — the channel or webhook is gone. Never retry: retrying a
    /// deleted destination cannot succeed and only hammers Discord.
    NotFound(String),
    Other(String),
}

impl std::fmt::Display for DiscordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscordError::RateLimited {
                retry_after_secs,
                global,
            } => write!(
                f,
                "bị giới hạn (429), thử lại sau {}s, toàn cục={}",
                retry_after_secs, global
            ),
            DiscordError::Unauthorized(m) => write!(f, "không có quyền (401): {m}"),
            DiscordError::Forbidden(m) => write!(f, "bị cấm (403): {m}"),
            DiscordError::NotFound(m) => write!(f, "không tìm thấy (404): {m}"),
            DiscordError::Other(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for DiscordError {}

#[derive(Clone)]
pub struct DiscordClient {
    token: String,
    http: Client,
}

impl std::fmt::Debug for DiscordClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the token: Debug output must be safe to log.
        // (MessageSender's derived Debug uses this impl.)
        f.debug_struct("DiscordClient")
            .field("token", &"<redacted>")
            .field("http", &self.http)
            .finish()
    }
}

impl DiscordClient {
    /// None when the HTTP client cannot be built (TLS backend or
    /// resolver init failure) — the caller treats the credential as
    /// unusable instead of crashing. The build used to `expect`,
    /// which panicked on the engine thread.
    pub fn new(token: &str) -> Option<Self> {
        let http = Client::builder()
            .user_agent("Nicord/0.1.0 (personal use)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .ok()?;
        Some(Self {
            token: token.to_string(),
            http,
        })
    }

    fn auth_header(&self) -> String {
        // User token: no "Bot " prefix. That prefix is exactly what makes
        // this a self-bot rather than an official bot — see the warning above.
        self.token.clone()
    }

    /// Raw token, in-memory only. Needed by the gateway client: the
    /// gateway Identify payload carries the same token. Never logged.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Fetch one channel (`GET /channels/{id}`) — used to resolve the
    /// guild a slash command runs in (`Channel.guild_id`).
    pub async fn get_channel(&self, channel_id: &str) -> Result<Channel, DiscordError> {
        let resp = self
            .http
            .get(channel_url(channel_id))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| reqwest_error("lỗi mạng", e))?;
        match resp.status() {
            StatusCode::OK => resp
                .json()
                .await
                .map_err(|e| reqwest_error("phản hồi kênh không hợp lệ", e)),
            s => Err(self.status_error(s, resp).await),
        }
    }

    /// Resolve a slash command's ID/version by name through the
    /// channel's application-commands search (the client search the
    /// Discord app itself uses). Best-effort: if the command is not
    /// found, the caller must fall back to a manually pasted ID.
    pub async fn search_application_commands(
        &self,
        channel_id: &str,
        query: &str,
    ) -> Result<Vec<SlashCommandInfo>, DiscordError> {
        let url = format!(
            "{}?query={}&limit=10",
            app_commands_search_url(channel_id),
            urlencoding(query)
        );
        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| reqwest_error("lỗi mạng", e))?;
        match resp.status() {
            StatusCode::OK => {
                let v: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| reqwest_error("phản hồi tìm kiếm không hợp lệ", e))?;
                Ok(parse_application_commands(&v))
            }
            s => Err(self.status_error(s, resp).await),
        }
    }

    /// Invoke a slash command (`POST /interactions`, type 2).
    /// Discord answers 204 (no content) on success for user accounts;
    /// 200 with a JSON body is also accepted.
    pub async fn invoke_interaction(&self, payload: serde_json::Value) -> Result<(), DiscordError> {
        let resp = self
            .http
            .post(interactions_url())
            .header("Authorization", self.auth_header())
            .json(&payload)
            .send()
            .await
            .map_err(|e| reqwest_error("lỗi mạng", e))?;
        match resp.status() {
            StatusCode::NO_CONTENT | StatusCode::OK | StatusCode::CREATED => Ok(()),
            s => Err(self.status_error(s, resp).await),
        }
    }

    /// Send with optional embeds (Q8). Embeds serialize via
    /// `Embed::to_json` into the existing `embeds` body field.
    pub async fn send_message_with_embeds(
        &self,
        channel_id: &str,
        content: &str,
        embeds: &[crate::embed::Embed],
        opts: &SendOptions,
        poll: Option<&crate::poll::Poll>,
    ) -> Result<SentMessage, DiscordError> {
        let url = format!("{DISCORD_API}/channels/{channel_id}/messages");
        let body = SendMessageBody {
            content: content.to_string(),
            embeds: if embeds.is_empty() {
                None
            } else {
                Some(embeds.iter().map(|e| e.to_json()).collect())
            },
            allowed_mentions: opts.allowed_mentions(),
            flags: opts.flags(),
            poll: poll.map(|p| p.to_json()),
        };
        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .await
            .map_err(|e| reqwest_error("lỗi mạng", e))?;
        self.handle_response(resp).await
    }

    /// Send a message with file attachments (idea #4): the same
    /// JSON body as `send_message_with_embeds`, carried as the
    /// `payload_json` part of a multipart request. Empty `files`
    /// behaves like a plain send (callers branch on that).
    pub async fn send_message_with_files(
        &self,
        channel_id: &str,
        content: &str,
        embeds: &[crate::embed::Embed],
        opts: &SendOptions,
        poll: Option<&crate::poll::Poll>,
        files: &[AttachedFile],
    ) -> Result<SentMessage, DiscordError> {
        let url = format!("{DISCORD_API}/channels/{channel_id}/messages");
        let body = SendMessageBody {
            content: content.to_string(),
            embeds: if embeds.is_empty() {
                None
            } else {
                Some(embeds.iter().map(|e| e.to_json()).collect())
            },
            allowed_mentions: opts.allowed_mentions(),
            flags: opts.flags(),
            poll: poll.map(|p| p.to_json()),
        };
        let payload_json =
            serde_json::to_string(&body).map_err(|e| DiscordError::Other(e.to_string()))?;
        let form = build_multipart_form(&payload_json, files);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.auth_header())
            .multipart(form)
            .send()
            .await
            .map_err(|e| reqwest_error("lỗi mạng", e))?;
        self.handle_response(resp).await
    }

    /// Edit a message this account sent (idea #2, edit-in-place).
    /// PATCH carries content/embeds/allowed_mentions/flags only —
    /// polls cannot be changed after sending, so the poll is never
    /// included here. The message id must be digits-only (it goes
    /// into the URL path).
    pub async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
        embeds: &[crate::embed::Embed],
        opts: &SendOptions,
    ) -> Result<SentMessage, DiscordError> {
        let id = message_id.trim();
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(DiscordError::Other(
                "id tin nhắn phải là số snowflake của Discord".into(),
            ));
        }
        if content.chars().count() > 2000 {
            return Err(DiscordError::Other(
                "tin nhắn dài hơn giới hạn 2000 ký tự của Discord".into(),
            ));
        }
        let url = format!("{DISCORD_API}/channels/{channel_id}/messages/{id}");
        let body = SendMessageBody {
            content: content.to_string(),
            embeds: if embeds.is_empty() {
                None
            } else {
                Some(embeds.iter().map(|e| e.to_json()).collect())
            },
            allowed_mentions: opts.allowed_mentions(),
            flags: opts.flags(),
            poll: None,
        };
        let resp = self
            .http
            .patch(&url)
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .await
            .map_err(|e| reqwest_error("lỗi mạng", e))?;
        self.handle_response(resp).await
    }

    /// Auto Typing: show "typing..." in the channel.
    /// Discord answers 204 with an empty body; any error is returned so
    /// the caller can decide (the send path ignores typing failures).
    pub async fn send_typing(&self, channel_id: &str) -> Result<(), DiscordError> {
        let resp = self
            .http
            .post(typing_url(channel_id))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| reqwest_error("lỗi mạng", e))?;
        match resp.status() {
            StatusCode::NO_CONTENT | StatusCode::OK => Ok(()),
            s => Err(self.status_error(s, resp).await),
        }
    }

    /// Auto Delete: delete a message the app just sent.
    /// Discord answers 204 with an empty body.
    pub async fn delete_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<(), DiscordError> {
        let resp = self
            .http
            .delete(delete_url(channel_id, message_id))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| reqwest_error("lỗi mạng", e))?;
        match resp.status() {
            StatusCode::NO_CONTENT | StatusCode::OK => Ok(()),
            s => Err(self.status_error(s, resp).await),
        }
    }

    pub async fn get_me(&self) -> Result<serde_json::Value, DiscordError> {
        let resp = self
            .http
            .get(format!("{DISCORD_API}/users/@me"))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| reqwest_error("lỗi mạng", e))?;
        match resp.status() {
            StatusCode::OK => resp
                .json()
                .await
                .map_err(|e| reqwest_error("phản hồi /users/@me không hợp lệ", e)),
            s => Err(self.status_error(s, resp).await),
        }
    }

    /// Typed `/users/@me` — used once at engine start so template vars
    /// (`{{username}}`) use the real account name, not "you".
    pub async fn get_me_typed(&self) -> Result<Me, DiscordError> {
        let v = self.get_me().await?;
        serde_json::from_value(v)
            .map_err(|e| DiscordError::Other(format!("nội dung /users/@me không hợp lệ: {e}")))
    }

    #[allow(dead_code)]
    pub async fn list_guilds(&self) -> Result<Vec<Guild>, DiscordError> {
        let resp = self
            .http
            .get(format!("{DISCORD_API}/users/@me/guilds?limit=100"))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| reqwest_error("lỗi mạng", e))?;
        match resp.status() {
            StatusCode::OK => resp
                .json()
                .await
                .map_err(|e| reqwest_error("phản hồi danh sách server không hợp lệ", e)),
            s => Err(self.status_error(s, resp).await),
        }
    }

    #[allow(dead_code)]
    pub async fn list_channels(&self, guild_id: &str) -> Result<Vec<Channel>, DiscordError> {
        let resp = self
            .http
            .get(format!("{DISCORD_API}/guilds/{guild_id}/channels"))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| reqwest_error("lỗi mạng", e))?;
        match resp.status() {
            StatusCode::OK => resp
                .json()
                .await
                .map_err(|e| reqwest_error("phản hồi danh sách kênh không hợp lệ", e)),
            s => Err(self.status_error(s, resp).await),
        }
    }

    async fn handle_response(&self, resp: reqwest::Response) -> Result<SentMessage, DiscordError> {
        match resp.status() {
            StatusCode::OK | StatusCode::CREATED => resp
                .json()
                .await
                .map_err(|e| reqwest_error("phản hồi gửi tin không hợp lệ", e)),
            s => Err(self.status_error(s, resp).await),
        }
    }

    async fn status_error(&self, status: StatusCode, resp: reqwest::Response) -> DiscordError {
        let body = resp.text().await.unwrap_or_default();
        discord_error_from_status(status, &body)
    }
}

/// Build a `DiscordError::Other` from a reqwest error, with the URL
/// stripped: `reqwest::Error`'s `Display` appends ` for url (<url>)`,
/// and webhook request URLs contain the token — formatting the raw
/// error leaked full webhook URLs into send logs, the database, the
/// Logs UI, and tracing output on any network failure. Every reqwest
/// error formatted into a `DiscordError` must go through here.
pub(crate) fn reqwest_error(prefix: &str, e: reqwest::Error) -> DiscordError {
    DiscordError::Other(format!("{prefix}: {}", e.without_url()))
}

/// Map an HTTP status + body to a DiscordError. Pure: unit-testable
/// without a network call. Both `DiscordClient` and `WebhookClient`
/// use it, so 429 / 401 / 403 / 404 mean the same thing in both modes.
pub fn discord_error_from_status(status: StatusCode, body: &str) -> DiscordError {
    match status {
        StatusCode::TOO_MANY_REQUESTS => {
            let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
            let retry_after = v.get("retry_after").and_then(|x| x.as_f64()).unwrap_or(5.0);
            let global = v.get("global").and_then(|x| x.as_bool()).unwrap_or(false);
            DiscordError::RateLimited {
                retry_after_secs: clamp_retry_after(retry_after),
                global,
            }
        }
        StatusCode::UNAUTHORIZED => {
            DiscordError::Unauthorized("token không hợp lệ hoặc đã hết hạn".into())
        }
        StatusCode::FORBIDDEN => DiscordError::Forbidden(
            "không có quyền gửi trong kênh này (hoặc token bị hạn chế)".into(),
        ),
        StatusCode::NOT_FOUND => DiscordError::NotFound(
            "kênh hoặc webhook không còn tồn tại (đã xóa hoặc đã tạo lại)".into(),
        ),
        _ => DiscordError::Other(format!("Discord HTTP {}: {}", status.as_u16(), body)),
    }
}

/// Upper bound for a 429 `retry_after`: Discord sends small values.
/// An uncapped value can overflow `Instant` arithmetic downstream
/// (values over ~292 years panic) or wedge a destination for years.
const MAX_RETRY_AFTER_SECS: u64 = 3600;

/// Clamp a fractional `retry_after` to whole seconds in
/// `0..=MAX_RETRY_AFTER_SECS`. Saturating: huge, negative, and
/// non-finite inputs all land inside the range.
fn clamp_retry_after(retry_after: f64) -> u64 {
    retry_after.ceil().clamp(0.0, MAX_RETRY_AFTER_SECS as f64) as u64
}

/// Parse a `retry_after` (seconds, may be fractional) from a 429 body.
#[allow(dead_code)]
pub fn parse_retry_after(body: &str) -> u64 {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("retry_after")?.as_f64())
        .map(clamp_retry_after)
        .unwrap_or(5)
}

/// One application command found by the channel search: the ID and
/// version a slash invocation needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommandInfo {
    pub id: String,
    pub application_id: String,
    pub name: String,
    pub version: String,
}

/// Parse the application-commands search response. Discord returns a
/// plain JSON array here; accept an object with an
/// `application_commands` array too, so a shape change degrades
/// gracefully instead of failing the whole lookup.
pub fn parse_application_commands(v: &serde_json::Value) -> Vec<SlashCommandInfo> {
    let arr = match v {
        serde_json::Value::Array(a) => Some(a),
        serde_json::Value::Object(o) => o.get("application_commands").and_then(|x| x.as_array()),
        _ => None,
    };
    let Some(arr) = arr else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|c| {
            Some(SlashCommandInfo {
                id: c.get("id")?.as_str()?.to_string(),
                application_id: c
                    .get("application_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                name: c
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                version: c
                    .get("version")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// Convert one slash option's raw text value to its JSON value, per
/// Discord option type code (3 string, 4 integer, 5 boolean, 6 user,
/// 7 channel, 8 role, 10 number). Unparseable numbers/bools fall back
/// to 0 / false rather than failing the whole invocation.
pub fn slash_option_value(kind: u8, raw: &str) -> serde_json::Value {
    let raw = raw.trim();
    match kind {
        4 => serde_json::Value::from(raw.parse::<i64>().unwrap_or(0)),
        10 => serde_json::Value::from(raw.parse::<f64>().unwrap_or(0.0)),
        5 => serde_json::Value::from(matches!(
            raw.to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "co" | "có"
        )),
        _ => serde_json::Value::from(raw),
    }
}

/// Build a slash-command interaction payload (type 2). `command_id`
/// must already be resolved (from config or the search endpoint).
/// `guild_id` is None for DM channels — the field is then omitted.
#[allow(clippy::too_many_arguments)]
pub fn build_interaction_payload(
    application_id: &str,
    guild_id: Option<&str>,
    channel_id: &str,
    session_id: &str,
    nonce: &str,
    command_id: &str,
    command_name: &str,
    version: &str,
    options: &[(String, u8, String)],
) -> serde_json::Value {
    let opts: Vec<serde_json::Value> = options
        .iter()
        .filter(|(name, _, _)| !name.trim().is_empty())
        .map(|(name, kind, value)| {
            serde_json::json!({
                "name": name.trim(),
                "type": kind,
                "value": slash_option_value(*kind, value),
            })
        })
        .collect();
    let mut data = serde_json::json!({
        "id": command_id,
        "name": command_name,
        "type": 1,
        "options": opts,
    });
    if !version.trim().is_empty() {
        data["version"] = serde_json::Value::from(version.trim());
    }
    let mut payload = serde_json::json!({
        "type": 2,
        "application_id": application_id,
        "channel_id": channel_id,
        "session_id": session_id,
        "nonce": nonce,
        "data": data,
    });
    if let Some(g) = guild_id {
        if !g.is_empty() {
            payload["guild_id"] = serde_json::Value::from(g);
        }
    }
    payload
}

/// Interaction nonce: a snowflake-style unique decimal string
/// (timestamp-based, same shape as Discord message nonces).
pub fn make_nonce() -> String {
    use rand::Rng;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let ts = now_ms.saturating_sub(1_420_070_400_000);
    let rand_bits: u64 = rand::rng().random_range(0..(1 << 22));
    ((ts << 22) | rand_bits).to_string()
}

/// Percent-encode a query value (small helper, no extra dependency).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Single-channel endpoint (`GET /channels/{id}`).
pub fn channel_url(channel_id: &str) -> String {
    format!("{DISCORD_API}/channels/{channel_id}")
}

/// Application-command invocation endpoint (`POST /interactions`).
pub fn interactions_url() -> String {
    format!("{DISCORD_API}/interactions")
}

/// Application-commands search endpoint for a channel.
pub fn app_commands_search_url(channel_id: &str) -> String {
    format!("{DISCORD_API}/channels/{channel_id}/application-commands/search")
}

/// Typing-indicator endpoint for a channel (Auto Typing).
pub fn typing_url(channel_id: &str) -> String {
    format!("{DISCORD_API}/channels/{channel_id}/typing")
}

/// Single-message endpoint (Auto Delete).
pub fn delete_url(channel_id: &str, message_id: &str) -> String {
    format!("{DISCORD_API}/channels/{channel_id}/messages/{message_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_guessing_covers_common_files() {
        assert_eq!(guess_content_type("a.png"), "image/png");
        assert_eq!(guess_content_type("A.JPG"), "image/jpeg");
        assert_eq!(guess_content_type("clip.mp4"), "video/mp4");
        assert_eq!(guess_content_type("doc.pdf"), "application/pdf");
        assert_eq!(guess_content_type("noext"), "application/octet-stream");
        assert_eq!(guess_content_type("weird.xyz"), "application/octet-stream");
        // Multi-dot names use the last extension.
        assert_eq!(guess_content_type("archive.tar.zip"), "application/zip");
    }

    #[test]
    fn retry_after_parses_fractional_seconds() {
        assert_eq!(
            parse_retry_after(r#"{"retry_after": 2.5, "global": false}"#),
            3
        );
        assert_eq!(parse_retry_after("not json"), 5);
    }

    #[test]
    fn error_mapping_covers_404_and_429() {
        let e =
            discord_error_from_status(StatusCode::NOT_FOUND, r#"{"message":"Unknown Webhook"}"#);
        assert!(matches!(e, DiscordError::NotFound(_)));
        let e = discord_error_from_status(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"retry_after":1.5,"global":true}"#,
        );
        match e {
            DiscordError::RateLimited {
                retry_after_secs,
                global,
            } => {
                assert_eq!(retry_after_secs, 2);
                assert!(global);
            }
            _ => panic!("wrong variant for 429"),
        }
        let e = discord_error_from_status(StatusCode::UNAUTHORIZED, "");
        assert!(matches!(e, DiscordError::Unauthorized(_)));
    }

    #[test]
    fn text_channels_are_detected() {
        let ch = Channel {
            id: "1".into(),
            name: "general".into(),
            kind: 0,
            guild_id: None,
        };
        assert!(ch.is_text());
        let voice = Channel {
            kind: 2,
            ..ch.clone()
        };
        assert!(!voice.is_text());
    }

    #[test]
    fn slash_payload_has_type_2_and_typed_options() {
        let options = vec![
            ("user".to_string(), 6u8, "123456".to_string()),
            ("count".to_string(), 4u8, "5".to_string()),
            ("amount".to_string(), 10u8, "2.5".to_string()),
            ("loud".to_string(), 5u8, "true".to_string()),
            ("text".to_string(), 3u8, "hello".to_string()),
            ("".to_string(), 3u8, "nameless skipped".to_string()),
        ];
        let p = build_interaction_payload(
            "app1",
            Some("guild1"),
            "chan1",
            "sess1",
            "nonce1",
            "cmd1",
            "daily",
            "v3",
            &options,
        );
        assert_eq!(p["type"], 2);
        assert_eq!(p["application_id"], "app1");
        assert_eq!(p["guild_id"], "guild1");
        assert_eq!(p["channel_id"], "chan1");
        assert_eq!(p["session_id"], "sess1");
        assert_eq!(p["nonce"], "nonce1");
        assert_eq!(p["data"]["id"], "cmd1");
        assert_eq!(p["data"]["name"], "daily");
        assert_eq!(p["data"]["type"], 1);
        assert_eq!(p["data"]["version"], "v3");
        let opts = p["data"]["options"].as_array().unwrap();
        assert_eq!(opts.len(), 5, "empty-named option is skipped");
        assert_eq!(opts[0]["value"], "123456");
        assert_eq!(opts[1]["value"], 5);
        assert_eq!(opts[2]["value"], 2.5);
        assert_eq!(opts[3]["value"], true);
        assert_eq!(opts[4]["value"], "hello");
        // DM channel: no guild_id field at all.
        let dm = build_interaction_payload(
            "app1",
            None,
            "chan1",
            "sess1",
            "nonce1",
            "cmd1",
            "daily",
            "",
            &[],
        );
        assert!(dm.get("guild_id").is_none());
        assert!(dm["data"].get("version").is_none(), "empty version omitted");
    }

    #[test]
    fn slash_option_values_fall_back_gracefully() {
        assert_eq!(slash_option_value(4, "abc"), 0);
        assert_eq!(slash_option_value(10, "xyz"), 0.0);
        assert_eq!(slash_option_value(5, "false"), false);
        assert_eq!(slash_option_value(5, "có"), true);
        assert_eq!(slash_option_value(3, " hi "), "hi");
        assert_eq!(slash_option_value(7, "998877"), "998877");
    }

    #[test]
    fn application_commands_search_parses_both_shapes() {
        let arr: serde_json::Value = serde_json::json!([
            {"id": "111", "application_id": "app1", "name": "daily", "version": "v1"},
            {"id": "222", "application_id": "app1", "name": "work"}
        ]);
        let found = parse_application_commands(&arr);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "daily");
        assert_eq!(found[0].version, "v1");
        assert_eq!(found[1].version, "", "missing version defaults to empty");
        let obj: serde_json::Value = serde_json::json!({"application_commands": [
            {"id": "333", "application_id": "app2", "name": "rank", "version": "v2"}
        ]});
        let found2 = parse_application_commands(&obj);
        assert_eq!(found2.len(), 1);
        assert_eq!(found2[0].id, "333");
        assert!(parse_application_commands(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn nonce_is_a_unique_decimal_snowflake() {
        let a = make_nonce();
        let b = make_nonce();
        assert_ne!(a, b);
        assert!(
            a.chars().all(|c| c.is_ascii_digit()),
            "nonce is decimal: {a}"
        );
        assert!(a.len() >= 15, "snowflake-length nonce: {a}");
    }

    #[test]
    fn interaction_and_channel_urls() {
        assert_eq!(
            interactions_url(),
            "https://discord.com/api/v10/interactions"
        );
        assert_eq!(
            channel_url("123"),
            "https://discord.com/api/v10/channels/123"
        );
        assert_eq!(
            app_commands_search_url("123"),
            "https://discord.com/api/v10/channels/123/application-commands/search"
        );
        assert_eq!(urlencoding("daily quest"), "daily%20quest");
    }

    #[test]
    fn typing_and_delete_urls() {
        assert_eq!(
            typing_url("123"),
            "https://discord.com/api/v10/channels/123/typing"
        );
        assert_eq!(
            delete_url("123", "456"),
            "https://discord.com/api/v10/channels/123/messages/456"
        );
    }

    #[test]
    fn me_parses_validator_fields() {
        let v: serde_json::Value = serde_json::json!({
            "username": "nico",
            "global_name": "Nico",
            "id": "123456",
            "email": "nico@example.com",
            "verified": true,
            "mfa_enabled": true,
            "premium_type": 2
        });
        let me: Me = serde_json::from_value(v).unwrap();
        assert_eq!(me.display_name(), "Nico");
        assert_eq!(me.id, "123456");
        assert_eq!(me.email.as_deref(), Some("nico@example.com"));
        assert!(me.verified && me.mfa_enabled);
        assert!(me.has_nitro());
        assert_eq!(me.nitro_label(), "Nitro");
        // Missing fields default (old /users/@me bodies still parse).
        let bare: Me = serde_json::from_value(serde_json::json!({"username": "x"})).unwrap();
        assert_eq!(bare.id, "");
        assert!(!bare.has_nitro());
        assert_eq!(bare.nitro_label(), "Không có");
    }

    #[test]
    fn retry_after_is_clamped() {
        let err = discord_error_from_status(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"retry_after": 1e20, "global": false}"#,
        );
        match err {
            DiscordError::RateLimited {
                retry_after_secs, ..
            } => assert_eq!(retry_after_secs, 3600),
            _ => panic!("expected RateLimited"),
        }
        let err = discord_error_from_status(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"retry_after": 5.5, "global": false}"#,
        );
        match err {
            DiscordError::RateLimited {
                retry_after_secs, ..
            } => assert_eq!(retry_after_secs, 6),
            _ => panic!("expected RateLimited"),
        }
    }

    #[test]
    fn debug_masks_the_token() {
        let client = DiscordClient::new("SECRET-TOKEN-VALUE-1234").unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("SECRET-TOKEN-VALUE-1234"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn send_options_block_mass_pings_and_set_silent_flag() {
        let opts = SendOptions::default();
        let mentions = opts.allowed_mentions().unwrap();
        assert_eq!(
            serde_json::to_value(&mentions).unwrap(),
            serde_json::json!({"parse": ["users"]})
        );
        assert_eq!(opts.flags(), None);
        let opts = SendOptions {
            allow_mass_mentions: true,
            silent: true,
        };
        assert!(opts.allowed_mentions().is_none());
        assert_eq!(opts.flags(), Some(1 << 12));
    }

    #[test]
    fn send_message_body_omits_unset_option_fields() {
        let opts = SendOptions::default();
        let body = SendMessageBody {
            content: "hi".to_string(),
            embeds: None,
            allowed_mentions: opts.allowed_mentions(),
            flags: opts.flags(),
            poll: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["allowed_mentions"]["parse"][0], "users");
        assert!(json.get("flags").is_none());
        assert!(json.get("embeds").is_none());
        let opts = SendOptions {
            allow_mass_mentions: true,
            silent: false,
        };
        let body = SendMessageBody {
            content: "hi".to_string(),
            embeds: None,
            allowed_mentions: opts.allowed_mentions(),
            flags: opts.flags(),
            poll: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("allowed_mentions").is_none());
    }

    #[test]
    fn send_message_body_carries_poll_when_present() {
        let poll = crate::poll::Poll {
            question: "Q?".to_string(),
            answers: vec!["A".to_string(), "B".to_string()],
            duration_hours: 24,
            allow_multiselect: true,
        };
        let body = SendMessageBody {
            content: String::new(),
            embeds: None,
            allowed_mentions: None,
            flags: None,
            poll: Some(poll.to_json()),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["poll"]["question"]["text"], "Q?");
        assert_eq!(json["poll"]["duration"], 24);
    }
}
