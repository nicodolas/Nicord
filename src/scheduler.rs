//! Scheduler + send engine (Spec §6).
//! One queue per channel, never two sends to the same channel in parallel.
//! Missed runs while the machine is off are NOT bulk-sent on restart —
//! only the next scheduled run fires, and the miss is logged.
//!
//! Firing rule (see `scenario::due_time`): a scenario fires once per
//! scheduled time. `last_run_at` (the last *scheduled* time that fired)
//! is persisted to SQLite on every successful send, so a restart can
//! never re-fire or duplicate a run. `Once` fires exactly once and is
//! then disabled.

use crate::{
    antiban::{sanitize_content, AntiBanConfig},
    destination::{Destination, FailureState},
    discord::{build_interaction_payload, make_nonce, DiscordClient, DiscordError},
    gateway::{Gateway, SESSION_WAIT_SECS},
    scenario::{due_time, render_template_with_vars, CommandConfig, Scenario, Schedule},
    spintax::{apply_spintax, vary_message},
    store::Store,
    webhook::MessageSender,
};
use chrono::{DateTime, Local, NaiveDate, Timelike, Utc};
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct SendLogEntry {
    pub scenario_id: String,
    pub scenario_name: String,
    pub channel_id: String,
    pub status: String,
    pub detail: String,
    /// Discord message id of the sent message ("" when unknown, e.g.
    /// a webhook 204 or a failure). Used by manual delete (Q13).
    pub message_id: String,
    /// Destination the send went through ("" in user-token mode).
    pub destination_id: String,
}

/// Single-product rule: at most 1 distinct channel across all *enabled*
/// scenarios. Returns the offending description, if any.
pub fn channels_violation(scenarios: &[Scenario]) -> Option<String> {
    let mut channels: Vec<&str> = scenarios
        .iter()
        .filter(|s| s.enabled)
        .map(|s| s.channel_id.as_str())
        .filter(|c| !c.is_empty())
        .collect();
    channels.sort_unstable();
    channels.dedup();
    if channels.len() > 1 {
        Some(format!(
            "Nicord allows 1 channel, but enabled scenarios use {} distinct channels",
            channels.len()
        ))
    } else {
        None
    }
}

/// Has the UTC-day bucket rolled over since `last_day`?
/// Pure over its inputs: callers pass a UTC instant, so the result never
/// depends on the machine timezone. In particular, local midnight does
/// NOT reset the daily cap — only 00:00 UTC does.
pub fn utc_day_rolled_over(last_day: NaiveDate, now_utc: DateTime<Utc>) -> bool {
    last_day != now_utc.date_naive()
}

/// Auto Break state: sends since the last break and the send count
/// that triggers the next one (drawn fresh after every break).
#[derive(Debug, Clone, Copy)]
struct BreakState {
    sent_since_break: u32,
    next_break_at: u32,
}

pub struct Engine {
    pub store: Arc<Store>,
    /// Anti-ban settings, live-editable from Settings: the UI writes a
    /// new config while the engine reads a clone per send, so a toggle
    /// takes effect without restarting the engine.
    pub antiban: std::sync::RwLock<AntiBanConfig>,
    /// Last send time per channel (enforces the per-channel interval).
    last_sent: Mutex<HashMap<String, Instant>>,
    /// Last message index sent per scenario (rotation never repeats).
    last_msg_index: Mutex<HashMap<String, usize>>,
    /// Messages sent today per account (daily cap). Initialised from the
    /// persisted send log, so a restart does not reset the count.
    /// The bucket is the UTC date (`last_day`): the cap resets at 00:00
    /// UTC, never at local midnight.
    sent_today: Mutex<u32>,
    last_day: Mutex<chrono::NaiveDate>,
    pub emergency_stop: Arc<AtomicBool>,
    pub running: Arc<AtomicBool>,
    /// True while a `run_loop` task holds the single-loop claim,
    /// including wind-down after Emergency Stop. Distinct from
    /// `running`: `stop_all()` clears `running` immediately, but the
    /// old loop may still be finishing a send, so a new loop must not
    /// start until this claim is released.
    loop_active: Arc<AtomicBool>,
    /// Per-destination 429 cooldowns (destination id -> resume time).
    /// std Mutex: short critical sections, no await while held, and
    /// the UI never needs to read them.
    cooldowns: std::sync::Mutex<HashMap<String, Instant>>,
    /// Global 429 pause (all destinations) until this instant.
    global_pause_until: std::sync::Mutex<Option<Instant>>,
    /// Consecutive errors per destination id (auto-pause tracking).
    dest_errors: std::sync::Mutex<HashMap<String, u32>>,
    /// Global daily cap (webhook mode, live-editable from Settings).
    global_daily_cap: AtomicU32,
    /// Consecutive-error limit before auto-pause (live-editable).
    dest_error_limit: AtomicU32,
    /// Real identity for template vars, fetched once at engine start
    /// (`get_me` + first guild). Falls back to ("you", "server") only
    /// when that call fails, and the failure is logged.
    identity: Mutex<(String, String)>,
    identity_last_try: Mutex<Option<Instant>>,
    /// Auto Break progress (Discord-BLUE).
    break_state: Mutex<BreakState>,
    /// Gateway client for slash commands (lazy: connects only when a
    /// slash command actually fires).
    gateway: Arc<Gateway>,
    /// Failed sends this session (dashboard success rate). Skips
    /// ("skip: ...") are not failures and are never counted here.
    /// Messages deleted by Auto Delete this session.
    deleted_today: std::sync::Mutex<u32>,
    /// When this engine was created (dashboard send rate / uptime).
    pub started_at: Instant,
}

impl Engine {
    pub fn new(
        store: Arc<Store>,
        antiban: AntiBanConfig,
        global_daily_cap: u32,
        dest_error_limit: u32,
    ) -> Self {
        let sent = store.count_sends_today();
        Self {
            store,
            antiban: std::sync::RwLock::new(antiban),
            last_sent: Mutex::new(HashMap::new()),
            last_msg_index: Mutex::new(HashMap::new()),
            sent_today: Mutex::new(sent),
            last_day: Mutex::new(Utc::now().date_naive()),
            emergency_stop: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
            loop_active: Arc::new(AtomicBool::new(false)),
            cooldowns: std::sync::Mutex::new(HashMap::new()),
            global_pause_until: std::sync::Mutex::new(None),
            dest_errors: std::sync::Mutex::new(HashMap::new()),
            global_daily_cap: AtomicU32::new(global_daily_cap),
            dest_error_limit: AtomicU32::new(dest_error_limit),
            identity: Mutex::new(("you".to_string(), "server".to_string())),
            identity_last_try: Mutex::new(None),
            break_state: Mutex::new(BreakState {
                sent_since_break: 0,
                next_break_at: 0,
            }),
            gateway: Arc::new(Gateway::new()),
            deleted_today: std::sync::Mutex::new(0),
            started_at: Instant::now(),
        }
    }

