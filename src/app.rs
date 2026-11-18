//! Native egui UI (eframe, immediate-mode — no React / WebView).
//! Screens: Onboarding (ToS warning → Token) → Dashboard →
//! Builder (list-based, add/remove/move up-down) → Logs → Settings.
//! Light by design: simple widgets, no canvas, no animations.
//!
//! Layout: left sidebar navigation (brand, nav, today status, Emergency
//! Stop) + central content with a header and a status banner. Discord-like
//! dark theme lives in `crate::theme`.
//!
//! One simple app for everyone: 1 channel, 300 messages/day
//! (cap resets 00:00 UTC). No tiers.

use crate::i18n::{t, Lang};
use crate::{
    crypto::mask_token,
    destination::Destination,
    discord::{Channel, DiscordClient, DiscordError, Guild, Me},
    preview::{char_count, expand_shortcodes, over_limit, parse_markdown},
    scenario::{
        analyze_import, slash_option_kind_label, CommandConfig, Node, NodeKind, Scenario, Schedule,
        SlashOption, SLASH_OPTION_KINDS,
    },
    scheduler::{channels_violation, Engine},
    secret_store::{
        delete_secret, destination_entry, load_secret, migrate_secret, store_secret,
        ENTRY_ACCOUNT_TOKEN, ENTRY_LEGACY_WEBHOOK,
    },
    store::{
        config_dir, csv_escape, load_config, load_config_status, restrict_permissions, save_config,
        scrub_secrets, AppConfig, ConfigLoad, SendMode, Store, WebhookHealth,
    },
    theme,
    updater::{self, CheckResult, UpdateState},
    webhook::{parse_webhook_url, MessageSender, WebhookClient, WebhookInfo},
};
use egui::RichText;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Onboarding,
    Dashboard,
    Builder,
    Destinations,
    Logs,
    Settings,
}

pub struct NicordApp {
    pub store: Arc<Store>,
    pub engine: Arc<Engine>,
    pub scenarios: Arc<Mutex<Vec<Scenario>>>,
    pub config: AppConfig,
    pub screen: Screen,
    pub status: Arc<Mutex<String>>,
    pub tos_checked: bool,
    pub token_input: String,
    /// Onboarding mode choice (new users default to Webhook).
    pub onboarding_mode: SendMode,
    /// Webhook URL being typed (never stored in plaintext).
    pub webhook_url_input: String,
    /// Result of the last "Kiểm tra webhook".
    pub webhook_info: Arc<Mutex<Option<WebhookInfo>>>,
    /// Status text for the webhook check (shared with its thread).
    pub webhook_check_status: Arc<Mutex<String>>,
    pub new_scenario_name: String,
    pub new_message: String,
    /// Builder list search query. Session-only: a search is
    /// transient, nobody expects it to survive a restart.
    pub builder_search: String,
    /// Last sane (non-minimized, non-maximized) window rect, for
    /// the geometry memory saved on exit / hide-to-tray.
    pub last_window_rect: Option<egui::Rect>,
    /// The start-minimized command is sent once, on the first frame.
    pub did_initial_minimize: bool,
    pub selected: Option<usize>,
    /// Builder: node selected for editing (command nodes have an
    /// editor panel; other kinds show no editor yet).
    pub selected_node: Option<String>,
    pub sent_today: u32,
    /// When `sent_today` was last re-read from the database. The
    /// count is refreshed at most every 5 seconds (see `update()`):
    /// the query costs ~3.3 ms over 10k rows, and running it every
    /// frame at 60 fps was pure waste.
    pub sent_today_updated: std::time::Instant,
    /// Display-only cache of next-run times (see `NextRunCache`).
    /// Session-only: recomputed from the schedules, never saved.
    pub next_run_cache: NextRunCache,
    /// Dashboard timeline (see `timeline::upcoming_runs`), refreshed
    /// at most every 30 s. Session-only: recomputed, never saved.
    pub timeline_cache: Vec<crate::timeline::UpcomingRun>,
    pub timeline_updated: std::time::Instant,
    /// Builder schedule simulation, keyed by the draft schedule +
    /// timezone + destination so it recomputes only when the draft
    /// changes. Session-only.
    pub sim_cache: Option<(String, Vec<crate::timeline::SimulatedRun>)>,
    pub guilds: Arc<Mutex<Vec<Guild>>>,
    pub channels: Arc<Mutex<Vec<Channel>>>,
    pub picker_guild_id: String,
    pub log_filter: String,
    /// Logs status filter ("" = all, else "sent"/"failed"/"deleted").
    pub log_status_filter: String,
    /// Logs scenario filter ("" = all, else a scenario id).
    pub log_scenario_filter: String,
    /// Logs destination filter ("" = all, else a channel id).
    pub log_channel_filter: String,
    /// Log row awaiting delete confirmation (Q13). Session-only.
    pub pending_delete_log: Option<i64>,
    /// Log row shown in the detail view (Group 3). Session-only.
    pub selected_log: Option<i64>,
    /// When the scheduled auto-backup was last checked (Group 3).
    /// Checked at most every 60 s from `update()`; the check
    /// itself is a timestamp comparison, the backup is a small
    /// local export. Initialized in the past so the first frame
    /// checks (and backs up on first run, when no backup exists).
    pub auto_backup_checked: std::time::Instant,
    pub import_path: String,
    /// Warnings from the last import analysis (QoL, shown in Builder).
    pub import_warnings: Vec<String>,
    /// Live webhook health, shared with the check thread; synced
    /// into `config` (and saved) from `update()`.
    pub webhook_health_shared: Arc<Mutex<WebhookHealth>>,
    /// Live last-checked timestamp (RFC 3339 UTC), same sharing.
    pub webhook_last_checked_shared: Arc<Mutex<String>>,
    /// Destination editor: selected id (None = creating new) + the
    /// working copy being edited (None = no editor open).
    pub dest_selected_id: Option<String>,
    pub dest_draft: Option<Destination>,
    /// Pasted URL for the destination editor (never prefilled).
    pub dest_url_input: String,
    /// New default message being composed in the editor.
    pub dest_new_message: String,
    /// Destination id awaiting delete confirmation (None = none).
    pub dest_delete_confirm: Option<String>,
    /// Reassignment target on delete ("" = disable the scenarios).
    pub dest_delete_target: String,
    /// Name for a new template saved from the compose field (Q6).
    pub template_name: String,
    /// Whether the emoji picker grid is open (Q10).
    pub show_emoji_picker: bool,
    /// Custom emoji markup/URL being composed (Q10).
    pub custom_emoji_input: String,
    /// Backup/export UI state.
    pub backup_export_mode_portable: bool,
    pub backup_passphrase: String,
    pub backup_passphrase_confirm: String,
    pub backup_export_path: String,
    pub backup_import_path: String,
    pub backup_import_passphrase: String,
    pub backup_import_confirm: bool,
    pub backup_warnings: Vec<String>,
    /// Startup health report (Q15). Computed once at startup and on
    /// demand; dismissing is session-only.
    pub health_issues: Vec<crate::health::HealthIssue>,
    pub health_dismissed: bool,
    /// Database rows too corrupt to parse, found at startup. Kept
    /// for the health report so they are visible instead of silently
    /// dropped (see `load_*_with_warnings`).
    pub load_warnings: Vec<String>,
    /// Result of the last "Kiểm tra token" (Token Validator).
    pub token_info: Arc<Mutex<Option<Me>>>,
    /// Auto-update state, shared with the background check/download
    /// threads (see `crate::updater`).
    pub update_status: Arc<Mutex<updater::UpdateStatus>>,
    /// System tray icon (Windows only, when enabled in Settings).
    #[cfg(target_os = "windows")]
    pub tray: Option<crate::tray::Tray>,
    /// Set by the tray "Thoát" menu item: the next close is a real quit.
    #[cfg(target_os = "windows")]
    pub allow_quit: bool,
}

/// Builder list search: case-insensitive substring match against
/// the scenario name or channel id. An empty query matches all.
fn scenario_matches_query(s: &Scenario, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    s.name.to_lowercase().contains(&q) || s.channel_id.to_lowercase().contains(&q)
}

/// Display-only cache of next-run times. Cron `next_after` walks
/// forward minute-by-minute and costs ~38 ms per call for sparse
/// schedules (measured; `0 0 29 2 *`), and the UI asks up to 4×
/// per scenario per frame (sidebar, next-run line, destination
/// cards, scenario rows) — one sparse cron could eat ~153 ms per
/// frame. Results are minute-granular, so a 60 s TTL is exact to
/// the displayed precision, and a cached value that already passed
/// is recomputed immediately (the UI never shows a stale past
/// time). Keyed by scenario id + the schedule's Debug form +
/// timezone, so any schedule or timezone edit invalidates
/// automatically. Display ONLY: the engine fires via `due_time`
/// and never reads this cache, so caching cannot affect sending.
#[derive(Default)]
pub struct NextRunCache {
    entries: std::collections::HashMap<
        String,
        (String, chrono::DateTime<chrono::Local>, std::time::Instant),
    >,
}

impl NextRunCache {
    /// Next run for one scenario, cached. `None` when the scenario
    /// has no schedule.
    pub fn next_run(
        &mut self,
        scenario: &Scenario,
        now: chrono::DateTime<chrono::Local>,
    ) -> Option<chrono::DateTime<chrono::Local>> {
        let sched = scenario.schedule.as_ref()?;
        let key = format!("{:?}|{}", sched, scenario.timezone);
        if let Some((k, v, at)) = self.entries.get(&scenario.id) {
            if k == &key && at.elapsed() < std::time::Duration::from_secs(60) && *v > now {
                return Some(*v);
            }
        }
        let v = sched.next_after(now, &scenario.timezone);
        self.entries
            .insert(scenario.id.clone(), (key, v, std::time::Instant::now()));
        // Deleted scenarios leave one stale entry each; prune when
        // the map grows past what the live list can justify.
        if self.entries.len() > 512 {
            self.entries
                .retain(|_, (_, _, at)| at.elapsed() < std::time::Duration::from_secs(60));
        }
        Some(v)
    }

    /// Next runs for a whole scenario list, in one pass: the
    /// per-screen call sites build this map once per frame and
    /// look up from it, instead of calling `next_after` per row.
    pub fn next_runs(
        &mut self,
        scenarios: &[Scenario],
        now: chrono::DateTime<chrono::Local>,
    ) -> std::collections::HashMap<String, chrono::DateTime<chrono::Local>> {
        scenarios
            .iter()
            .filter_map(|s| self.next_run(s, now).map(|v| (s.id.clone(), v)))
            .collect()
    }
}

/// Enable one scenario with the same validation the Builder's
/// Enable button applies. Returns an error message when the
/// scenario must stay disabled (invalid, or — user-token mode
/// only — a second distinct channel). `None` means enabled.
/// Already-enabled scenarios are a silent no-op.
fn try_enable_scenario(list: &mut [Scenario], i: usize, is_webhook: bool) -> Option<String> {
    let s = list.get(i)?;
    if s.enabled {
        return None;
    }
    let valid = if is_webhook {
        s.validate_for_webhook()
    } else {
        s.validate()
    };
    if let Err(e) = valid {
        return Some(format!("Cannot enable: {e}"));
    }
    if !is_webhook {
        // Hypothetical list with this scenario enabled: only 1
        // distinct channel is allowed across enabled scenarios
        // (user mode; webhook mode ignores channel_id).
        let mut hypo = list.to_vec();
        if let Some(s) = hypo.get_mut(i) {
            s.enabled = true;
        }
        if let Some(v) = channels_violation(&hypo) {
            return Some(v);
        }
    }
    if let Some(s) = list.get_mut(i) {
        s.enabled = true;
        // A fired `Once` scenario keeps its last_run_at, so due_time
        // would never fire it again — re-enabling would silently do
        // nothing. Re-enabling is the user's explicit "run it
        // again": clear the anchor so the Once fires (immediately
        // when its time already passed).
        if matches!(s.schedule, Some(Schedule::Once { .. })) {
            s.last_run_at = None;
        }
    }
    None
}

/// Build a sender from a config snapshot, following its send mode.
/// None when the active mode has no usable credential.
/// User-token mode only in practice for the engine: webhook mode
/// resolves per-scenario destinations (see below).
fn sender_for_config(config: &AppConfig) -> Option<MessageSender> {
    match config.send_mode {
        SendMode::UserToken => load_secret(&config.encrypted_token)
            .ok()
            .and_then(|t| DiscordClient::new(&t).map(MessageSender::User)),
        SendMode::Webhook => {
            let url = load_secret(&config.encrypted_webhook_url).ok()?;
            let parsed = parse_webhook_url(&url).ok()?;
            WebhookClient::new(parsed, &config.webhook_username).map(MessageSender::Webhook)
        }
    }
}

/// Resolve a scenario's sender + destination from a config snapshot.
/// Webhook mode: the scenario's `destination_id` selects the
/// destination; the sender is built from that destination's own
/// encrypted URL and username override. None when the scenario has
/// no destination, the destination is missing, or its URL cannot be
/// decrypted/parsed — the engine skips such scenarios instead of
/// silently sending to a different channel. User-token mode:
/// the account sender, no destination.
fn resolve_sender_for_scenario(
    store: &Store,
    config: &AppConfig,
    scenario: &Scenario,
) -> Option<(MessageSender, Option<Destination>)> {
    match config.send_mode {
        SendMode::UserToken => sender_for_config(config).map(|s| (s, None)),
        SendMode::Webhook => {
            let dest_id = scenario.destination_id.as_deref()?;
            let dest = store.load_destination(dest_id).ok()??;
            let url = load_secret(&dest.encrypted_url).ok()?;
            let parsed = parse_webhook_url(&url).ok()?;
            let client = WebhookClient::new(parsed, &dest.username_override)?
                .with_avatar_url(&dest.avatar_url_override)
                .with_thread_id(&dest.thread_id);
            Some((MessageSender::Webhook(client), Some(dest)))
        }
    }
}

/// Build a sender for one destination (Destinations "Gửi thử").
/// None when the destination has no usable URL.
fn sender_for_destination(dest: &Destination) -> Option<MessageSender> {
    let url = load_secret(&dest.encrypted_url).ok()?;
    let parsed = parse_webhook_url(&url).ok()?;
    let client = WebhookClient::new(parsed, &dest.username_override)?
        .with_avatar_url(&dest.avatar_url_override)
        .with_thread_id(&dest.thread_id);
    Some(MessageSender::Webhook(client))
}

impl NicordApp {
    pub fn new(store: Arc<Store>) -> Self {
        let (mut config, config_load) = load_config_status();
        // An unreadable config must never be overwritten by startup
        // saves: a transient read failure would destroy the user's
        // real settings. Defaults run in memory; the file on disk
        // is left alone and the health report warns about it.
        let config_unreadable = config_load == ConfigLoad::Unreadable;
        // The autostart toggle shows the real OS state, not just the
        // last saved flag (the user may have changed it outside Nicord).
        config.autostart = crate::autostart::is_enabled();
        // One-time migration: legacy single webhook -> destinations
        // table. Idempotent; the config is saved below when it changed.
        let migrated = store
            .ensure_destinations_migrated(&mut config)
            .unwrap_or(false);
        if migrated && !config_unreadable {
            let _ = save_config(&config);
        }
        // One-time secret migration: move machine-encrypted blobs
        // into the OS keyring when it is available. Best-effort and
        // idempotent — anything that cannot migrate stays a blob
        // and keeps working (see `secret_store::migrate_secret`).
        let mut secrets_migrated = false;
        if let Some(m) = migrate_secret(&config.encrypted_token, ENTRY_ACCOUNT_TOKEN) {
            config.encrypted_token = m;
            secrets_migrated = true;
        }
        if let Some(m) = migrate_secret(&config.encrypted_webhook_url, ENTRY_LEGACY_WEBHOOK) {
            config.encrypted_webhook_url = m;
            secrets_migrated = true;
        }
        if secrets_migrated && !config_unreadable {
            let _ = save_config(&config);
        }
        if let Ok(dests) = store.load_destinations() {
            for mut d in dests {
                let entry = destination_entry(&d.id);
                if let Some(m) = migrate_secret(&d.encrypted_url, &entry) {
                    d.encrypted_url = m;
                    let _ = store.save_destination(&d);
                }
            }
        }
        // Read before the struct literal below moves `config`.
        #[cfg(target_os = "windows")]
        let tray_enabled = config.tray_enabled;
        let (scenarios, mut load_warnings) =
            store.load_scenarios_with_warnings().unwrap_or_default();
        let (_, dest_warnings) = store.load_destinations_with_warnings().unwrap_or_default();
        load_warnings.extend(dest_warnings);
        if config_unreadable {
            load_warnings.push(
                "Không đọc được file cấu hình (config.json) — đang dùng cấu hình mặc định. File gốc được giữ nguyên; hãy kiểm tra ổ đĩa và quyền truy cập trước khi lưu cài đặt."
                    .to_string(),
            );
        }
        for w in &load_warnings {
            tracing::warn!("{w}");
        }
        // Log retention (Group 3): prune once at startup.
        // Best-effort — a failure must never block startup; it
        // is logged and the manual button in Settings stays
        // available. Retention 0 means "keep forever".
        if config.log_retention_days > 0 {
            match store.prune_logs(config.log_retention_days) {
                Ok(n) if n > 0 => {
                    tracing::info!("pruned {n} old send-log rows at startup");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("startup log prune failed: {e}"),
            }
        }
        // Read before the struct literal below moves `config`.
        let webhook_health_init = config.webhook_health;
        let webhook_last_checked_init = config.webhook_last_checked_at.clone();
        // Onboarding is skipped when the self-bot ToS was accepted OR a
        // webhook is already saved (webhook users never accept the
        // self-bot warning — it does not apply to them).
        let screen = if !config.tos_accepted && config.encrypted_webhook_url.is_empty() {
            Screen::Onboarding
        } else {
            Screen::Dashboard
        };
        let engine = Arc::new(Engine::new(
            store.clone(),
            config.antiban.clone(),
            config.global_daily_cap,
            config.consecutive_error_limit,
        ));
        let has_enabled = scenarios.iter().any(|s| s.enabled);
        // First "sent today" value, so the Dashboard is correct from
        // the first frame (the 5 s cache in update() takes over after).
        let sent_today_init = store.count_sends_today();
        let mut app = Self {
            store,
            engine,
            scenarios: Arc::new(Mutex::new(scenarios)),
            config,
            screen,
            status: Arc::new(Mutex::new(String::new())),
            tos_checked: false,
            token_input: String::new(),
            onboarding_mode: SendMode::Webhook,
            webhook_url_input: String::new(),
            webhook_info: Arc::new(Mutex::new(None)),
            webhook_check_status: Arc::new(Mutex::new(String::new())),
            new_scenario_name: String::new(),
            new_message: String::new(),
            builder_search: String::new(),
            last_window_rect: None,
            did_initial_minimize: false,
            selected: None,
            selected_node: None,
            sent_today: sent_today_init,
            sent_today_updated: std::time::Instant::now(),
            next_run_cache: NextRunCache::default(),
            timeline_cache: Vec::new(),
            // Ancient: the first Dashboard frame computes it.
            timeline_updated: std::time::Instant::now() - std::time::Duration::from_secs(3600),
            sim_cache: None,
            guilds: Arc::new(Mutex::new(Vec::new())),
            channels: Arc::new(Mutex::new(Vec::new())),
            picker_guild_id: String::new(),
            log_filter: String::new(),
            log_status_filter: String::new(),
            log_scenario_filter: String::new(),
            log_channel_filter: String::new(),
            pending_delete_log: None,
            selected_log: None,
            auto_backup_checked: std::time::Instant::now() - std::time::Duration::from_secs(3600),
            import_path: String::new(),
            import_warnings: Vec::new(),
            webhook_health_shared: Arc::new(Mutex::new(webhook_health_init)),
            webhook_last_checked_shared: Arc::new(Mutex::new(webhook_last_checked_init)),
            dest_selected_id: None,
            dest_draft: None,
            dest_url_input: String::new(),
            dest_new_message: String::new(),
            dest_delete_confirm: None,
            dest_delete_target: String::new(),
            template_name: String::new(),
            show_emoji_picker: false,
            custom_emoji_input: String::new(),
            backup_export_mode_portable: true,
            backup_passphrase: String::new(),
            backup_passphrase_confirm: String::new(),
            backup_export_path: String::new(),
            backup_import_path: String::new(),
            backup_import_passphrase: String::new(),
            backup_import_confirm: false,
            backup_warnings: Vec::new(),
            health_issues: Vec::new(),
            health_dismissed: false,
            load_warnings,
            token_info: Arc::new(Mutex::new(None)),
            update_status: Arc::new(Mutex::new(updater::UpdateStatus::default())),
            #[cfg(target_os = "windows")]
            tray: if tray_enabled {
                crate::tray::Tray::new()
            } else {
                None
            },
            #[cfg(target_os = "windows")]
            allow_quit: false,
        };
        // Auto-resume: enabled scenarios keep running across restarts.
        // The engine is started here (not only from the Dashboard button)
        // when there is anything enabled and the active mode has a
        // credential (user-token mode additionally requires ToS acceptance).
        let can_start = match app.config.send_mode {
            SendMode::UserToken => {
                app.config.tos_accepted && !app.config.encrypted_token.is_empty()
            }
            SendMode::Webhook => {
                // Any destination with a URL (or the legacy URL, kept
                // as a fallback until migration is confirmed stable).
                !app.config.encrypted_webhook_url.is_empty()
                    || app
                        .store
                        .load_destinations()
                        .map(|ds| ds.iter().any(|d| !d.encrypted_url.trim().is_empty()))
                        .unwrap_or(false)
            }
        };
        if has_enabled && can_start {
            app.start_engine();
            app.set_status("Resumed enabled scenarios from last session.");
        }
        // Auto-update: one quiet check on startup (never on Onboarding,
        // so the dialog cannot appear before setup is done). A failed
        // check or "no releases yet" stays silent; only a newer release
        // opens the dialog.
        if app.config.auto_check_updates && app.screen != Screen::Onboarding {
            app.check_updates(false);
        }
        // Startup health report (Q15): offline checks, shown on the
        // Dashboard until dismissed.
        app.refresh_health();
        app
    }

    /// Recompute the startup health report from the current state.
    /// "m:ss" left in the 3-consecutive-errors engine pause, if
    /// the pause is in effect (F-GUI-4). Read fresh every frame
    /// by the sidebar and the Dashboard engine card.
    fn error_pause_label(&self) -> Option<String> {
        self.engine
            .error_pause_remaining()
            .map(|d| format!("{}:{:02}", d.as_secs() / 60, d.as_secs() % 60))
    }

    fn refresh_health(&mut self) {
        let destinations = self.store.load_destinations().unwrap_or_default();
        let scenarios = self.scenarios.lock().map(|l| l.clone()).unwrap_or_default();
        self.health_issues = crate::health::startup_checks(&self.config, &destinations, &scenarios);
        // Corrupt rows found at startup: visible here (max 5, then a
        // collapse line, matching the health module's convention).
        for w in self.load_warnings.iter().take(5) {
            self.health_issues.push(crate::health::HealthIssue {
                severity: crate::health::Severity::Warning,
                message: w.clone(),
                target: None,
            });
        }
        if self.load_warnings.len() > 5 {
            self.health_issues.push(crate::health::HealthIssue {
                severity: crate::health::Severity::Warning,
                message: format!(
                    "... và thêm {} hàng lỗi khác.",
                    self.load_warnings.len() - 5
                ),
                target: None,
            });
        }
    }

    /// Current UI language (from config, applied instantly).
    fn lang(&self) -> Lang {
        Lang::from_code(&self.config.language)
    }

    fn set_status(&self, msg: &str) {
        if let Ok(mut s) = self.status.lock() {
            *s = msg.to_string();
        }
    }

    fn status_text(&self) -> String {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn save_scenarios(&self) {
        if let Ok(list) = self.scenarios.lock() {
            for s in list.iter() {
                let _ = self.store.save_scenario(s);
            }
        }
    }

    /// Create a scenario from the "New scenario name" field (or
    /// "Untitled scenario" when it is empty), pre-selecting the
    /// first sendable destination in webhook mode. Shared by the
    /// "+ New scenario" button and the Ctrl+N shortcut.
    fn create_scenario(&mut self) {
        // No scenario-count limit: the only limit is 1 distinct
        // channel, enforced when a scenario is enabled.
        let name = if self.new_scenario_name.trim().is_empty() {
            "Untitled scenario".to_string()
        } else {
            self.new_scenario_name.trim().to_string()
        };
        let mut s = Scenario::new(&name);
        // Webhook mode: pre-select the first enabled destination
        // so the scenario can be enabled without an extra step
        // (user can change it).
        if self.config.send_mode == SendMode::Webhook {
            if let Ok(dests) = self.store.load_destinations() {
                if let Some(first) = dests.iter().find(|d| d.is_sendable()) {
                    s.destination_id = Some(first.id.clone());
                }
            }
        }
        if let Ok(mut list) = self.scenarios.lock() {
            list.push(s);
        }
        self.save_scenarios();
        self.new_scenario_name.clear();
    }

    /// The scenario a Dashboard "Test send" would send: the
    /// Builder-selected one when set and in range, else the first.
    /// The Dashboard button names this scenario so the user always
    /// sees the target before a REAL message goes out.
    fn test_send_target(&self) -> Option<Scenario> {
        let list = self.scenarios.lock().ok()?;
        if let Some(idx) = self.selected {
            if let Some(s) = list.get(idx) {
                return Some(s.clone());
            }
        }
        list.first().cloned()
    }

    /// Spawn a background thread that sends one test message for the
    /// selected-or-first scenario (see `test_send_target`), using
    /// the current send mode's stored credential. In webhook mode
    /// the scenario's own destination is used.
    fn test_send(&self) {
        let Some(scenario) = self.test_send_target() else {
            self.set_status("No scenario yet — create one first.");
            return;
        };
        let (sender, destination) =
            match resolve_sender_for_scenario(&self.store, &self.config, &scenario) {
                Some(pair) => pair,
                None => {
                    self.set_status(
                    "No valid credential/destination for this scenario in the current send mode.",
                );
                    return;
                }
            };
        let engine = self.engine.clone();
        let status = self.status.clone();
        std::thread::spawn(move || {
            let rt = match shared_runtime() {
                Ok(rt) => rt,
                Err(e) => {
                    if let Ok(mut s) = status.lock() {
                        *s = format!("cannot start runtime: {e}");
                    }
                    return;
                }
            };
            // Template identity: per-destination in webhook mode
            // (stored name, no network), account identity in user mode.
            let (username, server_name) = if let Some(dest) = &destination {
                (dest.effective_username(), String::new())
            } else {
                rt.block_on(async {
                    match &sender {
                        MessageSender::User(client) => {
                            let u = client
                                .get_me_typed()
                                .await
                                .map(|m| m.display_name())
                                .unwrap_or_else(|_| "you".to_string());
                            let s = client
                                .list_guilds()
                                .await
                                .ok()
                                .and_then(|g| g.first().map(|x| x.name.clone()))
                                .unwrap_or_else(|| "server".to_string());
                            (u, s)
                        }
                        MessageSender::Webhook(_) => ("you".to_string(), String::new()),
                    }
                })
            };
            let result = rt.block_on(engine.send_once(
                &scenario,
                &sender,
                destination.as_ref(),
                &username,
                &server_name,
            ));
            // The engine already beeps on success; beep here too for
            // failures (test sends bypass the run loop's failure log).
            // The throttle keeps a success from beeping twice.
            if result.is_ok() {
                crate::notify::play(crate::notify::SoundEvent::SendSuccess);
            } else {
                crate::notify::play(crate::notify::SoundEvent::SendFailure);
            }
            if let Ok(mut s) = status.lock() {
                *s = match result {
                    Ok(id) if id.is_empty() => "Test sent! (webhook, no id returned)".to_string(),
                    Ok(id) => format!("Test sent! message id {id}"),
                    Err(e) => format!("Test failed: {e}"),
                };
            }
        });
    }

    /// Token Validator (Discord-BLUE): check the stored token with
    /// `GET /users/@me` in a background thread and show the account
    /// (username, id, email, 2FA, Nitro). The token itself is never shown.
    fn check_token(&self) {
        let token = match load_secret(&self.config.encrypted_token) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(&format!("No valid token stored: {e}"));
                return;
            }
        };
        let info = self.token_info.clone();
        let status = self.status.clone();
        std::thread::spawn(move || {
            let rt = match shared_runtime() {
                Ok(rt) => rt,
                Err(e) => {
                    if let Ok(mut s) = status.lock() {
                        *s = format!("cannot start runtime: {e}");
                    }
                    return;
                }
            };
            let Some(client) = DiscordClient::new(&token) else {
                if let Ok(mut s) = status.lock() {
                    *s = "Cannot build the HTTP client (TLS/resolver init failed).".to_string();
                }
                return;
            };
            match rt.block_on(client.get_me_typed()) {
                Ok(me) => {
                    if let Ok(mut i) = info.lock() {
                        *i = Some(me);
                    }
                    if let Ok(mut s) = status.lock() {
                        *s = "Token hợp lệ.".to_string();
                    }
                }
                Err(e) => {
                    if let Ok(mut i) = info.lock() {
                        *i = None;
                    }
                    if let Ok(mut s) = status.lock() {
                        *s = match e {
                            DiscordError::Unauthorized(_) => {
                                "Token không hợp lệ hoặc đã hết hạn.".to_string()
                            }
                            _ => format!("Không kiểm tra được token: {e}"),
                        };
                    }
                }
            }
        });
    }

