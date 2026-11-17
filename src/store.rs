//! Local storage (Spec §9, §10): SQLite for scenarios + logs,
//! JSON for the app config (encrypted token, settings).
//! Nothing here is ever uploaded anywhere.

use crate::antiban::AntiBanConfig;
use crate::destination::{Destination, FailureState};
use crate::scenario::Scenario;
use crate::scheduler::SendLogEntry;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Mutex};

/// How Nicord sends messages. Default is UserToken so existing
/// configs keep their current behavior; new users choose in
/// onboarding. Webhook is the official, recommended path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SendMode {
    #[default]
    UserToken,
    Webhook,
}

/// Last known health of the stored webhook (QoL destination status).
/// Persisted in config.json so the Dashboard can show it without a
/// network call. `Unknown` means never checked (or checked long ago
/// under an older version).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WebhookHealth {
    #[default]
    Unknown,
    /// Last GET succeeded.
    Ok,
    /// 404: webhook deleted or URL regenerated.
    NotFound,
    /// 401/403: no permission / invalid credential.
    Forbidden,
    /// Network error or other failure reaching Discord.
    NetworkError,
}

impl WebhookHealth {
    /// Short Vietnamese label for the UI.
    pub fn label(&self) -> &'static str {
        match self {
            WebhookHealth::Unknown => "Chưa kiểm tra",
            WebhookHealth::Ok => "Tốt",
            WebhookHealth::NotFound => "Không tìm thấy (404)",
            WebhookHealth::Forbidden => "Không có quyền",
            WebhookHealth::NetworkError => "Lỗi mạng",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// AES-256-GCM encrypted Discord token (base64), or empty.
    #[serde(default)]
    pub encrypted_token: String,
    #[serde(default)]
    pub token_last4: String,
    #[serde(default)]
    pub tos_accepted: bool,
    /// Send transport. Missing in old config.json files → UserToken.
    #[serde(default)]
    pub send_mode: SendMode,
    /// AES-256-GCM encrypted webhook URL (base64), or empty.
    /// Encrypted with the same functions as the token.
    #[serde(default)]
    pub encrypted_webhook_url: String,
    /// Webhook ID (numeric, not secret) for display.
    #[serde(default)]
    pub webhook_id: String,
    /// Webhook name from GET (display only).
    #[serde(default)]
    pub webhook_name: String,
    /// Optional per-send username override (empty = webhook's own name).
    #[serde(default)]
    pub webhook_username: String,
    /// User-chosen friendly name for the webhook destination
    /// (empty = use `webhook_name`). Shown everywhere instead of IDs.
    #[serde(default)]
    pub webhook_friendly_name: String,
    /// Last known webhook health (see `WebhookHealth`).
    #[serde(default)]
    pub webhook_health: WebhookHealth,
    /// RFC 3339 UTC timestamp of the last webhook check (empty = never).
    #[serde(default)]
    pub webhook_last_checked_at: String,
    /// Global daily cap across all destinations (webhook mode).
    #[serde(default = "default_global_daily_cap")]
    pub global_daily_cap: u32,
    /// Maximum destinations the user allows (hard ceiling 20).
    #[serde(default = "default_max_destinations")]
    pub max_destinations: u32,
    /// True once the legacy single-webhook config has been migrated
    /// into the destinations table (see `ensure_destinations_migrated`).
    #[serde(default)]
    pub destinations_migrated: bool,
    /// Consecutive errors before a destination auto-pauses (3..=10).
    #[serde(default = "default_consecutive_error_limit")]
    pub consecutive_error_limit: u32,
    /// UI language code ("vi" default, "en"). Applied instantly;
    /// unknown codes fall back to Vietnamese (see `i18n::Lang`).
    #[serde(default = "default_language")]
    pub language: String,
    /// UI theme code ("dark" default, "light"). Applied instantly;
    /// unknown codes fall back to Dark (see `theme::ThemeChoice`).
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Notification sounds on send failures / destination pauses.
    /// Missing in old configs → true: failures deserve attention.
    #[serde(default = "default_true")]
    pub sound_enabled: bool,
    /// Also play a sound on successful sends. Default false: with
    /// frequent schedules, a sound per send would be noise.
    #[serde(default)]
    pub sound_on_success: bool,
    /// RFC 3339 UTC timestamp of the last config export (empty =
    /// never). Used by the startup health report to suggest a backup.
    #[serde(default)]
    pub last_export_at: String,
    #[serde(default = "default_true")]
    pub autostart: bool,
    #[serde(default)]
    pub active_scenario_id: Option<String>,
    /// Anti-ban settings (Smart Templates, Auto Typing, Auto Break,
    /// Auto Delete). Missing in old config.json files → defaults.
    #[serde(default)]
    pub antiban: AntiBanConfig,
    /// Minimise to the system tray instead of quitting (Windows only;
    /// the checkbox is disabled on other platforms).
    #[serde(default)]
    pub tray_enabled: bool,
    /// Check GitHub Releases for a newer version when the app opens.
    /// Missing in old config.json files → true.
    #[serde(default = "default_true")]
    pub auto_check_updates: bool,
}

impl Default for AppConfig {
    /// The same defaults serde uses for missing fields. This must
    /// stay in sync with the `#[serde(default = ...)]` attributes
    /// above: a derived Default gave 0/"" everywhere, so a fresh or
    /// corrupt config silently disabled the global cap, the
    /// destination limit, and update checks. The test
    /// `default_matches_serde_defaults` guards the sync.
    fn default() -> Self {
        Self {
            encrypted_token: String::new(),
            token_last4: String::new(),
            tos_accepted: false,
            send_mode: SendMode::default(),
            encrypted_webhook_url: String::new(),
            webhook_id: String::new(),
            webhook_name: String::new(),
            webhook_username: String::new(),
            webhook_friendly_name: String::new(),
            webhook_health: WebhookHealth::default(),
            webhook_last_checked_at: String::new(),
            global_daily_cap: default_global_daily_cap(),
            max_destinations: default_max_destinations(),
            destinations_migrated: false,
            consecutive_error_limit: default_consecutive_error_limit(),
            language: default_language(),
            theme: default_theme(),
            autostart: true,
            active_scenario_id: None,
            antiban: AntiBanConfig::default(),
            tray_enabled: false,
            auto_check_updates: true,
            sound_enabled: true,
            sound_on_success: false,
            last_export_at: String::new(),
        }
    }
}

impl AppConfig {
    /// Display name for the webhook destination: the user's friendly
    /// name wins, then the webhook's own name, then a generic label.
    /// Never returns an ID or a URL.
    pub fn webhook_display_name(&self) -> String {
        if !self.webhook_friendly_name.trim().is_empty() {
            return self.webhook_friendly_name.trim().to_string();
        }
        if !self.webhook_name.trim().is_empty() {
            return self.webhook_name.trim().to_string();
        }
        "Webhook".to_string()
    }
}

fn default_true() -> bool {
    true
}

fn default_global_daily_cap() -> u32 {
    crate::destination::DEFAULT_GLOBAL_DAILY_CAP
}

fn default_max_destinations() -> u32 {
    crate::destination::DEFAULT_MAX_DESTINATIONS
}

fn default_consecutive_error_limit() -> u32 {
    crate::destination::DEFAULT_CONSECUTIVE_ERROR_LIMIT
}

fn default_language() -> String {
    "vi".to_string()
}

fn default_theme() -> String {
    "dark".to_string()
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("nicord")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn db_path() -> PathBuf {
    config_dir().join("nicord.db")
}

/// One row of the send history, as returned by `query_logs`.
/// `message_id` / `destination_id` are "" for rows written before
/// Q13 (or for sends that returned no id, e.g. a webhook 204).
#[derive(Debug, Clone)]
pub struct LogRow {
    pub id: i64,
    pub created_at: String,
    pub scenario_id: String,
    pub scenario_name: String,
    pub channel_id: String,
    pub status: String,
    pub detail: String,
    pub message_id: String,
    pub destination_id: String,
}

pub struct Store {
    db: Mutex<Connection>,
}

impl Store {
    pub fn open() -> Result<Self, String> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
        let conn = Connection::open(db_path()).map_err(|e| format!("cannot open database: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS scenarios (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS send_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scenario_id TEXT,
                scenario_name TEXT,
                channel_id TEXT,
                status TEXT,
                detail TEXT,
                message_id TEXT,
                destination_id TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS destinations (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS templates (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                content TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )
        .map_err(|e| format!("cannot create tables: {e}"))?;
        // Migration: databases created before Q13 lack the
        // message_id/destination_id columns (CREATE TABLE IF NOT
        // EXISTS never adds them). Guarded by a PRAGMA check, so
        // running it on every open is safe.
        Self::ensure_send_log_columns(&conn)?;
        Ok(Self {
            db: Mutex::new(conn),
        })
    }

    /// Add `message_id` / `destination_id` to `send_logs` when an
    /// older database lacks them. Old rows keep NULL, which reads
    /// back as "" via COALESCE in `query_logs`.
    fn ensure_send_log_columns(conn: &Connection) -> Result<(), String> {
        let cols: Vec<String> = {
            let mut stmt = conn
                .prepare("PRAGMA table_info(send_logs)")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<_, _>>().map_err(|e| e.to_string())?
        };
        for (col, ddl) in [
            (
                "message_id",
                "ALTER TABLE send_logs ADD COLUMN message_id TEXT",
            ),
            (
                "destination_id",
                "ALTER TABLE send_logs ADD COLUMN destination_id TEXT",
            ),
        ] {
            if !cols.iter().any(|c| c == col) {
                conn.execute(ddl, []).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// Open a throwaway in-memory store (used by tests).
    #[cfg(test)]
    pub fn open_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE scenarios (id TEXT PRIMARY KEY, json TEXT NOT NULL, updated_at TEXT NOT NULL);
             CREATE TABLE send_logs (id INTEGER PRIMARY KEY AUTOINCREMENT, scenario_id TEXT, scenario_name TEXT, channel_id TEXT, status TEXT, detail TEXT, message_id TEXT, destination_id TEXT, created_at TEXT NOT NULL);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE destinations (id TEXT PRIMARY KEY, json TEXT NOT NULL, updated_at TEXT NOT NULL);
             CREATE TABLE templates (id TEXT PRIMARY KEY, name TEXT NOT NULL, content TEXT NOT NULL, updated_at TEXT NOT NULL);",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            db: Mutex::new(conn),
        })
    }

    pub fn get_meta(&self, key: &str) -> Option<String> {
        let conn = self.db.lock().ok()?;
        conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), String> {
        self.db
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Days since the app first ran with any account (warm-up clock).
    /// Records today (UTC date) on first call and returns 0.
    pub fn account_age_days(&self) -> u32 {
        let today = chrono::Utc::now().date_naive();
        match self.get_meta("first_seen") {
            Some(v) => chrono::NaiveDate::parse_from_str(&v, "%Y-%m-%d")
                .map(|d| (today - d).num_days().max(0) as u32)
                .unwrap_or(0),
            None => {
                let _ = self.set_meta("first_seen", &today.format("%Y-%m-%d").to_string());
                0
            }
        }
    }

    /// Number of successful sends today (UTC date), from the persisted log.
    /// Survives restarts — the Dashboard reads this, not a memory counter.
    /// The bucket is the UTC date, never the local date: the cap resets at
    /// 00:00 UTC, so changing the machine timezone cannot grant extras.
    pub fn count_sends_today(&self) -> u32 {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let conn = match self.db.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        conn.query_row(
            "SELECT COUNT(*) FROM send_logs WHERE status = 'sent' AND substr(created_at, 1, 10) = ?1",
            params![today],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        .max(0) as u32
    }

    pub fn save_scenario(&self, scenario: &Scenario) -> Result<(), String> {
        let json = serde_json::to_string(scenario).map_err(|e| e.to_string())?;
        let now = chrono::Utc::now().to_rfc3339();
        self.db
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "INSERT INTO scenarios (id, json, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
                params![scenario.id, json, now],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load_scenarios(&self) -> Result<Vec<Scenario>, String> {
        let conn = self.db.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT json FROM scenarios ORDER BY updated_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let json = row.map_err(|e| e.to_string())?;
            if let Ok(s) = serde_json::from_str::<Scenario>(&json) {
                out.push(s);
            }
        }
        Ok(out)
    }

    // ---- Destinations (multi-channel, webhook mode only) ----

    pub fn save_destination(&self, dest: &Destination) -> Result<(), String> {
        let json = serde_json::to_string(dest).map_err(|e| e.to_string())?;
        let now = chrono::Utc::now().to_rfc3339();
        self.db
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "INSERT INTO destinations (id, json, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
                params![dest.id, json, now],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// All destinations, sorted by (name, id) for a stable engine
    /// order. Rows that fail to parse are skipped (same policy as
    /// scenarios); a corrupt row must not hide the rest.
    pub fn load_destinations(&self) -> Result<Vec<Destination>, String> {
        let conn = self.db.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT json FROM destinations ORDER BY updated_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let json = row.map_err(|e| e.to_string())?;
            if let Ok(d) = serde_json::from_str::<Destination>(&json) {
                out.push(d);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(out)
    }

    pub fn load_destination(&self, id: &str) -> Result<Option<Destination>, String> {
        Ok(self.load_destinations()?.into_iter().find(|d| d.id == id))
    }

    pub fn delete_destination(&self, id: &str) -> Result<(), String> {
        self.db
            .lock()
            .map_err(|e| e.to_string())?
            .execute("DELETE FROM destinations WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Set a destination's failure state (engine pause/resume).
    /// No-op when the destination does not exist.
    pub fn set_destination_failure(
        &self,
        id: &str,
        state: FailureState,
        last_error: &str,
    ) -> Result<(), String> {
        let Some(mut dest) = self.load_destination(id)? else {
            return Ok(());
        };
        dest.failure_state = state;
        dest.last_error = last_error.chars().take(500).collect();
        dest.paused_at = if state.is_paused() {
            Some(chrono::Utc::now().to_rfc3339())
        } else {
            None
        };
        dest.updated_at = Some(chrono::Utc::now().to_rfc3339());
        self.save_destination(&dest)
    }

    /// One-time migration from the legacy single-webhook config
    /// fields into the destinations table. Idempotent: safe to call
    /// on every startup. Returns true when it did work.
    ///
    /// - If already migrated, or destinations already exist: only
    ///   marks the flag, never duplicates.
    /// - If a legacy URL exists: creates one destination from it
    ///   (encrypted URL copied as-is, never decrypted here) and,
    ///   when the global mode is Webhook, assigns every scenario's
    ///   `destination_id` to it.
    /// - Legacy config fields are KEPT (not deleted) so a downgrade
    ///   or a failed migration loses nothing. The caller saves the
    ///   config after this returns.
    pub fn ensure_destinations_migrated(&self, cfg: &mut AppConfig) -> Result<bool, String> {
        if cfg.destinations_migrated {
            return Ok(false);
        }
        let existing = self.load_destinations()?;
        if !existing.is_empty() {
            cfg.destinations_migrated = true;
            return Ok(true);
        }
        if cfg.encrypted_webhook_url.trim().is_empty() {
            cfg.destinations_migrated = true;
            return Ok(true);
        }
        let name = if !cfg.webhook_friendly_name.trim().is_empty() {
            cfg.webhook_friendly_name.trim().to_string()
        } else if !cfg.webhook_name.trim().is_empty() {
            cfg.webhook_name.trim().to_string()
        } else {
            "Webhook chính".to_string()
        };
        let mut dest = Destination::new(&name);
        dest.encrypted_url = cfg.encrypted_webhook_url.clone();
        dest.webhook_id = cfg.webhook_id.clone();
        dest.webhook_name = cfg.webhook_name.clone();
        dest.username_override = cfg.webhook_username.clone();
        // Channel/guild ids are unknown until the first GET check
        // refreshes them; the destination works without them.
        self.save_destination(&dest)?;
        if cfg.send_mode == SendMode::Webhook {
            let scenarios = self.load_scenarios()?;
            for mut s in scenarios {
                if s.destination_id.is_none() {
                    s.destination_id = Some(dest.id.clone());
                    self.save_scenario(&s)?;
                }
            }
        }
        cfg.destinations_migrated = true;
        Ok(true)
    }

    /// Sends today (UTC date) to one channel, for per-destination caps.
    /// In webhook mode the engine logs the destination's channel id,
    /// so this is the destination's own count.
    pub fn count_sends_today_for_channel(&self, channel_id: &str) -> u32 {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let conn = match self.db.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        conn.query_row(
            "SELECT COUNT(*) FROM send_logs WHERE status = 'sent' AND channel_id = ?1 AND substr(created_at, 1, 10) = ?2",
            params![channel_id, today],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        .max(0) as u32
    }

    // ---- Templates (Q6 shared message library) ----

    /// A reusable message template: a name + content (≤2000 chars).
    /// Templates contain no secrets and are included in config
    /// exports (see the export format).
    pub fn save_template(&self, name: &str, content: &str) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("template name is empty".into());
        }
        if crate::preview::over_limit(content) {
            return Err("template content is over Discord's 2000-character limit".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.db
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "INSERT INTO templates (id, name, content, updated_at) VALUES (?1, ?2, ?3, ?4)",
                params![id, name, content, now],
            )
            .map_err(|e| e.to_string())?;
        Ok(id)
    }

    pub fn load_templates(&self) -> Result<Vec<(String, String, String)>, String> {
        let conn = self.db.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT id, name, content FROM templates ORDER BY name")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    pub fn delete_template(&self, id: &str) -> Result<(), String> {
        self.db
            .lock()
            .map_err(|e| e.to_string())?
            .execute("DELETE FROM templates WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn delete_scenario(&self, id: &str) -> Result<(), String> {
        self.db
            .lock()
            .map_err(|e| e.to_string())?
            .execute("DELETE FROM scenarios WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn log_send(&self, entry: &SendLogEntry) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        self.db
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "INSERT INTO send_logs (scenario_id, scenario_name, channel_id, status, detail, message_id, destination_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    entry.scenario_id,
                    entry.scenario_name,
                    entry.channel_id,
                    entry.status,
                    entry.detail,
                    entry.message_id,
                    entry.destination_id,
                    now
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Recent logs with combined filters (QoL): a case-insensitive
    /// keyword over scenario name / status / detail, an exact status,
    /// an exact scenario id, and an exact channel id (destination
    /// filter). `None`/empty means "no filter" for that dimension.
    /// The keyword is LIKE-escaped, so `%` and `_` typed by the user
    /// match literally instead of acting as wildcards.
    pub fn query_logs(
        &self,
        limit: usize,
        keyword: &str,
        status: Option<&str>,
        scenario_id: Option<&str>,
        channel_id: Option<&str>,
    ) -> Result<Vec<LogRow>, String> {
        let conn = self.db.lock().map_err(|e| e.to_string())?;
        let like = format!("%{}%", escape_like(keyword));
        let mut stmt = conn
            .prepare(
                "SELECT id, created_at, scenario_id, scenario_name, channel_id, status, detail,
                        COALESCE(message_id, ''), COALESCE(destination_id, '') FROM send_logs
                 WHERE (?1 = '%%' OR scenario_name LIKE ?1 ESCAPE '\\' COLLATE NOCASE
                        OR status LIKE ?1 ESCAPE '\\' COLLATE NOCASE
                        OR detail LIKE ?1 ESCAPE '\\' COLLATE NOCASE)
                   AND (?2 IS NULL OR status = ?2)
                   AND (?3 IS NULL OR scenario_id = ?3)
                   AND (?4 IS NULL OR channel_id = ?4)
                 ORDER BY id DESC LIMIT ?5",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(
                params![like, status, scenario_id, channel_id, limit as i64],
                |row| {
                    Ok(LogRow {
                        id: row.get(0)?,
                        created_at: row.get(1)?,
                        scenario_id: row.get(2)?,
                        scenario_name: row.get(3)?,
                        channel_id: row.get(4)?,
                        status: row.get(5)?,
                        detail: row.get(6)?,
                        message_id: row.get(7)?,
                        destination_id: row.get(8)?,
                    })
                },
            )
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    /// Timestamp (RFC 3339 UTC) of the most recent successful send,
    /// or None when nothing was ever sent.
    pub fn last_send_time(&self) -> Option<String> {
        let conn = self.db.lock().ok()?;
        conn.query_row(
            "SELECT MAX(created_at) FROM send_logs WHERE status = 'sent'",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    }

    /// Number of logs with the given status today (UTC date), from
    /// the persisted log. Used for persisted failure counts, so the
    /// Dashboard does not mix a persisted sent count with a
    /// session-only failed count.
    pub fn count_status_today(&self, status: &str) -> u32 {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let conn = match self.db.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        conn.query_row(
            "SELECT COUNT(*) FROM send_logs WHERE status = ?1 AND substr(created_at, 1, 10) = ?2",
            params![status, today],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        .max(0) as u32
    }
}

/// Escape a user-typed keyword for use inside a SQL LIKE pattern:
/// backslash, `%`, and `_` become literals (with `ESCAPE '\'`).
pub fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Escape one CSV field (RFC 4180): quote when the field contains a
/// comma, quote, or line break, and double any quotes inside.
pub fn csv_escape(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// Redact webhook URLs from text before it is exported or displayed.
/// Logs should never contain a URL in the first place; this is
/// defence in depth so an export can never leak one. The redaction
/// keeps the host + `/api/webhooks/` prefix out entirely: the whole
/// URL (scheme, host, id, token) becomes one placeholder.
pub fn scrub_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        // Find the next webhook path marker.
        let Some(marker) = rest.find("api/webhooks/") else {
            out.push_str(rest);
            break;
        };
        // Walk back over an optional scheme + host so the whole URL
        // is redacted, not just the path.
        let mut start = marker;
        while start > 0 {
            let prev = rest.as_bytes()[start - 1] as char;
            if prev.is_ascii_alphanumeric() || matches!(prev, '.' | '-' | ':' | '/') {
                start -= 1;
            } else {
                break;
            }
        }
        out.push_str(&rest[..start]);
        // Walk forward to the end of the URL (whitespace, quote,
        // comma, or end of string ends it).
        let mut end = marker + "api/webhooks/".len();
        let bytes = rest.as_bytes();
        while end < bytes.len() {
            let c = bytes[end] as char;
            if c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '<' | '>') {
                break;
            }
            end += 1;
        }
        out.push_str("[redacted webhook URL]");
        rest = &rest[end..];
    }
    out
}

pub fn load_config() -> AppConfig {
    let path = config_path();
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<AppConfig>(&s).ok())
        .unwrap_or_default()
}

pub fn save_config(cfg: &AppConfig) -> Result<(), String> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    write_config_file(&config_path(), &json)
}

/// Write `json` to `path` atomically: temp file in the same directory,
/// then rename. A crash mid-write leaves the old file intact. On Unix
/// the file is restricted to 0600 — it holds an encrypted token.
fn write_config_file(path: &std::path::Path, json: &str) -> Result<(), String> {
    let dir = path.parent().ok_or("config path has no parent directory")?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    let tmp_path = dir.join(format!(".{file_name}.tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<(), String> {
        std::fs::write(&tmp_path, json).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp_path, path).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only: insert a 'sent' log row stamped at `created_at`
    /// (RFC 3339), so tests can pin the clock instead of using now.
    #[cfg(test)]
    fn insert_send_log_at(store: &Store, created_at: &str) {
        store
            .db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO send_logs (scenario_id, scenario_name, channel_id, status, detail, created_at)
                 VALUES ('t', 't', 'c', 'sent', 'test', ?1)",
                params![created_at],
            )
            .unwrap();
    }

    #[test]
    fn scenario_roundtrip_in_memory_store() {
        let store = Store::open_memory().unwrap();
        let s = Scenario::new("roundtrip");
        store.save_scenario(&s).unwrap();
        let loaded = store.load_scenarios().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "roundtrip");
    }

    #[test]
    fn query_logs_escapes_like_wildcards_and_filters() {
        let store = Store::open_memory().unwrap();
        let log = |id: &str, name: &str, status: &str| {
            store
                .log_send(&SendLogEntry {
                    scenario_id: id.to_string(),
                    scenario_name: name.to_string(),
                    channel_id: "c".to_string(),
                    status: status.to_string(),
                    detail: format!("detail {name}"),
                    message_id: String::new(),
                    destination_id: String::new(),
                })
                .unwrap();
        };
        log("s1", "100%_done", "sent");
        log("s1", "plain", "failed");
        log("s2", "other", "sent");
        // Literal % and _ match only the literal name, not everything.
        let rows = store.query_logs(10, "100%_done", None, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].scenario_name, "100%_done");
        assert_eq!(rows[0].message_id, "");
        // Status + scenario filters combine.
        let rows = store
            .query_logs(10, "", Some("sent"), Some("s2"), None)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].scenario_name, "other");
        let rows = store
            .query_logs(10, "", Some("failed"), None, None)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(store.count_status_today("failed"), 1);
        assert!(store.last_send_time().is_some());
    }

    #[test]
    fn log_send_roundtrips_message_and_destination_ids() {
        let store = Store::open_memory().unwrap();
        store
            .log_send(&SendLogEntry {
                scenario_id: "s1".to_string(),
                scenario_name: "n".to_string(),
                channel_id: "c".to_string(),
                status: "sent".to_string(),
                detail: "message 123".to_string(),
                message_id: "123".to_string(),
                destination_id: "d1".to_string(),
            })
            .unwrap();
        let rows = store.query_logs(10, "", None, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message_id, "123");
        assert_eq!(rows[0].destination_id, "d1");
        assert!(rows[0].id > 0);
    }

    #[test]
    fn send_log_migration_adds_columns_to_legacy_schema() {
        // A database from before Q13: no message_id/destination_id.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE send_logs (id INTEGER PRIMARY KEY AUTOINCREMENT, scenario_id TEXT, scenario_name TEXT, channel_id TEXT, status TEXT, detail TEXT, created_at TEXT NOT NULL);
             INSERT INTO send_logs (scenario_id, scenario_name, channel_id, status, detail, created_at) VALUES ('s','n','c','sent','old','2026-01-01T00:00:00Z');",
        )
        .unwrap();
        Store::ensure_send_log_columns(&conn).unwrap();
        // Idempotent: a second run must not error.
        Store::ensure_send_log_columns(&conn).unwrap();
        let (mid, did): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT message_id, destination_id FROM send_logs LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((mid, did), (None, None));
    }

    #[test]
    fn csv_escape_and_scrub_secrets() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_escape("line1\nline2"), "\"line1\nline2\"");
        let scrubbed = scrub_secrets("see https://discord.com/api/webhooks/123/SECRETTOKEN here");
        assert_eq!(scrubbed, "see [redacted webhook URL] here");
        assert!(!scrubbed.contains("SECRETTOKEN"));
        assert_eq!(scrub_secrets("nothing secret"), "nothing secret");
    }

    #[test]
    fn destination_roundtrip_and_failure_state() {
        let store = Store::open_memory().unwrap();
        let mut d = Destination::new("Lớp học");
        d.channel_id = "chan-1".to_string();
        store.save_destination(&d).unwrap();
        let loaded = store.load_destinations().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "Lớp học");
        store
            .set_destination_failure(&d.id, FailureState::Paused404, "gone")
            .unwrap();
        let reloaded = store.load_destination(&d.id).unwrap().unwrap();
        assert_eq!(reloaded.failure_state, FailureState::Paused404);
        assert_eq!(reloaded.last_error, "gone");
        assert!(reloaded.paused_at.is_some());
        store
            .set_destination_failure(&d.id, FailureState::Ok, "")
            .unwrap();
        let resumed = store.load_destination(&d.id).unwrap().unwrap();
        assert_eq!(resumed.failure_state, FailureState::Ok);
        assert!(resumed.paused_at.is_none());
    }

    #[test]
    fn migration_creates_one_destination_and_assigns_scenarios() {
        let store = Store::open_memory().unwrap();
        let mut s = Scenario::new("migrated");
        s.destination_id = None;
        store.save_scenario(&s).unwrap();
        let mut cfg = AppConfig {
            send_mode: SendMode::Webhook,
            encrypted_webhook_url: "ENC".to_string(),
            webhook_id: "999".to_string(),
            webhook_name: "Announce".to_string(),
            ..AppConfig::default()
        };
        assert!(store.ensure_destinations_migrated(&mut cfg).unwrap());
        assert!(cfg.destinations_migrated);
        let dests = store.load_destinations().unwrap();
        assert_eq!(dests.len(), 1);
        assert_eq!(dests[0].webhook_id, "999");
        assert_eq!(dests[0].encrypted_url, "ENC");
        let scenarios = store.load_scenarios().unwrap();
        assert_eq!(scenarios[0].destination_id, Some(dests[0].id.clone()));
        // Second run is a no-op (idempotent, no duplicates).
        assert!(!store.ensure_destinations_migrated(&mut cfg).unwrap());
        assert_eq!(store.load_destinations().unwrap().len(), 1);
        // Legacy fields are kept, not deleted.
        assert_eq!(cfg.encrypted_webhook_url, "ENC");
    }

    #[test]
    fn template_crud_validates() {
        let store = Store::open_memory().unwrap();
        assert!(store.save_template("", "hi").is_err());
        assert!(store.save_template("x", &"a".repeat(2001)).is_err());
        let id = store.save_template("Morning", "Good morning!").unwrap();
        let list = store.load_templates().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].1, "Morning");
        store.delete_template(&id).unwrap();
        assert!(store.load_templates().unwrap().is_empty());
    }

    #[test]
    fn default_matches_serde_defaults() {
        // A fresh install (no config.json) gets Default; an old
        // config.json missing new fields gets the serde defaults.
        // Both must agree, or fresh and upgraded installs behave
        // differently. Compare via JSON: no PartialEq needed.
        let from_empty: AppConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(
            serde_json::to_string(&AppConfig::default()).unwrap(),
            serde_json::to_string(&from_empty).unwrap()
        );
        let cfg = AppConfig::default();
        assert_eq!(cfg.global_daily_cap, 300);
        assert_eq!(cfg.max_destinations, 10);
        assert_eq!(cfg.consecutive_error_limit, 5);
        assert_eq!(cfg.language, "vi");
        assert_eq!(cfg.theme, "dark");
        assert!(cfg.autostart);
        assert!(cfg.auto_check_updates);
        assert!(cfg.sound_enabled);
    }

    #[test]
    fn webhook_display_name_prefers_friendly_name() {
        let mut cfg = AppConfig::default();
        assert_eq!(cfg.webhook_display_name(), "Webhook");
        cfg.webhook_name = "Announce".to_string();
        assert_eq!(cfg.webhook_display_name(), "Announce");
        cfg.webhook_friendly_name = "Thông báo lớp".to_string();
        assert_eq!(cfg.webhook_display_name(), "Thông báo lớp");
    }

    /// Perf harness (ignored: run with `cargo test -- --ignored`).
    /// Measures the raw `count_sends_today` query the Dashboard used
    /// to run every frame. Kept in the repo so the number can be
    /// re-measured after storage changes; it prints, never asserts
    /// on timing (CI machines vary too much for timing asserts).
    #[test]
    #[ignore]
    fn bench_count_sends_today() {
        let store = Store::open_memory().unwrap();
        let today = chrono::Utc::now().to_rfc3339();
        {
            let db = store.db.lock().unwrap();
            for _ in 0..10_000 {
                db.execute(
                    "INSERT INTO send_logs (scenario_id, scenario_name, channel_id, status, detail, created_at) VALUES ('s','n','c','sent','d', ?1)",
                    params![today],
                )
                .unwrap();
            }
        }
        let start = std::time::Instant::now();
        let n = 1000;
        for _ in 0..n {
            assert_eq!(store.count_sends_today(), 10_000);
        }
        let avg_us = start.elapsed().as_micros() as f64 / n as f64;
        eprintln!("count_sends_today over 10k rows: {avg_us:.1} µs/call (1000 calls)");
    }

    #[test]
    fn count_sends_today_uses_utc_date() {
        let store = Store::open_memory().unwrap();
        let today = chrono::Utc::now().date_naive();
        let yesterday = today - chrono::Duration::days(1);
        // A send stamped yesterday UTC must not count — even though in a
        // far-eastern timezone (e.g. +14) it is still "today" locally.
        insert_send_log_at(&store, &format!("{yesterday}T12:00:00+00:00"));
        insert_send_log_at(&store, &format!("{today}T00:00:00+00:00"));
        assert_eq!(store.count_sends_today(), 1);
    }

    #[test]
    fn config_write_is_atomic_and_restricted_on_unix() {
        let dir = std::env::temp_dir().join(format!("nicord-config-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "old").unwrap();
        write_config_file(&path, "{\"new\":true}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"new\":true}");
        // No temp files left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