    /// Current anti-ban config (clone, so the engine never holds the
    /// lock across an await).
    pub fn antiban(&self) -> AntiBanConfig {
        self.antiban.read().map(|c| c.clone()).unwrap_or_default()
    }

    /// Replace the anti-ban config live (Settings Save). Takes effect
    /// on the next send — no engine restart needed.
    pub fn set_antiban(&self, cfg: AntiBanConfig) {
        if let Ok(mut c) = self.antiban.write() {
            *c = cfg;
        }
    }

    /// Atomically claim the single engine loop. False means a loop is
    /// already active (including wind-down) and the caller must not
    /// start a second one: two loops hold separate last-run state and
    /// can fire the same scheduled time twice.
    pub fn try_claim_loop(&self) -> bool {
        self.loop_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn release_loop(&self) {
        self.loop_active.store(false, Ordering::SeqCst);
    }

    /// True while a `run_loop` task holds the claim (including
    /// wind-down). The UI checks this before spawning a new loop; the
    /// atomic claim in `run_loop` is the correctness guard.
    pub fn is_loop_active(&self) -> bool {
        self.loop_active.load(Ordering::SeqCst)
    }

    /// Live-update the global daily cap (Settings save).
    pub fn set_global_daily_cap(&self, cap: u32) {
        self.global_daily_cap.store(cap, Ordering::SeqCst);
    }

    /// Live-update the consecutive-error limit (Settings save).
    pub fn set_dest_error_limit(&self, limit: u32) {
        self.dest_error_limit.store(limit, Ordering::SeqCst);
    }

    /// True while a global 429 pause is in effect.
    pub fn is_globally_paused(&self) -> bool {
        self.global_pause_until
            .lock()
            .map(|g| g.map(|t| t > Instant::now()).unwrap_or(false))
            .unwrap_or(false)
    }

    fn set_global_pause(&self, secs: u64) {
        if let Ok(mut g) = self.global_pause_until.lock() {
            *g = Some(Instant::now() + Duration::from_secs(secs.max(1)));
        }
    }

    fn set_cooldown(&self, dest_id: &str, secs: u64) {
        if let Ok(mut map) = self.cooldowns.lock() {
            map.insert(
                dest_id.to_string(),
                Instant::now() + Duration::from_secs(secs.max(1)),
            );
        }
    }

    fn cooldown_remaining(&self, dest_id: &str) -> Option<Duration> {
        self.cooldowns.lock().ok().and_then(|map| {
            map.get(dest_id)
                .and_then(|t| t.checked_duration_since(Instant::now()))
        })
    }

    /// Record a destination error. Returns true when the destination
    /// just crossed the auto-pause limit (caller persists the pause).
    fn record_dest_error(&self, dest_id: &str) -> bool {
        let limit = self.dest_error_limit.load(Ordering::SeqCst).max(1);
        self.dest_errors
            .lock()
            .map(|mut map| {
                let count = map.entry(dest_id.to_string()).or_insert(0);
                *count += 1;
                *count >= limit
            })
            .unwrap_or(false)
    }

    fn reset_dest_errors(&self, dest_id: &str) {
        if let Ok(mut map) = self.dest_errors.lock() {
            map.remove(dest_id);
        }
    }

    /// Messages deleted by Auto Delete this session (dashboard).
    pub fn deleted_count(&self) -> u32 {
        self.deleted_today.lock().map(|c| *c).unwrap_or(0)
    }

    /// Sleep that aborts early when Emergency Stop is pressed.
    /// Returns true when the sleep was interrupted.
    async fn sleep_interruptible(&self, dur: Duration) -> bool {
        if dur.is_zero() {
            return self.emergency_stop.load(Ordering::SeqCst);
        }
        let deadline = Instant::now() + dur;
        loop {
            if self.emergency_stop.load(Ordering::SeqCst) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            tokio::time::sleep(left.min(Duration::from_millis(200))).await;
        }
    }

    pub fn stop_all(&self) {
        self.emergency_stop.store(true, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
        // The gateway task exits too (a later slash send reconnects).
        self.gateway.stop();
    }

    pub fn resume(&self) {
        self.emergency_stop.store(false, Ordering::SeqCst);
    }

    /// Real identity for templates. Falls back to ("you", "server").
    pub async fn identity(&self) -> (String, String) {
        self.identity.lock().await.clone()
    }

    /// Fetch the real username / first server name once (retry at most
    /// once a minute while it keeps failing). Never fails the engine.
    pub async fn refresh_identity(&self, client: &DiscordClient) {
        {
            let mut last = self.identity_last_try.lock().await;
            if let Some(t) = *last {
                if t.elapsed() < Duration::from_secs(60) {
                    return;
                }
            }
            *last = Some(Instant::now());
        }
        match client.get_me_typed().await {
            Ok(me) => {
                let username = me.display_name();
                let server = match client.list_guilds().await {
                    Ok(gs) => gs
                        .first()
                        .map(|g| g.name.clone())
                        .unwrap_or_else(|| "server".to_string()),
                    Err(e) => {
                        tracing::warn!("cannot list guilds for server name: {e}");
                        "server".to_string()
                    }
                };
                *self.identity.lock().await = (username, server);
            }
            Err(e) => {
                tracing::warn!(
                    "cannot fetch account identity ({e}); template vars fall back to you/server"
                );
            }
        }
    }

    /// Wait until the per-channel interval + jitter has elapsed.
    async fn wait_for_slot(&self, channel_id: &str) {
        let antiban = self.antiban();
        let interval = Duration::from_secs(antiban.effective_interval_secs());
        let mut rng = StdRng::from_os_rng();
        let jitter = Duration::from_secs(antiban.jitter_secs(&mut rng));
        let wait = {
            let mut last = self.last_sent.lock().await;
            let now = Instant::now();
            let elapsed = last
                .get(channel_id)
                .map(|t| now.duration_since(*t))
                .unwrap_or(interval + jitter);
            let need = interval.saturating_sub(elapsed) + jitter;
            last.insert(channel_id.to_string(), now + need);
            need
        };
        if !wait.is_zero() {
            // Abort early on Emergency Stop; callers re-check the flag
            // right after this returns.
            let _ = self.sleep_interruptible(wait).await;
        }
    }

    /// Send one scenario's next message, applying all anti-ban layers.
    /// Returns Ok(message_id) or Err(reason). Errors starting with
    /// `skip: ` are intentional skips (sleep window, daily cap, time
    /// window, paused destination) — the caller consumes the run but
    /// must not count them as failures. Errors starting with `defer: `
    /// are short waits (minimum interval, 429 cooldown, global pause)
    /// — the caller must NOT consume the run, count an error, or log
    /// a failure; the run retries on a later tick, slightly late.
    /// `destination` is Some in webhook mode (per-destination checks),
    /// None in user-token mode. Never panics on Discord errors.
    pub async fn send_once(
        &self,
        scenario: &Scenario,
        sender: &MessageSender,
        destination: Option<&Destination>,
        username: &str,
        server_name: &str,
    ) -> Result<String, String> {
        if self.emergency_stop.load(Ordering::SeqCst) {
            return Err("emergency stop is active".into());
        }
        // Global 429 pause blocks every send until it expires.
        if self.is_globally_paused() {
            return Err("defer: Discord global rate-limit pause in effect".into());
        }
        // Destination pre-checks (webhook mode only).
        if let Some(dest) = destination {
            if !dest.enabled {
                return Err(format!(
                    "skip: destination '{}' is disabled",
                    dest.display_name()
                ));
            }
            if dest.failure_state.is_paused() {
                return Err(format!(
                    "skip: destination '{}' is paused ({}) — resume it in Destinations",
                    dest.display_name(),
                    dest.failure_state.label()
                ));
            }
            if dest.encrypted_url.trim().is_empty() {
                return Err(format!(
                    "skip: destination '{}' has no webhook URL yet",
                    dest.display_name()
                ));
            }
            // Quiet window (fail-closed on a broken window).
            let now_local = Local::now();
            let minutes = now_local.hour() * 60 + now_local.minute();
            if dest.is_quiet_at(minutes) {
                return Err(format!(
                    "skip: destination '{}' is inside its quiet window",
                    dest.display_name()
                ));
            }
            // Per-destination 429 cooldown: defer, do not consume.
            if let Some(remaining) = self.cooldown_remaining(&dest.id) {
                return Err(format!(
                    "defer: destination '{}' cooling down for {}s (429)",
                    dest.display_name(),
                    remaining.as_secs()
                ));
            }
            // Per-destination daily cap (DB count, UTC date).
            let effective_cap = if dest.daily_cap == 0 {
                self.global_daily_cap.load(Ordering::SeqCst)
            } else {
                dest.daily_cap
            };
            if !dest.channel_id.is_empty() {
                let sent = self.store.count_sends_today_for_channel(&dest.channel_id);
                if sent >= effective_cap {
                    return Err(format!(
                        "skip: destination '{}' daily cap reached ({sent}/{effective_cap})",
                        dest.display_name()
                    ));
                }
            }
        }
        // Live anti-ban config for this send (Settings edits apply
        // without restarting the engine).
        let antiban = self.antiban();
        // Layer 5: sleep window.
        if antiban.in_sleep_window(Local::now()) {
            return Err("skip: inside night sleep window".into());
        }
        // Mode-aware validation: webhook scenarios need no channel,
        // but command nodes are rejected (webhooks cannot send them).
        if sender.is_webhook() {
            scenario
                .validate_for_webhook()
                .map_err(|e| format!("invalid scenario: {e}"))?;
        } else {
            scenario
                .validate()
                .map_err(|e| format!("invalid scenario: {e}"))?;
        }
        // ConditionTimeWindow nodes: outside the window = skip, not error.
        if let Err(e) = scenario.check_time_windows(Local::now()) {
            return Err(format!("skip: {e}"));
        }
        // Daily cap. User mode: 300/day with warm-up, additionally
        // capped by the per-scenario limit. Webhook mode: the global
        // daily cap from Settings (no warm-up — webhooks are official
        // API usage, not an account warming up), plus the per-scenario
        // limit. The persisted send count is the source of truth.
        {
            // The bucket is the UTC date: the cap resets at 00:00 UTC,
            // never at local midnight, so changing the machine timezone
            // cannot grant extra messages.
            let now_utc = Utc::now();
            let mut day = self.last_day.lock().await;
            if utc_day_rolled_over(*day, now_utc) {
                *day = now_utc.date_naive();
                *self.sent_today.lock().await = self.store.count_sends_today();
            }
            let sent = *self.sent_today.lock().await;
            let scenario_cap = if scenario.limits.daily_cap == 0 {
                u32::MAX
            } else {
                scenario.limits.daily_cap
            };
            if destination.is_some() {
                let eff = self
                    .global_daily_cap
                    .load(Ordering::SeqCst)
                    .min(scenario_cap);
                if sent >= eff {
                    return Err(format!("skip: global daily cap reached ({sent}/{eff})"));
                }
            } else {
                let base_cap = antiban.daily_cap;
                let age = self.store.account_age_days();
                let account_eff = antiban.effective_daily_cap(base_cap, age);
                let eff = account_eff.min(scenario_cap);
                if sent >= eff {
                    return Err(format!(
                        "skip: daily cap reached ({sent}/{eff}, warm-up age {age}d)"
                    ));
                }
            }
        }

        // A Send Command node takes precedence over message nodes: one
        // scheduled firing runs exactly the first command node (see
        // `Scenario::command_node` for why only the first one runs).
        // Webhook mode rejects commands defensively here too, even
        // though validation above already rejects them.
        if let Some(cmd_node) = scenario.command_node() {
            if sender.is_webhook() {
                return Err(
                    "webhook mode cannot send commands — use a message scenario or switch back to account mode"
                        .into(),
                );
            }
            let cfg = CommandConfig::from_node(cmd_node);
            return self
                .send_command(scenario, &cfg, sender, username, server_name, &antiban)
                .await;
        }
        // Node execution (in node order): a Send Message / Send Embed node
        // with non-empty content wins over the plain message list.
        // UtilityWait nodes add an explicit delay before sending.
        let wait_secs = scenario.wait_seconds();
        let mut rng = StdRng::from_os_rng();
        // Content fallback chain (Q6): node message → scenario
        // messages → destination default messages → error. When
        // `use_destination_messages` is set, the destination
        // defaults replace the scenario list outright. Never an
        // empty send, never invented content.
        let use_dest_first = scenario.use_destination_messages && destination.is_some();
        let raw = if let Some(node_msg) = scenario.node_message() {
            node_msg
        } else if !use_dest_first && !scenario.messages.is_empty() {
            let last_idx = self.last_msg_index.lock().await.get(&scenario.id).copied();
            let (idx, msg) = scenario
                .pick_message_with_index(last_idx, &mut rng)
                .ok_or_else(|| "scenario has no messages".to_string())?;
            self.last_msg_index
                .lock()
                .await
                .insert(scenario.id.clone(), idx);
            msg
        } else if let Some(dest) = destination {
            if dest.default_messages.is_empty() {
                return Err(
                    "scenario has no messages and the destination has no default messages".into(),
                );
            }
            // Rotate through the destination defaults without
            // repeating the last pick (same rule as scenarios).
            let last_idx = self.last_msg_index.lock().await.get(&scenario.id).copied();
            let n = dest.default_messages.len();
            let idx = if n == 1 {
                0
            } else {
                loop {
                    let candidate = rng.random_range(0..n);
                    if Some(candidate) != last_idx {
                        break candidate;
                    }
                }
            };
            self.last_msg_index
                .lock()
                .await
                .insert(scenario.id.clone(), idx);
            dest.default_messages[idx].clone()
        } else {
            return Err("scenario has no messages".to_string());
        };
        let mut vars: HashMap<String, String> = scenario
            .variables
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect();
        vars.extend(scenario.set_variable_overrides());
        let rendered =
            render_template_with_vars(&raw, username, server_name, &scenario.channel_id, &vars);
        // Smart Templates (Discord-BLUE): Spintax + variation are applied
        // AFTER template vars are resolved — so `{{username}}` is never
        // mangled — and only when the toggle is on. Webhook mode never
        // varies content: webhook sends are plain scheduled messages.
        let final_text = if antiban.smart_templates && !sender.is_webhook() {
            vary_message(&apply_spintax(&rendered, &mut rng), &mut rng)
        } else {
            rendered
        };
        // Layer 4: content checks. Empty content is allowed when an
        // embed is attached (Discord requires content OR embeds).
        let (clean, warnings) = sanitize_content(&final_text, &antiban);
        for w in &warnings {
            tracing::warn!("anti-ban: {w}");
        }
        let has_embed = scenario
            .embed
            .as_ref()
            .map(|e| !e.is_empty())
            .unwrap_or(false);
        if clean.trim().is_empty() && !has_embed {
            return Err("message is empty after sanitising".into());
        }

        if wait_secs > 0 {
            let _ = self
                .sleep_interruptible(Duration::from_secs(wait_secs))
                .await;
            if self.emergency_stop.load(Ordering::SeqCst) {
                return Err("emergency stop pressed while waiting".into());
            }
        }
        // Minimum interval. User mode: sleep until the slot opens
        // (existing behaviour). Webhook mode: defer without consuming
        // the run, so other destinations keep running on this tick.
        if let Some(dest) = destination {
            let interval = Duration::from_secs(u64::from(dest.min_interval_secs));
            let too_soon = {
                let last = self.last_sent.lock().await;
                last.get(&dest.id)
                    .map(|t| t.elapsed() < interval)
                    .unwrap_or(false)
            };
            if too_soon {
                return Err(format!(
                    "defer: destination '{}' minimum interval not reached yet",
                    dest.display_name()
                ));
            }
        } else {
            self.wait_for_slot(&scenario.channel_id).await;
        }
        if self.emergency_stop.load(Ordering::SeqCst) {
            return Err("emergency stop pressed while waiting".into());
        }

        // Auto Typing (Discord-BLUE): show "typing..." for a random
        // 2–5s before sending. A typing failure never blocks the send.
        // Webhooks have no typing endpoint, so this is skipped entirely
        // in webhook mode.
        if antiban.auto_typing && !sender.is_webhook() {
            if let MessageSender::User(client) = sender {
                let _ = client.send_typing(&scenario.channel_id).await;
            }
            let typing = Duration::from_secs(antiban.typing_secs(&mut rng));
            if self.sleep_interruptible(typing).await {
                return Err("emergency stop pressed while typing".into());
            }
        }

        // Retry: network errors up to 3x with backoff; 429 waits retry_after;
        // 401/403/404 stop immediately (Spec §6).
        let embeds: Vec<crate::embed::Embed> = scenario
            .embed
            .clone()
            .filter(|e| !e.is_empty())
            .into_iter()
            .collect();
        self.send_message_with_retry(
            scenario,
            sender,
            destination,
            &clean,
            &embeds,
            &antiban,
            &mut rng,
        )
        .await
    }

    /// Send one message with the retry policy, then run the post-send
    /// layers (Auto Delete, Auto Break). Returns the message id.
    // The retry path genuinely needs all seven inputs (scenario,
    // sender, destination, content, embeds, anti-ban, rng); grouping
    // them into a struct would churn every call site for no gain.
    #[allow(clippy::too_many_arguments)]
    async fn send_message_with_retry(
        &self,
        scenario: &Scenario,
        sender: &MessageSender,
        destination: Option<&Destination>,
        clean: &str,
        embeds: &[crate::embed::Embed],
        antiban: &AntiBanConfig,
        rng: &mut StdRng,
    ) -> Result<String, String> {
        // Log channel: the destination's channel in webhook mode (so
        // per-destination stats and filters work), the scenario's
        // channel in user mode.
        let log_channel = destination
            .map(|d| d.channel_id.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| scenario.channel_id.clone());
        let mut attempt = 0;
        loop {
            attempt += 1;
            match sender
                .send_with_embeds(&scenario.channel_id, clean, embeds)
                .await
            {
                Ok(sent) => {
                    *self.sent_today.lock().await += 1;
                    // Record the actual send time for the destination's
                    // minimum-interval check.
                    if let Some(dest) = destination {
                        self.last_sent
                            .lock()
                            .await
                            .insert(dest.id.clone(), Instant::now());
                        self.reset_dest_errors(&dest.id);
                    }
                    // A webhook 204 has no message id: do not claim one.
                    let detail = if sent.id.is_empty() {
                        "message sent (webhook, no id returned)".to_string()
                    } else {
                        format!("message {}", sent.id)
                    };
                    let entry = SendLogEntry {
                        scenario_id: scenario.id.clone(),
                        scenario_name: scenario.name.clone(),
                        channel_id: log_channel.clone(),
                        status: "sent".into(),
                        detail,
                        message_id: sent.id.clone(),
                        destination_id: destination.map(|d| d.id.clone()).unwrap_or_default(),
                    };
                    let _ = self.store.log_send(&entry);
                    crate::notify::play(crate::notify::SoundEvent::SendSuccess);
                    self.maybe_auto_delete(scenario, sender, &sent.id, antiban)
                        .await?;
                    self.maybe_auto_break(antiban, rng).await?;
                    return Ok(sent.id);
                }
                Err(DiscordError::RateLimited {
                    retry_after_secs,
                    global,
                }) => {
                    tracing::warn!("Discord 429, waiting {retry_after_secs}s (global={global})");
                    // Webhook mode: record the cooldown and defer —
                    // other destinations keep running, and this run
                    // retries after `retry_after` without consuming.
                    // Never shorten `retry_after`.
                    if let Some(dest) = destination {
                        if global {
                            self.set_global_pause(retry_after_secs);
                        } else {
                            self.set_cooldown(&dest.id, retry_after_secs);
                        }
                        return Err(format!(
                            "defer: Discord rate limit, retry after {retry_after_secs}s (global={global})"
                        ));
                    }
                    // User mode: same waits as before, but abort early
                    // on Emergency Stop instead of sleeping up to 1h
                    // uninterruptibly.
                    let paused = if global {
                        // Spec §6: a global limit warning pauses everything 1h.
                        self.sleep_interruptible(Duration::from_secs(3600)).await
                    } else {
                        self.sleep_interruptible(Duration::from_secs(retry_after_secs.max(1)))
                            .await
                    };
                    if paused {
                        return Err("emergency stop pressed during rate-limit pause".into());
                    }
                    if attempt >= 3 {
                        return Err(format!(
                            "rate limited repeatedly (last retry_after {retry_after_secs}s)"
                        ));
                    }
                }
                Err(DiscordError::Unauthorized(m)) | Err(DiscordError::Forbidden(m)) => {
                    if let Some(dest) = destination {
                        let _ = self.store.set_destination_failure(
                            &dest.id,
                            FailureState::Paused401,
                            &format!("stopped immediately: {m}"),
                        );
                        crate::notify::play(crate::notify::SoundEvent::DestinationPaused);
                    }
                    return Err(format!("stopped immediately: {m}"));
                }
                Err(DiscordError::NotFound(m)) => {
                    // A gone webhook pauses the destination (persisted):
                    // further sends would only 404 again. The user
                    // resumes it from Destinations after fixing it.
                    if let Some(dest) = destination {
                        let _ = self.store.set_destination_failure(
                            &dest.id,
                            FailureState::Paused404,
                            &format!("stopped immediately: {m}"),
                        );
                        crate::notify::play(crate::notify::SoundEvent::DestinationPaused);
                    }
                    return Err(format!("stopped immediately: {m}"));
                }
                Err(DiscordError::Other(m)) => {
                    if attempt >= 3 {
                        return Err(format!("failed after 3 attempts: {m}"));
                    }
                    let backoff = 2u64.pow(attempt) * 2;
                    if self.sleep_interruptible(Duration::from_secs(backoff)).await {
                        return Err("emergency stop pressed during retry backoff".into());
                    }
                }
            }
        }
    }

    /// Auto Delete (Discord-BLUE): delete the message after the
    /// configured delay. A delete failure is only logged — the send
    /// already succeeded. Err only when Emergency Stop interrupts the
    /// wait (the message stays up in that case).
    async fn maybe_auto_delete(
        &self,
        scenario: &Scenario,
        sender: &MessageSender,
        message_id: &str,
        antiban: &AntiBanConfig,
    ) -> Result<(), String> {
        // Webhook mode has no auto-delete in v1: sent messages stay up.
        if sender.is_webhook() {
            return Ok(());
        }
        let MessageSender::User(client) = sender else {
            return Ok(());
        };
        if !antiban.auto_delete {
            return Ok(());
        }
        if self
            .sleep_interruptible(Duration::from_millis(antiban.delete_delay_ms))
            .await
        {
            return Err("emergency stop pressed while waiting to delete".into());
        }
        match client
            .delete_message(&scenario.channel_id, message_id)
            .await
        {
            Ok(()) => {
                if let Ok(mut d) = self.deleted_today.lock() {
                    *d += 1;
                }
                let _ = self.store.log_send(&SendLogEntry {
                    scenario_id: scenario.id.clone(),
                    scenario_name: scenario.name.clone(),
                    channel_id: scenario.channel_id.clone(),
                    status: "deleted".into(),
                    detail: format!("deleted message {message_id}"),
                    message_id: message_id.to_string(),
                    destination_id: String::new(),
                });
            }
            Err(e) => {
                tracing::warn!("auto-delete failed for {message_id}: {e}");
            }
        }
        Ok(())
    }

    /// Auto Break (Discord-BLUE): after a random 15–25 sends, rest for
    /// a random 10–30 minutes. Err only when Emergency Stop interrupts
    /// the rest.
    async fn maybe_auto_break(
        &self,
        antiban: &AntiBanConfig,
        rng: &mut StdRng,
    ) -> Result<(), String> {
        if !antiban.auto_break {
            return Ok(());
        }
        let break_sleep = {
            let mut st = self.break_state.lock().await;
            if st.next_break_at == 0 {
                st.next_break_at = antiban.break_after_target(rng);
            }
            st.sent_since_break += 1;
            if st.sent_since_break >= st.next_break_at {
                let reached = st.next_break_at;
                let dur = antiban.break_duration_secs(rng);
                st.sent_since_break = 0;
                st.next_break_at = antiban.break_after_target(rng);
                Some((reached, Duration::from_secs(dur)))
            } else {
                None
            }
        };
        if let Some((reached, d)) = break_sleep {
            tracing::info!(
                "auto-break: resting {} minutes after {} sends",
                d.as_secs() / 60,
                reached
            );
            if self.sleep_interruptible(d).await {
                return Err("emergency stop pressed during auto-break".into());
            }
        }
        Ok(())
    }

    /// Execute the first `ActionSendCommand` node of a scenario.
    /// Prefix commands flow through the normal message pipeline
    /// (template vars, sanitize, slot, typing, retry, auto-delete,
    /// auto-break) — but NOT Smart Templates variation, which would
    /// corrupt a command (random emoji / case flips). Slash commands
    /// use the interaction endpoint instead (see `send_slash`).
    async fn send_command(
        &self,
        scenario: &Scenario,
        cfg: &CommandConfig,
        sender: &MessageSender,
        username: &str,
        server_name: &str,
        antiban: &AntiBanConfig,
    ) -> Result<String, String> {
        // Defensive: send_once already rejects commands in webhook mode.
        let MessageSender::User(client) = sender else {
            return Err("webhook mode cannot send commands".into());
        };
        let mut rng = StdRng::from_os_rng();
        if cfg.is_slash() {
            return self.send_slash(scenario, cfg, sender, antiban).await;
        }
        let mut vars: HashMap<String, String> = scenario
            .variables
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect();
        vars.extend(scenario.set_variable_overrides());
        let composed = cfg.compose_prefix();
        let rendered = render_template_with_vars(
            &composed,
            username,
            server_name,
            &scenario.channel_id,
            &vars,
        );
        let (clean, warnings) = sanitize_content(&rendered, antiban);
        for w in &warnings {
            tracing::warn!("anti-ban: {w}");
        }
        if clean.trim().is_empty() {
            return Err("command is empty after sanitising".into());
        }
        let wait_secs = scenario.wait_seconds();
        if wait_secs > 0 {
            let _ = self
                .sleep_interruptible(Duration::from_secs(wait_secs))
                .await;
            if self.emergency_stop.load(Ordering::SeqCst) {
                return Err("emergency stop pressed while waiting".into());
            }
        }
        self.wait_for_slot(&scenario.channel_id).await;
        if self.emergency_stop.load(Ordering::SeqCst) {
            return Err("emergency stop pressed while waiting".into());
        }
        if antiban.auto_typing {
            let _ = client.send_typing(&scenario.channel_id).await;
            let typing = Duration::from_secs(antiban.typing_secs(&mut rng));
            if self.sleep_interruptible(typing).await {
                return Err("emergency stop pressed while typing".into());
            }
        }
        // Prefix commands are user-mode only: no destination, no embed.
        self.send_message_with_retry(scenario, sender, None, &clean, &[], antiban, &mut rng)
            .await
    }

    /// Invoke a slash command (`POST /interactions`).
    /// Steps: resolve the guild from the channel, resolve the command
    /// ID/version (config, else the channel's application-commands
    /// search), get a gateway session (lazy connect), then invoke.
    /// Counts toward the daily cap like a message send.
    async fn send_slash(
        &self,
        scenario: &Scenario,
        cfg: &CommandConfig,
        sender: &MessageSender,
        antiban: &AntiBanConfig,
    ) -> Result<String, String> {
        // Defensive: send_once already rejects commands in webhook mode.
        let MessageSender::User(client) = sender else {
            return Err("webhook mode cannot send commands".into());
        };
        let mut rng = StdRng::from_os_rng();
        let wait_secs = scenario.wait_seconds();
        if wait_secs > 0 {
            let _ = self
                .sleep_interruptible(Duration::from_secs(wait_secs))
                .await;
            if self.emergency_stop.load(Ordering::SeqCst) {
                return Err("emergency stop pressed while waiting".into());
            }
        }
        self.wait_for_slot(&scenario.channel_id).await;
        if self.emergency_stop.load(Ordering::SeqCst) {
            return Err("emergency stop pressed while waiting".into());
        }
        // Guild of the channel (None for DMs — the payload omits it).
        let guild_id = match client.get_channel(&scenario.channel_id).await {
            Ok(ch) => ch.guild_id,
            Err(e) => {
                return Err(format!("cannot read channel for slash command: {e}"));
            }
        };
        // Command ID / version: config first, else auto-resolve by name.
        let (command_id, version) = if !cfg.command_id.trim().is_empty() {
            (cfg.command_id.trim().to_string(), cfg.version.clone())
        } else {
            match client
                .search_application_commands(&scenario.channel_id, cfg.name.trim())
                .await
            {
                Ok(found) => {
                    let hit = found.iter().find(|c| {
                        c.name == cfg.name.trim()
                            && (cfg.application_id.trim().is_empty()
                                || c.application_id == cfg.application_id.trim())
                    });
                    match hit {
                        Some(h) => (h.id.clone(), h.version.clone()),
                        None => {
                            return Err(format!(
                                "slash command /{} not found in this channel — paste its Command ID manually in the node editor",
                                cfg.name.trim()
                            ));
                        }
                    }
                }
                Err(e) => {
                    return Err(format!(
                        "cannot search application commands ({e}) — paste the Command ID manually in the node editor"
                    ));
                }
            }
        };
        // Gateway session (lazy connect — only slash needs it).
        self.gateway.ensure_started(client.token().to_string());
        let session = match self.wait_for_session().await {
            Some(s) => s,
            None => {
                // Fallback: one-shot connect (the background task may be
                // wedged or still backing off) instead of failing outright.
                match Gateway::connect_once(client.token(), Duration::from_secs(15)).await {
                    Ok(s) => s,
                    Err(e) => {
                        return Err(format!(
                            "không kết nối được gateway — lệnh slash cần phiên gateway ({e}); kiểm tra mạng rồi thử lại"
                        ));
                    }
                }
            }
        };
        // Options: name/type/value triples from the node config.
        let options: Vec<(String, u8, String)> = cfg
            .options
            .iter()
            .map(|o| (o.name.clone(), o.kind, o.value.clone()))
            .collect();
        let payload = build_interaction_payload(
            cfg.application_id.trim(),
            guild_id.as_deref(),
            &scenario.channel_id,
            &session,
            &make_nonce(),
            &command_id,
            cfg.name.trim(),
            &version,
            &options,
        );
        // Retry like a message send: 429 waits, 401/403 stop at once.
        let mut attempt = 0;
        loop {
            attempt += 1;
            match client.invoke_interaction(payload.clone()).await {
                Ok(()) => {
                    *self.sent_today.lock().await += 1;
                    let _ = self.store.log_send(&SendLogEntry {
                        scenario_id: scenario.id.clone(),
                        scenario_name: scenario.name.clone(),
                        channel_id: scenario.channel_id.clone(),
                        status: "sent".into(),
                        detail: format!("slash /{}", cfg.name.trim()),
                        message_id: String::new(),
                        destination_id: String::new(),
                    });
                    self.maybe_auto_break(antiban, &mut rng).await?;
                    return Ok(format!("slash /{}", cfg.name.trim()));
                }
                Err(DiscordError::RateLimited {
                    retry_after_secs,
                    global,
                }) => {
                    tracing::warn!(
                        "Discord 429 on slash, waiting {retry_after_secs}s (global={global})"
                    );
                    let paused = if global {
                        self.sleep_interruptible(Duration::from_secs(3600)).await
                    } else {
                        self.sleep_interruptible(Duration::from_secs(retry_after_secs.max(1)))
                            .await
                    };
                    if paused {
                        return Err("emergency stop pressed during rate-limit pause".into());
                    }
                    if attempt >= 3 {
                        return Err(format!(
                            "rate limited repeatedly (last retry_after {retry_after_secs}s)"
                        ));
                    }
                }
                Err(DiscordError::Unauthorized(m))
                | Err(DiscordError::Forbidden(m))
                | Err(DiscordError::NotFound(m)) => {
                    return Err(format!("stopped immediately: {m}"));
                }
                Err(DiscordError::Other(m)) => {
                    if attempt >= 3 {
                        return Err(format!("failed after 3 attempts: {m}"));
                    }
                    let backoff = 2u64.pow(attempt) * 2;
                    if self.sleep_interruptible(Duration::from_secs(backoff)).await {
                        return Err("emergency stop pressed during retry backoff".into());
                    }
                }
            }
        }
    }

    /// Wait up to SESSION_WAIT_SECS for the gateway session id,
    /// polling every 500ms. None on timeout or Emergency Stop.
    async fn wait_for_session(&self) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(SESSION_WAIT_SECS);
        loop {
            if let Some(s) = self.gateway.session_id().await {
                return Some(s);
            }
            if self.emergency_stop.load(Ordering::SeqCst) {
                return None;
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Main loop: every second, fire scenarios whose schedule is due.
    /// `get_sender` supplies a fresh sender; `scenarios` is re-read each tick
    /// so edits in the UI take effect without restarting the engine.
    /// Main engine loop. `resolve_sender` maps each scenario to its
    /// sender plus its destination (Some in webhook mode, None in
    /// user-token mode). One loop only (atomic guard); destinations
    /// are processed sequentially in a stable order, with a fixed
    /// 1-second stagger between webhook sends in one tick.
    pub async fn run_loop(
        self: Arc<Self>,
        resolve_sender: impl Fn(&Scenario) -> Option<(MessageSender, Option<Destination>)>
            + Send
            + Sync
            + 'static,
        scenarios_fn: impl Fn() -> Vec<Scenario> + Send + Sync + 'static,
    ) {
        // Single-loop guard: refuse a second concurrent loop (e.g. a
        // double Start click, or a Start during wind-down after Stop).
        if !self.try_claim_loop() {
            tracing::warn!("engine loop already active; refusing a second loop");
            return;
        }
        self.running.store(true, Ordering::SeqCst);
        // Authoritative last-run state: scenario id -> (schedule label,
        // last scheduled time that fired). Persisted to SQLite on every
        // fire, so a restart resumes exactly where it left off.
        let mut last_runs: HashMap<String, (String, Option<DateTime<Utc>>)> = HashMap::new();
        let mut consecutive_errors: u32 = 0;
        // No startup identity fetch: user-mode identity is refreshed
        // per scenario below (throttled), and webhook-mode identity
        // comes from the stored destination (no network needed).
        while self.running.load(Ordering::SeqCst) && !self.emergency_stop.load(Ordering::SeqCst) {
            let now = Local::now();
            let scenarios = scenarios_fn();
            // Only the first distinct channel may send.
            let allowed_channel: Option<String> = scenarios
                .iter()
                .filter(|s| s.enabled && !s.channel_id.is_empty())
                .map(|s| s.channel_id.clone())
                .min();
            for mut scenario in scenarios.into_iter().filter(|s| s.enabled) {
                // Initialise creation time once, and reset last-run when
                // the schedule itself was edited.
                if scenario.created_at.is_none() {
                    scenario.created_at = Some(Utc::now());
                    let _ = self.store.save_scenario(&scenario);
                }
                let label = scenario
                    .schedule
                    .as_ref()
                    .map(|s| s.label())
                    .unwrap_or_default();
                let entry = last_runs
                    .entry(scenario.id.clone())
                    .or_insert_with(|| (label.clone(), scenario.last_run_at));
                if entry.0 != label {
                    entry.0 = label;
                    entry.1 = None;
                    scenario.last_run_at = None;
                    let _ = self.store.save_scenario(&scenario);
                }
                let last_run = entry.1;
                let Some(schedule) = scenario.schedule.clone() else {
                    continue;
                };
                let Some(due) = due_time(
                    &schedule,
                    last_run,
                    scenario.created_at,
                    now,
                    &scenario.timezone,
                ) else {
                    continue;
                };
                let Some((sender, destination)) = resolve_sender(&scenario) else {
                    continue;
                };
                // Only the first distinct channel may send (user mode).
                // Webhook mode ignores channel_id: the URL is channel-bound.
                if destination.is_none() && !sender.is_webhook() {
                    if let Some(allowed) = &allowed_channel {
                        if &scenario.channel_id != allowed {
                            tracing::warn!(
                                "scenario {} uses channel {} but only {} may send — skipped",
                                scenario.name,
                                scenario.channel_id,
                                allowed
                            );
                            continue;
                        }
                    }
                }
                // Template identity: per-destination in webhook mode
                // (stored name, no network), global account identity
                // in user mode (refreshed when still the fallback).
                let (username, server_name) = if let Some(dest) = &destination {
                    (dest.effective_username(), String::new())
                } else {
                    if self.identity().await.0 == "you" {
                        if let MessageSender::User(client) = &sender {
                            self.refresh_identity(client).await;
                        }
                    }
                    self.identity().await
                };
                match self
                    .send_once(
                        &scenario,
                        &sender,
                        destination.as_ref(),
                        &username,
                        &server_name,
                    )
                    .await
                {
                    Ok(_) => {
                        consecutive_errors = 0;
                        // Consume this scheduled run exactly once.
                        let due_utc = due.with_timezone(&Utc);
                        entry.1 = Some(due_utc);
                        scenario.last_run_at = Some(due_utc);
                        if matches!(schedule, Schedule::Once { .. }) {
                            scenario.enabled = false;
                            tracing::info!(
                                "Once scenario '{}' fired — disabling it",
                                scenario.name
                            );
                        }
                        let _ = self.store.save_scenario(&scenario);
                        // Fixed 1-second stagger between webhook sends
                        // in one tick (transparent, never random).
                        if destination.is_some()
                            && self.sleep_interruptible(Duration::from_secs(1)).await
                        {
                            break;
                        }
                    }
                    Err(e) if e.starts_with("defer:") => {
                        // Short wait (interval / 429 cooldown / global
                        // pause): do NOT consume the run, count an
                        // error, or log a failure. The run retries on
                        // a later tick, slightly late.
                        tracing::info!("scenario {} deferred: {e}", scenario.name);
                    }
                    Err(e) if e.starts_with("skip:") => {
                        // Intentional skip (sleep window / cap / time
                        // window): consume the run so it is not retried
                        // every second, but do not count it as a failure.
                        tracing::info!("scenario {} skipped: {e}", scenario.name);
                        let due_utc = due.with_timezone(&Utc);
                        entry.1 = Some(due_utc);
                        scenario.last_run_at = Some(due_utc);
                        if matches!(schedule, Schedule::Once { .. }) {
                            scenario.enabled = false;
                        }
                        let _ = self.store.save_scenario(&scenario);
                    }
                    Err(e) => {
                        tracing::warn!("scenario {} failed: {e}", scenario.name);
                        // Persist failures so Logs can filter by status
                        // and the Dashboard can count them (QoL). The
                        // detail is truncated: Discord error bodies can
                        // be long, and the log is for diagnosis, not a
                        // transcript. Errors never contain credentials.
                        let detail: String = e.chars().take(1000).collect();
                        let _ = self.store.log_send(&SendLogEntry {
                            scenario_id: scenario.id.clone(),
                            scenario_name: scenario.name.clone(),
                            channel_id: scenario.channel_id.clone(),
                            status: "failed".into(),
                            detail,
                            message_id: String::new(),
                            destination_id: destination
                                .as_ref()
                                .map(|d| d.id.clone())
                                .unwrap_or_default(),
                        });
                        crate::notify::play(crate::notify::SoundEvent::SendFailure);
                        consecutive_errors += 1;
                        // Per-destination consecutive errors: pause only
                        // that destination when it crosses the limit.
                        // Never overwrite a 404/401 pause set by the
                        // send path itself.
                        if let Some(dest) = &destination {
                            if self.record_dest_error(&dest.id) {
                                if let Ok(Some(current)) = self.store.load_destination(&dest.id) {
                                    if current.failure_state == FailureState::Ok {
                                        let _ = self.store.set_destination_failure(
                                            &dest.id,
                                            FailureState::PausedErrors,
                                            &e,
                                        );
                                        crate::notify::play(
                                            crate::notify::SoundEvent::DestinationPaused,
                                        );
                                    }
                                }
                            }
                        }
                        // Spec §6: 3 consecutive errors pauses the engine.
                        // Interruptible: Emergency Stop must not wait
                        // out the full 10 minutes.
                        if consecutive_errors >= 3 {
                            tracing::error!("3 consecutive errors — pausing engine for 10 min");
                            if self.sleep_interruptible(Duration::from_secs(600)).await {
                                break;
                            }
                            consecutive_errors = 0;
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        self.running.store(false, Ordering::SeqCst);
        self.release_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Build a UTC instant (never via Local wall-clock: these tests must
    /// pass under any machine timezone).
    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn cap_resets_at_utc_midnight_not_local_midnight() {
        let last_day = NaiveDate::from_ymd_opt(2026, 11, 16).unwrap();
        // 23:30 UTC on 16 Nov is already 06:30 on 17 Nov in +07 — the
        // local day has rolled over, but the UTC day has not: no reset.
        assert!(!utc_day_rolled_over(last_day, utc(2026, 11, 16, 23, 30)));
        // 17:00 UTC on 16 Nov is exactly 00:00 on 17 Nov in +07 (local
        // midnight) — still the same UTC day: no reset.
        assert!(!utc_day_rolled_over(last_day, utc(2026, 11, 16, 17, 0)));
        // 00:30 UTC on 17 Nov: the UTC day rolled over — reset.
        assert!(utc_day_rolled_over(last_day, utc(2026, 11, 17, 0, 30)));
        // Same UTC day, midday: no reset.
        assert!(!utc_day_rolled_over(last_day, utc(2026, 11, 16, 12, 0)));
        // A new UTC day also resets after a local midnight in -05
        // (05:00 UTC is 00:00 in -05, still the same UTC day: no reset).
        assert!(!utc_day_rolled_over(last_day, utc(2026, 11, 16, 5, 0)));
    }

    #[test]
    fn one_distinct_channel_enforced() {
        let mut a = Scenario::new("a");
        a.enabled = true;
        a.channel_id = "111".to_string();
        let mut b = Scenario::new("b");
        b.enabled = true;
        b.channel_id = "222".to_string();
        assert!(channels_violation(&[a.clone(), b.clone()]).is_some());
        // Same channel is fine.
        b.channel_id = "111".to_string();
        assert!(channels_violation(&[a.clone(), b.clone()]).is_none());
        // Disabled scenarios don't count.
        b.enabled = false;
        b.channel_id = "222".to_string();
        assert!(channels_violation(&[a.clone(), b.clone()]).is_none());
        // Empty channel ids don't count.
        let c = Scenario::new("c");
        assert!(channels_violation(&[a, c]).is_none());
    }

    #[test]
    fn loop_claim_is_exclusive_until_released() {
        let store = Arc::new(Store::open_memory().unwrap());
        let engine = Engine::new(store, AntiBanConfig::default(), 300, 5);
        assert!(engine.try_claim_loop());
        assert!(!engine.try_claim_loop());
        assert!(engine.is_loop_active());
        engine.release_loop();
        assert!(!engine.is_loop_active());
        assert!(engine.try_claim_loop());
        engine.release_loop();
    }

    #[test]
    fn cooldown_and_error_tracking() {
        let store = Arc::new(Store::open_memory().unwrap());
        let engine = Engine::new(store, AntiBanConfig::default(), 300, 3);
        assert!(!engine.is_globally_paused());
        engine.set_global_pause(60);
        assert!(engine.is_globally_paused());
        assert!(engine.cooldown_remaining("d1").is_none());
        engine.set_cooldown("d1", 30);
        assert!(engine.cooldown_remaining("d1").is_some());
        assert!(engine.cooldown_remaining("d2").is_none());
        assert!(!engine.record_dest_error("d1"));
        assert!(!engine.record_dest_error("d1"));
        assert!(engine.record_dest_error("d1"));
        engine.reset_dest_errors("d1");
        assert!(!engine.record_dest_error("d1"));
    }
}