    /// Webhook check: GET the webhook from a pasted URL (not yet saved)
    /// in a background thread and show its name + channel. The URL
    /// itself is never shown or logged. `url` is the raw pasted text.
    fn check_webhook(&self, url: String) {
        let parsed = match parse_webhook_url(&url) {
            Ok(p) => p,
            Err(e) => {
                self.set_status(&e);
                if let Ok(mut i) = self.webhook_info.lock() {
                    *i = None;
                }
                return;
            }
        };
        let info = self.webhook_info.clone();
        let check_status = self.webhook_check_status.clone();
        let status = self.status.clone();
        let health_shared = self.webhook_health_shared.clone();
        let checked_shared = self.webhook_last_checked_shared.clone();
        let engine = self.engine.clone();
        std::thread::spawn(move || {
            let rt = match shared_runtime() {
                Ok(rt) => rt,
                Err(e) => {
                    if let Ok(mut s) = status.lock() {
                        *s = format!("cannot start runtime: {e}");
                    }
                    return;
                }
            };
            let Some(client) = WebhookClient::new(parsed, "") else {
                if let Ok(mut s) = status.lock() {
                    *s = "Cannot build the HTTP client (TLS/resolver init failed).".to_string();
                }
                return;
            };
            let now = chrono::Utc::now().to_rfc3339();
            match rt.block_on(client.get_webhook()) {
                Ok(w) => {
                    let msg = format!(
                        "Webhook hợp lệ: {} (kênh {})",
                        w.display_name(),
                        w.channel_id.clone().unwrap_or_default()
                    );
                    let checked_id = w.id.clone();
                    if let Ok(mut i) = info.lock() {
                        *i = Some(w);
                    }
                    if let Ok(mut h) = health_shared.lock() {
                        *h = WebhookHealth::Ok;
                    }
                    if let Ok(mut c) = checked_shared.lock() {
                        *c = now;
                    }
                    // A good check clears the pause of any destination
                    // backed by this webhook.
                    if let Ok(dests) = engine.store.load_destinations() {
                        for d in dests {
                            if d.webhook_id == checked_id {
                                let _ = engine.store.set_destination_failure(
                                    &d.id,
                                    crate::destination::FailureState::Ok,
                                    "",
                                );
                            }
                        }
                    }
                    if let Ok(mut s) = check_status.lock() {
                        *s = msg.clone();
                    }
                    if let Ok(mut s) = status.lock() {
                        *s = msg;
                    }
                }
                Err(e) => {
                    if let Ok(mut i) = info.lock() {
                        *i = None;
                    }
                    // Rate limits are transient: keep the previous
                    // health instead of crying wolf.
                    let (health, msg) = match &e {
                        DiscordError::NotFound(_) => (
                            Some(WebhookHealth::NotFound),
                            "Webhook không tồn tại (đã xóa hoặc URL sai).".to_string(),
                        ),
                        DiscordError::Forbidden(_) | DiscordError::Unauthorized(_) => (
                            Some(WebhookHealth::Forbidden),
                            format!("Không kiểm tra được webhook: {e}"),
                        ),
                        DiscordError::RateLimited { .. } => (
                            None,
                            "Discord đang giới hạn (429) — thử kiểm tra lại sau.".to_string(),
                        ),
                        DiscordError::Other(_) => (
                            Some(WebhookHealth::NetworkError),
                            format!("Không kiểm tra được webhook: {e}"),
                        ),
                    };
                    if let Some(h) = health {
                        if let Ok(mut hs) = health_shared.lock() {
                            *hs = h;
                        }
                        if let Ok(mut c) = checked_shared.lock() {
                            *c = now;
                        }
                    }
                    if let Ok(mut s) = check_status.lock() {
                        *s = msg.clone();
                    }
                    if let Ok(mut s) = status.lock() {
                        *s = msg;
                    }
                }
            }
        });
    }

    /// Dual-write: after a legacy webhook URL is saved (onboarding
    /// or Settings), mirror it into the destinations table so the
    /// engine — which sends via destinations — can use it. Updates
    /// the destination with the same webhook id, or creates one.
    /// Best-effort: a failure here never blocks the config save.
    fn upsert_destination_from_legacy(&self) {
        if self.config.encrypted_webhook_url.trim().is_empty() {
            return;
        }
        let Ok(mut dests) = self.store.load_destinations() else {
            return;
        };
        let name = if !self.config.webhook_friendly_name.trim().is_empty() {
            self.config.webhook_friendly_name.trim().to_string()
        } else if !self.config.webhook_name.trim().is_empty() {
            self.config.webhook_name.trim().to_string()
        } else {
            "Webhook chính".to_string()
        };
        if let Some(existing) = dests
            .iter_mut()
            .find(|d| !self.config.webhook_id.is_empty() && d.webhook_id == self.config.webhook_id)
        {
            // Resolve + re-store under the destination's own keyring
            // entry: copying the config's stored value would copy a
            // keyring reference bound to the config slot. If the
            // stored URL cannot be resolved, leave the destination's
            // existing URL alone instead of propagating breakage.
            if let Ok(plain) = load_secret(&self.config.encrypted_webhook_url) {
                if let Ok(stored) = store_secret(&destination_entry(&existing.id), &plain) {
                    existing.encrypted_url = stored;
                    // Only a successfully resolved + re-stored URL
                    // clears a persisted pause: clearing it when the
                    // URL could not be resolved would un-pause a
                    // destination whose URL is still broken.
                    existing.failure_state = crate::destination::FailureState::Ok;
                    existing.last_error.clear();
                    existing.paused_at = None;
                }
            }
            existing.webhook_name = self.config.webhook_name.clone();
            existing.username_override = self.config.webhook_username.clone();
            let _ = self.store.save_destination(existing);
        } else {
            let mut dest = Destination::new(&name);
            // Same resolve + re-store as above: the new destination
            // gets its own keyring entry, never the config slot's
            // reference. Unresolvable input leaves the URL empty.
            if let Ok(plain) = load_secret(&self.config.encrypted_webhook_url) {
                if let Ok(stored) = store_secret(&destination_entry(&dest.id), &plain) {
                    dest.encrypted_url = stored;
                }
            }
            dest.webhook_id = self.config.webhook_id.clone();
            dest.webhook_name = self.config.webhook_name.clone();
            dest.username_override = self.config.webhook_username.clone();
            // Fill channel/guild from the last matching check.
            if let Ok(info) = self.webhook_info.lock() {
                if let Some(w) = info.as_ref() {
                    if w.id == self.config.webhook_id {
                        dest.channel_id = w.channel_id.clone().unwrap_or_default();
                        dest.guild_id = w.guild_id.clone().unwrap_or_default();
                    }
                }
            }
            let _ = self.store.save_destination(&dest);
        }
    }

    /// Re-check the STORED webhook URL (Settings "Kiểm tra lại").
    /// Decrypts the saved URL and runs the same check as a paste.
    fn recheck_stored_webhook(&self) {
        match load_secret(&self.config.encrypted_webhook_url) {
            Ok(url) => self.check_webhook(url),
            Err(e) => self.set_status(&format!("No valid webhook URL stored: {e}")),
        }
    }

    /// Send one command node once, right now (Builder "Test lệnh").
    /// Builds a minimal scenario (trigger + that command node only)
    /// and runs it through the normal send pipeline (daily cap, slot,
    /// typing for prefix; gateway + interaction for slash).
    /// User-token mode only: webhooks cannot send commands.
    fn test_command(&self, i: usize, node_id: &str) {
        if self.config.send_mode == SendMode::Webhook {
            self.set_status("Webhook mode cannot send commands.");
            return;
        }
        let token = match load_secret(&self.config.encrypted_token) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(&format!("No valid token stored: {e}"));
                return;
            }
        };
        let (scenario, name) = {
            let Ok(list) = self.scenarios.lock() else {
                self.set_status("Cannot read scenarios.");
                return;
            };
            let Some(s) = list.get(i) else {
                self.set_status("No scenario selected.");
                return;
            };
            let Some(n) = s.nodes.iter().find(|n| n.id == node_id).cloned() else {
                self.set_status("Node not found.");
                return;
            };
            let display = CommandConfig::from_node(&n).display_name();
            let mut test = s.clone();
            test.nodes = vec![Node::new(NodeKind::TriggerSchedule), n];
            test.enabled = true;
            (test, display)
        };
        let engine = self.engine.clone();
        let status = self.status.clone();
        std::thread::spawn(move || {
            let rt = match shared_runtime() {
                Ok(rt) => rt,
                Err(e) => {
                    if let Ok(mut s) = status.lock() {
                        *s = format!("cannot start runtime: {e}");
                    }
                    return;
                }
            };
            let Some(client) = DiscordClient::new(&token) else {
                return;
            };
            let (username, server_name) = rt.block_on(async {
                let u = client
                    .get_me_typed()
                    .await
                    .map(|m| m.display_name())
                    .unwrap_or_else(|_| "you".to_string());
                let s = client
                    .list_guilds()
                    .await
                    .ok()
                    .and_then(|g| g.first().map(|x| x.name.clone()))
                    .unwrap_or_else(|| "server".to_string());
                (u, s)
            });
            let sender = MessageSender::User(client);
            let result =
                rt.block_on(engine.send_once(&scenario, &sender, None, &username, &server_name));
            if let Ok(mut s) = status.lock() {
                *s = match result {
                    Ok(id) => format!("Lệnh {name} đã gửi! ({id})"),
                    Err(e) => format!("Test lệnh thất bại: {e}"),
                };
            }
        });
    }

    /// Check GitHub Releases for a newer version, on a background
    /// thread (never blocks the UI). `manual` is the Settings button:
    /// it reports the result in the status banner, while the startup
    /// auto-check stays silent unless a newer release exists.
    fn check_updates(&self, manual: bool) {
        let shared = self.update_status.clone();
        let status = self.status.clone();
        if let Ok(mut u) = shared.lock() {
            u.state = UpdateState::Checking;
            u.dialog = false;
        }
        if manual {
            self.set_status("Đang kiểm tra cập nhật…");
        }
        std::thread::spawn(move || {
            let rt = match shared_runtime() {
                Ok(rt) => rt,
                Err(e) => {
                    if let Ok(mut u) = shared.lock() {
                        u.state = UpdateState::Failed(format!("cannot start runtime: {e}"));
                    }
                    return;
                }
            };
            let result = rt.block_on(updater::check_for_update());
            tracing::info!(
                "update check: {}",
                match &result {
                    CheckResult::Available(info) => format!("new release {}", info.tag),
                    CheckResult::UpToDate => "up to date".to_string(),
                    CheckResult::NoRelease => "no releases".to_string(),
                    CheckResult::Failed(e) => format!("failed: {e}"),
                }
            );
            let mut text: Option<String> = None;
            if let Ok(mut u) = shared.lock() {
                match &result {
                    CheckResult::Available(info) => {
                        u.state = UpdateState::Available(info.clone());
                        // The one case that opens the dialog — and only
                        // the dialog asks; nothing installs silently.
                        u.dialog = true;
                        text = Some(format!("Có bản mới {}!", info.tag));
                    }
                    CheckResult::UpToDate => {
                        u.state = UpdateState::UpToDate;
                        text = Some("Bạn đang dùng bản mới nhất.".to_string());
                    }
                    CheckResult::NoRelease => {
                        u.state = UpdateState::NoRelease;
                        text = Some("Chưa có bản phát hành.".to_string());
                    }
                    CheckResult::Failed(e) => {
                        // Quiet by design (auto-check): no error dialog.
                        u.state = UpdateState::Failed(e.clone());
                        text = Some(format!("Kiểm tra cập nhật thất bại: {e}"));
                    }
                }
            }
            // Manual checks report in the banner; the startup check
            // only speaks up when there is actually a newer release.
            if manual {
                if let (Some(t), Ok(mut s)) = (text, status.lock()) {
                    *s = t;
                }
            } else if let CheckResult::Available(info) = &result {
                if let Ok(mut s) = status.lock() {
                    *s = format!("Có bản mới {} — xem hộp thoại cập nhật.", info.tag);
                }
            }
        });
    }

    /// One-line Settings status for the updater (read once per frame).
    fn update_status_line(&self) -> String {
        let Ok(u) = self.update_status.lock() else {
            return String::new();
        };
        match &u.state {
            UpdateState::Idle => "Chưa kiểm tra.".to_string(),
            UpdateState::Checking => "Đang kiểm tra…".to_string(),
            UpdateState::Available(info) => format!("Có bản mới {} — xem hộp thoại.", info.tag),
            UpdateState::UpToDate => "Bạn đang dùng bản mới nhất.".to_string(),
            UpdateState::NoRelease => "Chưa có bản phát hành.".to_string(),
            UpdateState::Downloading => {
                let total = u
                    .total
                    .map(|t| format!(" / {}", updater::fmt_bytes(t)))
                    .unwrap_or_default();
                format!("Đang tải… {}{}", updater::fmt_bytes(u.downloaded), total)
            }
            UpdateState::ReadyToInstall => "Đã tải xong, sẵn sàng cập nhật.".to_string(),
            UpdateState::Failed(e) => format!("Kiểm tra thất bại: {e}"),
        }
    }

    /// Download the release zip for the available update, on a
    /// background thread with progress. Extracts it into a temp
    /// staging dir; nothing in the install folder is touched yet.
    fn start_download(&self) {
        let info = {
            let Ok(u) = self.update_status.lock() else {
                return;
            };
            match &u.state {
                UpdateState::Available(info) => info.clone(),
                _ => return,
            }
        };
        let shared = self.update_status.clone();
        let status = self.status.clone();
        if let Ok(mut u) = shared.lock() {
            u.state = UpdateState::Downloading;
            u.downloaded = 0;
            u.total = info.asset.size;
            u.plan = None;
        }
        std::thread::spawn(move || {
            let rt = match shared_runtime() {
                Ok(rt) => rt,
                Err(e) => {
                    if let Ok(mut u) = shared.lock() {
                        u.state = UpdateState::Failed(format!("cannot start runtime: {e}"));
                        u.dialog = true;
                    }
                    return;
                }
            };
            let stage_dir =
                std::env::temp_dir().join(format!("nicord-update-{}", uuid::Uuid::new_v4()));
            if let Err(e) = std::fs::create_dir_all(&stage_dir) {
                if let Ok(mut u) = shared.lock() {
                    u.state = UpdateState::Failed(format!("không tạo được thư mục tạm: {e}"));
                    u.dialog = true;
                }
                return;
            }
            let zip_path = stage_dir.join(&info.asset.name);
            let progress = Arc::new(Mutex::new(updater::DownloadProgress {
                downloaded: 0,
                total: info.asset.size,
            }));
            let progress_reader = progress.clone();
            // Pump progress into the shared UI state while downloading.
            let shared_pump = shared.clone();
            let pump = std::thread::spawn(move || loop {
                let (d, t) = match progress_reader.lock() {
                    Ok(p) => (p.downloaded, p.total),
                    Err(_) => break,
                };
                let done = if let Ok(mut u) = shared_pump.lock() {
                    u.downloaded = d;
                    u.total = t;
                    matches!(u.state, UpdateState::Downloading)
                } else {
                    false
                };
                if !done {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            });
            let result = rt.block_on(updater::download_asset(
                &info.asset.url,
                &zip_path,
                progress,
                updater::MAX_DOWNLOAD_BYTES,
            ));
            let _ = pump.join();
            // Verify the download against the release checksums
            // BEFORE anything is extracted or executed. A release
            // without a checksum file fails closed here.
            let result = result.and_then(|()| {
                rt.block_on(updater::download_and_verify_checksums(
                    &info, &zip_path, &stage_dir,
                ))
            });
            match result {
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&stage_dir);
                    if let Ok(mut u) = shared.lock() {
                        u.state = UpdateState::Failed(format!("Tải bản mới thất bại ({e})"));
                        u.dialog = true;
                    }
                    if let Ok(mut s) = status.lock() {
                        *s = "Tải bản mới thất bại.".to_string();
                    }
                }
                Ok(()) => match updater::prepare_install(&zip_path) {
                    Err(e) => {
                        let _ = std::fs::remove_dir_all(&stage_dir);
                        if let Ok(mut u) = shared.lock() {
                            u.state = UpdateState::Failed(e);
                            u.dialog = true;
                        }
                    }
                    Ok(plan) => {
                        if let Ok(mut u) = shared.lock() {
                            u.state = UpdateState::ReadyToInstall;
                            u.plan = Some(plan);
                        }
                    }
                },
            }
        });
    }

    /// Install the staged update: write the helper script, stop the
    /// engine cleanly, then launch the helper (detached) and quit.
    /// The helper waits for this process to exit, swaps the files,
    /// and relaunches Nicord. Runs on a thread so the UI stays alive
    /// until the process actually exits.
    fn install_update(&self) {
        let plan = {
            let Ok(u) = self.update_status.lock() else {
                return;
            };
            match &u.plan {
                Some(p) => p.clone(),
                None => return,
            }
        };
        let shared = self.update_status.clone();
        let engine = self.engine.clone();
        std::thread::spawn(move || {
            let helper = match updater::write_helper(&plan) {
                Ok(h) => h,
                Err(e) => {
                    if let Ok(mut u) = shared.lock() {
                        u.state = UpdateState::Failed(e);
                        u.dialog = true;
                    }
                    return;
                }
            };
            // Stop the engine first: no more sends go out, the
            // gateway task exits too. The helper additionally waits
            // for this PID to disappear before touching any file.
            engine.stop_all();
            std::thread::sleep(std::time::Duration::from_millis(500));
            if let Err(e) = updater::launch_helper_and_exit(&helper) {
                // The helper never started: report it and keep
                // running instead of quitting with no update.
                if let Ok(mut u) = shared.lock() {
                    u.state = UpdateState::Failed(e);
                    u.dialog = true;
                }
            }
        });
    }

    /// Build or drop the tray icon to match the Settings toggle
    /// (Windows only).
    #[cfg(target_os = "windows")]
    fn sync_tray(&mut self) {
        if self.config.tray_enabled && self.tray.is_none() {
            self.tray = crate::tray::Tray::new();
            if self.tray.is_none() {
                self.set_status("Could not create the tray icon.");
            }
        } else if !self.config.tray_enabled {
            self.tray = None;
        }
    }

    fn start_engine(&self) {
        // Do not spawn a second loop while one is active (including
        // wind-down after Stop). The atomic claim in `run_loop` is the
        // correctness guard; this avoids spawning a wasted thread.
        if self.engine.is_loop_active() {
            self.set_status("Engine is already running.");
            return;
        }
        self.engine.resume();
        let engine = self.engine.clone();
        let scenarios = self.scenarios.clone();
        let store = self.store.clone();
        std::thread::spawn(move || {
            let rt = match shared_runtime() {
                Ok(rt) => rt,
                Err(_) => return,
            };
            let engine2 = engine.clone();
            let scenarios2 = scenarios.clone();
            rt.block_on(async move {
                // Per-scenario sender resolution: destinations in
                // webhook mode, the account sender in user mode.
                // Both the config and the destinations are read fresh
                // on every resolution, so credential and destination
                // edits in Settings apply without an engine restart.
                // (Capturing a config clone here used to freeze the
                // credentials until restart.)
                let store2 = store.clone();
                let resolve_sender = move |scenario: &Scenario| {
                    let cfg = load_config();
                    resolve_sender_for_scenario(&store2, &cfg, scenario)
                };
                let scenarios_fn = move || scenarios2.lock().map(|l| l.clone()).unwrap_or_default();
                engine2.run_loop(resolve_sender, scenarios_fn).await;
            });
        });
        self.set_status("Engine started — running in background.");
    }

    /// Load the account's servers in a background thread (channel picker).
    fn load_guilds(&self) {
        let token = match load_secret(&self.config.encrypted_token) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(&format!("No valid token stored: {e}"));
                return;
            }
        };
        let guilds = self.guilds.clone();
        let status = self.status.clone();
        std::thread::spawn(move || {
            let rt = match shared_runtime() {
                Ok(rt) => rt,
                Err(_) => return,
            };
            let Some(client) = DiscordClient::new(&token) else {
                return;
            };
            match rt.block_on(client.list_guilds()) {
                Ok(list) => {
                    let n = list.len();
                    if let Ok(mut g) = guilds.lock() {
                        *g = list;
                    }
                    if let Ok(mut s) = status.lock() {
                        *s = format!("Loaded {n} servers — pick one below.");
                    }
                }
                Err(e) => {
                    if let Ok(mut s) = status.lock() {
                        *s = format!("Cannot list servers: {e}");
                    }
                }
            }
        });
    }

    /// Load text channels of one server in a background thread.
    fn load_channels(&self, guild_id: String) {
        let token = match load_secret(&self.config.encrypted_token) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(&format!("No valid token stored: {e}"));
                return;
            }
        };
        let channels = self.channels.clone();
        let status = self.status.clone();
        std::thread::spawn(move || {
            let rt = match shared_runtime() {
                Ok(rt) => rt,
                Err(_) => return,
            };
            let Some(client) = DiscordClient::new(&token) else {
                return;
            };
            match rt.block_on(client.list_channels(&guild_id)) {
                Ok(list) => {
                    let text: Vec<Channel> = list.into_iter().filter(|c| c.is_text()).collect();
                    let n = text.len();
                    if let Ok(mut c) = channels.lock() {
                        *c = text;
                    }
                    if let Ok(mut s) = status.lock() {
                        *s = format!("Loaded {n} text channels — pick one below.");
                    }
                }
                Err(e) => {
                    if let Ok(mut s) = status.lock() {
                        *s = format!("Cannot list channels: {e}");
                    }
                }
            }
        });
    }

    /// Keyboard shortcuts (listed in Settings). Ignored on the
    /// Onboarding screen so no shortcut can bypass the ToS gate.
    /// `command` is Ctrl on Windows/Linux, Cmd on macOS.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.screen == Screen::Onboarding {
            return;
        }
        let (save, new_scenario, screen, dismiss) = ctx.input(|i| {
            let cmd = i.modifiers.command;
            let save = cmd && i.key_pressed(egui::Key::S);
            let new_scenario = cmd && i.key_pressed(egui::Key::N);
            let screen = if cmd {
                if i.key_pressed(egui::Key::Num1) {
                    Some(Screen::Dashboard)
                } else if i.key_pressed(egui::Key::Num2) {
                    Some(Screen::Builder)
                } else if i.key_pressed(egui::Key::Num3) {
                    Some(Screen::Destinations)
                } else if i.key_pressed(egui::Key::Num4) {
                    Some(Screen::Logs)
                } else if i.key_pressed(egui::Key::Num5) {
                    Some(Screen::Settings)
                } else {
                    None
                }
            } else {
                None
            };
            // Plain Escape dismisses inline confirmations and the
            // emoji picker; with modifiers it does nothing.
            let dismiss = i.key_pressed(egui::Key::Escape) && i.modifiers.is_none();
            (save, new_scenario, screen, dismiss)
        });
        if save {
            self.save_scenarios();
            let _ = save_config(&self.config);
            self.set_status("Đã lưu.");
        }
        if new_scenario {
            self.screen = Screen::Builder;
            self.create_scenario();
        }
        if let Some(screen) = screen {
            self.screen = screen;
        }
        if dismiss {
            self.pending_delete_log = None;
            self.selected_log = None;
            self.dest_delete_confirm = None;
            self.show_emoji_picker = false;
        }
    }

    /// Remember the current window rect when it is a "normal" rect:
    /// not minimized, not maximized, sane size. Maximized and
    /// minimized rects would restore the wrong geometry, so the
    /// last normal rect is kept instead.
    fn capture_window_rect(&mut self, ctx: &egui::Context) {
        let rect = ctx.input(|i| {
            let v = i.viewport();
            if v.minimized == Some(true) || v.maximized == Some(true) {
                return None;
            }
            v.inner_rect.filter(|r| {
                r.width() >= 100.0
                    && r.height() >= 100.0
                    && r.min.x.is_finite()
                    && r.min.y.is_finite()
            })
        });
        if let Some(rect) = rect {
            if self.last_window_rect != Some(rect) {
                self.last_window_rect = Some(rect);
            }
        }
    }

    /// Write the remembered rect into the config (only when it
    /// changed). Called on exit and on hide-to-tray — never
    /// per-frame, so resizing costs no disk writes.
    fn persist_window_geometry(&mut self) {
        if let Some(rect) = self.last_window_rect {
            let (w, h, x, y) = (rect.width(), rect.height(), rect.min.x, rect.min.y);
            if (self.config.window_width - w).abs() > f32::EPSILON
                || (self.config.window_height - h).abs() > f32::EPSILON
                || (self.config.window_x - x).abs() > f32::EPSILON
                || (self.config.window_y - y).abs() > f32::EPSILON
            {
                self.config.window_width = w;
                self.config.window_height = h;
                self.config.window_x = x;
                self.config.window_y = y;
                let _ = save_config(&self.config);
            }
        }
    }
}

