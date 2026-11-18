//! Destination model: one destination = one official Discord webhook
//! = one channel. Multi-channel exists ONLY in Webhook mode; user-token
//! mode keeps its single-channel rule and never uses this model.
//!
//! A destination owns its own settings: name, URL (encrypted), display
//! metadata from Discord, enable flag, username/avatar overrides,
//! optional thread id, daily cap, minimum interval, quiet window, and
//! default messages. Failure state is persisted, so a paused
//! destination stays paused across restarts until the user resumes it.

use serde::{Deserialize, Serialize};

/// Hard ceiling on the number of destinations. The user-facing
/// default is 10 (AppConfig); this is the absolute maximum the app
/// will ever allow, to keep credential management sane.
pub const MAX_DESTINATIONS_HARD: usize = 20;
/// Default maximum destinations for a fresh config.
pub const DEFAULT_MAX_DESTINATIONS: u32 = 10;
/// Default per-destination daily cap (messages/day). 0 means "use the
/// global cap" instead.
pub const DEFAULT_DESTINATION_DAILY_CAP: u32 = 50;
/// Default minimum interval between two sends to one destination.
pub const DEFAULT_MIN_INTERVAL_SECS: u32 = 60;
/// Floor for the minimum interval: never below 5 seconds, never 0.
pub const MIN_INTERVAL_FLOOR_SECS: u32 = 5;
/// Default global daily cap (all destinations combined).
pub const DEFAULT_GLOBAL_DAILY_CAP: u32 = 300;
/// Default consecutive-error limit before a destination auto-pauses.
pub const DEFAULT_CONSECUTIVE_ERROR_LIMIT: u32 = 5;
/// Max characters for the per-send username override. Discord
/// documents no limit for execute-time overrides (the 80-char rule
/// is for webhook names at creation); 80 is a precaution, enforced
/// at the UI (while typing) and in `Destination::validate`.
pub const MAX_USERNAME_OVERRIDE_CHARS: usize = 80;

/// Truncate `s` to at most `MAX_USERNAME_OVERRIDE_CHARS` characters
/// (char-boundary safe). The UI calls this after each edit so the
/// limit is visible while typing instead of failing on save.
pub fn truncate_username_override(s: &mut String) {
    if s.chars().count() > MAX_USERNAME_OVERRIDE_CHARS {
        let end = s
            .char_indices()
            .nth(MAX_USERNAME_OVERRIDE_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        s.truncate(end);
    }
}

/// Persisted failure state of a destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FailureState {
    #[default]
    Ok,
    /// 404: the webhook was deleted or regenerated in Discord.
    Paused404,
    /// 401/403: no permission / invalid credential.
    Paused401,
    /// Too many consecutive errors (limit from AppConfig).
    PausedErrors,
}

impl FailureState {
    pub fn is_paused(&self) -> bool {
        !matches!(self, FailureState::Ok)
    }

