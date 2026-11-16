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
    discord::{DiscordClient, DiscordError},
    license::Plan,
    scenario::{due_time, render_template_with_vars, spin_text, Scenario, Schedule},
    store::Store,
};
use chrono::{DateTime, Local, Utc};
use rand::{rngs::StdRng, SeedableRng};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
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
}

/// Free-plan rule: at most 1 distinct channel across all *enabled*
/// scenarios. Returns the offending description, if any.
pub fn free_channels_violation(scenarios: &[Scenario], plan: Plan) -> Option<String> {
    if plan != Plan::Free {
        return None;
    }
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
            "Free plan allows 1 channel, but enabled scenarios use {} distinct channels",
            channels.len()
        ))
    } else {
        None
    }
}

pub struct Engine {
    pub store: Arc<Store>,
    pub antiban: AntiBanConfig,
    /// Last send time per channel (enforces the per-channel interval).
    last_sent: Mutex<HashMap<String, Instant>>,
    /// Last message index sent per scenario (rotation never repeats).
    last_msg_index: Mutex<HashMap<String, usize>>,
    /// Messages sent today per account (daily cap). Initialised from the
    /// persisted send log, so a restart does not reset the count.
    sent_today: Mutex<u32>,
    last_day: Mutex<chrono::NaiveDate>,
    pub emergency_stop: Arc<AtomicBool>,
    pub running: Arc<AtomicBool>,
    /// Plan cap (Free 100, Pro/Team 1000). Set from the license plan.
    /// Plain std Mutex: read once per tick / per send, never held across
    /// an await, so the UI thread can also update it synchronously.
    plan_daily_cap: std::sync::Mutex<u32>,
    plan: std::sync::Mutex<Plan>,
    /// Real identity for template vars, fetched once at engine start
    /// (`get_me` + first guild). Falls back to ("you", "server") only
    /// when that call fails, and the failure is logged.
    identity: Mutex<(String, String)>,
    identity_last_try: Mutex<Option<Instant>>,
}

impl Engine {
    pub fn new(store: Arc<Store>, antiban: AntiBanConfig) -> Self {
        let sent = store.count_sends_today();
        Self {
            store,
            antiban,
            last_sent: Mutex::new(HashMap::new()),
            last_msg_index: Mutex::new(HashMap::new()),
            sent_today: Mutex::new(sent),
            last_day: Mutex::new(Local::now().date_naive()),
            emergency_stop: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
            plan_daily_cap: std::sync::Mutex::new(Plan::Free.daily_cap()),
            plan: std::sync::Mutex::new(Plan::Free),
            identity: Mutex::new(("you".to_string(), "server".to_string())),
            identity_last_try: Mutex::new(None),
        }
    }

    pub fn set_plan(&self, plan: Plan) {
        if let Ok(mut c) = self.plan_daily_cap.lock() {
            *c = plan.daily_cap();
        }
        if let Ok(mut p) = self.plan.lock() {
            *p = plan;
        }
    }

    pub fn current_plan(&self) -> Plan {
        self.plan.lock().map(|p| *p).unwrap_or(Plan::Free)
    }

    pub fn stop_all(&self) {
        self.emergency_stop.store(true, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
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
        let interval = Duration::from_secs(self.antiban.effective_interval_secs());
        let mut rng = StdRng::from_os_rng();
        let jitter = Duration::from_secs(self.antiban.jitter_secs(&mut rng));
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
            tokio::time::sleep(wait).await;
        }
    }

