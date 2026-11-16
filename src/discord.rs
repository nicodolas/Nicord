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

#[derive(Debug, Clone, Serialize)]
pub struct SendMessageBody {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embeds: Option<Vec<serde_json::Value>>,
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
                "rate limited (429), retry after {}s, global={}",
                retry_after_secs, global
            ),
            DiscordError::Unauthorized(m) => write!(f, "unauthorized (401): {m}"),
            DiscordError::Forbidden(m) => write!(f, "forbidden (403): {m}"),
            DiscordError::Other(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for DiscordError {}

#[derive(Debug, Clone)]
pub struct DiscordClient {
    token: String,
    http: Client,
}

impl DiscordClient {
    pub fn new(token: &str) -> Self {
        Self {
            token: token.to_string(),
            http: Client::builder()
                .user_agent("Nicord/0.1.0 (personal use)")
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client builds"),
        }
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
            .map_err(|e| DiscordError::Other(format!("network error: {e}")))?;
        match resp.status() {
            StatusCode::OK => resp
                .json()
                .await
                .map_err(|e| DiscordError::Other(format!("bad channel response: {e}"))),
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
            .map_err(|e| DiscordError::Other(format!("network error: {e}")))?;
        match resp.status() {
            StatusCode::OK => {
                let v: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| DiscordError::Other(format!("bad search response: {e}")))?;
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
            .map_err(|e| DiscordError::Other(format!("network error: {e}")))?;
        match resp.status() {
            StatusCode::NO_CONTENT | StatusCode::OK | StatusCode::CREATED => Ok(()),
            s => Err(self.status_error(s, resp).await),
        }
    }

    pub async fn send_message(
        &self,
        channel_id: &str,
        content: &str,
    ) -> Result<SentMessage, DiscordError> {
        let url = format!("{DISCORD_API}/channels/{channel_id}/messages");
        let body = SendMessageBody {
            content: content.to_string(),
            embeds: None,
        };
        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .await
            .map_err(|e| DiscordError::Other(format!("network error: {e}")))?;
        self.handle_response(resp).await
    }

    /// Auto Typing (Discord-BLUE): show "typing..." in the channel.
    /// Discord answers 204 with an empty body; any error is returned so
    /// the caller can decide (the send path ignores typing failures).
    pub async fn send_typing(&self, channel_id: &str) -> Result<(), DiscordError> {
        let resp = self
            .http
            .post(typing_url(channel_id))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| DiscordError::Other(format!("network error: {e}")))?;
        match resp.status() {
            StatusCode::NO_CONTENT | StatusCode::OK => Ok(()),
            s => Err(self.status_error(s, resp).await),
        }
    }

    /// Auto Delete (Discord-BLUE): delete a message the app just sent.
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
            .map_err(|e| DiscordError::Other(format!("network error: {e}")))?;
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
            .map_err(|e| DiscordError::Other(format!("network error: {e}")))?;
        match resp.status() {
            StatusCode::OK => resp
                .json()
                .await
                .map_err(|e| DiscordError::Other(format!("bad /users/@me response: {e}"))),
            s => Err(self.status_error(s, resp).await),
        }
    }

    /// Typed `/users/@me` — used once at engine start so template vars
    /// (`{{username}}`) use the real account name, not "you".
    pub async fn get_me_typed(&self) -> Result<Me, DiscordError> {
        let v = self.get_me().await?;
        serde_json::from_value(v)
            .map_err(|e| DiscordError::Other(format!("bad /users/@me body: {e}")))
    }

    #[allow(dead_code)]
    pub async fn list_guilds(&self) -> Result<Vec<Guild>, DiscordError> {
        let resp = self
            .http
            .get(format!("{DISCORD_API}/users/@me/guilds?limit=100"))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| DiscordError::Other(format!("network error: {e}")))?;
        match resp.status() {
            StatusCode::OK => resp
                .json()
                .await
                .map_err(|e| DiscordError::Other(format!("bad guilds response: {e}"))),
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
            .map_err(|e| DiscordError::Other(format!("network error: {e}")))?;
        match resp.status() {
            StatusCode::OK => resp
                .json()
                .await
                .map_err(|e| DiscordError::Other(format!("bad channels response: {e}"))),
            s => Err(self.status_error(s, resp).await),
        }
    }

    async fn handle_response(&self, resp: reqwest::Response) -> Result<SentMessage, DiscordError> {
        match resp.status() {
            StatusCode::OK | StatusCode::CREATED => resp
                .json()
                .await
                .map_err(|e| DiscordError::Other(format!("bad send response: {e}"))),
            s => Err(self.status_error(s, resp).await),
        }
    }

    async fn status_error(&self, status: StatusCode, resp: reqwest::Response) -> DiscordError {
        let body = resp.text().await.unwrap_or_default();
        match status {
            StatusCode::TOO_MANY_REQUESTS => {
                let retry_after: f64 = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("retry_after")?.as_f64())
                    .unwrap_or(5.0);
                let global = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("global")?.as_bool())
                    .unwrap_or(false);
                DiscordError::RateLimited {
                    retry_after_secs: retry_after.ceil() as u64,
                    global,
                }
            }
            StatusCode::UNAUTHORIZED => {
                DiscordError::Unauthorized("token is invalid or expired".into())
            }
            StatusCode::FORBIDDEN => DiscordError::Forbidden(
                "no permission to send in this channel (or token is restricted)".into(),
            ),
            _ => DiscordError::Other(format!("Discord HTTP {}: {}", status.as_u16(), body)),
        }
    }
}

/// Parse a `retry_after` (seconds, may be fractional) from a 429 body.
#[allow(dead_code)]
pub fn parse_retry_after(body: &str) -> u64 {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("retry_after")?.as_f64())
        .map(|f| f.ceil() as u64)
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
    fn retry_after_parses_fractional_seconds() {
        assert_eq!(
            parse_retry_after(r#"{"retry_after": 2.5, "global": false}"#),
            3
        );
        assert_eq!(parse_retry_after("not json"), 5);
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
}