    pub fn label(&self) -> &'static str {
        match self {
            FailureState::Ok => "Đang chạy",
            FailureState::Paused404 => "Tạm dừng (404)",
            FailureState::Paused401 => "Tạm dừng (không có quyền)",
            FailureState::PausedErrors => "Tạm dừng (lỗi liên tiếp)",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Destination {
    pub id: String,
    /// Friendly name chosen by the user (required, non-empty).
    pub name: String,
    /// AES-GCM encrypted webhook URL (base64). Empty = not set yet;
    /// a destination without a URL can never be enabled.
    #[serde(default)]
    pub encrypted_url: String,
    /// Webhook id (numeric, not secret) for display.
    #[serde(default)]
    pub webhook_id: String,
    /// Webhook name from GET (display only).
    #[serde(default)]
    pub webhook_name: String,
    /// Channel id from GET (display + log filtering).
    #[serde(default)]
    pub channel_id: String,
    /// Guild id from GET (display only, may be empty).
    #[serde(default)]
    pub guild_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-send username override (empty = webhook's own name).
    #[serde(default)]
    pub username_override: String,
    /// Per-send avatar URL override (empty = webhook's avatar).
    /// Must be https when set.
    #[serde(default)]
    pub avatar_url_override: String,
    /// Optional thread id (digits only). Stored and validated now;
    /// sending into a thread is enabled only after the webhook
    /// `thread_id` behaviour is verified against Discord's docs.
    #[serde(default)]
    pub thread_id: String,
    /// Daily cap for this destination (0 = use the global cap).
    #[serde(default = "default_destination_daily_cap")]
    pub daily_cap: u32,
    /// Minimum seconds between two sends to this destination.
    #[serde(default = "default_min_interval_secs")]
    pub min_interval_secs: u32,
    /// Quiet window start ("HH:MM", local time). Both ends must be
    /// set together, or both empty. Overnight windows (start > end)
    /// are supported.
    #[serde(default)]
    pub quiet_start: Option<String>,
    #[serde(default)]
    pub quiet_end: Option<String>,
    /// Default messages for this destination (fallback when a
    /// scenario has no messages of its own; see Q6).
    #[serde(default)]
    pub default_messages: Vec<String>,
    #[serde(default)]
    pub failure_state: FailureState,
    /// Last error message (never contains a URL or token).
    #[serde(default)]
    pub last_error: String,
    /// RFC 3339 UTC timestamp when the destination was paused.
    #[serde(default)]
    pub paused_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_destination_daily_cap() -> u32 {
    DEFAULT_DESTINATION_DAILY_CAP
}

fn default_min_interval_secs() -> u32 {
    DEFAULT_MIN_INTERVAL_SECS
}

impl Destination {
    pub fn new(name: &str) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            encrypted_url: String::new(),
            webhook_id: String::new(),
            webhook_name: String::new(),
            channel_id: String::new(),
            guild_id: String::new(),
            enabled: true,
            username_override: String::new(),
            avatar_url_override: String::new(),
            thread_id: String::new(),
            daily_cap: DEFAULT_DESTINATION_DAILY_CAP,
            min_interval_secs: DEFAULT_MIN_INTERVAL_SECS,
            quiet_start: None,
            quiet_end: None,
            default_messages: Vec::new(),
            failure_state: FailureState::Ok,
            last_error: String::new(),
            paused_at: None,
            created_at: Some(now.clone()),
            updated_at: Some(now),
        }
    }

    /// Display name: the friendly name, or the webhook name, or a
    /// generic label. Never an id or a URL.
    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_string();
        }
        if !self.webhook_name.trim().is_empty() {
            return self.webhook_name.trim().to_string();
        }
        "Webhook".to_string()
    }

    /// Username used for template vars and (via override) for sends:
    /// the override wins, then the webhook's own name.
    pub fn effective_username(&self) -> String {
        if !self.username_override.trim().is_empty() {
            return self.username_override.trim().to_string();
        }
        if !self.webhook_name.trim().is_empty() {
            return self.webhook_name.trim().to_string();
        }
        "you".to_string()
    }