    /// Send one scenario's next message, applying all anti-ban layers.
    /// Returns Ok(message_id) or Err(reason). Errors starting with
    /// `skip: ` are intentional skips (sleep window, daily cap, time
    /// window) — the caller must not count them as failures.
    /// Never panics on Discord errors.
    pub async fn send_once(
        &self,
        scenario: &Scenario,
        client: &DiscordClient,
        username: &str,
        server_name: &str,
    ) -> Result<String, String> {
        if self.emergency_stop.load(Ordering::SeqCst) {
            return Err("emergency stop is active".into());
        }
        // Layer 5: sleep window.
        if self.antiban.in_sleep_window(Local::now()) {
            return Err("skip: inside night sleep window".into());
        }
        scenario
            .validate()
            .map_err(|e| format!("invalid scenario: {e}"))?;
        // ConditionTimeWindow nodes: outside the window = skip, not error.
        if let Err(e) = scenario.check_time_windows(Local::now()) {
            return Err(format!("skip: {e}"));
        }
        // Daily cap: plan cap with warm-up, additionally capped by the
        // per-scenario limit when one is set (0 = no per-scenario limit).
        // The persisted send count is the source of truth for "sent today".
        {
            let today = Local::now().date_naive();
            let mut day = self.last_day.lock().await;
            if *day != today {
                *day = today;
                *self.sent_today.lock().await = self.store.count_sends_today();
            }
            let sent = *self.sent_today.lock().await;
            let base_cap = self
                .plan_daily_cap
                .lock()
                .map(|c| *c)
                .unwrap_or(Plan::Free.daily_cap());
            let age = self.store.account_age_days();
            let plan_eff = self.antiban.effective_daily_cap(base_cap, age);
            let scenario_cap = if scenario.limits.daily_cap == 0 {
                u32::MAX
            } else {
                scenario.limits.daily_cap
            };
            let eff = plan_eff.min(scenario_cap);
            if sent >= eff {
                return Err(format!(
                    "skip: daily cap reached ({sent}/{eff}, warm-up age {age}d)"
                ));
            }
        }

        // Node execution (in node order): a Send Message / Send Embed node
        // with non-empty content wins over the plain message list.
        // UtilityWait nodes add an explicit delay before sending.
        let wait_secs = scenario.wait_seconds();
        let mut rng = StdRng::from_os_rng();
        let raw = if let Some(node_msg) = scenario.node_message() {
            node_msg
        } else {
            let last_idx = self.last_msg_index.lock().await.get(&scenario.id).copied();
            let (idx, msg) = scenario
                .pick_message_with_index(last_idx, &mut rng)
                .ok_or_else(|| "scenario has no messages".to_string())?;
            self.last_msg_index
                .lock()
                .await
                .insert(scenario.id.clone(), idx);
            msg
        };
        let spun = spin_text(&raw, &mut rng);
        let mut vars: HashMap<String, String> = scenario
            .variables
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect();
        vars.extend(scenario.set_variable_overrides());
        let rendered =
            render_template_with_vars(&spun, username, server_name, &scenario.channel_id, &vars);
        // Layer 4: content checks.
        let (clean, warnings) = sanitize_content(&rendered, &self.antiban);
        for w in &warnings {
            tracing::warn!("anti-ban: {w}");
        }
        if clean.trim().is_empty() {
            return Err("message is empty after sanitising".into());
        }

        if wait_secs > 0 {
            tokio::time::sleep(Duration::from_secs(wait_secs)).await;
            if self.emergency_stop.load(Ordering::SeqCst) {
                return Err("emergency stop pressed while waiting".into());
            }
        }
        self.wait_for_slot(&scenario.channel_id).await;
        if self.emergency_stop.load(Ordering::SeqCst) {
            return Err("emergency stop pressed while waiting".into());
        }

        // Retry: network errors up to 3x with backoff; 429 waits retry_after;
        // 401/403 stop immediately (Spec §6).
        let mut attempt = 0;
        loop {
            attempt += 1;
            match client.send_message(&scenario.channel_id, &clean).await {
                Ok(sent) => {
                    *self.sent_today.lock().await += 1;
                    let entry = SendLogEntry {
                        scenario_id: scenario.id.clone(),
                        scenario_name: scenario.name.clone(),
                        channel_id: scenario.channel_id.clone(),
                        status: "sent".into(),
                        detail: format!("message {}", sent.id),
                    };
                    let _ = self.store.log_send(&entry);
                    return Ok(sent.id);
                }
                Err(DiscordError::RateLimited {
                    retry_after_secs,
                    global,
                }) => {
                    tracing::warn!("Discord 429, waiting {retry_after_secs}s (global={global})");
                    if global {
                        // Spec §6: a global limit warning pauses everything 1h.
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                    } else {
                        tokio::time::sleep(Duration::from_secs(retry_after_secs.max(1))).await;
                    }
                    if attempt >= 3 {
                        return Err(format!(
                            "rate limited repeatedly (last retry_after {retry_after_secs}s)"
                        ));
                    }
                }
                Err(DiscordError::Unauthorized(m)) | Err(DiscordError::Forbidden(m)) => {
                    return Err(format!("stopped immediately: {m}"));
                }
                Err(DiscordError::Other(m)) => {
                    if attempt >= 3 {
                        return Err(format!("failed after 3 attempts: {m}"));
                    }
                    let backoff = 2u64.pow(attempt) * 2;
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                }
            }
        }
    }

