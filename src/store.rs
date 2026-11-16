//! Local storage (Spec §9, §10): SQLite for scenarios + logs,
//! JSON for the app config (encrypted token, settings).
//! Nothing here is ever uploaded anywhere.

use crate::antiban::AntiBanConfig;
use crate::scenario::Scenario;
use crate::scheduler::SendLogEntry;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Mutex};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    /// AES-256-GCM encrypted Discord token (base64), or empty.
    #[serde(default)]
    pub encrypted_token: String,
    #[serde(default)]
    pub token_last4: String,
    #[serde(default)]
    pub tos_accepted: bool,
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
}

fn default_true() -> bool {
    true
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
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )
        .map_err(|e| format!("cannot create tables: {e}"))?;
        Ok(Self {
            db: Mutex::new(conn),
        })
    }

    /// Open a throwaway in-memory store (used by tests).
    #[cfg(test)]
    pub fn open_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE scenarios (id TEXT PRIMARY KEY, json TEXT NOT NULL, updated_at TEXT NOT NULL);
             CREATE TABLE send_logs (id INTEGER PRIMARY KEY AUTOINCREMENT, scenario_id TEXT, scenario_name TEXT, channel_id TEXT, status TEXT, detail TEXT, created_at TEXT NOT NULL);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
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
                "INSERT INTO send_logs (scenario_id, scenario_name, channel_id, status, detail, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    entry.scenario_id,
                    entry.scenario_name,
                    entry.channel_id,
                    entry.status,
                    entry.detail,
                    now
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Recent logs filtered by a case-insensitive substring over the
    /// scenario name or the status (empty filter = everything).
    pub fn recent_logs_filtered(
        &self,
        limit: usize,
        filter: &str,
    ) -> Result<Vec<(String, String, String, String)>, String> {
        let conn = self.db.lock().map_err(|e| e.to_string())?;
        let like = format!("%{}%", filter);
        let mut stmt = conn
            .prepare(
                "SELECT created_at, scenario_name, status, detail FROM send_logs
                 WHERE (?1 = '' OR scenario_name LIKE ?1 COLLATE NOCASE OR status LIKE ?1 COLLATE NOCASE)
                 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![like, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }
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
    std::fs::write(config_path(), json).map_err(|e| e.to_string())?;
    Ok(())
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
}