    /// Validate the destination's own fields (not the URL's
    /// reachability — that needs a network check). Returns the first
    /// problem found.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("destination name is empty".into());
        }
        if self.username_override.chars().count() > MAX_USERNAME_OVERRIDE_CHARS {
            return Err("display name must be at most 80 characters".into());
        }
        if !self.avatar_url_override.trim().is_empty() {
            let url = self.avatar_url_override.trim();
            // Length > 8: "https://" alone must name a host (it 400s
            // at send time).
            if !url.starts_with("https://")
                || url.len() <= "https://".len()
                || url.contains(char::is_whitespace)
                || url.len() > 2048
            {
                return Err("avatar URL must be an https URL without spaces".into());
            }
        }
        if !self.thread_id.trim().is_empty()
            && !self.thread_id.trim().chars().all(|c| c.is_ascii_digit())
        {
            return Err("thread ID must contain only digits".into());
        }
        if self.daily_cap > 300 {
            return Err("daily cap must be 0 (global) or 1..=300".into());
        }
        if self.min_interval_secs < MIN_INTERVAL_FLOOR_SECS {
            return Err(format!(
                "minimum interval must be at least {MIN_INTERVAL_FLOOR_SECS} seconds"
            ));
        }
        match (&self.quiet_start, &self.quiet_end) {
            (None, None) => {}
            (Some(s), Some(e)) => {
                parse_hhmm(s).ok_or("quiet start must be HH:MM (24h)")?;
                parse_hhmm(e).ok_or("quiet end must be HH:MM (24h)")?;
            }
            _ => {
                return Err("quiet window needs both a start and an end time".into());
            }
        }
        for (idx, msg) in self.default_messages.iter().enumerate() {
            if msg.trim().is_empty() {
                return Err(format!("default message {} is empty", idx + 1));
            }
            if crate::preview::over_limit(msg) {
                return Err(format!(
                    "default message {} is over Discord's 2000-character limit",
                    idx + 1
                ));
            }
        }
        Ok(())
    }

    /// Pick the next default message, rotating without repeating the
    /// last one. Returns (index, message). Empty/whitespace-only
    /// entries are never picked (same rule as the scenario picker):
    /// the returned index is the ORIGINAL position. None when no
    /// usable default message exists.
    pub fn pick_default_message(
        &self,
        last_index: Option<usize>,
        rng: &mut impl rand::Rng,
    ) -> Option<(usize, String)> {
        let candidates: Vec<usize> = self
            .default_messages
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.trim().is_empty())
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        if candidates.len() == 1 {
            let idx = candidates[0];
            return Some((idx, self.default_messages[idx].clone()));
        }
        let mut idx = candidates[rng.random_range(0..candidates.len())];
        if Some(idx) == last_index {
            let pos = candidates.iter().position(|&c| c == idx).unwrap_or(0);
            idx = candidates[(pos + 1) % candidates.len()];
        }
        Some((idx, self.default_messages[idx].clone()))
    }

    /// True when the destination can send right now: enabled, has a
    /// URL, and is not paused. Validation is separate (see `validate`).
    pub fn is_sendable(&self) -> bool {
        self.enabled && !self.encrypted_url.trim().is_empty() && !self.failure_state.is_paused()
    }

    /// True when `local_minutes` (minutes since local midnight) falls
    /// inside the quiet window. No window = never quiet. Overnight
    /// windows (start > end) are supported. Semantics match
    /// `Scenario::check_time_windows` exactly: end-EXCLUSIVE (quiet
    /// up to but not including the end minute) and start==end means
    /// never quiet. A one-sided or unparseable window is
    /// fail-closed: treated as quiet, so nothing is sent on a broken
    /// configuration.
    pub fn is_quiet_at(&self, local_minutes: u32) -> bool {
        let (start, end) = match (&self.quiet_start, &self.quiet_end) {
            (None, None) => return false,
            (Some(s), Some(e)) => (s, e),
            // One-sided: fail closed, like an unparseable window.
            _ => return true,
        };
        let (Some(s), Some(e)) = (parse_hhmm(start), parse_hhmm(end)) else {
            // Fail-closed: a broken window blocks sends.
            return true;
        };
        if s <= e {
            local_minutes >= s && local_minutes < e
        } else {
            local_minutes >= s || local_minutes < e
        }
    }
}