impl eframe::App for NicordApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Keyboard shortcuts, window geometry memory, start-minimized.
        self.handle_shortcuts(ctx);
        self.capture_window_rect(ctx);
        if self.config.start_minimized && !self.did_initial_minimize {
            self.did_initial_minimize = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        // Mirror sound settings for engine threads (cheap, idempotent).
        crate::notify::sync_settings(self.config.sound_enabled, self.config.sound_on_success);
        // "Sent today" is the persisted count, not a memory counter.
        // Refreshed at most every 5 s: the query is a full scan
        // (~3.3 ms over 10k rows, measured) and the Dashboard only
        // needs a recent number, not a per-frame one. At 60 fps this
        // cuts ~3,600 queries/minute to 12.
        if self.sent_today_updated.elapsed() >= std::time::Duration::from_secs(5) {
            self.sent_today = self.store.count_sends_today();
            self.sent_today_updated = std::time::Instant::now();
        }
        // Scheduled automatic backups (Group 3): the due check
        // runs at most every 60 s; the backup itself is a small
        // local export (milliseconds) and runs inline.
        if self.auto_backup_checked.elapsed() >= std::time::Duration::from_secs(60) {
            self.auto_backup_checked = std::time::Instant::now();
            self.maybe_auto_backup();
        }
        // Sync webhook health from the check thread into the config
        // (persisted). Runs at most once per change: after syncing,
        // shared and config agree, so no save happens next frame.
        {
            let health = self
                .webhook_health_shared
                .lock()
                .map(|h| *h)
                .unwrap_or(WebhookHealth::Unknown);
            let checked = self
                .webhook_last_checked_shared
                .lock()
                .map(|s| s.clone())
                .unwrap_or_default();
            if health != self.config.webhook_health
                || checked != self.config.webhook_last_checked_at
            {
                self.config.webhook_health = health;
                self.config.webhook_last_checked_at = checked;
                let _ = save_config(&self.config);
            }
        }

        // System tray (Windows only): closing the window hides Nicord
        // to the tray instead of quitting; the tray menu can show it
        // again or quit for real.
        #[cfg(target_os = "windows")]
        {
            if ctx.input(|i| i.viewport().close_requested())
                && self.tray.is_some()
                && !self.allow_quit
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                // The window is about to hide: remember its geometry
                // now, while the last normal rect is still valid.
                self.persist_window_geometry();
            }
            if let Some(tray) = &self.tray {
                // Keep the engine menu label truthful (it only
                // touches the menu when the state changed).
                tray.set_engine_running(self.engine.is_loop_active());
                match tray.poll() {
                    crate::tray::TrayAction::Show => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    crate::tray::TrayAction::ShowDashboard => {
                        self.screen = Screen::Dashboard;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    crate::tray::TrayAction::ToggleEngine => {
                        if self.engine.is_loop_active() {
                            self.engine.stop_all();
                            self.set_status("Engine stopped.");
                        } else {
                            self.start_engine();
                        }
                    }
                    crate::tray::TrayAction::Quit => {
                        self.allow_quit = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    crate::tray::TrayAction::None => {}
                }
                // Keep polling the tray while the window is hidden:
                // hidden windows get no repaint on their own.
                ctx.request_repaint_after(std::time::Duration::from_millis(500));
            }
        }

        // Left sidebar: brand, navigation, today status, Emergency Stop.
        // It is on every screen, so Emergency Stop is always one click away.
        egui::SidePanel::left("sidebar")
            .exact_width(208.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::palette().sidebar_bg)
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(ctx, |ui| {
                self.show_sidebar(ui);
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::palette().bg)
                    .inner_margin(egui::Margin {
                        left: 18,
                        right: 18,
                        top: 14,
                        bottom: 14,
                    }),
            )
            .show(ctx, |ui| {
                // Header: screen title + the always-visible limit.
                if self.screen != Screen::Onboarding {
                    let (title, subtitle) = match self.screen {
                        Screen::Onboarding => ("", ""),
                        Screen::Dashboard => (
                            t(self.lang(), "title.dashboard"),
                            t(self.lang(), "subtitle.dashboard"),
                        ),
                        Screen::Builder => (
                            t(self.lang(), "title.builder"),
                            t(self.lang(), "subtitle.builder"),
                        ),
                        Screen::Destinations => (
                            t(self.lang(), "title.destinations"),
                            t(self.lang(), "subtitle.destinations"),
                        ),
                        Screen::Logs => (
                            t(self.lang(), "title.logs"),
                            t(self.lang(), "subtitle.logs"),
                        ),
                        Screen::Settings => (
                            t(self.lang(), "title.settings"),
                            t(self.lang(), "subtitle.settings"),
                        ),
                    };
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.heading(title);
                            ui.label(
                                RichText::new(subtitle)
                                    .small()
                                    .color(theme::palette().text_muted),
                            );
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new("1 channel • 300 messages/day • resets 00:00 UTC")
                                    .small()
                                    .color(theme::palette().text_muted),
                            );
                        });
                    });
                    ui.add_space(6.0);
                }
                // Status banner (same message as before, now in a card).
                let status = self.status_text();
                if !status.is_empty() {
                    egui::Frame::new()
                        .fill(theme::palette().card)
                        .stroke(egui::Stroke::new(1.0_f32, theme::palette().warning))
                        .corner_radius(8)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(RichText::new(&status).color(theme::palette().warning));
                        });
                    ui.add_space(10.0);
                }
                // F-GUI-5: Dashboard, Destinations, and Settings
                // have no ScrollArea of their own, so at the default
                // 1100x700 window their lower content (Actions row,
                // timeline, lower Settings cards) was unreachable
                // without resizing. Wrap those three here; Builder
                // and Logs already scroll internally, and Onboarding
                // centers from available_height (a ScrollArea would
                // break its layout). Distinct id_salts keep each
                // screen's scroll state separate.
                match self.screen {
                    Screen::Onboarding => self.show_onboarding(ui),
                    Screen::Dashboard => {
                        egui::ScrollArea::vertical()
                            .id_salt("scroll_dashboard")
                            .show(ui, |ui| self.show_dashboard(ui));
                    }
                    Screen::Builder => self.show_builder(ui),
                    Screen::Destinations => {
                        egui::ScrollArea::vertical()
                            .id_salt("scroll_destinations")
                            .show(ui, |ui| self.show_destinations(ui));
                    }
                    Screen::Logs => self.show_logs(ui),
                    Screen::Settings => {
                        egui::ScrollArea::vertical()
                            .id_salt("scroll_settings")
                            .show(ui, |ui| self.show_settings(ui));
                    }
                }
            });
        // Auto-update dialog (only when a newer release was found, or
        // a download/install step needs the user's attention).
        self.show_update_dialog(ctx);
        // Keep the engine ticking / status fresh.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Last chance to remember the window geometry. Hide-to-tray
        // does not reach here (the app keeps running), so this only
        // fires on a real quit.
        self.persist_window_geometry();
    }
}

impl NicordApp {
    /// Left sidebar: brand, nav, today status card, Emergency Stop.
    /// Next-run times for all scenarios, via the display cache.
    /// Each screen calls this once per frame and looks up from the
    /// map, instead of running the (potentially ~38 ms) cron scan
    /// once per row per call site.
    fn cached_next_runs(
        &mut self,
    ) -> std::collections::HashMap<String, chrono::DateTime<chrono::Local>> {
        let scenarios = self
            .scenarios
            .lock()
            .map(|l| l.to_vec())
            .unwrap_or_default();
        self.next_run_cache
            .next_runs(&scenarios, chrono::Local::now())
    }

    /// Recompute the Dashboard timeline at most every 30 s. The
    /// computation walks each enabled scenario's schedule forward
    /// (a sparse cron costs ~38 ms per step), so it must not run
    /// every frame; 30 s staleness is invisible on a 7-day view.
    fn refresh_timeline(&mut self) {
        if self.timeline_updated.elapsed() < std::time::Duration::from_secs(30) {
            return;
        }
        let scenarios = self
            .scenarios
            .lock()
            .map(|l| l.to_vec())
            .unwrap_or_default();
        let destinations = self.store.load_destinations().unwrap_or_default();
        let mut sent_by_dest = std::collections::HashMap::new();
        for d in &destinations {
            sent_by_dest.insert(
                d.id.clone(),
                self.store.count_sends_today_for_destination(&d.id),
            );
        }
        let is_webhook = self.config.send_mode == SendMode::Webhook;
        let global_cap = self.config.global_daily_cap;
        self.timeline_cache = crate::timeline::upcoming_runs(
            &scenarios,
            &destinations,
            is_webhook,
            &sent_by_dest,
            global_cap,
            chrono::Local::now(),
            7,
            10,
            20,
        );
        self.timeline_updated = std::time::Instant::now();
    }

