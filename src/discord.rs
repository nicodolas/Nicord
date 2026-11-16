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
#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
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
}