/// Parse "HH:MM" (24h) into minutes since midnight. Strict: exactly
/// two digits, colon, two digits; hours 0..=23, minutes 0..=59.
pub fn parse_hhmm(text: &str) -> Option<u32> {
    let text = text.trim();
    let (h, m) = text.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let hours: u32 = h.parse().ok()?;
    let minutes: u32 = m.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(hours * 60 + minutes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_catches_bad_fields() {
        let mut d = Destination::new("ok");
        assert!(d.validate().is_ok());
        d.name = "  ".to_string();
        assert!(d.validate().is_err());
        d.name = "ok".to_string();
        d.avatar_url_override = "http://x/y.png".to_string();
        assert!(d.validate().is_err());
        d.avatar_url_override = "https://cdn.example.com/a.png".to_string();
        assert!(d.validate().is_ok());
        d.thread_id = "12a".to_string();
        assert!(d.validate().is_err());
        d.thread_id = "123".to_string();
        d.daily_cap = 301;
        assert!(d.validate().is_err());
        d.daily_cap = 0;
        d.min_interval_secs = 4;
        assert!(d.validate().is_err());
        d.min_interval_secs = 5;
        d.quiet_start = Some("25:00".to_string());
        d.quiet_end = Some("06:00".to_string());
        assert!(d.validate().is_err());
        d.quiet_start = Some("22:00".to_string());
        assert!(d.validate().is_ok());
        d.default_messages = vec!["a".repeat(2001)];
        assert!(d.validate().is_err());
        d.default_messages = vec![];
        d.username_override = "é".repeat(81);
        assert!(d.validate().is_err());
        d.username_override = "é".repeat(80);
        assert!(d.validate().is_ok());
        let mut s = "é".repeat(81);
        truncate_username_override(&mut s);
        assert_eq!(s.chars().count(), 80);
    }

    #[test]
    fn quiet_window_handles_overnight_and_fail_closed() {
        let mut d = Destination::new("q");
        assert!(!d.is_quiet_at(0));
        d.quiet_start = Some("22:00".to_string());
        d.quiet_end = Some("06:00".to_string());
        assert!(d.is_quiet_at(23 * 60));
        assert!(d.is_quiet_at(3 * 60));
        assert!(!d.is_quiet_at(12 * 60));
        // End-exclusive: 06:00 itself is no longer quiet.
        assert!(!d.is_quiet_at(6 * 60));
        assert!(d.is_quiet_at(6 * 60 - 1));
        // start == end: never quiet (matches Time Window nodes).
        d.quiet_start = Some("08:00".to_string());
        d.quiet_end = Some("08:00".to_string());
        assert!(!d.is_quiet_at(8 * 60));
        // One-sided: fail closed.
        d.quiet_end = None;
        assert!(d.is_quiet_at(12 * 60));
        d.quiet_start = Some("bad".to_string());
        d.quiet_end = Some("06:00".to_string());
        assert!(d.is_quiet_at(12 * 60)); // fail-closed
    }

    #[test]
    fn validate_rejects_empty_default_messages_and_bare_avatar_scheme() {
        let mut d = Destination::new("q");
        d.default_messages = vec!["hi".to_string(), "  ".to_string()];
        assert!(d.validate().is_err());
        d.default_messages = vec!["hi".to_string()];
        d.avatar_url_override = "https://".to_string();
        assert!(d.validate().is_err());
        d.avatar_url_override = "https://cdn.example.com/a.png".to_string();
        assert!(d.validate().is_ok());
    }

    #[test]
    fn default_picker_never_returns_empty_messages() {
        use rand::{rngs::StdRng, SeedableRng};
        let mut d = Destination::new("q");
        d.default_messages = vec!["".to_string(), "hey".to_string(), " ".to_string()];
        let mut rng = StdRng::seed_from_u64(2);
        for _ in 0..20 {
            let (idx, msg) = d.pick_default_message(None, &mut rng).unwrap();
            assert_eq!(idx, 1);
            assert_eq!(msg, "hey");
        }
        d.default_messages = vec!["".to_string()];
        assert!(d.pick_default_message(None, &mut rng).is_none());
    }

    #[test]
    fn parse_hhmm_strict() {
        assert_eq!(parse_hhmm("08:30"), Some(510));
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        assert_eq!(parse_hhmm("8:30"), None);
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("08:60"), None);
        assert_eq!(parse_hhmm("08-30"), None);
    }
}