    /// Main loop: every second, fire scenarios whose schedule is due.
    /// `get_token` supplies a fresh client; `scenarios` is re-read each tick
    /// so edits in the UI take effect without restarting the engine.
    pub async fn run_loop(
        self: Arc<Self>,
        get_client: impl Fn() -> Option<DiscordClient> + Send + Sync + 'static,
        scenarios_fn: impl Fn() -> Vec<Scenario> + Send + Sync + 'static,
    ) {
        self.running.store(true, Ordering::SeqCst);
        // Authoritative last-run state: scenario id -> (schedule label,
        // last scheduled time that fired). Persisted to SQLite on every
        // fire, so a restart resumes exactly where it left off.
        let mut last_runs: HashMap<String, (String, Option<DateTime<Utc>>)> = HashMap::new();
        let mut consecutive_errors: u32 = 0;
        // Fetch the real identity once at startup (retried ≤1/min on failure).
        if let Some(client) = get_client() {
            self.refresh_identity(&client).await;
        }
        while self.running.load(Ordering::SeqCst) && !self.emergency_stop.load(Ordering::SeqCst) {
            let now = Local::now();
            let scenarios = scenarios_fn();
            // Free plan: only the first distinct channel may send.
            let plan = self.current_plan();
            let allowed_channel: Option<String> = if plan == Plan::Free {
                scenarios
                    .iter()
                    .filter(|s| s.enabled && !s.channel_id.is_empty())
                    .map(|s| s.channel_id.clone())
                    .min()
            } else {
                None
            };
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
                if plan == Plan::Free {
                    if let Some(allowed) = &allowed_channel {
                        if &scenario.channel_id != allowed {
                            tracing::warn!(
                                "Free plan: scenario {} uses channel {} but only {} may send — skipped",
                                scenario.name, scenario.channel_id, allowed
                            );
                            continue;
                        }
                    }
                }
                let Some(due) = due_time(
                    &schedule,
                    last_run,
                    scenario.created_at,
                    now,
                    &scenario.timezone,
                ) else {
                    continue;
                };
                let Some(client) = get_client() else {
                    continue;
                };
                // Refresh identity if it is still the fallback.
                if self.identity().await.0 == "you" {
                    self.refresh_identity(&client).await;
                }
                let (username, server_name) = self.identity().await;
                match self
                    .send_once(&scenario, &client, &username, &server_name)
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
                        consecutive_errors += 1;
                        // Spec §6: 3 consecutive errors pauses the engine.
                        if consecutive_errors >= 3 {
                            tracing::error!("3 consecutive errors — pausing engine for 10 min");
                            tokio::time::sleep(Duration::from_secs(600)).await;
                            consecutive_errors = 0;
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        self.running.store(false, Ordering::SeqCst);
    }
}