    /// 7-day sent/failed bar chart, hand-drawn with the egui
    /// painter (no chart dependency). Days with no history show
    /// no bars; the value labels show exact counts.
    fn show_week_chart(&self, ui: &mut egui::Ui) {
        let counts = self.store.counts_by_day(7).unwrap_or_default();
        let today = chrono::Utc::now().date_naive();
        let mut days: Vec<(String, u32, u32)> = Vec::new();
        for back in (0..7).rev() {
            let d = today - chrono::Duration::days(back);
            let key = d.format("%Y-%m-%d").to_string();
            let (s, f) = counts
                .iter()
                .find(|(k, _, _)| *k == key)
                .map(|(_, s, f)| (*s, *f))
                .unwrap_or((0, 0));
            days.push((d.format("%d/%m").to_string(), s, f));
        }
        let max = days.iter().map(|(_, s, f)| (*s).max(*f)).max().unwrap_or(0);
        if max == 0 {
            theme::muted(ui, "Chưa có lịch sử gửi trong 7 ngày qua.");
            return;
        }
        let height = 110.0;
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), height),
            egui::Sense::hover(),
        );
        let painter = ui.painter_at(rect);
        let palette = theme::palette();
        let small = egui::TextStyle::Small.resolve(ui.style());
        let slot = rect.width() / 7.0;
        let label_h = 16.0;
        let top_pad = 14.0;
        let plot_h = height - label_h - top_pad;
        let base = rect.top() + top_pad + plot_h;
        for (i, (label, sent, failed)) in days.iter().enumerate() {
            let cx = rect.left() + (i as f32 + 0.5) * slot;
            let bar_w = (slot * 0.32).min(22.0);
            let gap = 3.0;
            let sent_h = plot_h * (*sent as f32 / max as f32);
            let failed_h = plot_h * (*failed as f32 / max as f32);
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(cx - bar_w - gap / 2.0, base - sent_h),
                    egui::vec2(bar_w, sent_h),
                ),
                2.0,
                palette.success,
            );
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(cx + gap / 2.0, base - failed_h),
                    egui::vec2(bar_w, failed_h),
                ),
                2.0,
                palette.danger,
            );
            if *sent > 0 {
                painter.text(
                    egui::pos2(cx - bar_w / 2.0 - gap / 2.0, base - sent_h - 2.0),
                    egui::Align2::CENTER_BOTTOM,
                    sent.to_string(),
                    small.clone(),
                    palette.text,
                );
            }
            if *failed > 0 {
                painter.text(
                    egui::pos2(cx + bar_w / 2.0 + gap / 2.0, base - failed_h - 2.0),
                    egui::Align2::CENTER_BOTTOM,
                    failed.to_string(),
                    small.clone(),
                    palette.text,
                );
            }
            painter.text(
                egui::pos2(cx, rect.bottom() - 2.0),
                egui::Align2::CENTER_BOTTOM,
                label,
                small.clone(),
                palette.text_muted,
            );
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("■").color(palette.success));
            ui.label("Đã gửi");
            ui.add_space(12.0);
            ui.label(RichText::new("■").color(palette.danger));
            ui.label("Thất bại");
        });
    }

    fn show_sidebar(&mut self, ui: &mut egui::Ui) {
        let next_runs = self.cached_next_runs();
        ui.horizontal(|ui| {
            ui.label(theme::semibold("Nicord", 22.0).color(theme::palette().text));
            ui.label(RichText::new("•").size(22.0).color(theme::ACCENT_LIGHT));
        });
        ui.label(
            RichText::new("Discord auto-sender")
                .small()
                .color(theme::palette().text_muted),
        );
        ui.add_space(12.0);
        if self.screen != Screen::Onboarding {
            for (label, screen) in [
                (t(self.lang(), "nav.dashboard"), Screen::Dashboard),
                (t(self.lang(), "nav.builder"), Screen::Builder),
                (t(self.lang(), "nav.destinations"), Screen::Destinations),
                (t(self.lang(), "nav.logs"), Screen::Logs),
                (t(self.lang(), "nav.settings"), Screen::Settings),
            ] {
                if theme::nav_button(ui, label, self.screen == screen).clicked() {
                    self.screen = screen;
                }
                ui.add_space(4.0);
            }
        } else {
            ui.label(
                RichText::new("Hoàn tất thiết lập để tiếp tục")
                    .small()
                    .color(theme::palette().text_muted),
            );
        }
        // Bottom of the sidebar: today status, then Emergency Stop last.
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            if ui
                .add_sized(
                    [ui.available_width(), 40.0],
                    egui::Button::new(RichText::new("Emergency Stop").color(theme::WHITE))
                        .fill(theme::palette().danger),
                )
                .clicked()
            {
                self.engine.stop_all();
                self.set_status("EMERGENCY STOP — all queues halted.");
            }
            ui.add_space(8.0);
            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                // The footer uses a bottom-up layout, so force the card's
                // own contents back to top-down (otherwise "Today" would
                // render at the bottom of the card).
                ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                    ui.label(theme::semibold("Today", 14.0));
                    ui.label(
                        RichText::new(format!("Sent today: {} / 300", self.sent_today)).small(),
                    );
                    // Hand-drawn bar: the stock ProgressBar shows a blue
                    // pill even at 0/300 (min-width fill).
                    theme::progress_bar(
                        ui,
                        self.sent_today as f32 / 300.0,
                        &format!("{} / 300", self.sent_today),
                    );
                    theme::muted(ui, "Resets 00:00 UTC");
                    ui.add_space(6.0);
                    let running = self
                        .engine
                        .running
                        .load(std::sync::atomic::Ordering::SeqCst);
                    // F-GUI-4: surface the 3-error engine pause
                    // (warning dot + countdown) instead of a
                    // misleading "running".
                    let pause_label = self.error_pause_label();
                    ui.horizontal(|ui| {
                        ui.label(theme::dot(if pause_label.is_some() {
                            theme::palette().warning
                        } else if running {
                            theme::palette().success
                        } else {
                            theme::palette().text_muted
                        }));
                        let status = if let Some(left) = &pause_label {
                            format!("Engine paused ({left})")
                        } else if running {
                            "Engine running".to_string()
                        } else {
                            "Engine stopped".to_string()
                        };
                        ui.label(RichText::new(status).small());
                    });
                    let count = self.scenarios.lock().map(|l| l.len()).unwrap_or(0);
                    theme::muted(ui, &format!("Scenarios: {count}"));
                    // Next scheduled run across enabled scenarios — fills
                    // the card with something useful instead of dead space.
                    let next = self.scenarios.lock().map(|l| {
                        l.iter()
                            .filter(|s| s.enabled)
                            .filter_map(|s| next_runs.get(&s.id).copied())
                            .min()
                    });
                    if let Ok(Some(t)) = next {
                        theme::muted(ui, &format!("Next run: {}", t.format("%d/%m %H:%M")));
                    }
                });
            });
        });
    }

    fn show_onboarding(&mut self, ui: &mut egui::Ui) {
        // Center the card vertically: the panel is ~640 px tall and the
        // card ~470 px, so a top spacer balances the empty space below it.
        let top = ((ui.available_height() - 470.0) / 2.0).max(8.0);
        ui.add_space(top);
        ui.vertical_centered(|ui| {
            theme::card_frame().show(ui, |ui| {
                ui.set_max_width(620.0);
                ui.heading("Welcome to Nicord");
                theme::muted(ui, "Gửi tin nhắn Discord tự động, theo lịch của bạn");
                ui.add_space(10.0);
                theme::section_title(ui, "Chọn cách gửi");
                ui.horizontal(|ui| {
                    ui.radio_value(
                        &mut self.onboarding_mode,
                        SendMode::Webhook,
                        "Webhook (khuyến nghị)",
                    );
                    ui.radio_value(
                        &mut self.onboarding_mode,
                        SendMode::UserToken,
                        "Tài khoản Discord",
                    );
                });
                ui.add_space(10.0);
                if self.onboarding_mode == SendMode::Webhook {
                    self.show_onboarding_webhook(ui);
                } else {
                    self.show_onboarding_token(ui);
                }
                ui.add_space(10.0);
                if theme::ghost_button(ui, "Skip for now -> Dashboard").clicked() {
                    self.config.tos_accepted = self.tos_checked;
                    let _ = save_config(&self.config);
                    // F-GUI-1: the health card was computed at startup
                    // (before this choice); refresh so Dashboard does
                    // not show stale issues for the other mode.
                    self.refresh_health();
                    self.screen = Screen::Dashboard;
                }
            });
        });
    }

    /// Onboarding — Webhook path (official, recommended). The URL is
    /// the credential: it is validated, checked against Discord, then
    /// stored encrypted. It is never shown in full.
    fn show_onboarding_webhook(&mut self, ui: &mut egui::Ui) {
        ui.label(
            "Webhook là cách chính thức của Discord để gửi tin nhắn tự động \
             tới 1 kênh. Tạo webhook trong Cài đặt kênh -> Tích hợp -> Webhook, \
             rồi dán URL vào đây. URL webhook là mật khẩu — đừng chia sẻ. \
             Webhook chỉ gửi tin nhắn, không gửi được lệnh.",
        );
        ui.add_space(8.0);
        if !self.config.webhook_id.is_empty() {
            ui.label(format!(
                "Webhook đã lưu: {} (ID {})",
                self.config.webhook_display_name(),
                self.config.webhook_id
            ));
        }
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.webhook_url_input)
                .password(true)
                .hint_text("paste your webhook URL here"),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if theme::ghost_button(ui, "Kiểm tra webhook").clicked() {
                let url = self.webhook_url_input.clone();
                self.check_webhook(url);
            }
            if theme::primary_button(ui, "Lưu webhook").clicked() {
                let input = self.webhook_url_input.trim().to_string();
                if input.is_empty() {
                    self.set_status("Webhook URL is empty.");
                } else {
                    match parse_webhook_url(&input) {
                        Ok(parsed) => match store_secret(ENTRY_LEGACY_WEBHOOK, &input) {
                            Ok(enc) => {
                                self.config.encrypted_webhook_url = enc;
                                self.config.webhook_id = parsed.id.clone();
                                // Keep a checked name when it matches this webhook.
                                if let Ok(info) = self.webhook_info.lock() {
                                    if let Some(w) = info.as_ref() {
                                        if w.id == parsed.id {
                                            self.config.webhook_name = w.display_name();
                                        }
                                    }
                                }
                                self.config.send_mode = SendMode::Webhook;
                                if save_config(&self.config).is_ok() {
                                    self.upsert_destination_from_legacy();
                                    self.webhook_url_input.clear();
                                    // F-GUI-1: refresh so the health card
                                    // reflects webhook mode, not the
                                    // startup-time token-mode warning.
                                    self.refresh_health();
                                    self.screen = Screen::Dashboard;
                                    self.set_status("Webhook saved (encrypted, local only).");
                                } else {
                                    self.set_status("Failed to save config.");
                                }
                            }
                            Err(e) => self.set_status(&format!("Cannot encrypt webhook URL: {e}")),
                        },
                        Err(e) => self.set_status(&e),
                    }
                }
            }
        });
        let check_msg = self
            .webhook_check_status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        if !check_msg.is_empty() {
            ui.add_space(4.0);
            ui.label(check_msg);
        }
    }

    /// Onboarding — user-token (self-bot) path. Kept verbatim, including
    /// the ToS warning: this mode carries a ban risk.
    fn show_onboarding_token(&mut self, ui: &mut egui::Ui) {
        // Red ToS warning (kept verbatim, now in a red-bordered card).
        theme::danger_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new("WARNING — READ FIRST")
                    .color(theme::palette().danger)
                    .text_style(egui::TextStyle::Name("SectionTitle".into())),
            );
            ui.label(
                "Automating a normal Discord user account (self-bot) is forbidden \
                 by Discord's Terms of Service and can result in PERMANENT account \
                 termination. Nicord makes no guarantee you will never be banned. \
                 Use a SPARE account, never your main account.",
            );
        });
        ui.add_space(10.0);
        ui.checkbox(
            &mut self.tos_checked,
            "I understand the risk and accept responsibility for my own account.",
        );
        ui.add_space(10.0);
        theme::section_title(ui, "Step 1 — Your Discord token");
        ui.label("Paste once, stored encrypted locally.");
        ui.label("Get it yourself from Discord like a developer token. Never share it with anyone — not even Nicord support.");
        if !self.config.token_last4.is_empty() {
            // token_last4 holds ONLY the last 4 chars (never the full token);
            // mask_token shows at most those 4, so even an old config that
            // still contains a full token can never leak it on screen.
            ui.label(format!(
                "Token stored: {}",
                mask_token(&self.config.token_last4)
            ));
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.token_input)
                    .password(true)
                    .hint_text("paste your Discord token here"),
            );
            if theme::primary_button(ui, "Save token").clicked() {
                if self.token_input.trim().is_empty() {
                    self.set_status("Token is empty.");
                } else {
                    match store_secret(ENTRY_ACCOUNT_TOKEN, self.token_input.trim()) {
                        Ok(enc) => {
                            // SECURITY: store ONLY the last 4 chars for display.
                            // The full token must never be written to config.json
                            // in plaintext (Spec §9).
                            let tok = self.token_input.trim();
                            self.config.token_last4 = tok
                                .chars()
                                .rev()
                                .take(4)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect();
                            self.config.encrypted_token = enc;
                            self.config.tos_accepted = self.tos_checked;
                            self.config.send_mode = SendMode::UserToken;
                            if save_config(&self.config).is_ok() {
                                self.token_input.clear();
                                // F-GUI-1: same staleness as the webhook
                                // path — refresh before showing Dashboard.
                                self.refresh_health();
                                self.screen = Screen::Dashboard;
                                self.set_status("Token saved (encrypted, local only).");
                            } else {
                                self.set_status("Failed to save config.");
                            }
                        }
                        Err(e) => self.set_status(&format!("Cannot encrypt token: {e}")),
                    }
                }
            }
        });
    }

    /// Destination card (QoL Q2/Q4): one card for the active
    /// destination — health + last check (webhook mode), last send,
    /// persisted failures today, and the next scheduled run.
    fn show_destination_card(
        &self,
        ui: &mut egui::Ui,
        next_text: &str,
        next_runs: &std::collections::HashMap<String, chrono::DateTime<chrono::Local>>,
    ) {
        let is_webhook = self.config.send_mode == SendMode::Webhook;
        let failed_today = self.store.count_status_today("failed");
        let last_send = self
            .store
            .last_send_time()
            .unwrap_or_else(|| "Chưa gửi lần nào".to_string());
        if self.engine.is_globally_paused() {
            theme::card(ui, |ui| {
                ui.label(
                    RichText::new(
                        "Discord yêu cầu chờ toàn cục (429) — mọi đích đến đang tạm chờ, sẽ tự tiếp tục.",
                    )
                    .color(theme::palette().danger),
                );
            });
            ui.add_space(8.0);
        }
        theme::card(ui, |ui| {
            theme::section_title(ui, t(self.lang(), "dash.destination"));
            if is_webhook {
                let dests = self.store.load_destinations().unwrap_or_default();
                if dests.is_empty() {
                    theme::muted(
                        ui,
                        "Chưa có đích đến nào — thêm một webhook trong Đích đến.",
                    );
                }
                for dest in &dests {
                    let sent = self.store.count_sends_today_for_destination(&dest.id);
                    let state_color = if !dest.enabled {
                        theme::palette().text_secondary
                    } else if dest.failure_state.is_paused() {
                        theme::palette().danger
                    } else {
                        theme::palette().success
                    };
                    let state_text = if !dest.enabled {
                        "Đã tắt"
                    } else {
                        dest.failure_state.label()
                    };
                    ui.horizontal(|ui| {
                        ui.label(theme::semibold(&dest.display_name(), 15.0));
                        ui.label(RichText::new(state_text).color(state_color));
                        theme::muted(ui, &format!("đã gửi hôm nay: {sent}"));
                    });
                    // Next run for this destination (Q7), with the
                    // scenario's timezone shown.
                    let next_for_dest = self.scenarios.lock().map(|l| {
                        l.iter()
                            .filter(|s| {
                                s.enabled && s.destination_id.as_deref() == Some(dest.id.as_str())
                            })
                            .filter_map(|s| next_runs.get(&s.id).map(|t| (*t, s.timezone.clone())))
                            .min_by_key(|(t, _)| *t)
                    });
                    if let Ok(Some((t, tz))) = next_for_dest {
                        theme::muted(
                            ui,
                            &format!("Lịch tới: {} ({})", t.format("%d/%m %H:%M"), tz),
                        );
                    }
                    if dest.failure_state.is_paused() && !dest.last_error.is_empty() {
                        theme::muted(ui, &format!("Lỗi: {}", dest.last_error));
                    }
                }
            } else {
                ui.label(format!(
                    "Tài khoản: {}",
                    mask_token(&self.config.token_last4)
                ));
            }
            theme::muted(ui, &format!("Gửi gần nhất: {last_send}"));
            theme::muted(ui, &format!("Thất bại hôm nay: {failed_today}"));
            theme::muted(ui, &format!("Lịch tới: {next_text}"));
        });
    }

    fn show_dashboard(&mut self, ui: &mut egui::Ui) {
        let next_runs = self.cached_next_runs();
        // M2: a failed last-run save is surfaced live — the
        // engine remembers the last persistence error, and it
        // joins the startup issues in this card until a save
        // succeeds. (Startup issues refresh only on events;
        // this one must appear within a frame of the failure.)
        let persist_issue = self.engine.last_persist_error().map(|e| {
            crate::health::HealthIssue {
                severity: crate::health::Severity::Warning,
                message: format!(
                    "Không lưu được trạng thái gửi ({e}) — nếu app tắt đột ngột, một lịch có thể gửi lặp lại. Kiểm tra ổ đĩa và quyền ghi."
                ),
                target: None,
            }
        });
        // Startup health report (Q15): shown until dismissed.
        if !self.health_dismissed && (!self.health_issues.is_empty() || persist_issue.is_some()) {
            let mut jump_to: Option<Screen> = None;
            let mut dismiss = false;
            let mut recheck = false;
            theme::card(ui, |ui| {
                theme::section_title(ui, "Tình trạng khởi động");
                for issue in self.health_issues.iter().chain(persist_issue.iter()) {
                    let (dot, label) = match issue.severity {
                        crate::health::Severity::Error => {
                            (theme::dot(theme::palette().danger), "Lỗi")
                        }
                        crate::health::Severity::Warning => {
                            (theme::dot(theme::palette().warning), "Chú ý")
                        }
                        crate::health::Severity::Info => (theme::dot(theme::ACCENT_LIGHT), "Gợi ý"),
                    };
                    ui.horizontal(|ui| {
                        ui.label(dot);
                        ui.label(format!("[{label}] {}", issue.message));
                        if let Some(target) = issue.target {
                            let (screen, text) = match target {
                                crate::health::HealthTarget::Destinations => {
                                    (Screen::Destinations, "Đi tới Đích đến")
                                }
                                crate::health::HealthTarget::Builder => {
                                    (Screen::Builder, "Đi tới Soạn")
                                }
                                crate::health::HealthTarget::Settings => {
                                    (Screen::Settings, "Đi tới Cài đặt")
                                }
                            };
                            if theme::ghost_button(ui, text).clicked() {
                                jump_to = Some(screen);
                            }
                        }
                    });
                }
                ui.horizontal(|ui| {
                    if theme::ghost_button(ui, "Đã hiểu").clicked() {
                        dismiss = true;
                    }
                    if theme::ghost_button(ui, "Kiểm tra lại").clicked() {
                        recheck = true;
                    }
                });
            });
            ui.add_space(12.0);
            if dismiss {
                self.health_dismissed = true;
            }
            if recheck {
                self.refresh_health();
            }
            if let Some(screen) = jump_to {
                self.screen = screen;
            }
        }
        // Stat cards: scenarios, running, sent today.
        let count = self.scenarios.lock().map(|l| l.len()).unwrap_or(0);
        let running = self
            .engine
            .running
            .load(std::sync::atomic::Ordering::SeqCst);
        // `.ok().flatten()` drops the lock guard (and the
        // poison-error carrier) immediately: holding the `Result`
        // would keep `self.scenarios` borrowed until the end of
        // this function and block later `&mut self` calls.
        let next_text = self
            .scenarios
            .lock()
            .map(|l| l.iter().filter_map(|s| next_runs.get(&s.id).copied()).min())
            .ok()
            .flatten()
            .map(|t| t.format("%d/%m %H:%M").to_string())
            .unwrap_or_else(|| "—".to_string());
        // Success rate uses persisted counts for both sides: sent
        // today AND failed today come from the log, so restarting the
        // app cannot reset the rate to 100%. (The session-only
        // failure counter still exists on the engine for live
        // status; the "Deleted" card below is honestly labeled
        // session-only.)
        let failed = self.store.count_status_today("failed");
        let deleted = self.engine.deleted_count();
        let total = self.sent_today + failed;
        let success_pct = if total == 0 {
            None
        } else {
            Some(self.sent_today as f64 * 100.0 / total as f64)
        };
        let elapsed_min = self.engine.started_at.elapsed().as_secs_f64() / 60.0;
        let rate_per_min = if elapsed_min > 0.0 {
            self.sent_today as f64 / elapsed_min
        } else {
            0.0
        };
        // F-GUI-4: show the 3-error engine pause (with countdown)
        // instead of a misleading "Running".
        let engine_status = if let Some(left) = self.error_pause_label() {
            format!("Paused ({left})")
        } else if running {
            "Running".to_string()
        } else {
            "Stopped".to_string()
        };
        ui.columns(3, |cols| {
            theme::stat_card(&mut cols[0], "Scenarios", &count.to_string());
            theme::stat_card(&mut cols[1], "Engine", &engine_status);
            theme::stat_card(
                &mut cols[2],
                "Sent today (limit 300)",
                &format!("{} / 300", self.sent_today),
            );
        });
        ui.add_space(4.0);
        ui.columns(3, |cols| {
            theme::stat_card(
                &mut cols[0],
                "Success rate",
                &success_pct
                    .map(|p| format!("{p:.0}%"))
                    .unwrap_or_else(|| "—".to_string()),
            );
            theme::stat_card(
                &mut cols[1],
                "Send rate",
                &format!("{rate_per_min:.1}/phút"),
            );
            theme::stat_card(&mut cols[2], "Deleted (this session)", &deleted.to_string());
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(theme::dot(theme::palette().text_muted));
            ui.label(
                RichText::new(format!("Next run: {next_text}"))
                    .small()
                    .color(theme::palette().text_muted),
            );
        });
        ui.add_space(4.0);
        // Send mode + destination, so the user always knows how sends go out.
        let mode_text = match self.config.send_mode {
            SendMode::Webhook => format!(
                "Chế độ: Webhook — {} (ID {})",
                self.config.webhook_display_name(),
                self.config.webhook_id
            ),
            SendMode::UserToken => format!(
                "Chế độ: Tài khoản — {}",
                mask_token(&self.config.token_last4)
            ),
        };
        ui.horizontal(|ui| {
            ui.label(theme::dot(theme::palette().text_muted));
            ui.label(
                RichText::new(mode_text)
                    .small()
                    .color(theme::palette().text_muted),
            );
        });
        ui.add_space(12.0);
        // Destination card (QoL Q2/Q4): health, last check, last send,
        // and persisted failure count for the active destination.
        self.show_destination_card(ui, &next_text, &next_runs);
        ui.add_space(12.0);
        // Actions.
        theme::card(ui, |ui| {
            theme::section_title(ui, "Actions");
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Start engine").clicked() {
                    self.start_engine();
                }
                if ui.button("Stop engine").clicked() {
                    self.engine.stop_all();
                    self.set_status("Engine stopped.");
                }
                let test_label = match self.test_send_target() {
                    Some(s) => {
                        let name: String = s.name.chars().take(24).collect();
                        format!("Test send '{name}' (sends a REAL message)")
                    }
                    None => "Test send (sends a REAL message)".to_string(),
                };
                if ui.button(test_label).clicked() {
                    self.test_send();
                }
                if ui.button("Minimise (engine keeps running)").clicked() {
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
                if ui.button("Export thống kê (.json)").clicked() {
                    let stats = serde_json::json!({
                        "sent_today": self.sent_today,
                        "failed_this_session": failed,
                        "deleted_this_session": deleted,
                        "success_rate_pct": success_pct,
                        "sends_per_min": rate_per_min,
                        "uptime_secs": self.engine.started_at.elapsed().as_secs(),
                        "generated_at_utc": chrono::Utc::now().to_rfc3339(),
                    });
                    let path = config_dir().join(format!(
                        "nicord-stats-{}.json",
                        chrono::Utc::now().format("%Y%m%d-%H%M%S")
                    ));
                    let result = serde_json::to_string_pretty(&stats)
                        .map_err(|e| e.to_string())
                        .and_then(|j| std::fs::write(&path, j).map_err(|e| e.to_string()));
                    match result {
                        Ok(()) => {
                            restrict_permissions(&path);
                            self.set_status(&format!("Stats exported to {}", path.display()))
                        }
                        Err(e) => self.set_status(&format!("Export failed: {e}")),
                    }
                }
            });
            ui.add_space(6.0);
            #[cfg(target_os = "windows")]
            let tray_note = if self.config.tray_enabled {
                "Closing the window hides Nicord to the tray — the engine keeps running. Right-click the tray icon to quit."
            } else {
                "Note: enable “System tray” in Settings to hide Nicord to the tray on close. Closing the window stops the engine."
            };
            #[cfg(not(target_os = "windows"))]
            let tray_note = "Note: there is no system-tray icon on this platform — keep the window open (minimised is fine). Closing the window stops the engine.";
            theme::muted(ui, tray_note);
        });
        ui.add_space(12.0);
        // Upcoming-sends timeline (Group 1): the next 20 runs
        // across enabled scenarios, with blocked-run warnings.
        self.refresh_timeline();
        let timeline = self.timeline_cache.clone();
        let mut jump_to: Option<String> = None;
        theme::card(ui, |ui| {
            theme::section_title(ui, t(self.lang(), "dash.timeline"));
            if timeline.is_empty() {
                theme::muted(
                    ui,
                    "Không có lịch gửi nào trong 7 ngày tới — bật một kịch bản để xem tại đây.",
                );
            } else {
                for run in &timeline {
                    ui.horizontal(|ui| {
                        ui.label(run.at.format("%d/%m %H:%M").to_string());
                        ui.label(&run.scenario_name);
                        if !run.destination_name.is_empty() {
                            theme::muted(ui, &format!("→ {}", run.destination_name));
                        }
                        if let Some(reason) = &run.blocked {
                            ui.label(
                                RichText::new(format!("sẽ bỏ qua: {reason}"))
                                    .color(theme::palette().warning)
                                    .small(),
                            );
                        }
                        if ui
                            .small_button(t(self.lang(), "dash.timeline_edit"))
                            .clicked()
                        {
                            jump_to = Some(run.scenario_id.clone());
                        }
                    });
                }
                theme::muted(
                    ui,
                    "Dự đoán từ lịch và cài đặt hiện tại; giới hạn ngày chỉ tính cho hôm nay, cooldown 429 và giờ ngủ đêm không được tính.",
                );
            }
        });
        if let Some(id) = jump_to {
            if let Ok(list) = self.scenarios.lock() {
                if let Some(i) = list.iter().position(|s| s.id == id) {
                    self.selected = Some(i);
                    self.screen = Screen::Builder;
                }
            }
        }
        ui.add_space(12.0);
        // Scenario list.
        theme::card(ui, |ui| {
            theme::section_title(ui, "Scenarios");
            let rows: Vec<(usize, String, bool, String, String)> = self
                .scenarios
                .lock()
                .map(|l| {
                    l.iter()
                        .enumerate()
                        .map(|(i, s)| {
                            let next = next_runs
                                .get(&s.id)
                                .map(|t| format!("next {}", t.format("%Y-%m-%d %H:%M")))
                                .unwrap_or_default();
                            (
                                i,
                                s.name.clone(),
                                s.enabled,
                                s.schedule.as_ref().map(|sc| sc.label()).unwrap_or_default(),
                                next,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            if rows.is_empty() {
                ui.label("No scenarios yet. Go to Builder -> New scenario.");
                ui.add_space(8.0);
                if theme::primary_button(ui, "Create a scenario").clicked() {
                    self.screen = Screen::Builder;
                }
            } else {
                let mut edit: Option<usize> = None;
                for (i, name, enabled, schedule, next) in &rows {
                    ui.horizontal(|ui| {
                        ui.label(theme::dot(if *enabled {
                            theme::palette().success
                        } else {
                            theme::palette().text_muted
                        }));
                        ui.label(theme::semibold(name, 15.0));
                        ui.label(
                            RichText::new(if *enabled { "enabled" } else { "disabled" })
                                .small()
                                .color(if *enabled {
                                    theme::palette().success
                                } else {
                                    theme::palette().text_muted
                                }),
                        );
                        ui.label(
                            RichText::new(format!("{schedule}  {next}"))
                                .small()
                                .color(theme::palette().text_muted),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::ghost_button(ui, "Edit").clicked() {
                                edit = Some(*i);
                            }
                        });
                    });
                    ui.separator();
                }
                if let Some(i) = edit {
                    self.selected = Some(i);
                    self.screen = Screen::Builder;
                }
            }
        });
        ui.add_space(12.0);
        // 7-day sent/failed chart (Group 1).
        theme::card(ui, |ui| {
            theme::section_title(ui, t(self.lang(), "dash.week_chart"));
            self.show_week_chart(ui);
        });
    }

    fn show_builder(&mut self, ui: &mut egui::Ui) {
        ui.label("Add nodes, move them up/down, edit the selected scenario. Dry-run validates without sending.");
        ui.add_space(12.0);
        // Two columns: scenario list | editor for the selected scenario.
        // The editor content scrolls, but the action bar (Validate, node
        // buttons, Export/Import) stays pinned below it, so the key
        // actions are always visible in the 700 px window without deep
        // scrolling.
        ui.columns(2, |cols| {
            // Distinct id_salts: two ScrollAreas in one frame must not
            // share an auto ID (egui debug-clash warnings + shared scroll
            // state).
            egui::ScrollArea::vertical()
                .id_salt("builder_list")
                .show(&mut cols[0], |ui| {
                    self.show_builder_list(ui);
                });
            let selected = self.selected;
            match selected {
                Some(i) => {
                    let action_h = 130.0;
                    let avail = cols[1].available_height();
                    egui::ScrollArea::vertical()
                        .id_salt("builder_editor")
                        .max_height((avail - action_h).max(140.0))
                        .show(&mut cols[1], |ui| {
                            self.show_builder_editor(ui, i);
                        });
                    cols[1].add_space(8.0);
                    self.show_builder_actions(&mut cols[1], i);
                }
                None => {
                    theme::card(&mut cols[1], |ui| {
                        theme::section_title(ui, t(self.lang(), "builder.editor"));
                        theme::muted(
                            ui,
                            "Select a scenario on the left to edit it, or create a new one in the list.",
                        );
                    });
                }
            }
        });
    }

    /// Left column of the Builder: the scenario list.
    fn show_builder_list(&mut self, ui: &mut egui::Ui) {
        theme::card(ui, |ui| {
            // New-scenario row lives in this card (not above the columns)
            // so the editor column gets the full height.
            ui.horizontal(|ui| {
                let field_w = (ui.available_width() - 132.0).max(100.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_scenario_name)
                        .hint_text("New scenario name")
                        .desired_width(field_w),
                );
                if theme::primary_button(ui, "+ New scenario").clicked() {
                    self.create_scenario();
                }
            });
            ui.add_space(8.0);
            // Search + bulk actions. Labels are resolved before the
            // row: the search field holds a &mut borrow of the query
            // while the row is built, so no `self` calls inside.
            let search_hint = t(self.lang(), "builder.search_hint");
            let enable_all_label = t(self.lang(), "builder.enable_all");
            let disable_all_label = t(self.lang(), "builder.disable_all");
            let mut bulk_enable = false;
            let mut bulk_disable = false;
            // F-GUI-2: the search field and the two bulk buttons used
            // to share one row with a fixed 170 px reserve, which the
            // buttons overflow at the default window width (the second
            // button slid under the editor panel). Two rows: the field
            // takes the full width, the buttons sit below it — robust
            // at any width, language, or UI scale.
            ui.add(
                egui::TextEdit::singleline(&mut self.builder_search)
                    .hint_text(search_hint)
                    .desired_width(ui.available_width()),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button(enable_all_label).clicked() {
                    bulk_enable = true;
                }
                if ui.button(disable_all_label).clicked() {
                    bulk_disable = true;
                }
            });
            ui.add_space(8.0);
            theme::section_title(ui, "Your scenarios");
            let mut selected = self.selected;
            let mut to_delete: Option<usize> = None;
            let mut to_toggle: Option<usize> = None;
            let mut to_clone: Option<usize> = None;
            // Indices matching the search query, in list order. Bulk
            // actions apply to exactly these (what you see is what
            // you act on); indices stay valid because the list is
            // only mutated after this scope.
            let query = self.builder_search.clone();
            let mut visible: Vec<usize> = Vec::new();
            let total: usize;
            {
                let list = self.scenarios.lock().unwrap();
                total = list.len();
                if list.is_empty() {
                    theme::muted(ui, "No scenarios yet — create your first one above.");
                }
                for (i, s) in list.iter().enumerate() {
                    if scenario_matches_query(s, &query) {
                        visible.push(i);
                    }
                }
                if total > 0 && visible.is_empty() {
                    theme::muted(ui, "Không có kịch bản nào khớp với tìm kiếm.");
                }
                for &i in &visible {
                    let s = &list[i];
                    ui.horizontal(|ui| {
                        if ui.selectable_label(selected == Some(i), &s.name).clicked() {
                            selected = Some(i);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Delete is destructive: red text so it can
                            // never be confused with Enable at a glance.
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("Delete").color(theme::palette().danger),
                                    )
                                    .fill(theme::palette().btn_bg),
                                )
                                .clicked()
                            {
                                to_delete = Some(i);
                            }
                            ui.add_space(6.0);
                            if ui
                                .button(if s.enabled { "Disable" } else { "Enable" })
                                .clicked()
                            {
                                to_toggle = Some(i);
                            }
                            ui.add_space(6.0);
                            if ui.button("Clone").clicked() {
                                to_clone = Some(i);
                            }
                        });
                    });
                    if !s.channel_id.is_empty() {
                        theme::muted(ui, &format!("channel: {}", s.channel_id));
                    }
                    ui.separator();
                }
            }
            if !query.trim().is_empty() && total > 0 {
                theme::muted(
                    ui,
                    &format!("Hiển thị {} / {} kịch bản", visible.len(), total),
                );
            }
            self.selected = selected;
            if let Some(i) = to_toggle {
                let is_webhook = self.config.send_mode == SendMode::Webhook;
                let mut msg: Option<String> = None;
                if let Ok(mut list) = self.scenarios.lock() {
                    let was_enabled = list.get(i).map(|s| s.enabled).unwrap_or(false);
                    if was_enabled {
                        if let Some(s) = list.get_mut(i) {
                            s.enabled = false;
                        }
                    } else {
                        // Same validation as before, now shared with
                        // bulk enable (see `try_enable_scenario`).
                        msg = try_enable_scenario(&mut list, i, is_webhook);
                    }
                }
                self.save_scenarios();
                if let Some(m) = msg {
                    self.set_status(&m);
                }
            }
            if bulk_enable {
                // Enable every visible scenario, validating each one
                // in turn (a failure leaves that scenario disabled
                // and is reported; the rest still go through).
                let is_webhook = self.config.send_mode == SendMode::Webhook;
                let mut enabled = 0usize;
                let mut failed = 0usize;
                let mut first_err: Option<String> = None;
                if let Ok(mut list) = self.scenarios.lock() {
                    for &i in &visible {
                        if list.get(i).map(|s| s.enabled).unwrap_or(true) {
                            continue;
                        }
                        match try_enable_scenario(&mut list, i, is_webhook) {
                            None => enabled += 1,
                            Some(e) => {
                                failed += 1;
                                if first_err.is_none() {
                                    first_err = Some(e);
                                }
                            }
                        }
                    }
                }
                self.save_scenarios();
                if failed == 0 {
                    self.set_status(&format!("Đã bật {enabled} kịch bản."));
                } else if let Some(e) = first_err {
                    self.set_status(&format!(
                        "Đã bật {enabled} kịch bản, {failed} không bật được: {e}"
                    ));
                }
            }
            if bulk_disable {
                let mut disabled = 0usize;
                if let Ok(mut list) = self.scenarios.lock() {
                    for &i in &visible {
                        if let Some(s) = list.get_mut(i) {
                            if s.enabled {
                                s.enabled = false;
                                disabled += 1;
                            }
                        }
                    }
                }
                self.save_scenarios();
                self.set_status(&format!("Đã tắt {disabled} kịch bản."));
            }
            if let Some(i) = to_delete {
                if let Ok(mut list) = self.scenarios.lock() {
                    if i < list.len() {
                        let id = list[i].id.clone();
                        list.remove(i);
                        let _ = self.store.delete_scenario(&id);
                    }
                }
                self.selected = None;
            }
            if let Some(i) = to_clone {
                // Clone (QoL Q5): fresh id, disabled, no run history,
                // name marked as a copy. The clone is selected so the
                // user can edit it right away.
                if let Ok(mut list) = self.scenarios.lock() {
                    if let Some(orig) = list.get(i).cloned() {
                        let mut copy = orig;
                        copy.id = uuid::Uuid::new_v4().to_string();
                        copy.name = format!("{} (bản sao)", copy.name);
                        copy.enabled = false;
                        copy.last_run_at = None;
                        copy.created_at = None;
                        list.push(copy.clone());
                        let idx = list.len() - 1;
                        let _ = self.store.save_scenario(&copy);
                        selected = Some(idx);
                    }
                }
                self.selected = selected;
                self.set_status("Scenario cloned (disabled — review it, then enable).");
            }
        });
    }

    /// Right column of the Builder: edit the selected scenario in place.
    fn show_builder_editor(&mut self, ui: &mut egui::Ui, i: usize) {
        let mut channel_id = String::new();
        let mut messages: Vec<String> = Vec::new();
        // (kind, id, command display name — "" for non-command nodes).
        let mut nodes: Vec<(NodeKind, String, String)> = Vec::new();
        if let Ok(list) = self.scenarios.lock() {
            if let Some(s) = list.get(i) {
                channel_id = s.channel_id.clone();
                messages = s.messages.clone();
                nodes = s
                    .nodes
                    .iter()
                    .map(|n| {
                        let cmd = if n.kind == NodeKind::ActionSendCommand {
                            CommandConfig::from_node(n).display_name()
                        } else {
                            String::new()
                        };
                        (n.kind.clone(), n.id.clone(), cmd)
                    })
                    .collect();
            }
        }
        theme::card(ui, |ui| {
            theme::section_title(ui, t(self.lang(), "builder.editor"));
            if self.config.send_mode == SendMode::Webhook {
                // Destination picker: each scenario sends to exactly
                // one destination. A scenario without one cannot be
                // enabled (validation blocks it with a clear error).
                let dests = self.store.load_destinations().unwrap_or_default();
                let current_dest_id = {
                    self.scenarios
                        .lock()
                        .map(|l| {
                            l.get(i)
                                .and_then(|s| s.destination_id.clone())
                                .unwrap_or_default()
                        })
                        .unwrap_or_default()
                };
                let current_label = dests
                    .iter()
                    .find(|d| d.id == current_dest_id)
                    .map(|d| d.display_name())
                    .unwrap_or_else(|| "— Chưa chọn đích đến —".to_string());
                ui.horizontal(|ui| {
                    ui.label("Gửi tới:");
                    egui::ComboBox::from_id_salt("builder_destination")
                        .selected_text(current_label)
                        .show_ui(ui, |ui| {
                            for d in dests.iter().filter(|d| d.enabled) {
                                if ui
                                    .selectable_label(d.id == current_dest_id, d.display_name())
                                    .clicked()
                                {
                                    if let Ok(mut list) = self.scenarios.lock() {
                                        if let Some(s) = list.get_mut(i) {
                                            s.destination_id = Some(d.id.clone());
                                            let _ = self.store.save_scenario(s);
                                        }
                                    }
                                }
                            }
                            if dests.iter().any(|d| !d.enabled) {
                                ui.separator();
                                theme::muted(ui, "Đích đến đã tắt không hiện ở đây.");
                            }
                        });
                    if ui.button("Sửa đích đến").clicked() {
                        self.screen = Screen::Destinations;
                    }
                });
                if let Some(dest) = dests.iter().find(|d| d.id == current_dest_id) {
                    theme::muted(
                        ui,
                        &format!(
                            "Giới hạn: {}/ngày, cách nhau tối thiểu {}s{}",
                            if dest.daily_cap == 0 {
                                "theo toàn cục".to_string()
                            } else {
                                dest.daily_cap.to_string()
                            },
                            dest.min_interval_secs,
                            match (&dest.quiet_start, &dest.quiet_end) {
                                (Some(s), Some(e)) => format!(", im lặng {s}–{e}"),
                                _ => String::new(),
                            }
                        ),
                    );
                } else if dests.is_empty() {
                    theme::muted(
                        ui,
                        "Chưa có đích đến nào — thêm một webhook trong Đích đến.",
                    );
                } else {
                    theme::muted(ui, "Chọn một đích đến để bật kịch bản này.");
                }
                theme::muted(ui, "Lệnh (Send Command) không hoạt động ở chế độ Webhook.");
                ui.add_space(6.0);
            } else {
                ui.label("Channel picker (loads your servers/channels from Discord):");
                ui.horizontal(|ui| {
                    if ui.button("Load servers").clicked() {
                        self.load_guilds();
                    }
                    let guilds = self.guilds.lock().map(|g| g.clone()).unwrap_or_default();
                    if !guilds.is_empty() {
                        let mut picked = self.picker_guild_id.clone();
                        egui::ComboBox::from_label("Server")
                            .selected_text(
                                guilds
                                    .iter()
                                    .find(|g| g.id == picked)
                                    .map(|g| g.name.clone())
                                    .unwrap_or_else(|| "pick a server".to_string()),
                            )
                            .show_ui(ui, |ui| {
                                for g in &guilds {
                                    if ui
                                        .selectable_value(&mut picked, g.id.clone(), &g.name)
                                        .clicked()
                                    {
                                        self.picker_guild_id = g.id.clone();
                                        self.load_channels(g.id.clone());
                                    }
                                }
                            });
                    }
                    let channels = self.channels.lock().map(|c| c.clone()).unwrap_or_default();
                    if !channels.is_empty() {
                        let mut picked = channel_id.clone();
                        egui::ComboBox::from_label("Channel")
                            .selected_text(
                                channels
                                    .iter()
                                    .find(|c| c.id == picked)
                                    .map(|c| format!("#{}", c.name))
                                    .unwrap_or_else(|| "pick a channel".to_string()),
                            )
                            .show_ui(ui, |ui| {
                                for c in &channels {
                                    if ui
                                        .selectable_value(
                                            &mut picked,
                                            c.id.clone(),
                                            format!("#{}", c.name),
                                        )
                                        .clicked()
                                    {
                                        channel_id = c.id.clone();
                                    }
                                }
                            });
                    }
                });
                ui.label("Destination channel ID (or pick above):");
                ui.add(
                    egui::TextEdit::singleline(&mut channel_id)
                        .hint_text("e.g. 123456789012345678"),
                );
            } // end user-mode channel picker
            ui.label("Messages (rotate, spin-text {a|b} supported):");
            if messages.is_empty() {
                theme::muted(ui, "No messages yet — add one below.");
            }
            egui::ScrollArea::vertical()
                .id_salt("builder_messages")
                .max_height(90.0)
                .show(ui, |ui| {
                    for m in &messages {
                        if over_limit(m) {
                            ui.label(
                                RichText::new(format!(
                                    "• {m} (quá 2000 ký tự — sẽ bị chặn khi bật)"
                                ))
                                .color(theme::palette().danger),
                            );
                        } else {
                            ui.label(format!("• {m}"));
                        }
                    }
                });
            ui.horizontal(|ui| {
                // The field must leave room for the button in this narrow
                // column, or the button is pushed past the right edge.
                let field_w = (ui.available_width() - 132.0).max(100.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_message)
                        .hint_text("Add a message...")
                        .desired_width(field_w),
                );
                // Over the Discord limit: the button is disabled and
                // the preview below shows why (QoL Q1).
                let too_long = over_limit(self.new_message.trim());
                let add_clicked = ui
                    .add_enabled(
                        !self.new_message.trim().is_empty() && !too_long,
                        egui::Button::new("+ Add message"),
                    )
                    .clicked();
                if add_clicked {
                    if let Ok(mut list) = self.scenarios.lock() {
                        if let Some(s) = list.get_mut(i) {
                            s.messages.push(self.new_message.trim().to_string());
                        }
                    }
                    self.new_message.clear();
                    self.save_scenarios();
                }
            });
            // Emoji picker (Q10) + content source line (Q6).
            self.show_emoji_picker(ui);
            ui.add_space(4.0);
            self.show_content_source(ui, i);
            ui.add_space(6.0);
            ui.label("Nodes (in order — click a command node to edit it):");
            // Q1 preview: live Discord-style preview of the text
            // being composed, with a 2000-character counter.
            self.show_message_preview(ui);
            ui.add_space(6.0);
            // Schedule editor (Q7) + template library (Q6) + embed (Q8).
            self.show_schedule_editor(ui, i);
            ui.add_space(6.0);
            self.show_template_library(ui, i);
            ui.add_space(6.0);
            self.show_embed_composer(ui, i);
            ui.add_space(6.0);
            self.show_poll_composer(ui, i);
            ui.add_space(6.0);
            // Import warnings from the last import (Q3), if any.
            if !self.import_warnings.is_empty() {
                theme::card(ui, |ui| {
                    theme::section_title(ui, "Cảnh báo import");
                    for w in &self.import_warnings {
                        theme::muted(ui, &format!("• {w}"));
                    }
                });
                ui.add_space(6.0);
            }
            for (kind, id, cmd_name) in &nodes {
                let label = if kind == &NodeKind::ActionSendCommand {
                    format!("• {} — Lệnh: {cmd_name}", kind.group())
                } else {
                    format!("• {} — {}", kind.group(), kind.label())
                };
                if ui
                    .selectable_label(self.selected_node.as_deref() == Some(id.as_str()), label)
                    .clicked()
                {
                    self.selected_node = Some(id.clone());
                }
            }
            self.show_command_editor(ui, i);
            // Node buttons + Validate + Export/Import live in the pinned
            // action bar below the editor (show_builder_actions), so they
            // are always visible without scrolling.
            // Persist channel_id edits (1 distinct channel).
            if let Ok(mut list) = self.scenarios.lock() {
                let cur = list.get(i).map(|s| s.channel_id.clone());
                if cur.as_deref() != Some(channel_id.as_str()) {
                    let mut hypo = list.clone();
                    if let Some(h) = hypo.get_mut(i) {
                        h.channel_id = channel_id.clone();
                    }
                    if let Some(v) = channels_violation(&hypo) {
                        self.set_status(&v);
                    } else if let Some(s) = list.get_mut(i) {
                        s.channel_id = channel_id;
                        let _ = self.store.save_scenario(s);
                    }
                }
            }
        });
    }

    /// Content source line (Q6): which messages will actually send,
    /// and — when the choice is unambiguous — the exact next message.
    /// Rotation is random (never repeats the last pick), so with
    /// several candidates the UI says so honestly instead of
    /// predicting a message it cannot know.
    fn show_content_source(&mut self, ui: &mut egui::Ui, i: usize) {
        let (scenario, dest) = {
            let list = self.scenarios.lock().map(|l| l.get(i).cloned());
            let scenario = list.unwrap_or(None);
            let dest = scenario
                .as_ref()
                .and_then(|s| s.destination_id.clone())
                .and_then(|id| self.store.load_destination(&id).ok().flatten());
            (scenario, dest)
        };
        let Some(scenario) = scenario else {
            return;
        };
        // Checkbox: use destination messages outright (webhook mode).
        if self.config.send_mode == SendMode::Webhook {
            if let Some(d) = &dest {
                let mut use_dest = scenario.use_destination_messages;
                if ui
                    .checkbox(
                        &mut use_dest,
                        "Dùng bộ tin của đích đến thay cho tin của kịch bản",
                    )
                    .changed()
                {
                    if let Ok(mut list) = self.scenarios.lock() {
                        if let Some(s) = list.get_mut(i) {
                            s.use_destination_messages = use_dest;
                            let _ = self.store.save_scenario(s);
                        }
                    }
                }
                let _ = d;
            }
        }
        // Mention control + silent send (idea #7). Same
        // lock-edit-save pattern as the checkbox above.
        let mut allow_mass = scenario.allow_mass_mentions;
        if ui
            .checkbox(
                &mut allow_mass,
                "Cho phép ping hàng loạt (@everyone, @here, vai trò)",
            )
            .changed()
        {
            if let Ok(mut list) = self.scenarios.lock() {
                if let Some(s) = list.get_mut(i) {
                    s.allow_mass_mentions = allow_mass;
                    let _ = self.store.save_scenario(s);
                }
            }
        }
        let mut silent = scenario.silent_send;
        if ui
            .checkbox(&mut silent, "Gửi im lặng (không tạo thông báo)")
            .changed()
        {
            if let Ok(mut list) = self.scenarios.lock() {
                if let Some(s) = list.get_mut(i) {
                    s.silent_send = silent;
                    let _ = self.store.save_scenario(s);
                }
            }
        }
        if !scenario.allow_mass_mentions {
            let has_mass_ping = scenario
                .messages
                .iter()
                .any(|m| m.contains("@everyone") || m.contains("@here"))
                || scenario
                    .node_message()
                    .is_some_and(|m| m.contains("@everyone") || m.contains("@here"));
            if has_mass_ping {
                ui.label(
                    RichText::new(
                        "Tin nhắn chứa @everyone/@here nhưng ping hàng loạt đang bị chặn — Discord sẽ hiển thị chữ, không ping ai. Bật ô trên nếu bạn thật sự muốn ping.",
                    )
                    .color(theme::palette().warning),
                );
            }
        }
        // Durable counters (idea #6): the counters this scenario's
        // messages reference, with their current values and a
        // reset. Values come from the store (durable), not from
        // the scenario clone.
        let mut counter_text = scenario.messages.join("\n");
        if let Some(m) = scenario.node_message() {
            counter_text.push('\n');
            counter_text.push_str(&m);
        }
        let counter_list = crate::scenario::counter_names(&counter_text);
        if !counter_list.is_empty() {
            for name in &counter_list {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "Bộ đếm '{name}': hiện tại {}",
                        self.store.get_counter(name)
                    ));
                    if theme::ghost_button(ui, "Đặt lại").clicked() {
                        let _ = self.store.delete_counter(name);
                    }
                });
            }
            theme::muted(
                ui,
                "Bộ đếm tăng 1 sau mỗi lần gửi thành công dùng tới nó; lần gửi đầu tiên hiển thị 1.",
            );
        }
        theme::muted(
            ui,
            "Biến dùng được trong tin nhắn: {{username}} {{server}} {{channel}} {{date}} {{time}} {{weekday}} {{week_number}} {{days_until:YYYY-MM-DD}} {{counter:tên}}.",
        );
        // Source line.
        if let Some(node_msg) = scenario.node_message() {
            theme::muted(ui, &format!("Tin tiếp theo (từ node): {node_msg}"));
        } else if scenario.use_destination_messages {
            match &dest {
                Some(d) if !d.default_messages.is_empty() => {
                    if d.default_messages.len() == 1 {
                        theme::muted(
                            ui,
                            &format!("Tin tiếp theo (bộ tin đích đến): {}", d.default_messages[0]),
                        );
                    } else {
                        theme::muted(ui, &format!(
                            "Nguồn tin: bộ tin của đích đến ({} tin, chọn ngẫu nhiên, không lặp lại tin vừa gửi)",
                            d.default_messages.len()
                        ));
                    }
                }
                _ => {
                    ui.label(
                        RichText::new(
                            "Cảnh báo: đang dùng bộ tin của đích đến nhưng đích đến chưa có tin nào — kịch bản sẽ không gửi được.",
                        )
                        .color(theme::palette().danger),
                    );
                }
            }
        } else if !scenario.messages.is_empty() {
            if scenario.messages.len() == 1 {
                theme::muted(ui, &format!("Tin tiếp theo: {}", scenario.messages[0]));
            } else {
                theme::muted(
                    ui,
                    &format!(
                    "Nguồn tin: {} tin của kịch bản (chọn ngẫu nhiên, không lặp lại tin vừa gửi)",
                    scenario.messages.len()
                ),
                );
            }
        } else if let Some(d) = &dest {
            if !d.default_messages.is_empty() {
                theme::muted(ui, &format!(
                    "Nguồn tin: bộ tin của đích đến ({} tin) — kịch bản đang trống nên dùng bộ tin này.",
                    d.default_messages.len()
                ));
            } else {
                ui.label(
                    RichText::new(
                        "Cảnh báo: kịch bản và đích đến đều chưa có tin nào — kịch bản sẽ không gửi được.",
                    )
                    .color(theme::palette().danger),
                );
            }
        } else if scenario.messages.is_empty() {
            ui.label(
                RichText::new("Cảnh báo: kịch bản chưa có tin nào.").color(theme::palette().danger),
            );
        }
    }

    /// Schedule editor (Q7): type + fields + presets + timezone.
    /// Reads a snapshot, renders controls, writes back on change.
    /// Invalid input (bad time, bad cron, unknown timezone) is shown
    /// as an error and never written (fail-closed).
    fn show_schedule_editor(&mut self, ui: &mut egui::Ui, i: usize) {
        let (mut schedule, mut timezone) = self
            .scenarios
            .lock()
            .map(|l| {
                l.get(i)
                    .map(|s| (s.schedule.clone(), s.timezone.clone()))
                    .unwrap_or((None, "Asia/Ho_Chi_Minh".to_string()))
            })
            .unwrap_or((None, "Asia/Ho_Chi_Minh".to_string()));
        let mut changed = false;
        let mut error: Option<String> = None;
        theme::card(ui, |ui| {
            theme::section_title(ui, t(self.lang(), "builder.schedule"));
            // Presets (Q7).
            ui.label("Mẫu nhanh:");
            ui.horizontal_wrapped(|ui| {
                let presets: [(&str, Schedule); 6] = [
                    ("Mỗi 30 phút", Schedule::EveryMinutes { minutes: 30 }),
                    ("Mỗi giờ", Schedule::EveryHours { hours: 1 }),
                    (
                        "Sáng 8:00",
                        Schedule::Daily {
                            time: chrono::NaiveTime::from_hms_opt(8, 0, 0).unwrap_or_default(),
                        },
                    ),
                    (
                        "Tối 20:00",
                        Schedule::Daily {
                            time: chrono::NaiveTime::from_hms_opt(20, 0, 0).unwrap_or_default(),
                        },
                    ),
                    (
                        "T2–T6 9:00",
                        Schedule::Weekly {
                            weekdays: vec![0, 1, 2, 3, 4],
                            time: chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap_or_default(),
                        },
                    ),
                    (
                        "Cuối tuần 10:00",
                        Schedule::Weekly {
                            weekdays: vec![5, 6],
                            time: chrono::NaiveTime::from_hms_opt(10, 0, 0).unwrap_or_default(),
                        },
                    ),
                ];
                for (label, preset) in presets {
                    if ui.button(label).clicked() {
                        schedule = Some(preset);
                        changed = true;
                    }
                }
            });
            ui.separator();
            // Type selector.
            let type_labels = ["Hàng ngày", "Hàng tuần", "Mỗi N phút", "Mỗi N giờ", "Cron"];
            let current_type = match &schedule {
                Some(Schedule::Daily { .. }) => 0,
                Some(Schedule::Weekly { .. }) => 1,
                Some(Schedule::EveryMinutes { .. }) => 2,
                Some(Schedule::EveryHours { .. }) => 3,
                Some(Schedule::Cron { .. }) => 4,
                Some(Schedule::Once { .. }) | None => 0,
            };
            let mut type_idx = current_type;
            egui::ComboBox::from_id_salt("schedule_type")
                .selected_text(type_labels[type_idx])
                .show_ui(ui, |ui| {
                    for (idx, label) in type_labels.iter().enumerate() {
                        ui.selectable_value(&mut type_idx, idx, *label);
                    }
                });
            if type_idx != current_type {
                schedule = Some(match type_idx {
                    1 => Schedule::Weekly {
                        weekdays: vec![0],
                        time: chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap_or_default(),
                    },
                    2 => Schedule::EveryMinutes { minutes: 30 },
                    3 => Schedule::EveryHours { hours: 1 },
                    4 => Schedule::Cron {
                        expr: "0 9 * * *".to_string(),
                    },
                    _ => Schedule::Daily {
                        time: chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap_or_default(),
                    },
                });
                changed = true;
            }
            // Per-type fields.
            match &mut schedule {
                Some(Schedule::Daily { time }) => {
                    let mut text = time.format("%H:%M").to_string();
                    ui.horizontal(|ui| {
                        ui.label("Giờ (HH:MM):");
                        if ui.text_edit_singleline(&mut text).changed() {
                            match chrono::NaiveTime::parse_from_str(&text, "%H:%M") {
                                Ok(t) => {
                                    *time = t;
                                    changed = true;
                                }
                                Err(_) => error = Some("Giờ phải là HH:MM (24h).".to_string()),
                            }
                        }
                    });
                }
                Some(Schedule::Weekly { weekdays, time }) => {
                    let names = ["T2", "T3", "T4", "T5", "T6", "T7", "CN"];
                    ui.horizontal(|ui| {
                        for (day, name) in names.iter().enumerate() {
                            let mut on = weekdays.contains(&(day as u8));
                            if ui.checkbox(&mut on, *name).changed() {
                                if on {
                                    weekdays.push(day as u8);
                                    weekdays.sort_unstable();
                                } else {
                                    weekdays.retain(|d| *d != day as u8);
                                }
                                changed = true;
                            }
                        }
                    });
                    if weekdays.is_empty() {
                        error = Some("Chọn ít nhất một ngày trong tuần.".to_string());
                    }
                    let mut text = time.format("%H:%M").to_string();
                    ui.horizontal(|ui| {
                        ui.label("Giờ (HH:MM):");
                        if ui.text_edit_singleline(&mut text).changed() {
                            match chrono::NaiveTime::parse_from_str(&text, "%H:%M") {
                                Ok(t) => {
                                    *time = t;
                                    changed = true;
                                }
                                Err(_) => error = Some("Giờ phải là HH:MM (24h).".to_string()),
                            }
                        }
                    });
                }
                Some(Schedule::EveryMinutes { minutes }) => {
                    ui.horizontal(|ui| {
                        ui.label("Mỗi (phút):");
                        if ui
                            .add(egui::DragValue::new(minutes).range(1..=1440))
                            .changed()
                        {
                            changed = true;
                        }
                    });
                }
                Some(Schedule::EveryHours { hours }) => {
                    ui.horizontal(|ui| {
                        ui.label("Mỗi (giờ):");
                        if ui.add(egui::DragValue::new(hours).range(1..=24)).changed() {
                            changed = true;
                        }
                    });
                }
                Some(Schedule::Cron { expr }) => {
                    ui.horizontal(|ui| {
                        ui.label("Cron (5 trường):");
                        if ui.text_edit_singleline(expr).changed() {
                            changed = true;
                        }
                    });
                    theme::muted(ui, "Ví dụ: 0 9 * * * = 9:00 hàng ngày.");
                }
                Some(Schedule::Once { at }) => {
                    theme::muted(
                        ui,
                        &format!(
                            "Chạy một lần: {} (không sửa được ở đây — import JSON để đổi).",
                            at.format("%Y-%m-%d %H:%M UTC")
                        ),
                    );
                }
                None => {
                    theme::muted(ui, "Chưa có lịch — chọn một mẫu nhanh ở trên.");
                }
            }
            // Timezone (Q7: shown with the schedule, fail-closed).
            ui.horizontal(|ui| {
                ui.label("Múi giờ:");
                if ui.text_edit_singleline(&mut timezone).changed() {
                    changed = true;
                }
            });
            ui.horizontal_wrapped(|ui| {
                for tz in [
                    "Asia/Ho_Chi_Minh",
                    "Asia/Bangkok",
                    "UTC",
                    "Asia/Tokyo",
                    "Europe/London",
                    "America/New_York",
                ] {
                    if ui.small_button(tz).clicked() {
                        timezone = tz.to_string();
                        changed = true;
                    }
                }
            });
            // Next run with timezone shown. Direct call, deliberately
            // not cached: this is the DRAFT schedule being edited
            // (one call per frame for one scenario), and every
            // keystroke changes the schedule anyway.
            if let Some(sc) = &schedule {
                let next = sc.next_after(chrono::Local::now(), &timezone);
                theme::muted(
                    ui,
                    &format!("Lịch tới: {} ({})", next.format("%d/%m/%Y %H:%M"), timezone),
                );
            }
            // Simulation (Group 1): the next 10 runs of the DRAFT
            // schedule with blocked-run warnings. Cached by draft
            // key so a sparse cron is not re-walked every frame.
            ui.separator();
            ui.label(RichText::new(t(self.lang(), "builder.simulation")).strong());
            let key = format!("{i}|{schedule:?}|{timezone}");
            let cached =
                self.sim_cache
                    .as_ref()
                    .and_then(|(k, v)| if *k == key { Some(v.clone()) } else { None });
            let sim_runs = match cached {
                Some(v) => v,
                None => {
                    let draft = self
                        .scenarios
                        .lock()
                        .map(|l| l.get(i).cloned())
                        .unwrap_or_default();
                    let runs = match draft {
                        Some(mut sc) => {
                            sc.schedule = schedule.clone();
                            sc.timezone = timezone.clone();
                            let dest = sc
                                .destination_id
                                .as_deref()
                                .and_then(|id| self.store.load_destination(id).ok().flatten());
                            let sent = dest
                                .as_ref()
                                .map(|d| self.store.count_sends_today_for_destination(&d.id))
                                .unwrap_or(0);
                            let is_webhook = self.config.send_mode == SendMode::Webhook;
                            let cap = self.config.global_daily_cap;
                            crate::timeline::simulate_scenario(
                                &sc,
                                dest.as_ref(),
                                is_webhook,
                                sent,
                                cap,
                                chrono::Local::now(),
                                10,
                            )
                        }
                        None => Vec::new(),
                    };
                    self.sim_cache = Some((key, runs.clone()));
                    runs
                }
            };
            if sim_runs.is_empty() {
                theme::muted(ui, "Không có lần chạy nào trong 30 ngày tới.");
            } else {
                for run in &sim_runs {
                    ui.horizontal(|ui| {
                        ui.label(run.at.format("%d/%m/%Y %H:%M").to_string());
                        if let Some(reason) = &run.blocked {
                            ui.label(
                                RichText::new(format!("sẽ bỏ qua: {reason}"))
                                    .color(theme::palette().warning)
                                    .small(),
                            );
                        }
                    });
                }
            }
            if let Some(e) = &error {
                ui.label(RichText::new(e).color(theme::palette().danger));
            }
        });
        // Write back (only when valid: schedule validates + timezone parses).
        if changed && error.is_none() {
            if timezone.parse::<chrono_tz::Tz>().is_err() {
                self.set_status(&format!("Unknown timezone '{timezone}' — not saved."));
                return;
            }
            if let Some(sc) = &schedule {
                if let Err(e) = sc.validate() {
                    self.set_status(&format!("Invalid schedule — not saved: {e}"));
                    return;
                }
            }
            if let Ok(mut list) = self.scenarios.lock() {
                if let Some(s) = list.get_mut(i) {
                    s.schedule = schedule;
                    s.timezone = timezone;
                    let _ = self.store.save_scenario(s);
                }
            }
        }
    }

    /// Template library (Q6): save the composed text as a template,
    /// insert a template into this scenario's messages, delete one.
    fn show_template_library(&mut self, ui: &mut egui::Ui, i: usize) {
        theme::card(ui, |ui| {
            theme::section_title(ui, t(self.lang(), "builder.templates"));
            let templates = self.store.load_templates().unwrap_or_default();
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.template_name).hint_text("Tên mẫu..."));
                let can_save = !self.template_name.trim().is_empty()
                    && !self.new_message.trim().is_empty()
                    && !over_limit(self.new_message.trim());
                if ui
                    .add_enabled(can_save, egui::Button::new("Lưu tin đang soạn thành mẫu"))
                    .clicked()
                {
                    match self
                        .store
                        .save_template(&self.template_name, self.new_message.trim())
                    {
                        Ok(_) => {
                            self.template_name.clear();
                            self.set_status("Template saved.");
                        }
                        Err(e) => self.set_status(&format!("Cannot save template: {e}")),
                    }
                }
            });
            theme::muted(
                ui,
                "Mẫu được lưu chung cho mọi kịch bản, không chứa secret.",
            );
            if templates.is_empty() {
                theme::muted(ui, "Chưa có mẫu nào.");
            }
            for (id, name, content) in &templates {
                ui.horizontal(|ui| {
                    ui.label(format!("• {name}: {content}"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Xóa").clicked() {
                            let _ = self.store.delete_template(id);
                        }
                        if ui.button("Chèn vào kịch bản").clicked() {
                            if let Ok(mut list) = self.scenarios.lock() {
                                if let Some(s) = list.get_mut(i) {
                                    s.messages.push(content.clone());
                                }
                            }
                            self.save_scenarios();
                            self.set_status("Template inserted into this scenario.");
                        }
                    });
                });
            }
        });
    }

    /// Emoji picker (Q10): offline grid from the shortcode table
    /// (inserts the real Unicode character, so the sent message
    /// contains the emoji itself) + custom emoji markup paste.
    fn show_emoji_picker(&mut self, ui: &mut egui::Ui) {
        if ui
            .button(if self.show_emoji_picker {
                "😀 Đóng bảng emoji"
            } else {
                "😀 Emoji"
            })
            .clicked()
        {
            self.show_emoji_picker = !self.show_emoji_picker;
        }
        if !self.show_emoji_picker {
            return;
        }
        theme::card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (_, emoji) in crate::preview::EMOJI_PICKER_LIST {
                    if ui.button(*emoji).clicked() {
                        self.new_message.push_str(emoji);
                    }
                }
            });
            ui.separator();
            ui.label("Emoji tùy chỉnh (dán <:tên:id> hoặc URL CDN):");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.custom_emoji_input)
                        .hint_text("<:name:123456789>"),
                );
                if ui.button("Chèn").clicked() && !self.custom_emoji_input.trim().is_empty() {
                    self.new_message.push_str(self.custom_emoji_input.trim());
                    self.custom_emoji_input.clear();
                }
            });
            theme::muted(ui, "Emoji tùy chỉnh chỉ hiện trong Discord nếu server có emoji đó (webhook dùng emoji của server chứa kênh). Xem trước hiện nguyên văn markup.");
        });
    }

    /// Embed composer (Q8, extended by idea #8): up to 10 rich
    /// embeds per scenario, sent with every message. Limits are
    /// shown live (per embed AND the 6000-char total across all
    /// of them); drafts save as edited, and validation gates
    /// enabling/sending instead of the editor.
    fn show_embed_composer(&mut self, ui: &mut egui::Ui, i: usize) {
        let mut embeds = self
            .scenarios
            .lock()
            .map(|l| l.get(i).map(|s| s.embeds.clone()).unwrap_or_default())
            .unwrap_or_default();
        let mut changed = false;
        let mut error: Option<String> = None;
        theme::card(ui, |ui| {
            theme::section_title(ui, t(self.lang(), "builder.embed"));
            let mut enabled = !embeds.is_empty();
            if ui.checkbox(&mut enabled, "Gửi kèm embed").changed() {
                if enabled {
                    if embeds.is_empty() {
                        embeds.push(crate::embed::Embed::default());
                    }
                } else {
                    embeds.clear();
                }
                changed = true;
            }
            if enabled {
                let mut to_remove: Option<usize> = None;
                for (idx, embed) in embeds.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(theme::semibold(&format!("Embed {}", idx + 1), 15.0));
                        if ui.button("Xóa embed này").clicked() {
                            to_remove = Some(idx);
                        }
                    });
                    Self::show_embed_editor(ui, embed, &mut changed, &mut error);
                    ui.separator();
                }
                if let Some(idx) = to_remove {
                    embeds.remove(idx);
                    changed = true;
                }
                if embeds.len() < crate::embed::EMBED_MAX_COUNT
                    && ui.button("+ Thêm embed").clicked()
                {
                    embeds.push(crate::embed::Embed::default());
                    changed = true;
                }
                let total: usize = embeds.iter().map(|e| e.total_chars()).sum();
                theme::muted(
                    ui,
                    &format!(
                        "Tổng tất cả embed: {total}/6000 ký tự (giới hạn của Discord, tính chung).",
                    ),
                );
                if let Err(validation) = crate::embed::validate_embeds(&embeds) {
                    ui.label(RichText::new(validation).color(theme::palette().danger));
                }
            }
            if let Some(e) = &error {
                ui.label(RichText::new(e).color(theme::palette().danger));
            }
        });
        if changed && error.is_none() {
            // Drafts save unconditionally: the composer is an
            // editor, not a gate. An invalid embed set cannot be
            // ENABLED or SENT (validate_inner rejects it), but
            // the draft must persist across frames — otherwise
            // the enable checkbox unchecks itself on the next
            // frame and the composer can never be filled in.
            // (This was a latent Q8 bug: the old fail-closed
            // save made the composer un-enterable.)
            if let Ok(mut list) = self.scenarios.lock() {
                if let Some(s) = list.get_mut(i) {
                    s.embeds = embeds;
                    let _ = self.store.save_scenario(s);
                }
            }
        }
    }

    /// The field editor + preview for ONE embed. Shared by every
    /// embed in the composer; touches no app state.
    fn show_embed_editor(
        ui: &mut egui::Ui,
        e: &mut crate::embed::Embed,
        changed: &mut bool,
        error: &mut Option<String>,
    ) {
        ui.horizontal(|ui| {
            ui.label("Tiêu đề (tối đa 256):");
            if ui.text_edit_singleline(&mut e.title).changed() {
                *changed = true;
            }
        });
        ui.label("Mô tả (tối đa 4096):");
        if ui
            .add(
                egui::TextEdit::multiline(&mut e.description)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            )
            .changed()
        {
            *changed = true;
        }
        ui.horizontal(|ui| {
            ui.label("Tác giả (tối đa 256):");
            if ui.text_edit_singleline(&mut e.author_name).changed() {
                *changed = true;
            }
        });
        ui.horizontal(|ui| {
            ui.label("Link tiêu đề (https, để trống = không link):");
            if ui.text_edit_singleline(&mut e.url).changed() {
                *changed = true;
            }
        });
        ui.horizontal(|ui| {
            ui.label("Ảnh lớn (URL https, để trống = không ảnh):");
            if ui.text_edit_singleline(&mut e.image_url).changed() {
                *changed = true;
            }
        });
        ui.horizontal(|ui| {
            ui.label("Ảnh nhỏ góc phải (URL https):");
            if ui.text_edit_singleline(&mut e.thumbnail_url).changed() {
                *changed = true;
            }
        });
        // Color: hex field + presets.
        let mut hex = e.color.map(|c| format!("{c:06X}")).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label("Màu (hex RRGGBB, để trống = mặc định):");
            if ui.text_edit_singleline(&mut hex).changed() {
                let hex_trim = hex.trim().trim_start_matches('#');
                if hex_trim.is_empty() {
                    e.color = None;
                    *changed = true;
                } else if hex_trim.len() == 6 && u32::from_str_radix(hex_trim, 16).is_ok() {
                    e.color = u32::from_str_radix(hex_trim, 16).ok();
                    *changed = true;
                } else {
                    *error = Some("Màu phải là 6 ký tự hex (vd: 5865F2).".to_string());
                }
            }
            for (label, value) in [
                ("Blurple", 0x5865F2u32),
                ("Xanh", 0x57F287),
                ("Đỏ", 0xED4245),
                ("Vàng", 0xFEE75C),
                ("Tím", 0x9B59B6),
            ] {
                if ui.small_button(label).clicked() {
                    e.color = Some(value);
                    *changed = true;
                }
            }
        });
        ui.label("Fields (tối đa 25, tên 256 / giá trị 1024):");
        let mut to_remove: Option<usize> = None;
        for (idx, field) in e.fields.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut field.name).hint_text("Tên field"));
                ui.add(egui::TextEdit::singleline(&mut field.value).hint_text("Giá trị"));
                ui.checkbox(&mut field.inline, "inline");
                if ui.button("Xóa").clicked() {
                    to_remove = Some(idx);
                }
            });
        }
        if let Some(idx) = to_remove {
            e.fields.remove(idx);
            *changed = true;
        }
        if e.fields.len() < crate::embed::EMBED_MAX_FIELDS && ui.button("+ Thêm field").clicked() {
            e.fields.push(crate::embed::EmbedField::default());
            *changed = true;
        }
        ui.horizontal(|ui| {
            ui.label("Chân trang (tối đa 2048):");
            if ui.text_edit_singleline(&mut e.footer_text).changed() {
                *changed = true;
            }
        });
        theme::muted(ui, &format!("Embed này: {}/6000 ký tự.", e.total_chars()));
        if let Err(validation) = e.validate() {
            ui.label(RichText::new(validation).color(theme::palette().danger));
        }
        // Preview card.
        ui.add_space(4.0);
        theme::card(ui, |ui| {
            if let Some(color) = e.color {
                ui.label(
                    RichText::new(format!("■ Màu #{color:06X}"))
                        .color(theme::palette().text_secondary),
                );
            }
            if !e.author_name.trim().is_empty() {
                theme::muted(ui, &e.author_name);
            }
            if !e.title.trim().is_empty() {
                ui.label(theme::semibold(&e.title, 16.0));
            }
            if !e.url.trim().is_empty() {
                theme::muted(ui, &format!("Link: {}", e.url));
            }
            if !e.description.trim().is_empty() {
                ui.label(&e.description);
            }
            for field in &e.fields {
                ui.horizontal(|ui| {
                    ui.label(theme::semibold(&field.name, 14.0));
                    ui.label(&field.value);
                });
            }
            if !e.footer_text.trim().is_empty() {
                theme::muted(ui, &e.footer_text);
            }
            if !e.thumbnail_url.trim().is_empty() {
                theme::muted(ui, &format!("Ảnh nhỏ: {}", e.thumbnail_url));
            }
            if !e.image_url.trim().is_empty() {
                theme::muted(ui, &format!("Ảnh lớn: {}", e.image_url));
            }
        });
    }

    /// Poll composer (idea #3): one poll per scenario, sent with
    /// every message. Limits are shown live; drafts save as
    /// edited, and validation gates enabling/sending instead of
    /// the editor. Results are NOT readable via webhook — the
    /// note says so.
    fn show_poll_composer(&mut self, ui: &mut egui::Ui, i: usize) {
        let mut poll = self
            .scenarios
            .lock()
            .map(|l| l.get(i).and_then(|s| s.poll.clone()))
            .unwrap_or(None);
        let mut changed = false;
        theme::card(ui, |ui| {
            theme::section_title(ui, t(self.lang(), "builder.poll"));
            let mut enabled = poll.is_some();
            if ui.checkbox(&mut enabled, "Gửi kèm bình chọn").changed() {
                poll = if enabled {
                    Some(crate::poll::Poll::default())
                } else {
                    None
                };
                changed = true;
            }
            if let Some(p) = &mut poll {
                ui.horizontal(|ui| {
                    ui.label("Câu hỏi (tối đa 300):");
                    if ui.text_edit_singleline(&mut p.question).changed() {
                        changed = true;
                    }
                });
                ui.label("Đáp án (2–10, mỗi đáp án tối đa 55 ký tự):");
                let mut to_remove: Option<usize> = None;
                // Length read before the mutable iteration: the
                // remove button must not borrow `p.answers` again.
                let n_answers = p.answers.len();
                for (idx, answer) in p.answers.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}.", idx + 1));
                        if ui.text_edit_singleline(answer).changed() {
                            changed = true;
                        }
                        if n_answers > crate::poll::POLL_MIN_ANSWERS && ui.button("Xóa").clicked()
                        {
                            to_remove = Some(idx);
                        }
                    });
                }
                if let Some(idx) = to_remove {
                    p.answers.remove(idx);
                    changed = true;
                }
                if p.answers.len() < crate::poll::POLL_MAX_ANSWERS
                    && ui.button("+ Thêm đáp án").clicked()
                {
                    p.answers.push(String::new());
                    changed = true;
                }
                ui.horizontal(|ui| {
                    ui.label("Hết hạn sau:");
                    if ui
                        .add(
                            egui::DragValue::new(&mut p.duration_hours)
                                .range(1..=168)
                                .suffix(" giờ"),
                        )
                        .changed()
                    {
                        changed = true;
                    }
                    if ui
                        .checkbox(&mut p.allow_multiselect, "Cho chọn nhiều đáp án")
                        .changed()
                    {
                        changed = true;
                    }
                });
                if let Err(validation) = p.validate() {
                    ui.label(RichText::new(validation).color(theme::palette().danger));
                }
                theme::muted(
                    ui,
                    "Kết quả bình chọn không đọc được qua webhook — xem trực tiếp trên Discord. Bình chọn có thể gửi kèm tin nhắn trống.",
                );
            }
        });
        if changed {
            // Drafts save unconditionally (same reason as the
            // embed composer): an invalid poll cannot be enabled
            // or sent, but the draft must persist across frames.
            if let Ok(mut list) = self.scenarios.lock() {
                if let Some(s) = list.get_mut(i) {
                    s.poll = poll;
                    let _ = self.store.save_scenario(s);
                }
            }
        }
    }

    /// Discord-style preview of the message being composed (QoL Q1):
    /// markdown segments, emoji shortcodes expanded for display, and
    /// a live character counter. Empty input shows nothing.
    fn show_message_preview(&self, ui: &mut egui::Ui) {
        if self.new_message.trim().is_empty() {
            return;
        }
        let raw = self.new_message.trim();
        let count = char_count(raw);
        let too_long = count > crate::preview::DISCORD_CONTENT_LIMIT;
        ui.horizontal(|ui| {
            ui.label("Xem trước:");
            let counter = format!("{count}/2000 ký tự");
            if too_long {
                ui.label(RichText::new(counter).color(theme::palette().danger));
            } else {
                theme::muted(ui, &counter);
            }
        });
        theme::card(ui, |ui| {
            // Expand shortcodes for display only; the sent text keeps
            // exactly what the user typed (see preview.rs).
            let expanded = expand_shortcodes(raw);
            ui.horizontal_wrapped(|ui| {
                for seg in parse_markdown(&expanded) {
                    let mut text = RichText::new(seg.text);
                    if seg.bold {
                        text = text.strong();
                    }
                    if seg.italic {
                        text = text.italics();
                    }
                    if seg.strike {
                        text = text.strikethrough();
                    }
                    if seg.underline {
                        text = text.underline();
                    }
                    if seg.code {
                        text = text.monospace();
                    }
                    ui.label(text);
                }
            });
        });
        theme::muted(
            ui,
            "Xem trước hiển thị emoji từ :tên:; tin gửi đi giữ nguyên văn bản đã gõ.",
        );
    }

    /// Editor panel for the selected `ActionSendCommand` node: kind,
    /// name, prefix/args or slash application/options, plus a Test
    /// button. Reads the node config into a local `CommandConfig`,
    /// edits it, and writes it back when changed (same pattern as
    /// channel_id above).
    fn show_command_editor(&mut self, ui: &mut egui::Ui, i: usize) {
        let Some(sel_id) = self.selected_node.clone() else {
            return;
        };
        let mut cfg_opt: Option<CommandConfig> = None;
        if let Ok(list) = self.scenarios.lock() {
            if let Some(s) = list.get(i) {
                if let Some(n) = s.nodes.iter().find(|n| n.id == sel_id) {
                    if n.kind == NodeKind::ActionSendCommand {
                        cfg_opt = Some(CommandConfig::from_node(n));
                    }
                }
            }
        }
        let Some(mut cfg) = cfg_opt else {
            return;
        };
        ui.separator();
        theme::section_title(ui, "Lệnh Discord");
        let mut is_slash = cfg.is_slash();
        egui::ComboBox::from_label("Loại lệnh")
            .selected_text(if is_slash {
                "Slash (/daily)"
            } else {
                "Lệnh thường (!rank)"
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut is_slash, false, "Lệnh thường (!rank)");
                ui.selectable_value(&mut is_slash, true, "Slash (/daily)");
            });
        cfg.kind = if is_slash {
            "slash".to_string()
        } else {
            "prefix".to_string()
        };
        ui.horizontal(|ui| {
            ui.label("Tên lệnh:");
            ui.add(egui::TextEdit::singleline(&mut cfg.name).hint_text("vd: daily, rank"));
        });
        if is_slash {
            ui.horizontal(|ui| {
                ui.label("Application ID:");
                ui.add(
                    egui::TextEdit::singleline(&mut cfg.application_id)
                        .hint_text("ID của bot (bắt buộc)"),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Command ID:");
                ui.add(
                    egui::TextEdit::singleline(&mut cfg.command_id).hint_text("để trống để tự tìm"),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Version:");
                ui.add(
                    egui::TextEdit::singleline(&mut cfg.version).hint_text("để trống để tự tìm"),
                );
            });
            ui.label("Tham số (options):");
            let mut opt_remove: Option<usize> = None;
            for (oi, opt) in cfg.options.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut opt.name)
                            .hint_text("tên")
                            .desired_width(90.0),
                    );
                    let mut kind = opt.kind;
                    egui::ComboBox::from_id_salt(format!("cmd_opt_kind_{oi}"))
                        .selected_text(slash_option_kind_label(kind))
                        .show_ui(ui, |ui| {
                            for (k, label) in SLASH_OPTION_KINDS {
                                ui.selectable_value(&mut kind, *k, *label);
                            }
                        });
                    opt.kind = kind;
                    ui.add(
                        egui::TextEdit::singleline(&mut opt.value)
                            .hint_text("giá trị")
                            .desired_width(110.0),
                    );
                    if ui.button("✕").clicked() {
                        opt_remove = Some(oi);
                    }
                });
            }
            if let Some(oi) = opt_remove {
                cfg.options.remove(oi);
            }
            if ui.button("+ Thêm tham số").clicked() {
                cfg.options.push(SlashOption::default());
            }
            theme::muted(
                ui,
                "Lệnh slash cần Application ID của bot. Command ID để trống sẽ tự tìm theo tên; nếu không tìm được, hãy dán ID thủ công. Chỉ lệnh đầu tiên trong kịch bản được chạy mỗi lần.",
            );
        } else {
            ui.horizontal(|ui| {
                ui.label("Tiền tố:");
                ui.add(
                    egui::TextEdit::singleline(&mut cfg.prefix)
                        .hint_text("!")
                        .desired_width(40.0),
                );
            });
            ui.label("Tham số (args):");
            let mut arg_remove: Option<usize> = None;
            for (ai, arg) in cfg.args.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(arg).hint_text("tham số"));
                    if ui.button("✕").clicked() {
                        arg_remove = Some(ai);
                    }
                });
            }
            if let Some(ai) = arg_remove {
                cfg.args.remove(ai);
            }
            if ui.button("+ Thêm tham số").clicked() {
                cfg.args.push(String::new());
            }
            ui.label(format!("Xem trước: {}", cfg.compose_prefix()));
        }
        if self.config.send_mode == SendMode::Webhook {
            theme::muted(
                ui,
                "Chế độ Webhook không gửi được lệnh — chuyển sang chế độ Tài khoản để dùng lệnh.",
            );
        }
        ui.horizontal(|ui| {
            let enabled = self.config.send_mode != SendMode::Webhook;
            if ui
                .add_enabled(enabled, egui::Button::new("Test lệnh"))
                .clicked()
            {
                self.test_command(i, &sel_id);
            }
            theme::muted(ui, "Gửi thử lệnh này một lần ngay bây giờ.");
        });
        // Write back when changed (same pattern as channel_id).
        if let Ok(mut list) = self.scenarios.lock() {
            if let Some(s) = list.get_mut(i) {
                if let Some(n) = s.nodes.iter_mut().find(|n| n.id == sel_id) {
                    let new_cfg = serde_json::to_value(&cfg).unwrap_or_default();
                    if n.config != new_cfg {
                        n.config = new_cfg;
                        let _ = self.store.save_scenario(s);
                    }
                }
            }
        }
    }

    /// Pinned action bar under the Builder editor: Validate, node
    /// buttons, Export/Import. Kept outside the editor's ScrollArea
    /// so the key actions are always visible in the 700 px window.
    fn show_builder_actions(&mut self, ui: &mut egui::Ui, i: usize) {
        theme::slim_card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if theme::primary_button(ui, "Validate (dry-run)").clicked() {
                    let result = self
                        .scenarios
                        .lock()
                        .ok()
                        .and_then(|l| l.get(i).map(|s| s.validate()));
                    match result {
                        Some(Ok(())) => {
                            self.set_status("Dry-run OK: scenario is valid, nothing was sent.")
                        }
                        Some(Err(e)) => self.set_status(&format!("Validation failed: {e}")),
                        None => self.set_status("No scenario selected."),
                    }
                }
                // Short labels so Validate + both node buttons fit on one
                // row in the narrow column (no wrap = no extra height).
                if ui.button("+ Send Message").clicked() {
                    if let Ok(mut list) = self.scenarios.lock() {
                        if let Some(s) = list.get_mut(i) {
                            s.nodes.push(Node::new(NodeKind::ActionSendMessage));
                        }
                    }
                    self.save_scenarios();
                }
                if ui.button("+ Send Command").clicked() {
                    if let Ok(mut list) = self.scenarios.lock() {
                        if let Some(s) = list.get_mut(i) {
                            s.nodes.push(Node::new(NodeKind::ActionSendCommand));
                        }
                    }
                    self.save_scenarios();
                }
                if ui.button("+ Wait").clicked() {
                    if let Ok(mut list) = self.scenarios.lock() {
                        if let Some(s) = list.get_mut(i) {
                            s.nodes.push(Node::new(NodeKind::UtilityWait));
                        }
                    }
                    self.save_scenarios();
                }
            });
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.import_path)
                    .hint_text("Path to a scenario .json to import"),
            );
            ui.horizontal(|ui| {
                if ui.button("Export scenario JSON").clicked() {
                    let path = config_dir().join(format!(
                        "nicord-export-{}.json",
                        self.scenarios
                            .lock()
                            .ok()
                            .and_then(|l| l.get(i).map(|s| s.id.clone()))
                            .unwrap_or_default()
                    ));
                    let result = self.scenarios.lock().ok().and_then(|l| {
                        l.get(i).map(|s| {
                            serde_json::to_string_pretty(s)
                                .map_err(|e| e.to_string())
                                .and_then(|j| std::fs::write(&path, j).map_err(|e| e.to_string()))
                        })
                    });
                    match result {
                        Some(Ok(())) => {
                            restrict_permissions(&path);
                            self.set_status(&format!("Exported to {}", path.display()))
                        }
                        Some(Err(e)) => self.set_status(&format!("Export failed: {e}")),
                        None => self.set_status("No scenario selected."),
                    }
                }
                if ui.button("Import scenario").clicked() {
                    let path = self.import_path.trim().to_string();
                    match crate::backup::read_import_file(std::path::Path::new(&path)) {
                        Ok(text) => {
                            // QoL Q3: analyse first (warnings + validation),
                            // import only when the file is usable.
                            let is_webhook = self.config.send_mode == SendMode::Webhook;
                            let (scenario, warnings) = analyze_import(&text, is_webhook);
                            self.import_warnings = warnings;
                            let Some(mut imported) = scenario else {
                                self.set_status("Import blocked — see warnings in the editor.");
                                return;
                            };
                            imported.id = uuid::Uuid::new_v4().to_string();
                            imported.enabled = false;
                            let mut ok = true;
                            if !is_webhook {
                                if let Ok(list) = self.scenarios.lock() {
                                    let mut hypo = list.clone();
                                    // Check the violation as if enabled: a disabled
                                    // import never counts, so check the enabled form.
                                    let mut as_enabled = imported.clone();
                                    as_enabled.enabled = true;
                                    hypo.push(as_enabled);
                                    if let Some(v) = channels_violation(&hypo) {
                                        self.set_status(&format!("Import blocked: {v}"));
                                        ok = false;
                                    }
                                }
                            }
                            if ok {
                                if let Ok(mut list) = self.scenarios.lock() {
                                    list.push(imported);
                                }
                                self.save_scenarios();
                                let n = self.import_warnings.len();
                                if n == 0 {
                                    self.set_status(
                                        "Scenario imported (disabled — enable it to run).",
                                    );
                                } else {
                                    self.set_status(&format!(
                                        "Scenario imported with {n} warning(s) — see editor (disabled — enable it to run)."
                                    ));
                                }
                            }
                        }
                        Err(e) => self.set_status(&format!("Cannot read import file: {e}")),
                    }
                }
            });
        });
    }
    /// Send one real test message to a destination (Destinations
    /// "Gửi thử"). Runs through the normal send pipeline, so caps,
    /// quiet window, and minimum interval all apply — there is no
    /// bypass. The message is clearly labelled as a test.
    fn test_destination(&self, dest_id: &str) {
        let Ok(Some(dest)) = self.store.load_destination(dest_id) else {
            self.set_status("Destination not found.");
            return;
        };
        let Some(sender) = sender_for_destination(&dest) else {
            self.set_status("Destination has no usable webhook URL yet.");
            return;
        };
        let mut scenario = Scenario::new("Nicord test");
        scenario.messages = vec![format!(
            "Tin thử từ Nicord — đích đến '{}'. Bạn có thể xóa tin này trong Discord.",
            dest.display_name()
        )];
        scenario.destination_id = Some(dest.id.clone());
        let engine = self.engine.clone();
        let status = self.status.clone();
        let username = dest.effective_username();
        std::thread::spawn(move || {
            let rt = match shared_runtime() {
                Ok(rt) => rt,
                Err(e) => {
                    if let Ok(mut s) = status.lock() {
                        *s = format!("cannot start runtime: {e}");
                    }
                    return;
                }
            };
            let result =
                rt.block_on(engine.send_once(&scenario, &sender, Some(&dest), &username, ""));
            if result.is_ok() {
                crate::notify::play(crate::notify::SoundEvent::SendSuccess);
            } else {
                crate::notify::play(crate::notify::SoundEvent::SendFailure);
            }
            if let Ok(mut s) = status.lock() {
                *s = match result {
                    Ok(id) if id.is_empty() => "Test sent! (webhook, no id returned)".to_string(),
                    Ok(id) => format!("Test sent! message id {id}"),
                    Err(e) => format!("Test failed: {e}"),
                };
            }
        });
    }

    /// Save the destination draft (validates first). A pasted URL in
    /// `dest_url_input` replaces the stored URL (parsed + encrypted);
    /// an empty input keeps the existing URL. A destination without a
    /// URL is saved disabled. Returns true on success.
    fn save_dest_draft(&mut self) -> bool {
        let Some(mut draft) = self.dest_draft.clone() else {
            return false;
        };
        let url_input = self.dest_url_input.trim().to_string();
        if !url_input.is_empty() {
            match parse_webhook_url(&url_input) {
                Ok(parsed) => match store_secret(&destination_entry(&draft.id), &url_input) {
                    Ok(enc) => {
                        draft.encrypted_url = enc;
                        draft.webhook_id = parsed.id.clone();
                        // Fill display metadata from the last check
                        // when it matches this webhook.
                        if let Ok(info) = self.webhook_info.lock() {
                            if let Some(w) = info.as_ref() {
                                if w.id == parsed.id {
                                    draft.webhook_name = w.display_name();
                                    draft.channel_id = w.channel_id.clone().unwrap_or_default();
                                    draft.guild_id = w.guild_id.clone().unwrap_or_default();
                                }
                            }
                        }
                        // A new URL clears any old pause.
                        draft.failure_state = crate::destination::FailureState::Ok;
                        draft.last_error.clear();
                        draft.paused_at = None;
                    }
                    Err(e) => {
                        self.set_status(&format!("Cannot encrypt webhook URL: {e}"));
                        return false;
                    }
                },
                Err(e) => {
                    self.set_status(&e);
                    return false;
                }
            }
        }
        if draft.encrypted_url.trim().is_empty() {
            draft.enabled = false;
        }
        if let Err(e) = draft.validate() {
            self.set_status(&format!("Cannot save destination: {e}"));
            return false;
        }
        draft.updated_at = Some(chrono::Utc::now().to_rfc3339());
        match self.store.save_destination(&draft) {
            Ok(()) => {
                self.dest_selected_id = Some(draft.id.clone());
                self.dest_draft = Some(draft);
                self.dest_url_input.clear();
                self.set_status("Destination saved.");
                true
            }
            Err(e) => {
                self.set_status(&format!("Cannot save destination: {e}"));
                false
            }
        }
    }

    /// Destinations screen (webhook mode only). Left: list + actions.
    /// Right: editor for the selected destination (or a new one).
    /// In user-token mode this screen explains that destinations
    /// only exist in webhook mode.
    fn show_destinations(&mut self, ui: &mut egui::Ui) {
        if self.config.send_mode != SendMode::Webhook {
            theme::card(ui, |ui| {
                ui.label(
                    "Đích đến chỉ tồn tại ở chế độ Webhook. Chế độ Tài khoản giữ nguyên quy tắc 1 kênh.",
                );
                theme::muted(
                    ui,
                    "Đổi sang chế độ Webhook trong Cài đặt để dùng nhiều kênh.",
                );
            });
            return;
        }
        let dests = self.store.load_destinations().unwrap_or_default();
        let max_allowed = self
            .config
            .max_destinations
            .min(crate::destination::MAX_DESTINATIONS_HARD as u32)
            .max(1);
        let at_limit = dests.len() as u32 >= max_allowed;
        ui.horizontal(|ui| {
            ui.label(format!("{} / {} đích đến", dests.len(), max_allowed));
            if at_limit {
                theme::muted(
                    ui,
                    "Đã đạt giới hạn — tăng giới hạn trong Cài đặt (tối đa 20).",
                );
            }
        });
        ui.add_space(8.0);
        ui.columns(2, |cols| {
            // Left: list.
            cols[0].vertical(|ui| {
                theme::card(ui, |ui| {
                    theme::section_title(ui, t(self.lang(), "dest.list"));
                    if ui
                        .add_enabled(!at_limit, egui::Button::new("+ Thêm đích đến"))
                        .clicked()
                    {
                        self.dest_selected_id = None;
                        self.dest_draft = Some(Destination::new("Đích đến mới"));
                        self.dest_url_input.clear();
                        self.dest_new_message.clear();
                    }
                    ui.add_space(6.0);
                    if dests.is_empty() {
                        theme::muted(ui, "Chưa có đích đến nào — thêm webhook đầu tiên của bạn.");
                    }
                    for dest in &dests {
                        let selected = self.dest_selected_id.as_deref() == Some(dest.id.as_str());
                        let state_text = if dest.encrypted_url.trim().is_empty() {
                            "chưa có URL"
                        } else if !dest.enabled {
                            "đã tắt"
                        } else {
                            dest.failure_state.label()
                        };
                        ui.horizontal(|ui| {
                            if ui
                                .selectable_label(
                                    selected,
                                    format!("{} — {state_text}", dest.display_name()),
                                )
                                .clicked()
                            {
                                self.dest_selected_id = Some(dest.id.clone());
                                self.dest_draft = Some(dest.clone());
                                self.dest_url_input.clear();
                                self.dest_new_message.clear();
                                self.dest_delete_confirm = None;
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("Gửi thử").clicked() {
                                        self.test_destination(&dest.id);
                                    }
                                    let toggle = if dest.enabled { "Tắt" } else { "Bật" };
                                    if ui.button(toggle).clicked() {
                                        let mut updated = dest.clone();
                                        updated.enabled = !dest.enabled;
                                        if updated.enabled
                                            && updated.encrypted_url.trim().is_empty()
                                        {
                                            self.set_status(
                                                "Cannot enable: destination has no webhook URL yet.",
                                            );
                                        } else if let Err(e) = updated.validate() {
                                            self.set_status(&format!("Cannot enable: {e}"));
                                        } else {
                                            let _ = self.store.save_destination(&updated);
                                            if self.dest_selected_id.as_deref()
                                                == Some(dest.id.as_str())
                                            {
                                                self.dest_draft = Some(updated);
                                            }
                                        }
                                    }
                                },
                            );
                        });
                        if !dest.channel_id.is_empty() {
                            theme::muted(ui, &format!("kênh: {}", dest.channel_id));
                        }
                        ui.separator();
                    }
                });
            });
            // Right: editor.
            cols[1].vertical(|ui| {
                self.show_destination_editor(ui, &dests);
            });
        });
    }

    /// Editor panel for the selected destination draft.
    fn show_destination_editor(&mut self, ui: &mut egui::Ui, dests: &[Destination]) {
        let Some(mut draft) = self.dest_draft.clone() else {
            theme::card(ui, |ui| {
                theme::muted(ui, "Chọn một đích đến để sửa, hoặc thêm mới.");
            });
            return;
        };
        let is_new = self.dest_selected_id.is_none();
        theme::card(ui, |ui| {
            theme::section_title(
                ui,
                if is_new {
                    t(self.lang(), "dest.create")
                } else {
                    t(self.lang(), "dest.edit")
                },
            );
            ui.horizontal(|ui| {
                ui.label("Tên thân thiện:");
                ui.text_edit_singleline(&mut draft.name);
            });
            // URL: never prefilled, never shown in full.
            ui.horizontal(|ui| {
                ui.label("Webhook URL:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.dest_url_input)
                        .password(true)
                        .hint_text(if draft.encrypted_url.is_empty() {
                            "paste webhook URL here"
                        } else {
                            "để trống để giữ URL đã lưu, dán URL mới để đổi"
                        }),
                );
            });
            if !draft.encrypted_url.is_empty() {
                theme::muted(ui, "URL đã lưu: có (không hiển thị).");
            }
            if !draft.webhook_id.is_empty() {
                theme::muted(
                    ui,
                    &format!(
                        "Webhook: {} (ID {}), kênh {}",
                        if draft.webhook_name.is_empty() {
                            "?"
                        } else {
                            &draft.webhook_name
                        },
                        draft.webhook_id,
                        if draft.channel_id.is_empty() {
                            "?"
                        } else {
                            &draft.channel_id
                        }
                    ),
                );
            }
            ui.horizontal(|ui| {
                if theme::ghost_button(ui, "Kiểm tra URL").clicked() {
                    let url = self.dest_url_input.clone();
                    if url.trim().is_empty() {
                        self.set_status("Paste a webhook URL first.");
                    } else {
                        self.check_webhook(url);
                    }
                }
                let check_msg = self
                    .webhook_check_status
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or_default();
                if !check_msg.is_empty() {
                    ui.label(check_msg);
                }
            });
            theme::muted(ui, "Tạo webhook trong Discord: Cài đặt kênh → Tích hợp → Webhook. URL là mật khẩu — đừng chia sẻ.");
            ui.separator();
            ui.checkbox(&mut draft.enabled, "Bật đích đến này");
            ui.horizontal(|ui| {
                ui.label("Tên hiển thị khi gửi (để trống = tên webhook):");
                ui.text_edit_singleline(&mut draft.username_override);
                crate::destination::truncate_username_override(&mut draft.username_override);
            });
            ui.horizontal(|ui| {
                ui.label("Avatar URL (https, để trống = avatar webhook):");
                ui.text_edit_singleline(&mut draft.avatar_url_override);
            });
            ui.horizontal(|ui| {
                ui.label("Thread ID (chỉ số, để trống = gửi vào kênh):");
                ui.text_edit_singleline(&mut draft.thread_id);
            });
            theme::muted(ui, "Thread phải thuộc kênh của webhook (Discord sẽ tự mở lại thread đã lưu trữ). Hành vi với thread bị khóa chưa được kiểm chứng — hãy thử trên thread thật trước khi dựa vào nó.");
            ui.horizontal(|ui| {
                ui.label("Giới hạn tin/ngày (0 = dùng giới hạn toàn cục, tối đa 300):");
                ui.add(egui::DragValue::new(&mut draft.daily_cap).range(0..=300));
            });
            ui.horizontal(|ui| {
                ui.label("Khoảng cách tối thiểu giữa 2 lần gửi (giây, tối thiểu 5):");
                ui.add(egui::DragValue::new(&mut draft.min_interval_secs).range(5..=3600));
            });
            // Quiet window.
            let mut quiet_enabled = draft.quiet_start.is_some();
            ui.checkbox(
                &mut quiet_enabled,
                "Cửa sổ im lặng (không gửi trong khoảng này)",
            );
            if quiet_enabled {
                let mut start = draft
                    .quiet_start
                    .clone()
                    .unwrap_or_else(|| "22:00".to_string());
                let mut end = draft
                    .quiet_end
                    .clone()
                    .unwrap_or_else(|| "06:00".to_string());
                ui.horizontal(|ui| {
                    ui.label("Từ (HH:MM):");
                    ui.text_edit_singleline(&mut start);
                    ui.label("đến (HH:MM):");
                    ui.text_edit_singleline(&mut end);
                });
                theme::muted(ui, "Hỗ trợ qua đêm (vd: 22:00 → 06:00). Giờ địa phương.");
                draft.quiet_start = Some(start);
                draft.quiet_end = Some(end);
            } else {
                draft.quiet_start = None;
                draft.quiet_end = None;
            }
            ui.separator();
            ui.label("Bộ tin mặc định (dùng khi kịch bản không có tin riêng):");
            let mut to_remove: Option<usize> = None;
            for (idx, msg) in draft.default_messages.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("• {msg}"));
                    if ui.button("Xóa").clicked() {
                        to_remove = Some(idx);
                    }
                });
            }
            if let Some(idx) = to_remove {
                draft.default_messages.remove(idx);
            }
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.dest_new_message)
                        .hint_text("Add a default message..."),
                );
                let too_long = over_limit(self.dest_new_message.trim());
                if ui
                    .add_enabled(
                        !self.dest_new_message.trim().is_empty() && !too_long,
                        egui::Button::new("+ Thêm"),
                    )
                    .clicked()
                {
                    draft
                        .default_messages
                        .push(self.dest_new_message.trim().to_string());
                    self.dest_new_message.clear();
                }
            });
            // Failure state + resume.
            if draft.failure_state.is_paused() {
                ui.separator();
                ui.label(
                    RichText::new(format!("Đang tạm dừng: {}", draft.failure_state.label()))
                        .color(theme::palette().danger),
                );
                if !draft.last_error.is_empty() {
                    theme::muted(ui, &format!("Lỗi: {}", draft.last_error));
                }
                if ui.button("Tiếp tục (thử lại)").clicked() {
                    let _ = self.store.set_destination_failure(
                        &draft.id,
                        crate::destination::FailureState::Ok,
                        "",
                    );
                    draft.failure_state = crate::destination::FailureState::Ok;
                    draft.last_error.clear();
                    draft.paused_at = None;
                    self.set_status("Destination resumed.");
                }
                theme::muted(ui, "Nếu webhook đã bị xóa trong Discord, hãy dán URL mới và kiểm tra lại thay vì chỉ tiếp tục.");
            }
            ui.separator();
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Lưu đích đến").clicked() {
                    self.dest_draft = Some(draft.clone());
                    self.save_dest_draft();
                    // Reload the draft from the saved state.
                    if let Some(id) = &self.dest_selected_id {
                        if let Ok(Some(saved)) = self.store.load_destination(id) {
                            self.dest_draft = Some(saved);
                        }
                    }
                }
                if !is_new {
                    if ui.button("Nhân bản").clicked() {
                        // Clone (Q5): settings copied, URL starts empty,
                        // disabled until a URL is pasted + checked.
                        let mut copy = draft.clone();
                        copy.id = uuid::Uuid::new_v4().to_string();
                        copy.name = format!("{} (bản sao)", copy.name);
                        copy.encrypted_url.clear();
                        copy.webhook_id.clear();
                        copy.webhook_name.clear();
                        copy.channel_id.clear();
                        copy.guild_id.clear();
                        copy.enabled = false;
                        copy.failure_state = crate::destination::FailureState::Ok;
                        copy.last_error.clear();
                        copy.paused_at = None;
                        let now = chrono::Utc::now().to_rfc3339();
                        copy.created_at = Some(now.clone());
                        copy.updated_at = Some(now);
                        if self.store.save_destination(&copy).is_ok() {
                            self.dest_selected_id = Some(copy.id.clone());
                            self.dest_draft = Some(copy);
                            self.dest_url_input.clear();
                            self.set_status(
                                "Destination cloned (no URL — paste one and check it).",
                            );
                        }
                    }
                    if theme::danger_button(ui, "Xóa").clicked() {
                        self.dest_delete_confirm = Some(draft.id.clone());
                        self.dest_delete_target = String::new();
                    }
                }
            });
            // Write the draft back (editor edits are local until Save).
            self.dest_draft = Some(draft);
        });
        // Delete confirmation + scenario reassignment.
        if let Some(confirm_id) = self.dest_delete_confirm.clone() {
            if let Ok(Some(target)) = self.store.load_destination(&confirm_id) {
                let users: Vec<String> = self
                    .scenarios
                    .lock()
                    .map(|list| {
                        list.iter()
                            .filter(|s| s.destination_id.as_deref() == Some(confirm_id.as_str()))
                            .map(|s| s.name.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                ui.add_space(8.0);
                theme::card(ui, |ui| {
                    theme::section_title(ui, "Xóa đích đến?");
                    ui.label(format!(
                        "Đích đến '{}' đang được {} kịch bản dùng.",
                        target.display_name(),
                        users.len()
                    ));
                    for name in &users {
                        theme::muted(ui, &format!("• {name}"));
                    }
                    if users.is_empty() {
                        theme::muted(ui, "Không có kịch bản nào dùng đích đến này.");
                    } else {
                        ui.label("Chuyển các kịch bản này sang:");
                        let others: Vec<Destination> = dests
                            .iter()
                            .filter(|d| d.id != confirm_id)
                            .cloned()
                            .collect();
                        let target_label = others
                            .iter()
                            .find(|d| d.id == self.dest_delete_target)
                            .map(|d| d.display_name())
                            .unwrap_or_else(|| "Tắt các kịch bản này".to_string());
                        egui::ComboBox::from_id_salt("dest_delete_target")
                            .selected_text(target_label)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.dest_delete_target,
                                    String::new(),
                                    "Tắt các kịch bản này",
                                );
                                for d in &others {
                                    ui.selectable_value(
                                        &mut self.dest_delete_target,
                                        d.id.clone(),
                                        d.display_name(),
                                    );
                                }
                            });
                    }
                    ui.horizontal(|ui| {
                        if theme::danger_button(ui, "Xóa thật").clicked() {
                            // Reassign or disable the scenarios first.
                            if let Ok(mut list) = self.scenarios.lock() {
                                for s in list.iter_mut() {
                                    if s.destination_id.as_deref() == Some(confirm_id.as_str()) {
                                        if self.dest_delete_target.is_empty() {
                                            s.enabled = false;
                                            s.destination_id = None;
                                        } else {
                                            s.destination_id =
                                                Some(self.dest_delete_target.clone());
                                        }
                                    }
                                }
                            }
                            self.save_scenarios();
                            // Remove the destination's keyring entry
                            // too, so deleting a destination does not
                            // orphan a live credential.
                            if let Ok(Some(d)) = self.store.load_destination(&confirm_id) {
                                delete_secret(&d.encrypted_url);
                            }
                            let _ = self.store.delete_destination(&confirm_id);
                            self.dest_delete_confirm = None;
                            self.dest_selected_id = None;
                            self.dest_draft = None;
                            self.set_status("Destination deleted.");
                        }
                        if ui.button("Hủy").clicked() {
                            self.dest_delete_confirm = None;
                        }
                    });
                });
            }
        }
    }

    fn show_logs(&mut self, ui: &mut egui::Ui) {
        ui.label("Send history (token is never logged).");
        ui.add_space(4.0);
        // Scenario names for the filter dropdown (read once per frame).
        let scenario_options: Vec<(String, String)> = self
            .scenarios
            .lock()
            .map(|list| {
                list.iter()
                    .map(|s| (s.id.clone(), s.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        // Destinations, loaded ONCE per frame for both the filter
        // dropdown and the manual-delete check below (two loads
        // per frame before; each parses every destination row).
        let log_dests = if self.config.send_mode == SendMode::Webhook {
            self.store.load_destinations().unwrap_or_default()
        } else {
            Vec::new()
        };
        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Filter:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.log_filter)
                        .hint_text("keyword (name, status, detail)"),
                );
                // Status filter (QoL Q4).
                let status_label = if self.log_status_filter.is_empty() {
                    "All statuses"
                } else {
                    self.log_status_filter.as_str()
                };
                egui::ComboBox::from_id_salt("log_status_filter")
                    .selected_text(status_label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.log_status_filter,
                            String::new(),
                            "All statuses",
                        );
                        ui.selectable_value(
                            &mut self.log_status_filter,
                            "sent".to_string(),
                            "sent",
                        );
                        ui.selectable_value(
                            &mut self.log_status_filter,
                            "failed".to_string(),
                            "failed",
                        );
                        ui.selectable_value(
                            &mut self.log_status_filter,
                            "deleted".to_string(),
                            "deleted",
                        );
                    });
                // Scenario filter (QoL Q4).
                let scenario_label = scenario_options
                    .iter()
                    .find(|(id, _)| *id == self.log_scenario_filter)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| "All scenarios".to_string());
                egui::ComboBox::from_id_salt("log_scenario_filter")
                    .selected_text(scenario_label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.log_scenario_filter,
                            String::new(),
                            "All scenarios",
                        );
                        for (id, name) in &scenario_options {
                            ui.selectable_value(&mut self.log_scenario_filter, id.clone(), name);
                        }
                    });
                // Destination filter (webhook mode: by channel id).
                // Two webhooks in one channel share a filter entry;
                // the scenario filter narrows further when needed.
                if self.config.send_mode == SendMode::Webhook {
                    let dest_label = log_dests
                        .iter()
                        .find(|d| d.channel_id == self.log_channel_filter)
                        .map(|d| d.display_name())
                        .unwrap_or_else(|| "All destinations".to_string());
                    egui::ComboBox::from_id_salt("log_channel_filter")
                        .selected_text(dest_label)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.log_channel_filter,
                                String::new(),
                                "All destinations",
                            );
                            for d in &log_dests {
                                if !d.channel_id.is_empty() {
                                    ui.selectable_value(
                                        &mut self.log_channel_filter,
                                        d.channel_id.clone(),
                                        d.display_name(),
                                    );
                                }
                            }
                        });
                }
                if ui.button("Export logs (CSV)").clicked() {
                    let path = config_dir().join("nicord-logs.csv");
                    let status = if self.log_status_filter.is_empty() {
                        None
                    } else {
                        Some(self.log_status_filter.clone())
                    };
                    let scenario = if self.log_scenario_filter.is_empty() {
                        None
                    } else {
                        Some(self.log_scenario_filter.clone())
                    };
                    let channel = if self.log_channel_filter.is_empty() {
                        None
                    } else {
                        Some(self.log_channel_filter.clone())
                    };
                    let result = self
                        .store
                        .query_logs(
                            1000,
                            &self.log_filter.clone(),
                            status.as_deref(),
                            scenario.as_deref(),
                            channel.as_deref(),
                        )
                        .map(|rows| {
                            let mut csv = String::from("time,scenario,status,detail,message_id\n");
                            for row in rows {
                                // Proper CSV quoting + secret scrubbing
                                // (QoL Q4): an export can never leak a
                                // webhook URL, even if one ever reached
                                // a log detail. Message ids are not
                                // secrets and help match rows to Discord.
                                csv.push_str(&format!(
                                    "{},{},{},{},{}\n",
                                    csv_escape(&row.created_at),
                                    csv_escape(&scrub_secrets(&row.scenario_name)),
                                    csv_escape(&row.status),
                                    csv_escape(&scrub_secrets(&row.detail)),
                                    csv_escape(&row.message_id)
                                ));
                            }
                            std::fs::write(&path, csv)
                        });
                    match result {
                        Ok(Ok(())) => {
                            restrict_permissions(&path);
                            self.set_status(&format!("Logs exported to {}", path.display()))
                        }
                        Ok(Err(e)) => self.set_status(&format!("Export failed: {e}")),
                        Err(e) => self.set_status(&format!("Cannot load logs: {e}")),
                    }
                }
                if ui.button("Refresh").clicked() {
                    ctx_request_repaint(ui);
                }
            });
        });
        ui.add_space(12.0);
        let log_status = if self.log_status_filter.is_empty() {
            None
        } else {
            Some(self.log_status_filter.clone())
        };
        let log_scenario = if self.log_scenario_filter.is_empty() {
            None
        } else {
            Some(self.log_scenario_filter.clone())
        };
        let log_channel = if self.log_channel_filter.is_empty() {
            None
        } else {
            Some(self.log_channel_filter.clone())
        };
        match self.store.query_logs(
            100,
            &self.log_filter.clone(),
            log_status.as_deref(),
            log_scenario.as_deref(),
            log_channel.as_deref(),
        ) {
            Ok(rows) => {
                // Detail view (Group 3): the full detail text is
                // selectable (copyable), plus the metadata the rows
                // do not show (message id, destination id, channel).
                // Scrubbed on display like the rows, for the same
                // reason: rows written before the scheduler scrub
                // boundary may hold a leaked URL.
                if let Some(selected) = self.selected_log {
                    if let Some(row) = rows.iter().find(|r| r.id == selected) {
                        // Labels resolved before the card: the card
                        // closure mutates `self.selected_log`.
                        let detail_title = t(self.lang(), "logs.detail_title");
                        let close_label = t(self.lang(), "logs.close");
                        theme::card(ui, |ui| {
                            ui.horizontal(|ui| {
                                theme::section_title(ui, detail_title);
                                if theme::ghost_button(ui, close_label).clicked() {
                                    self.selected_log = None;
                                }
                            });
                            ui.label(format!("Thời gian: {}", row.created_at));
                            ui.label(format!("Kịch bản: {}", row.scenario_name));
                            ui.label(format!("Trạng thái: {}", row.status));
                            if !row.channel_id.is_empty() {
                                ui.label(format!("Kênh: {}", row.channel_id));
                            }
                            if !row.message_id.is_empty() {
                                ui.label(format!("Message ID: {}", row.message_id));
                            }
                            if !row.destination_id.is_empty() {
                                ui.label(format!("Đích đến: {}", row.destination_id));
                            }
                            ui.add(egui::Label::new(scrub_secrets(&row.detail)).selectable(true));
                        });
                        ui.add_space(12.0);
                    }
                }
                if rows.is_empty() {
                    theme::card(ui, |ui| {
                        theme::muted(ui, "No logs yet — send history will appear here.");
                    });
                } else {
                    // Destinations for manual delete (Q13): a row is
                    // deletable only when its destination still exists
                    // with a URL, so the delete goes to the exact
                    // webhook that sent it.
                    let deletable: std::collections::HashSet<String> = log_dests
                        .iter()
                        .filter(|d| !d.encrypted_url.trim().is_empty())
                        .map(|d| d.id.clone())
                        .collect();
                    theme::card(ui, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for row in rows {
                                let status_color = if row.status == "sent" {
                                    theme::palette().success
                                } else if row.status.contains("fail")
                                    || row.status.contains("error")
                                {
                                    theme::palette().danger
                                } else {
                                    theme::palette().text_secondary
                                };
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(&row.created_at)
                                            .monospace()
                                            .color(theme::palette().text_muted),
                                    );
                                    ui.label(theme::semibold(&row.scenario_name, 15.0));
                                    ui.label(
                                        RichText::new(&row.status).color(status_color),
                                    );
                                    if theme::ghost_button(ui, t(self.lang(), "logs.detail_btn"))
                                        .clicked()
                                    {
                                        self.selected_log = Some(row.id);
                                    }
                                    // Manual delete (Q13): webhook mode
                                    // only, one message at a time, with
                                    // confirmation. Never automatic.
                                    let can_delete = row.status == "sent"
                                        && !row.message_id.is_empty()
                                        && deletable.contains(&row.destination_id);
                                    if can_delete {
                                        if self.pending_delete_log == Some(row.id) {
                                            ui.label(
                                                RichText::new("Xóa tin này khỏi Discord? Không hoàn tác được.")
                                                    .color(theme::palette().danger),
                                            );
                                            if theme::danger_button(ui, "Xóa").clicked() {
                                                self.pending_delete_log = None;
                                                self.delete_log_message(row.clone());
                                            }
                                            if theme::ghost_button(ui, "Hủy").clicked() {
                                                self.pending_delete_log = None;
                                            }
                                        } else if theme::ghost_button(ui, "Xóa tin").clicked()
                                        {
                                            self.pending_delete_log = Some(row.id);
                                        }
                                    }
                                });
                                // Scrub on display: rows written before
                                // the scheduler scrub boundary may still
                                // hold a leaked URL.
                                theme::muted(ui, &scrub_secrets(&row.detail));
                                ui.separator();
                            }
                        });
                    });
                }
            }
            Err(e) => {
                ui.label(format!("Cannot load logs: {e}"));
            }
        }
    }

    /// Delete one sent Discord message (Q13), after the Logs UI
    /// confirmation. Runs on a background thread: resolve the exact
    /// destination that sent it, delete via its webhook, then write
    /// an audit row (status "deleted") so the deletion itself is in
    /// the history. A 404 means the message was already gone and is
    /// reported honestly, with an audit row saying so. Nothing
    /// automatic ever calls this: no engine, schedule, or bulk path.
    fn delete_log_message(&mut self, row: crate::store::LogRow) {
        let store = self.store.clone();
        let status = self.status.clone();
        std::thread::spawn(move || {
            let fail = |msg: String| {
                if let Ok(mut s) = status.lock() {
                    *s = msg;
                }
            };
            let dest = match store.load_destination(&row.destination_id) {
                Ok(Some(d)) => d,
                _ => {
                    fail("Cannot delete: the destination no longer exists.".to_string());
                    return;
                }
            };
            let url = match load_secret(&dest.encrypted_url)
                .ok()
                .and_then(|u| crate::webhook::parse_webhook_url(&u).ok())
            {
                Some(u) => u,
                None => {
                    fail("Cannot delete: the destination URL is unreadable.".to_string());
                    return;
                }
            };
            let Some(client) = crate::webhook::WebhookClient::new(url, "") else {
                fail("Cannot delete: the HTTP client could not be built.".to_string());
                return;
            };
            let client = client.with_thread_id(&dest.thread_id);
            let rt = match shared_runtime() {
                Ok(rt) => rt,
                Err(e) => {
                    fail(format!("cannot start runtime: {e}"));
                    return;
                }
            };
            let message_id = row.message_id.clone();
            let result = rt.block_on(client.delete_message(&message_id));
            let audit = |detail: String| {
                let _ = store.log_send(&crate::scheduler::SendLogEntry {
                    scenario_id: row.scenario_id.clone(),
                    scenario_name: row.scenario_name.clone(),
                    channel_id: row.channel_id.clone(),
                    status: "deleted".into(),
                    detail,
                    message_id: message_id.clone(),
                    destination_id: row.destination_id.clone(),
                });
            };
            match result {
                Ok(()) => {
                    audit(format!(
                        "User deleted Discord message {message_id} (log #{})",
                        row.id
                    ));
                    fail(format!("Deleted message {message_id} from Discord."));
                }
                Err(crate::discord::DiscordError::NotFound(_)) => {
                    audit(format!(
                        "Delete requested for message {message_id} (log #{}): already gone (404)",
                        row.id
                    ));
                    fail(format!(
                        "Message {message_id} was already deleted on Discord."
                    ));
                }
                Err(e) => {
                    fail(format!("Delete failed: {e}"));
                }
            }
        });
    }

    /// Backup card (Settings): export/import the whole config for
    /// another machine. Export writes a versioned JSON file; import
    /// REPLACES the current config (after an automatic local backup)
    /// and requires the engine to be stopped + explicit confirmation.
    fn show_backup_card(&mut self, ui: &mut egui::Ui) {
        theme::card(ui, |ui| {
            theme::section_title(ui, "Sao lưu & khôi phục cấu hình");
            theme::muted(ui, "Xuất toàn bộ cấu hình (cài đặt, đích đến, kịch bản, mẫu) để dùng trên máy khác. Lịch sử gửi không được xuất. Copy trực tiếp file cấu hình sang máy khác sẽ KHÔNG dùng được secret, vì secret mã hóa theo máy — hãy dùng xuất portable dưới đây.");
            ui.separator();
            // ---- Export ----
            ui.label("Xuất cấu hình:");
            ui.horizontal(|ui| {
                ui.radio_value(
                    &mut self.backup_export_mode_portable,
                    true,
                    "Portable (có secret, bảo vệ bằng mật khẩu)",
                );
                ui.radio_value(
                    &mut self.backup_export_mode_portable,
                    false,
                    "An toàn (không secret, nhập lại trên máy mới)",
                );
            });
            if self.backup_export_mode_portable {
                ui.horizontal(|ui| {
                    ui.label("Mật khẩu bảo vệ file xuất:");
                    ui.add(egui::TextEdit::singleline(&mut self.backup_passphrase).password(true));
                });
                ui.horizontal(|ui| {
                    ui.label("Nhập lại mật khẩu:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.backup_passphrase_confirm)
                            .password(true),
                    );
                });
                theme::muted(ui, "Mật khẩu càng mạnh càng tốt — file xuất là một két chứa secret. Quên mật khẩu = không mở được file.");
            }
            ui.horizontal(|ui| {
                ui.label("Đường dẫn file xuất:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.backup_export_path)
                        .hint_text("để trống = thư mục cấu hình, tên tự động"),
                );
            });
            if theme::primary_button(ui, "Xuất cấu hình").clicked() {
                let mode = if self.backup_export_mode_portable {
                    crate::backup::ExportMode::Portable
                } else {
                    crate::backup::ExportMode::Safe
                };
                if mode == crate::backup::ExportMode::Portable
                    && self.backup_passphrase != self.backup_passphrase_confirm
                {
                    self.set_status("Passphrases do not match.");
                } else if mode == crate::backup::ExportMode::Portable
                    && self.backup_passphrase.len() < 8
                {
                    self.set_status("Passphrase must be at least 8 characters.");
                } else {
                    match crate::backup::build_export(
                        &self.store,
                        &self.config,
                        mode,
                        &self.backup_passphrase,
                    )
                    .and_then(|(f, warnings)| {
                        crate::backup::export_to_json(&f).map(|json| (json, warnings))
                    }) {
                        Ok((json, warnings)) => {
                            let path = if self.backup_export_path.trim().is_empty() {
                                let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                                config_dir().join(format!("nicord-config-{ts}.json"))
                            } else {
                                std::path::PathBuf::from(self.backup_export_path.trim())
                            };
                            match std::fs::write(&path, json) {
                                Ok(()) => {
                                    restrict_permissions(&path);
                                    // Record the export for the startup
                                    // health report's backup reminder.
                                    self.config.last_export_at = chrono::Utc::now().to_rfc3339();
                                    let _ = save_config(&self.config);
                                    if warnings.is_empty() {
                                        self.set_status(&format!(
                                            "Config exported to {}",
                                            path.display()
                                        ));
                                    } else {
                                        // The export is incomplete:
                                        // corrupt rows were left out.
                                        // Name the first one; the rest
                                        // are in the startup health
                                        // report.
                                        self.set_status(&format!(
                                            "Config exported to {} — WARNING: {} unreadable row(s) skipped: {}",
                                            path.display(),
                                            warnings.len(),
                                            warnings[0]
                                        ));
                                    }
                                    self.backup_passphrase.clear();
                                    self.backup_passphrase_confirm.clear();
                                }
                                Err(e) => self.set_status(&format!("Export failed: {e}")),
                            }
                        }
                        Err(e) => self.set_status(&format!("Export failed: {e}")),
                    }
                }
            }
            ui.separator();
            // ---- Import ----
            ui.label("Nhập cấu hình:");
            let engine_active = self.engine.is_loop_active();
            if engine_active {
                ui.label(
                    RichText::new(
                        "Dừng engine trước khi nhập (import thay thế cấu hình engine đang dùng).",
                    )
                    .color(theme::palette().danger),
                );
            }
            ui.horizontal(|ui| {
                ui.label("Đường dẫn file nhập:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.backup_import_path)
                        .hint_text("đường dẫn tới file nicord-config-*.json"),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Mật khẩu (chỉ file portable):");
                ui.add(
                    egui::TextEdit::singleline(&mut self.backup_import_passphrase).password(true),
                );
            });
            ui.checkbox(
                &mut self.backup_import_confirm,
                "Tôi hiểu: nhập sẽ THAY THẾ toàn bộ cấu hình, đích đến, kịch bản và mẫu hiện tại (một bản sao lưu tự động sẽ được tạo trước).",
            );
            theme::muted(
                ui,
                "Bản sao lưu tự động chỉ khôi phục được trên máy này (secret mã hóa theo máy).",
            );
            if ui
                .add_enabled(
                    !engine_active && self.backup_import_confirm,
                    egui::Button::new("Nhập & thay thế cấu hình"),
                )
                .clicked()
            {
                self.do_import();
            }
            if !self.backup_warnings.is_empty() {
                ui.add_space(6.0);
                theme::section_title(ui, "Cảnh báo nhập");
                for w in &self.backup_warnings {
                    theme::muted(ui, &format!("• {w}"));
                }
            }
        });
    }

    /// Run an import: read + parse the file, take an automatic local
    /// backup, apply the replacement, reload UI state. Errors leave
    /// the current config untouched (the backup is written first,
    /// Run a scheduled automatic backup when one is due (Group 3).
    /// Failures are logged, not shown in the status line: a backup
    /// that fails every 60 s must not spam the user. The Settings
    /// card shows the last successful backup time, so a stale
    /// timestamp is the visible signal.
    fn maybe_auto_backup(&mut self) {
        if !self.config.auto_backup_enabled {
            return;
        }
        let now = chrono::Utc::now();
        if !crate::backup::auto_backup_due(
            &self.config.last_auto_backup_at,
            self.config.auto_backup_interval_days,
            now,
        ) {
            return;
        }
        match crate::backup::run_auto_backup(
            &self.store,
            &self.config,
            self.config.auto_backup_keep,
        ) {
            Ok(path) => {
                self.config.last_auto_backup_at = now.to_rfc3339();
                let _ = save_config(&self.config);
                tracing::info!("automatic backup written to {}", path.display());
            }
            Err(e) => tracing::warn!("automatic backup failed: {e}"),
        }
    }

    /// Scheduled backups + log retention (Group 3). The settings
    /// are saved via Save settings, like the limits card; the two
    /// action buttons run immediately and report in the status line.
    fn show_maintenance_card(&mut self, ui: &mut egui::Ui) {
        // Labels resolved before the card: the card closure
        // mutates `self.config` (checkbox, drag values, combo).
        let maintenance_title = t(self.lang(), "settings.maintenance");
        let auto_backup_label = t(self.lang(), "settings.auto_backup");
        let every_label = t(self.lang(), "settings.every");
        let days_label = t(self.lang(), "settings.days");
        let keep_label = t(self.lang(), "settings.keep");
        let copies_label = t(self.lang(), "settings.copies");
        let backup_now_label = t(self.lang(), "settings.backup_now");
        let retention_label = t(self.lang(), "settings.retention");
        let forever_label = t(self.lang(), "settings.forever");
        let prune_now_label = t(self.lang(), "settings.prune_now");
        theme::card(ui, |ui| {
            theme::section_title(ui, maintenance_title);
            ui.checkbox(&mut self.config.auto_backup_enabled, auto_backup_label);
            ui.horizontal(|ui| {
                ui.label(every_label);
                ui.add(
                    egui::DragValue::new(&mut self.config.auto_backup_interval_days)
                        .range(1..=30)
                        .suffix(format!(" {days_label}")),
                );
                ui.label(keep_label);
                ui.add(
                    egui::DragValue::new(&mut self.config.auto_backup_keep)
                        .range(1..=30)
                        .suffix(format!(" {copies_label}")),
                );
            });
            if self.config.last_auto_backup_at.is_empty() {
                theme::muted(ui, "Chưa từng sao lưu tự động.");
            } else {
                theme::muted(
                    ui,
                    &format!("Sao lưu gần nhất: {}", self.config.last_auto_backup_at),
                );
            }
            theme::muted(
                ui,
                "Bản sao lưu nằm trong thư mục backups cạnh config.json và chỉ khôi phục được trên máy này (giống bản sao lưu trước khi import). Lưu cùng Save settings ở dưới.",
            );
            if theme::primary_button(ui, backup_now_label).clicked() {
                match crate::backup::run_auto_backup(
                    &self.store,
                    &self.config,
                    self.config.auto_backup_keep,
                ) {
                    Ok(path) => {
                        self.config.last_auto_backup_at = chrono::Utc::now().to_rfc3339();
                        let _ = save_config(&self.config);
                        self.set_status(&format!("Đã sao lưu: {}", path.display()));
                    }
                    Err(e) => self.set_status(&format!("Sao lưu thất bại: {e}")),
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(retention_label);
                let selected = match self.config.log_retention_days {
                    0 => forever_label.to_string(),
                    d => format!("{d} {days_label}"),
                };
                egui::ComboBox::from_id_salt("log_retention")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for (value, label) in [
                            (7u32, format!("7 {days_label}")),
                            (30, format!("30 {days_label}")),
                            (90, format!("90 {days_label}")),
                            (365, format!("365 {days_label}")),
                            (0, forever_label.to_string()),
                        ] {
                            ui.selectable_value(&mut self.config.log_retention_days, value, label);
                        }
                    });
            });
            theme::muted(
                ui,
                "Log cũ hơn số ngày này sẽ bị xóa khi mở app. Xóa log không ảnh hưởng lịch gửi hay thống kê hôm nay.",
            );
            if theme::ghost_button(ui, prune_now_label).clicked() {
                match self.store.prune_logs(self.config.log_retention_days) {
                    Ok(0) => self.set_status("Không có log cũ cần xóa."),
                    Ok(n) => self.set_status(&format!("Đã xóa {n} dòng log cũ.")),
                    Err(e) => self.set_status(&format!("Dọn dẹp thất bại: {e}")),
                }
            }
        });
    }

    /// and `apply_import` validates before deleting anything it
    /// cannot replace — a failure after deletion is reported with
    /// the backup path so the user can restore manually).
    fn do_import(&mut self) {
        let path = self.backup_import_path.trim().to_string();
        if path.is_empty() {
            self.set_status("Enter the import file path first.");
            return;
        }
        let text = match crate::backup::read_import_file(std::path::Path::new(&path)) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(&format!("Cannot read import file: {e}"));
                return;
            }
        };
        let file = match crate::backup::parse_export(&text) {
            Ok(f) => f,
            Err(e) => {
                self.set_status(&format!("Import rejected: {e}"));
                return;
            }
        };
        // Automatic local backup of the CURRENT state first.
        let backup_path = {
            let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
            config_dir().join(format!("nicord-backup-{ts}.json"))
        };
        let backup_note = match crate::backup::write_local_backup(
            &self.store,
            &self.config,
            &backup_path,
        ) {
            Ok(warnings) => {
                if warnings.is_empty() {
                    format!("Backup saved to {}.", backup_path.display())
                } else {
                    // The safety net itself is incomplete: say so
                    // before the replace happens.
                    format!(
                        "Backup saved to {} — WARNING: {} unreadable row(s) were skipped from the backup.",
                        backup_path.display(),
                        warnings.len()
                    )
                }
            }
            Err(e) => {
                self.set_status(&format!(
                    "Import aborted: cannot write automatic backup ({e}). Nothing was changed."
                ));
                return;
            }
        };
        let passphrase = self.backup_import_passphrase.clone();
        match crate::backup::apply_import(&self.store, &file, &passphrase) {
            Ok((new_config, report)) => {
                self.config = new_config;
                let _ = save_config(&self.config);
                // Reload scenarios + live-apply caps to the engine.
                // Import replaced the whole database, so the startup
                // load warnings are stale: refresh them (and the
                // health report) from the new state.
                if let Ok((list, warnings)) = self.store.load_scenarios_with_warnings() {
                    if let Ok(mut mine) = self.scenarios.lock() {
                        *mine = list;
                    }
                    self.load_warnings = warnings;
                    let (_, dest_warnings) = self
                        .store
                        .load_destinations_with_warnings()
                        .unwrap_or_default();
                    self.load_warnings.extend(dest_warnings);
                    self.refresh_health();
                }
                self.engine
                    .set_global_daily_cap(self.config.global_daily_cap);
                self.engine
                    .set_dest_error_limit(self.config.consecutive_error_limit);
                self.engine.set_antiban(self.config.antiban.clone());
                self.backup_warnings = report.warnings;
                self.backup_import_confirm = false;
                self.backup_import_passphrase.clear();
                self.selected = None;
                self.dest_selected_id = None;
                self.dest_draft = None;
                self.set_status(&format!(
                    "Import done: {} scenarios, {} destinations, {} templates. {backup_note}",
                    report.scenarios_imported,
                    report.destinations_imported,
                    report.templates_imported
                ));
            }
            Err(e) => {
                self.set_status(&format!(
                    "Import failed: {e} Nothing was changed. {backup_note}"
                ));
            }
        }
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        // Send mode: Webhook (official, recommended) or user-token
        // account mode. Switching never deletes the other credential.
        theme::card(ui, |ui| {
            theme::section_title(ui, t(self.lang(), "settings.send_mode"));
            ui.horizontal(|ui| {
                ui.radio_value(
                    &mut self.config.send_mode,
                    SendMode::Webhook,
                    "Webhook (khuyến nghị)",
                );
                ui.radio_value(
                    &mut self.config.send_mode,
                    SendMode::UserToken,
                    "Tài khoản Discord",
                );
            });
            theme::muted(
                ui,
                "Webhook là cách chính thức của Discord: chỉ gửi tin nhắn tới 1 kênh, \
                 không gửi được lệnh, không có typing. Tài khoản (self-bot) có nguy cơ bị khóa. \
                 Đổi chế độ không xóa thông tin đã lưu của chế độ kia. Bấm Save settings ở dưới để lưu.",
            );
            if self.config.send_mode == SendMode::Webhook
                && self.config.encrypted_webhook_url.is_empty()
            {
                ui.label("Chưa có webhook URL — dán URL ở dưới và lưu lại.");
            }
            if self.config.send_mode == SendMode::UserToken
                && self.config.encrypted_token.is_empty()
            {
                ui.label("Chưa có token — quay lại Onboarding để lưu token, hoặc chọn Webhook.");
            }
        });
        ui.add_space(12.0);
        if self.config.send_mode == SendMode::Webhook {
            theme::card(ui, |ui| {
                theme::section_title(ui, t(self.lang(), "settings.webhook"));
                if !self.config.webhook_id.is_empty() {
                    ui.label(format!(
                        "Webhook đã lưu: {} (ID {})",
                        self.config.webhook_display_name(),
                        self.config.webhook_id
                    ));
                    // Health + last check (QoL Q2).
                    let health = self
                        .webhook_health_shared
                        .lock()
                        .map(|h| *h)
                        .unwrap_or(WebhookHealth::Unknown);
                    let checked = self
                        .webhook_last_checked_shared
                        .lock()
                        .map(|s| s.clone())
                        .unwrap_or_default();
                    let health_color = match health {
                        WebhookHealth::Ok => theme::palette().success,
                        WebhookHealth::Unknown => theme::palette().text_secondary,
                        _ => theme::palette().danger,
                    };
                    ui.label(
                        RichText::new(format!("Trạng thái: {}", health.label()))
                            .color(health_color),
                    );
                    if !checked.is_empty() {
                        theme::muted(ui, &format!("Kiểm tra gần nhất: {checked}"));
                    }
                    if health == WebhookHealth::NotFound {
                        ui.label(
                            RichText::new(
                                "Webhook đã bị xóa hoặc URL đã đổi. Tạo webhook mới trong Discord (Cài đặt kênh → Tích hợp → Webhook), dán URL mới bên dưới và lưu lại.",
                            )
                            .color(theme::palette().danger),
                        );
                    }
                    // Paused destinations (if any) are listed in the
                    // Destinations screen; a successful re-check here
                    // also clears their pause.
                    if ui.button("Kiểm tra lại webhook đã lưu").clicked() {
                        self.recheck_stored_webhook();
                    }
                }
                ui.label("Đổi webhook URL (URL mới sẽ được mã hóa khi lưu):");
                ui.add(
                    egui::TextEdit::singleline(&mut self.webhook_url_input)
                        .password(true)
                        .hint_text("paste a new webhook URL here"),
                );
                ui.horizontal(|ui| {
                    if theme::ghost_button(ui, "Kiểm tra webhook").clicked() {
                        let url = self.webhook_url_input.clone();
                        self.check_webhook(url);
                    }
                    if theme::primary_button(ui, "Lưu webhook").clicked() {
                        let input = self.webhook_url_input.trim().to_string();
                        if input.is_empty() {
                            self.set_status("Webhook URL is empty.");
                        } else {
                            match parse_webhook_url(&input) {
                                Ok(parsed) => match store_secret(ENTRY_LEGACY_WEBHOOK, &input) {
                                    Ok(enc) => {
                                        self.config.encrypted_webhook_url = enc;
                                        self.config.webhook_id = parsed.id.clone();
                                        if let Ok(info) = self.webhook_info.lock() {
                                            if let Some(w) = info.as_ref() {
                                                if w.id == parsed.id {
                                                    self.config.webhook_name = w.display_name();
                                                }
                                            }
                                        }
                                        if save_config(&self.config).is_ok() {
                                            self.upsert_destination_from_legacy();
                                            self.webhook_url_input.clear();
                                            self.set_status(
                                                "Webhook saved (encrypted, local only).",
                                            );
                                        } else {
                                            self.set_status("Failed to save config.");
                                        }
                                    }
                                    Err(e) => {
                                        self.set_status(&format!("Cannot encrypt webhook URL: {e}"))
                                    }
                                },
                                Err(e) => self.set_status(&e),
                            }
                        }
                    }
                });
                let check_msg = self
                    .webhook_check_status
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or_default();
                if !check_msg.is_empty() {
                    ui.label(check_msg);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Tên hiển thị khi gửi (để trống = tên webhook):");
                    ui.text_edit_singleline(&mut self.config.webhook_username);
                    crate::destination::truncate_username_override(
                        &mut self.config.webhook_username,
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Tên thân thiện cho đích đến (để trống = tên webhook):");
                    ui.text_edit_singleline(&mut self.config.webhook_friendly_name);
                });
                theme::muted(
                    ui,
                    "Tên hiển thị và tên thân thiện được lưu cùng Save settings ở dưới.",
                );
            });
            ui.add_space(12.0);
        }
        // Destination limits (webhook mode). Saved via Save settings.
        if self.config.send_mode == SendMode::Webhook {
            theme::card(ui, |ui| {
                theme::section_title(ui, t(self.lang(), "settings.limits"));
                ui.horizontal(|ui| {
                    ui.label("Giới hạn tin/ngày toàn cục (mọi đích đến cộng lại):");
                    ui.add(egui::DragValue::new(&mut self.config.global_daily_cap).range(1..=1000));
                });
                theme::muted(ui, "Lưới an toàn chống chạy nhầm/chạy lặp. Đặt cao sẽ hiện cảnh báo khi vượt quá 300 — giới hạn thật của Discord (429) luôn được tôn trọng khi chặt hơn.");
                ui.horizontal(|ui| {
                    ui.label("Số đích đến tối đa (1–20):");
                    ui.add(egui::DragValue::new(&mut self.config.max_destinations).range(1..=20));
                });
                ui.horizontal(|ui| {
                    ui.label("Lỗi liên tiếp thì tạm dừng đích đến (3–10):");
                    ui.add(
                        egui::DragValue::new(&mut self.config.consecutive_error_limit)
                            .range(3..=10),
                    );
                });
                theme::muted(
                    ui,
                    "Lưu cùng Save settings ở dưới; áp dụng ngay cho engine đang chạy.",
                );
            });
            ui.add_space(12.0);
        }
        // Token Validator (Discord-BLUE): check the stored token and
        // show the account behind it. The token itself is never shown.
        theme::card(ui, |ui| {
            theme::section_title(ui, t(self.lang(), "settings.account"));
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Kiểm tra token").clicked() {
                    self.check_token();
                }
                theme::muted(ui, "Kiểm tra token đã lưu: tên, ID, email, 2FA, Nitro.");
            });
            if let Ok(info) = self.token_info.lock() {
                if let Some(me) = info.as_ref() {
                    ui.add_space(6.0);
                    ui.label(format!("Tên: {} (@{})", me.display_name(), me.username));
                    ui.label(format!(
                        "ID: {}",
                        if me.id.is_empty() {
                            "—"
                        } else {
                            me.id.as_str()
                        }
                    ));
                    ui.label(format!("Email: {}", me.email.as_deref().unwrap_or("—")));
                    ui.label(format!(
                        "Đã xác minh email: {}",
                        if me.verified { "Có" } else { "Không" }
                    ));
                    ui.label(format!(
                        "2FA: {}",
                        if me.mfa_enabled { "Bật" } else { "Tắt" }
                    ));
                    ui.label(format!("Nitro: {}", me.nitro_label()));
                }
            }
        });
        ui.add_space(12.0);
        // Anti-ban (Discord-BLUE): Smart Templates, Auto Typing,
        // Auto Break, Auto Delete. Saved to config.json and applied to
        // the running engine on Save.
        theme::card(ui, |ui| {
            theme::section_title(ui, "Chống ban");
            if self.config.send_mode == SendMode::Webhook {
                theme::muted(
                    ui,
                    "Chế độ Webhook: các tùy chọn dưới đây KHÔNG áp dụng — Webhook chỉ gửi \
                     tin nhắn đơn giản, không biến thể, không typing, không tự xóa. \
                     Giá trị vẫn được giữ nguyên nếu bạn quay lại chế độ Tài khoản.",
                );
            }
            ui.checkbox(
                &mut self.config.antiban.smart_templates,
                "Smart Templates: Spintax {a|b} + biến thể tin nhắn ngẫu nhiên",
            );
            theme::muted(
                ui,
                "Áp dụng sau khi thay biến {{username}}/{{server}} — tin nhắn không lặp y hệt.",
            );
            ui.checkbox(
                &mut self.config.antiban.auto_typing,
                "Auto Typing: hiện “đang gõ...” trước khi gửi",
            );
            ui.horizontal(|ui| {
                ui.label("Thời gian gõ (giây):");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.typing_min_sec)
                        .range(0..=30)
                        .speed(1),
                );
                ui.label("–");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.typing_max_sec)
                        .range(0..=30)
                        .speed(1),
                );
            });
            ui.checkbox(
                &mut self.config.antiban.auto_break,
                "Auto Break: nghỉ dài định kỳ sau một số tin",
            );
            ui.horizontal(|ui| {
                ui.label("Nghỉ sau (tin):");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.break_after_min)
                        .range(1..=500)
                        .speed(1),
                );
                ui.label("–");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.break_after_max)
                        .range(1..=500)
                        .speed(1),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Nghỉ trong (phút):");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.break_duration_min)
                        .range(1..=180)
                        .speed(1),
                );
                ui.label("–");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.break_duration_max)
                        .range(1..=180)
                        .speed(1),
                );
            });
            ui.checkbox(
                &mut self.config.antiban.auto_delete,
                "Auto Delete: tự xóa tin sau khi gửi",
            );
            ui.horizontal(|ui| {
                ui.label("Xóa sau (ms, 0 = ngay lập tức):");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.delete_delay_ms)
                        .range(0..=3_600_000)
                        .speed(100),
                );
            });
            theme::muted(
                ui,
                "Auto Delete chỉ xóa tin Nicord vừa gửi, không xóa tin cũ.",
            );
        });
        ui.add_space(12.0);
        theme::card(ui, |ui| {
            theme::section_title(ui, "Limits");
            ui.label("Limits: 1 channel • 300 messages/day (resets 00:00 UTC)");
            ui.checkbox(&mut self.config.autostart, "Start with the computer");
            #[cfg(target_os = "windows")]
            ui.checkbox(
                &mut self.config.tray_enabled,
                "Thu nhỏ xuống khay hệ thống khi đóng cửa sổ (X)",
            );
            #[cfg(not(target_os = "windows"))]
            theme::muted(ui, "System tray: chỉ có trên Windows.");
            // Start minimized: instant toggle, saved immediately.
            // Label resolved first: the checkbox holds a &mut
            // borrow of the config while it is built.
            let start_minimized_label = t(self.lang(), "settings.start_minimized");
            if ui
                .checkbox(&mut self.config.start_minimized, start_minimized_label)
                .changed()
            {
                let _ = save_config(&self.config);
            }
            theme::muted(ui, "Khởi động cùng máy được ghi thật vào hệ thống (Windows Run / Linux autostart). Khi bật khay hệ thống, đóng cửa sổ sẽ ẩn Nicord xuống khay thay vì thoát — bấm chuột phải vào icon khay để thoát hẳn.");
            // Language (Q9): instant switch, saved immediately.
            ui.horizontal(|ui| {
                ui.label(t(self.lang(), "settings.language"));
                let mut lang = self.lang();
                egui::ComboBox::from_id_salt("ui_language")
                    .selected_text(match lang {
                        Lang::Vi => "Tiếng Việt",
                        Lang::En => "English",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut lang, Lang::Vi, "Tiếng Việt");
                        ui.selectable_value(&mut lang, Lang::En, "English");
                    });
                if lang != self.lang() {
                    self.config.language = lang.code().to_string();
                    let _ = save_config(&self.config);
                    self.set_status("Language switched.");
                }
            });
            // Theme (Q12): instant switch, saved immediately.
            ui.horizontal(|ui| {
                ui.label("Giao diện:");
                let mut choice = crate::theme::ThemeChoice::from_code(&self.config.theme);
                egui::ComboBox::from_id_salt("ui_theme")
                    .selected_text(match choice {
                        crate::theme::ThemeChoice::Dark => "Tối",
                        crate::theme::ThemeChoice::Light => "Sáng",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut choice, crate::theme::ThemeChoice::Dark, "Tối");
                        ui.selectable_value(&mut choice, crate::theme::ThemeChoice::Light, "Sáng");
                    });
                if choice.code() != self.config.theme {
                    self.config.theme = choice.code().to_string();
                    let _ = save_config(&self.config);
                    crate::theme::apply_theme(
                        ui.ctx(),
                        choice,
                        crate::theme::UiScale::from_code(&self.config.ui_scale),
                    );
                    self.set_status("Theme switched.");
                }
            });
            // Font size: instant switch, saved immediately.
            ui.horizontal(|ui| {
                ui.label(t(self.lang(), "settings.font_size"));
                let mut scale = crate::theme::UiScale::from_code(&self.config.ui_scale);
                egui::ComboBox::from_id_salt("ui_scale")
                    .selected_text(match scale {
                        crate::theme::UiScale::Small => t(self.lang(), "settings.font_small"),
                        crate::theme::UiScale::Medium => t(self.lang(), "settings.font_medium"),
                        crate::theme::UiScale::Large => t(self.lang(), "settings.font_large"),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut scale,
                            crate::theme::UiScale::Small,
                            t(self.lang(), "settings.font_small"),
                        );
                        ui.selectable_value(
                            &mut scale,
                            crate::theme::UiScale::Medium,
                            t(self.lang(), "settings.font_medium"),
                        );
                        ui.selectable_value(
                            &mut scale,
                            crate::theme::UiScale::Large,
                            t(self.lang(), "settings.font_large"),
                        );
                    });
                if scale.code() != self.config.ui_scale {
                    self.config.ui_scale = scale.code().to_string();
                    let _ = save_config(&self.config);
                    crate::theme::apply_theme(
                        ui.ctx(),
                        crate::theme::ThemeChoice::from_code(&self.config.theme),
                        scale,
                    );
                    self.set_status("Font size switched.");
                }
            });
            // Notification sounds (Q11): instant toggles, saved immediately.
            let sound_changed = ui
                .checkbox(
                    &mut self.config.sound_enabled,
                    "Âm báo khi gửi lỗi hoặc kênh bị tạm dừng",
                )
                .changed()
                | ui.checkbox(
                    &mut self.config.sound_on_success,
                    "Âm báo cả khi gửi thành công",
                )
                .changed();
            if sound_changed {
                let _ = save_config(&self.config);
            }
            ui.horizontal(|ui| {
                if theme::ghost_button(ui, "Thử âm thanh").clicked() {
                    crate::notify::play_preview();
                }
                theme::muted(ui, "Âm báo không bao giờ chặn việc gửi tin.");
            });
            // Window geometry is remembered automatically; the reset
            // brings a lost/off-screen window back to the default.
            ui.horizontal(|ui| {
                if theme::ghost_button(ui, t(self.lang(), "settings.reset_window")).clicked() {
                    self.config.window_width = 0.0;
                    self.config.window_height = 0.0;
                    self.config.window_x = 0.0;
                    self.config.window_y = 0.0;
                    let _ = save_config(&self.config);
                    self.last_window_rect = None;
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                            1100.0, 700.0,
                        )));
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                            100.0, 100.0,
                        )));
                    self.set_status("Window size reset.");
                }
                theme::muted(
                    ui,
                    "Kích thước/vị trí cửa sổ được ghi nhớ tự động khi thoát.",
                );
            });
            // Keyboard shortcuts (reference only; see `handle_shortcuts`).
            theme::muted(
                ui,
                &format!(
                    "{}: Ctrl+S lưu · Ctrl+N kịch bản mới · Ctrl+1..5 chuyển màn hình · Esc đóng xác nhận",
                    t(self.lang(), "settings.shortcuts")
                ),
            );
            ui.add_space(8.0);
            if theme::primary_button(ui, t(self.lang(), "btn.save_settings")).clicked() {
                // Clamp destination limits to their documented bounds.
                self.config.max_destinations = self
                    .config
                    .max_destinations
                    .clamp(1, crate::destination::MAX_DESTINATIONS_HARD as u32);
                self.config.consecutive_error_limit =
                    self.config.consecutive_error_limit.clamp(3, 10);
                let _ = save_config(&self.config);
                // Live-apply the anti-ban settings to the running engine.
                self.engine.set_antiban(self.config.antiban.clone());
                // Live-apply the destination caps (no restart needed).
                self.engine
                    .set_global_daily_cap(self.config.global_daily_cap);
                self.engine
                    .set_dest_error_limit(self.config.consecutive_error_limit);
                // Apply the real OS autostart hook.
                match crate::autostart::set_enabled(self.config.autostart) {
                    Ok(()) => self.set_status("Settings saved."),
                    Err(e) => {
                        self.set_status(&format!("Settings saved, but autostart failed: {e}"))
                    }
                }
                #[cfg(target_os = "windows")]
                self.sync_tray();
            }
        });
        ui.add_space(12.0);
        theme::card(ui, |ui| {
            theme::section_title(ui, t(self.lang(), "settings.updates"));
            ui.checkbox(
                &mut self.config.auto_check_updates,
                "Tự động kiểm tra cập nhật khi mở app",
            );
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Kiểm tra cập nhật").clicked() {
                    self.check_updates(true);
                }
                ui.label(self.update_status_line());
            });
            theme::muted(
                ui,
                "Nicord chỉ hỏi bản phát hành công khai trên GitHub, không gửi dữ liệu cá nhân. Cập nhật chỉ diễn ra khi bạn đồng ý — file cài đặt được thay, còn token, kịch bản và lịch sử gửi vẫn nguyên.",
            );
        });
        ui.add_space(12.0);
        self.show_backup_card(ui);
        ui.add_space(12.0);
        self.show_maintenance_card(ui);
        ui.add_space(12.0);
        theme::danger_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new("Danger zone")
                    .color(theme::palette().danger)
                    .text_style(egui::TextStyle::Name("SectionTitle".into())),
            );
            if theme::danger_button(ui, "Delete all local data (token, scenarios)").clicked() {
                // Actually delete the scenarios too: resetting the config
                // alone left them in the DB (and in the UI), despite the
                // button promising to delete them.
                if let Ok(list) = self.scenarios.lock() {
                    for s in list.iter() {
                        let _ = self.store.delete_scenario(&s.id);
                    }
                }
                if let Ok(mut list) = self.scenarios.lock() {
                    list.clear();
                }
                self.selected = None;
                self.config = AppConfig::default();
                let _ = save_config(&self.config);
                self.screen = Screen::Onboarding;
                self.set_status("Local data cleared.");
            }
        });
    }

    /// Auto-update dialog: only opens when a newer release was found
    /// (or a download/install step failed after the user asked for
    /// it). "Để sau" dismisses until the next launch — nothing ever
    /// installs without the user pressing "Cập nhật ngay".
    fn show_update_dialog(&self, ctx: &egui::Context) {
        let (open, state, downloaded, total) = match self.update_status.lock() {
            Ok(u) => (u.dialog, u.state.clone(), u.downloaded, u.total),
            Err(_) => return,
        };
        if !open {
            return;
        }
        let current = updater::current_version();
        let mut close = false;
        egui::Window::new("Cập nhật Nicord")
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_width(380.0);
                match &state {
                    UpdateState::Available(info) => {
                        ui.label(format!(
                            "Có bản mới {} (bạn đang dùng v{}).",
                            info.tag, current
                        ));
                        theme::muted(
                            ui,
                            "Bản mới sẽ thay thế Nicord.exe, README.txt, LICENSE và icon.ico. Cài đặt, token và kịch bản của bạn không bị đụng tới.",
                        );
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if theme::primary_button(ui, "Cập nhật ngay").clicked() {
                                self.start_download();
                            }
                            if ui.button("Để sau").clicked() {
                                close = true;
                            }
                        });
                    }
                    UpdateState::Downloading => {
                        ui.label("Đang tải bản mới…");
                        if let Some(total) = total.filter(|t| *t > 0) {
                            ui.add(
                                egui::ProgressBar::new(downloaded as f32 / total as f32)
                                    .show_percentage(),
                            );
                        } else {
                            ui.spinner();
                        }
                        ui.label(format!(
                            "Đã tải {} / {}",
                            updater::fmt_bytes(downloaded),
                            total.map(updater::fmt_bytes).unwrap_or_else(|| "?".to_string())
                        ));
                    }
                    UpdateState::ReadyToInstall => {
                        ui.label("Đã tải xong. App sẽ tắt để cập nhật rồi tự mở lại.");
                        theme::muted(ui, "File cũ được giữ lại thành Nicord.exe.bak trong cùng thư mục.");
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if theme::primary_button(ui, "Cập nhật và khởi động lại").clicked() {
                                self.install_update();
                            }
                            if ui.button("Để sau").clicked() {
                                close = true;
                            }
                        });
                    }
                    UpdateState::Failed(e) => {
                        ui.label(format!("Cập nhật thất bại: {e}"));
                        ui.add_space(8.0);
                        if ui.button("Đóng").clicked() {
                            close = true;
                        }
                    }
                    // Checking / UpToDate / NoRelease / Idle never open
                    // the dialog; if the state moved on, close it.
                    _ => {
                        close = true;
                    }
                }
            });
        if close {
            if let Ok(mut u) = self.update_status.lock() {
                u.dialog = false;
                u.state = UpdateState::Idle;
            }
        }
    }
}

fn ctx_request_repaint(ui: &mut egui::Ui) {
    ui.ctx().request_repaint();
}

/// Process-wide shared tokio runtime for UI-triggered blocking calls
/// (token checks, webhook checks, test sends, deletes). Every call
/// site used to build a fresh multi-thread runtime per click; the
/// shared one is built once and reused. Safe because no call site
/// ever blocks nested inside another `block_on` on this runtime:
/// each handler blocks at most once, on the UI thread or on a
/// spawned thread that does one call. See the perf notes in
/// `docs/TECHNICAL.md` for the measured difference.
fn shared_runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RT: std::sync::OnceLock<Result<tokio::runtime::Runtime, String>> =
        std::sync::OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| e.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_search_matches_name_and_channel_case_insensitively() {
        let mut s = Scenario::new("Morning Greeting");
        s.channel_id = "123456789".to_string();
        assert!(scenario_matches_query(&s, ""));
        assert!(scenario_matches_query(&s, "   "));
        assert!(scenario_matches_query(&s, "morning"));
        assert!(scenario_matches_query(&s, "GREET"));
        assert!(scenario_matches_query(&s, "3456"));
        assert!(!scenario_matches_query(&s, "evening"));
        assert!(!scenario_matches_query(&s, "999"));
    }

    #[test]
    fn try_enable_validates_and_enables() {
        // Webhook mode, no destination: stays disabled with a message.
        let mut list = vec![Scenario::new("No dest")];
        let msg = try_enable_scenario(&mut list, 0, true);
        assert!(msg.is_some());
        assert!(!list[0].enabled);

        // With a destination the fresh scenario is valid: enabled.
        list[0].destination_id = Some("dest-1".to_string());
        let msg = try_enable_scenario(&mut list, 0, true);
        assert_eq!(msg, None);
        assert!(list[0].enabled);

        // Already enabled: silent no-op.
        let msg = try_enable_scenario(&mut list, 0, true);
        assert_eq!(msg, None);
        assert!(list[0].enabled);
    }

    #[test]
    fn next_run_cache_reuses_invalidates_and_recomputes_past_values() {
        use chrono::TimeZone;
        let mut cache = NextRunCache::default();
        let mut s = Scenario::new("t");
        s.schedule = Some(Schedule::Daily {
            time: chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        });
        s.timezone = "UTC".to_string();
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 11, 17, 8, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Local);
        let v1 = cache.next_run(&s, now).unwrap();
        // Second call reuses the cached value.
        assert_eq!(cache.next_run(&s, now).unwrap(), v1);
        // Schedule edit invalidates via the cache key.
        s.schedule = Some(Schedule::Daily {
            time: chrono::NaiveTime::from_hms_opt(10, 0, 0).unwrap(),
        });
        let v3 = cache.next_run(&s, now).unwrap();
        assert_ne!(v3, v1);
        // A cached value that already passed is recomputed at once.
        let later = chrono::Utc
            .with_ymd_and_hms(2026, 11, 17, 11, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Local);
        let v4 = cache.next_run(&s, later).unwrap();
        assert!(v4 > v3);
        // Timezone edit invalidates too.
        s.timezone = "America/New_York".to_string();
        let v5 = cache.next_run(&s, later).unwrap();
        assert_ne!(v5, v4);
        // No schedule → None; the batch helper covers the list.
        s.schedule = None;
        assert!(cache.next_run(&s, now).is_none());
        let mut s2 = Scenario::new("u");
        s2.schedule = Some(Schedule::Daily {
            time: chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        });
        s2.timezone = "UTC".to_string();
        let map = cache.next_runs(&[s, s2.clone()], now);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&s2.id));
    }

    #[test]
    fn try_enable_clears_last_run_for_once_scenarios() {
        use chrono::TimeZone;
        let mut s = Scenario::new("Once fired");
        s.destination_id = Some("dest-1".to_string());
        s.schedule = Some(Schedule::Once {
            at: chrono::Utc.with_ymd_and_hms(2026, 11, 17, 9, 0, 0).unwrap(),
        });
        s.last_run_at = Some(chrono::Utc.with_ymd_and_hms(2026, 11, 17, 9, 0, 5).unwrap());
        let mut list = vec![s];
        assert_eq!(try_enable_scenario(&mut list, 0, true), None);
        assert!(list[0].enabled);
        assert_eq!(list[0].last_run_at, None);

        // Recurring schedules keep their anchor: re-enabling must
        // not replay already-fired runs.
        let mut s = Scenario::new("Daily");
        s.destination_id = Some("dest-1".to_string());
        let fired = chrono::Utc.with_ymd_and_hms(2026, 11, 17, 9, 0, 5).unwrap();
        s.last_run_at = Some(fired);
        let mut list = vec![s];
        assert_eq!(try_enable_scenario(&mut list, 0, true), None);
        assert_eq!(list[0].last_run_at, Some(fired));
    }

    #[test]
    fn try_enable_user_mode_enforces_one_channel() {
        // Two valid user-mode scenarios on different channels: the
        // second enable must fail the 1-channel rule.
        let mut a = Scenario::new("A");
        a.channel_id = "111".to_string();
        let mut b = Scenario::new("B");
        b.channel_id = "222".to_string();
        let mut list = vec![a, b];
        assert_eq!(try_enable_scenario(&mut list, 0, false), None);
        assert!(list[0].enabled);
        let msg = try_enable_scenario(&mut list, 1, false);
        assert!(msg.is_some());
        assert!(!list[1].enabled);
    }

    /// Perf harness (ignored: run with `cargo test -- --ignored`).
    /// Compares building a fresh runtime per call against the shared
    /// runtime. Prints, never asserts on timing.
    #[test]
    #[ignore]
    fn bench_runtime_new_vs_shared() {
        let _ = shared_runtime(); // warm up, excluded from timing
        let n = 50u32;
        let start = std::time::Instant::now();
        for _ in 0..n {
            let _rt = tokio::runtime::Runtime::new().unwrap();
        }
        let new_avg_us = start.elapsed().as_micros() as f64 / n as f64;
        let start = std::time::Instant::now();
        for _ in 0..n {
            let _ = shared_runtime().unwrap();
        }
        let shared_avg_us = start.elapsed().as_micros() as f64 / n as f64;
        eprintln!("Runtime::new: {new_avg_us:.1} µs/call; shared_runtime: {shared_avg_us:.1} µs/call ({n} calls each)");
    }
}
