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
    scenario::{
        due_time, render_template_with_vars, ApiCallConfig, CommandConfig, Node, NodeKind,
        Scenario, Schedule,
    },
    spintax::{apply_spintax, vary_message},
    store::Store,
    webhook::MessageSender,
};
use chrono::{DateTime, Local, NaiveDate, Timelike, Utc};
use rand::{rngs::StdRng, SeedableRng};
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
            "Nicord cho phép 1 kênh, nhưng các kịch bản đang bật dùng {} kênh khác nhau",
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
    /// API host allowlist (idea #5), live-editable the same way:
    /// the UI writes the list, the engine reads a clone per fetch.
    /// Empty = any HTTPS host.
    pub api_allowed_hosts: std::sync::RwLock<Vec<String>>,
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
    /// Auto Break progress.
    break_state: Mutex<BreakState>,
    /// Gateway client for slash commands (lazy: connects only when a
    /// slash command actually fires).
    gateway: Arc<Gateway>,
    /// Guild id per channel, resolved from `GET /channels/{id}` the
    /// first time a slash command fires in that channel. Session-only
    /// (a restart re-resolves): a failed channel read falls back to
    /// the cached value, and only fails the send when both are
    /// missing. None = a DM channel (no guild).
    guild_cache: std::sync::Mutex<HashMap<String, Option<String>>>,
    /// Failed sends this session (dashboard success rate). Skips
    /// ("skip: ...") are not failures and are never counted here.
    /// Messages deleted by Auto Delete this session.
    deleted_today: std::sync::Mutex<u32>,
    /// When this engine was created (dashboard send rate / uptime).
    pub started_at: Instant,
    /// Last run-state persistence failure (M2): a failed
    /// last-run save means a restart re-fires the run, so the
    /// error is remembered for the Dashboard health card until a
    /// later save succeeds. None = no failure recorded.
    last_persist_error: std::sync::Mutex<Option<String>>,
    /// End of the 10-minute engine pause after 3 consecutive
    /// errors (F-GUI-4). Runtime-only on purpose: a restart
    /// resets the circuit breaker, like a fresh engine. None =
    /// not paused. The UI reads it so the pause is visible
    /// instead of silently looking like "Running".
    error_pause_until: std::sync::Mutex<Option<Instant>>,
}

/// Human-readable byte size ("340 B", "2.1 KB", "9.0 MB") for
/// attachment status lines. Shared with the Builder UI.
pub(crate) fn format_bytes(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

/// Read attachment files for one send (idea #4). Returns
/// (files, skipped): a missing/unreadable/oversize/non-file path
/// is skipped with its reason — never fatal, never a crash. Only
/// the file NAME (never the full path) is reported, to keep local
/// paths out of logs. At most MAX_ATTACHMENTS files are read
/// (validation enforces the same cap; this clamps defensively for
/// hand-built scenarios). Synchronous reads: files are local and
/// capped at 8 MiB each, so the block is short.
fn load_attachments(paths: &[String]) -> (Vec<crate::discord::AttachedFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for raw in paths.iter().take(crate::discord::MAX_ATTACHMENTS) {
        let path = raw.trim();
        if path.is_empty() {
            continue;
        }
        let name = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "(file chưa đặt tên)".to_string());
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                skipped.push(format!("{name} (không tìm thấy)"));
                continue;
            }
        };
        if !meta.is_file() {
            skipped.push(format!("{name} (không phải file)"));
            continue;
        }
        if meta.len() > crate::discord::MAX_ATTACHMENT_BYTES {
            skipped.push(format!(
                "{name} (vượt giới hạn 8 MB: {})",
                format_bytes(meta.len())
            ));
            continue;
        }
        match std::fs::read(path) {
            Ok(bytes) => {
                // Re-check after the read: the file may have grown
                // between the metadata check and the read.
                if bytes.len() as u64 > crate::discord::MAX_ATTACHMENT_BYTES {
                    skipped.push(format!(
                        "{name} (vượt giới hạn 8 MB: {})",
                        format_bytes(bytes.len() as u64)
                    ));
                    continue;
                }
                files.push(crate::discord::AttachedFile {
                    filename: name.clone(),
                    content_type: crate::discord::guess_content_type(&name).to_string(),
                    bytes,
                });
            }
            Err(e) => skipped.push(format!("{name} (không đọc được: {e})")),
        }
    }
    (files, skipped)
}

/// Storage key for a scenario's last message id (idea #2): the
/// destination id in webhook mode, the channel id in user-token
/// mode. One entry per target, so broadcast + edit mode track
/// each destination's living message separately.
fn edit_key(destination: Option<&Destination>, channel_id: &str) -> String {
    destination
        .map(|d| d.id.clone())
        .unwrap_or_else(|| channel_id.to_string())
}

/// One target's result inside a broadcast firing (idea #1).
struct TargetResult {
    /// Display label for error detail ("account" in user-token
    /// mode, the destination's display name in webhook mode).
    label: String,
    /// Destination id ("" in user-token mode) for the log row.
    destination_id: String,
    /// Channel id for the log row (destination channel in webhook
    /// mode, scenario channel in user-token mode).
    channel_id: String,
    /// Edit-in-place storage key (idea #2).
    edit_key: String,
    result: Result<String, String>,
}

/// Outcome of one broadcast firing: per-target results folded into
/// the run loop's existing error taxonomy, so one firing consumes
/// exactly one run and one destination's failure never blocks the
/// others.
enum BroadcastOutcome {
    /// At least one target accepted the send.
    Success,
    /// Every attempted target deferred (429 cooldown / min interval).
    Deferred(String),
    /// Every attempted target skipped (cap / quiet window / disabled).
    Skipped(String),
    /// The scenario itself is invalid.
    Invalid(String),
    /// Every attempted target failed with a hard error. Carries the
    /// combined detail naming every target.
    Failed(String),
}

/// Whether an error is a hard failure (counts toward error
/// limits), as opposed to the run loop's defer/skip/invalid
/// taxonomy.
fn is_hard_broadcast_error(e: &str) -> bool {
    !(e.starts_with("defer:") || e.starts_with("skip:") || e.starts_with("invalid scenario:"))
}

/// Demote a "defer:" error to a hard error once earlier commands
/// already ran in this firing: retrying the firing would re-run
/// them. With nothing run yet, the "defer:" passes through so the
/// run retries later as before (see `Engine::send_commands`).
fn demote_defer_after_partial(ran: usize, e: String) -> String {
    if ran == 0 {
        return e;
    }
    match e.strip_prefix("defer:") {
        Some(rest) => format!(
            "đã chạy {ran} lệnh rồi, lệnh tiếp theo bị hoãn nên không chạy lại được: {}",
            rest.trim()
        ),
        None => e,
    }
}

/// Fold per-target results into one outcome. Precedence: any
/// success wins; otherwise a defer wins (a retryable target must
/// never be consumed as failed); otherwise a hard failure wins
/// (it must surface and count); skip and invalid consume quietly.
/// `results` must be non-empty (the caller skips the firing when
/// nothing was attempted).
fn aggregate_broadcast(results: &[TargetResult]) -> BroadcastOutcome {
    if results.iter().any(|r| r.result.is_ok()) {
        return BroadcastOutcome::Success;
    }
    let first_with = |prefix: &str| {
        results
            .iter()
            .filter_map(|r| r.result.as_ref().err())
            .find(|e| e.starts_with(prefix))
            .cloned()
    };
    if let Some(e) = first_with("defer:") {
        return BroadcastOutcome::Deferred(e);
    }
    if results
        .iter()
        .filter_map(|r| r.result.as_ref().err())
        .any(|e| is_hard_broadcast_error(e))
    {
        let combined = results
            .iter()
            .filter_map(|r| r.result.as_ref().err().map(|e| format!("{}: {e}", r.label)))
            .collect::<Vec<_>>()
            .join("; ");
        return BroadcastOutcome::Failed(format!(
            "gửi tới nhiều đích đến thất bại cho {} đích: {combined}",
            results.len()
        ));
    }
    if let Some(e) = first_with("skip:") {
        return BroadcastOutcome::Skipped(e);
    }
    if let Some(e) = first_with("invalid scenario:") {
        return BroadcastOutcome::Invalid(e);
    }
    // Unreachable: every result is Err, and any Err is either
    // defer/skip/invalid or hard. Kept as a defensive fallback so a
    // future error prefix can never silently consume a run.
    BroadcastOutcome::Failed("gửi tới nhiều đích đến thất bại: lỗi không rõ".to_string())
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
            api_allowed_hosts: std::sync::RwLock::new(Vec::new()),
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
            guild_cache: std::sync::Mutex::new(HashMap::new()),
            deleted_today: std::sync::Mutex::new(0),
            started_at: Instant::now(),
            last_persist_error: std::sync::Mutex::new(None),
            error_pause_until: std::sync::Mutex::new(None),
        }
    }

    /// Remember a persistence failure (M2): logged loudly and
    /// surfaced in the Dashboard health card until a later save
    /// succeeds. The last error wins.
    fn record_persist_error(&self, what: &str, err: &str) {
        tracing::error!("persistence failure ({what}): {err}");
        if let Ok(mut slot) = self.last_persist_error.lock() {
            *slot = Some(format!("{what}: {err}"));
        }
    }

    fn clear_persist_error(&self) {
        if let Ok(mut slot) = self.last_persist_error.lock() {
            *slot = None;
        }
    }

    /// The last recorded persistence failure, if any (M2). Read
    /// by the Dashboard health card. Cleared by the next
    /// successful run-state save.
    pub fn last_persist_error(&self) -> Option<String> {
        self.last_persist_error.lock().ok().and_then(|s| s.clone())
    }

    /// Time left in the 3-consecutive-errors engine pause, if
    /// the pause is still in effect (F-GUI-4). Read by the
    /// Dashboard and sidebar so the pause is visible. Expires
    /// by time comparison; no clearing needed.
    pub fn error_pause_remaining(&self) -> Option<Duration> {
        self.error_pause_until
            .lock()
            .ok()
            .and_then(|g| (*g).and_then(|t| t.checked_duration_since(Instant::now())))
    }

    fn set_error_pause(&self, secs: u64) {
        if let Ok(mut slot) = self.error_pause_until.lock() {
            *slot = Some(Instant::now() + Duration::from_secs(secs.max(1)));
        }
    }

    /// Save a scenario's run state, recording failures (M2)
    /// instead of ignoring them: a failed save means a restart
    /// re-fires the run (a duplicate send), so it must be
    /// visible — in the logs immediately, and in the Dashboard
    /// health card until a save succeeds.
    fn save_run_state(&self, scenario: &Scenario, what: &str) {
        match self.store.save_scenario(scenario) {
            Ok(()) => self.clear_persist_error(),
            Err(e) => self.record_persist_error(what, &e),
        }
    }

    /// Write a send-log row, logging failures instead of
    /// ignoring them (M2's sibling: a lost log row hides
    /// history, though it cannot cause a duplicate send, so it
    /// is not surfaced in the health card).
    fn write_log(&self, entry: &SendLogEntry) {
        if let Err(e) = self.store.log_send(entry) {
            tracing::error!("could not write send log: {e}");
        }
    }

    /// Current anti-ban config (clone, so the engine never holds the
    /// lock across an await).
    pub fn antiban(&self) -> AntiBanConfig {
        self.antiban.read().map(|c| c.clone()).unwrap_or_default()
    }

    /// Current API host allowlist (clone per fetch).
    pub fn api_allowlist(&self) -> Vec<String> {
        self.api_allowed_hosts
            .read()
            .map(|c| c.clone())
            .unwrap_or_default()
    }

    /// Replace the API host allowlist live (Settings Save). Takes
    /// effect on the next fetch — no engine restart needed.
    pub fn set_api_allowlist(&self, hosts: Vec<String>) {
        if let Ok(mut c) = self.api_allowed_hosts.write() {
            *c = hosts;
        }
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

    /// Write one failure row per hard-failed broadcast target and
    /// run per-destination error escalation for each. Shared by the
    /// partial-success and total-failure arms of the run loop, so
    /// Logs shows every destination's failure in both cases.
    /// Details are scrubbed before truncating so a URL straddling
    /// the cut cannot survive in part.
    fn log_broadcast_failures(&self, scenario: &Scenario, results: &[TargetResult]) {
        for r in results {
            let Err(e) = &r.result else {
                continue;
            };
            if !is_hard_broadcast_error(e) {
                continue;
            }
            let safe_err = crate::store::scrub_secrets(e);
            let detail: String = safe_err.chars().take(1000).collect();
            self.write_log(&SendLogEntry {
                scenario_id: scenario.id.clone(),
                scenario_name: scenario.name.clone(),
                channel_id: r.channel_id.clone(),
                status: "failed".into(),
                detail,
                message_id: String::new(),
                destination_id: r.destination_id.clone(),
            });
            if !r.destination_id.is_empty() {
                self.escalate_dest_errors(&r.destination_id, &safe_err);
            }
        }
    }

    /// Per-destination consecutive-error escalation: pause only that
    /// destination when it crosses the limit. Never overwrite a
    /// 404/401 pause set by the send path itself.
    fn escalate_dest_errors(&self, dest_id: &str, safe_err: &str) {
        if self.record_dest_error(dest_id) {
            if let Ok(Some(current)) = self.store.load_destination(dest_id) {
                if current.failure_state == FailureState::Ok {
                    let _ = self.store.set_destination_failure(
                        dest_id,
                        FailureState::PausedErrors,
                        safe_err,
                    );
                    crate::notify::play(crate::notify::SoundEvent::DestinationPaused);
                }
            }
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
    /// Execute the scenario's Call API nodes (idea #5), in node
    /// order, returning variable name → fetched value. Runs before
    /// template rendering so `{{variable}}` resolves to fetched
    /// data. On failure: the "skip" policy (default) writes a
    /// failure row — so a dying API is visible in Logs — and
    /// returns a "skip:" error (the turn is consumed, with no
    /// engine-pause escalation); the "empty" policy binds "" and
    /// lets the send proceed. The URL is resolved from the secret
    /// store here and never logged; errors name the host at most.
    async fn fetch_api_vars(
        &self,
        scenario: &Scenario,
        destination: Option<&Destination>,
    ) -> Result<HashMap<String, String>, String> {
        let mut out: HashMap<String, String> = HashMap::new();
        let allowlist = self.api_allowlist();
        let log_channel = destination
            .map(|d| d.channel_id.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| scenario.channel_id.clone());
        for node in &scenario.nodes {
            if node.kind != NodeKind::UtilityCallApi {
                continue;
            }
            let cfg = ApiCallConfig::from_node(node);
            // Structural validation, fail-closed: a node that
            // cannot validate must not fetch. (send_once already
            // validated the scenario; this guards direct callers
            // and config edits between validation and fetch.)
            if let Err(e) = cfg.validate() {
                return Err(format!(
                    "invalid scenario: bước lấy dữ liệu {}: {e}",
                    node.id
                ));
            }
            let host = if cfg.url_host.trim().is_empty() {
                "host đã cấu hình".to_string()
            } else {
                cfg.url_host.trim().to_string()
            };
            let fetched = match crate::secret_store::load_secret(&cfg.url_ref) {
                Ok(url) => {
                    crate::apidata::fetch_api_value(
                        &url,
                        cfg.field_path.trim(),
                        cfg.timeout_secs,
                        crate::apidata::API_MAX_BYTES,
                        &allowlist,
                    )
                    .await
                }
                Err(e) => Err(format!("không tải được địa chỉ đã lưu ({e})")),
            };
            match fetched {
                Ok(value) => {
                    out.insert(cfg.variable.trim().to_string(), value);
                }
                Err(e) => {
                    if cfg.effective_on_failure() == "empty" {
                        tracing::warn!(
                            "kịch bản '{}': lấy dữ liệu API cho {{{}}} (host {host}) thất bại: {e} — gửi với giá trị rỗng",
                            scenario.name,
                            cfg.variable.trim()
                        );
                        out.insert(cfg.variable.trim().to_string(), String::new());
                    } else {
                        let detail = format!(
                            "lấy dữ liệu API cho {{{}}} (host {host}) thất bại: {e}",
                            cfg.variable.trim()
                        );
                        self.write_log(&SendLogEntry {
                            scenario_id: scenario.id.clone(),
                            scenario_name: scenario.name.clone(),
                            channel_id: log_channel.clone(),
                            status: "failed".into(),
                            detail: detail.chars().take(1000).collect(),
                            message_id: String::new(),
                            destination_id: destination.map(|d| d.id.clone()).unwrap_or_default(),
                        });
                        return Err(format!("skip: {detail}"));
                    }
                }
            }
        }
        Ok(out)
    }

    pub async fn send_once(
        &self,
        scenario: &Scenario,
        sender: &MessageSender,
        destination: Option<&Destination>,
        username: &str,
        server_name: &str,
    ) -> Result<String, String> {
        if self.emergency_stop.load(Ordering::SeqCst) {
            return Err("dừng khẩn cấp đang bật".into());
        }
        // Global 429 pause blocks every send until it expires.
        if self.is_globally_paused() {
            return Err("defer: Discord đang tạm dừng toàn cục do giới hạn (429)".into());
        }
        // Destination pre-checks (webhook mode only).
        if let Some(dest) = destination {
            if !dest.enabled {
                return Err(format!("skip: đích đến '{}' đang tắt", dest.display_name()));
            }
            if dest.failure_state.is_paused() {
                return Err(format!(
                    "skip: đích đến '{}' đang tạm dừng ({}) — tiếp tục trong màn hình Đích đến",
                    dest.display_name(),
                    dest.failure_state.label()
                ));
            }
            if dest.encrypted_url.trim().is_empty() {
                return Err(format!(
                    "skip: đích đến '{}' chưa có webhook URL",
                    dest.display_name()
                ));
            }
            // Quiet window (fail-closed on a broken window).
            let now_local = Local::now();
            let minutes = now_local.hour() * 60 + now_local.minute();
            if dest.is_quiet_at(minutes) {
                return Err(format!(
                    "skip: đích đến '{}' đang trong giờ yên tĩnh",
                    dest.display_name()
                ));
            }
            // Per-destination 429 cooldown: defer, do not consume.
            if let Some(remaining) = self.cooldown_remaining(&dest.id) {
                return Err(format!(
                    "defer: đích đến '{}' đang chờ hết giới hạn {}s (429)",
                    dest.display_name(),
                    remaining.as_secs()
                ));
            }
            // Per-destination daily cap (DB count, UTC date), counted
            // by destination id: unconditional — the old channel-id
            // count silently skipped the cap when the channel id was
            // empty or stale.
            let effective_cap = if dest.daily_cap == 0 {
                self.global_daily_cap.load(Ordering::SeqCst)
            } else {
                dest.daily_cap
            };
            let sent = self.store.count_sends_today_for_destination(&dest.id);
            if sent >= effective_cap {
                return Err(format!(
                    "skip: đích đến '{}' đã đạt giới hạn ngày ({sent}/{effective_cap})",
                    dest.display_name()
                ));
            }
        }
        // Live anti-ban config for this send (Settings edits apply
        // without restarting the engine).
        let antiban = self.antiban();
        // Layer 5: sleep window.
        if antiban.in_sleep_window(Local::now()) {
            return Err("skip: đang trong khung giờ ngủ đêm".into());
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
                    return Err(format!(
                        "skip: đã đạt giới hạn ngày toàn cục ({sent}/{eff})"
                    ));
                }
            } else {
                let base_cap = antiban.daily_cap;
                let age = self.store.account_age_days();
                let account_eff = antiban.effective_daily_cap(base_cap, age);
                let eff = account_eff.min(scenario_cap);
                if sent >= eff {
                    return Err(format!(
                        "skip: đã đạt giới hạn ngày ({sent}/{eff}, tài khoản đã làm quen {age} ngày)"
                    ));
                }
            }
        }

        // Send Command nodes take precedence over message nodes: one
        // scheduled firing runs ALL command nodes in node order (see
        // `send_commands` for the retry-safety rule). Webhook mode
        // rejects commands defensively here too, even though
        // validation above already rejects them.
        let cmd_nodes = scenario.command_nodes();
        if !cmd_nodes.is_empty() {
            if sender.is_webhook() {
                return Err(
                    "chế độ Webhook không gửi được lệnh — dùng kịch bản tin nhắn hoặc chuyển lại chế độ Tài khoản"
                        .into(),
                );
            }
            return self
                .send_commands(
                    scenario,
                    &cmd_nodes,
                    sender,
                    username,
                    server_name,
                    &antiban,
                )
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
                .ok_or_else(|| "kịch bản không có tin nhắn".to_string())?;
            self.last_msg_index
                .lock()
                .await
                .insert(scenario.id.clone(), idx);
            msg
        } else if let Some(dest) = destination {
            // Rotate through the destination defaults without
            // repeating the last pick (same rule as scenarios;
            // empty entries are never picked). When the
            // destination has no usable messages but the
            // scenario carries a poll, the content is empty —
            // Discord accepts a poll-only message.
            let last_idx = self.last_msg_index.lock().await.get(&scenario.id).copied();
            match dest.pick_default_message(last_idx, &mut rng) {
                Some((idx, msg)) => {
                    self.last_msg_index
                        .lock()
                        .await
                        .insert(scenario.id.clone(), idx);
                    msg
                }
                None if scenario.poll.is_some() => String::new(),
                None => {
                    return Err(
                            "kịch bản không có tin nhắn và đích đến không có tin nhắn mặc định dùng được"
                                .to_string(),
                        );
                }
            }
        } else if scenario.poll.is_some() {
            String::new()
        } else {
            return Err("kịch bản không có tin nhắn".to_string());
        };
        let mut vars: HashMap<String, String> = scenario
            .variables
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect();
        vars.extend(scenario.set_variable_overrides());
        // External data (idea #5): fetched values override
        // same-named scenario variables — fresher data wins.
        // A "skip" failure returns here, before any send.
        let api_vars = self.fetch_api_vars(scenario, destination).await?;
        vars.extend(api_vars);
        // Durable counters (idea #6): resolve the values the
        // template references BEFORE rendering; the increment
        // happens after a successful send, at the end of this fn.
        let counter_list = crate::scenario::counter_names(&raw);
        let counters: HashMap<String, u64> = counter_list
            .iter()
            .map(|n| (n.clone(), self.store.get_counter(n)))
            .collect();
        let rendered = render_template_with_vars(
            &raw,
            username,
            server_name,
            &scenario.channel_id,
            &vars,
            &counters,
        );
        // Smart Templates: Spintax + variation are applied
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
        // F-GUI-3: in webhook mode the scenario's own mass-mention
        // setting governs (idea #7, via Discord's official
        // allowed_mentions); the text defang must not silently
        // override an explicit opt-in. User-token mode unchanged.
        let defang = !(sender.is_webhook() && scenario.allow_mass_mentions);
        let (clean, warnings) = sanitize_content(&final_text, &antiban, defang);
        for w in &warnings {
            tracing::warn!("anti-ban: {w}");
        }
        let has_embed = scenario.embeds.iter().any(|e| !e.is_empty());
        if clean.trim().is_empty() && !has_embed && scenario.poll.is_none() {
            return Err("tin nhắn rỗng sau khi làm sạch".into());
        }

        if wait_secs > 0 {
            let _ = self
                .sleep_interruptible(Duration::from_secs(wait_secs))
                .await;
            if self.emergency_stop.load(Ordering::SeqCst) {
                return Err("đã bấm dừng khẩn cấp trong lúc chờ".into());
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
                    "defer: đích đến '{}' chưa đủ khoảng cách tối thiểu",
                    dest.display_name()
                ));
            }
        } else {
            self.wait_for_slot(&scenario.channel_id).await;
        }
        if self.emergency_stop.load(Ordering::SeqCst) {
            return Err("đã bấm dừng khẩn cấp trong lúc chờ".into());
        }

        // Auto Typing: show "typing..." for a random
        // 2–5s before sending. A typing failure never blocks the send.
        // Webhooks have no typing endpoint, so this is skipped entirely
        // in webhook mode.
        if antiban.auto_typing && !sender.is_webhook() {
            if let MessageSender::User(client) = sender {
                let _ = client.send_typing(&scenario.channel_id).await;
            }
            let typing = Duration::from_secs(antiban.typing_secs(&mut rng));
            if self.sleep_interruptible(typing).await {
                return Err("đã bấm dừng khẩn cấp trong lúc đang gõ".into());
            }
        }

        // Retry: network errors up to 3x with backoff; 429 waits retry_after;
        // 401/403/404 stop immediately (Spec §6).
        let embeds: Vec<crate::embed::Embed> = scenario.active_embeds();
        // Edit-in-place (idea #2): when enabled and a previous
        // message id is known for this target, PATCH it instead of
        // posting new. A gone target (404) falls back to a fresh
        // send below; the new id is recorded by the caller (the
        // run loop), never here — send_once stays side-effect free
        // on the scenario, so test sends cannot rewrite it.
        let edit_target = if scenario.edit_last_message {
            scenario
                .last_message_ids
                .get(&edit_key(destination, &scenario.channel_id))
                .filter(|id| !id.trim().is_empty())
                .cloned()
        } else {
            None
        };
        let result = if let Some(mid) = edit_target {
            match self
                .edit_message_with_retry(
                    scenario,
                    sender,
                    destination,
                    &mid,
                    &clean,
                    &embeds,
                    &antiban,
                    &mut rng,
                )
                .await
            {
                Ok(Some(id)) => Ok(id),
                Ok(None) => {
                    tracing::info!(
                        "scenario '{}': edited message {mid} is gone — sending a new message",
                        scenario.name
                    );
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
                Err(e) => Err(e),
            }
        } else {
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
        };
        // Counters increment ONLY on a successful send: a failed
        // or deferred run must not advance the series. A failed
        // increment is logged, never fatal — the message went out.
        if result.is_ok() {
            for name in &counter_list {
                if let Err(e) = self.store.increment_counter(name) {
                    tracing::warn!("could not increment counter '{name}': {e}");
                }
            }
        }
        result
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
        // Mention control + silent send (idea #7): the scenario's
        // settings ride on every attempt, including retries.
        let opts = crate::discord::SendOptions {
            allow_mass_mentions: scenario.allow_mass_mentions,
            silent: scenario.silent_send,
        };
        // Attachments (idea #4): read once per send (retries
        // re-send the same bytes, no re-read). Missing/oversize
        // files are skipped with a warning; the message still
        // goes out. Edits never reach this fn, so files ride
        // fresh sends only.
        let (files, skipped) = load_attachments(&scenario.attachments);
        for s in &skipped {
            tracing::warn!("scenario '{}': attachment skipped: {s}", scenario.name);
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            let attempt_result = if files.is_empty() {
                sender
                    .send_with_embeds(
                        &scenario.channel_id,
                        clean,
                        embeds,
                        &opts,
                        scenario.poll.as_ref(),
                    )
                    .await
            } else {
                sender
                    .send_with_files(
                        &scenario.channel_id,
                        clean,
                        embeds,
                        &opts,
                        scenario.poll.as_ref(),
                        &files,
                    )
                    .await
            };
            match attempt_result {
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
                    let mut detail = if sent.id.is_empty() {
                        "đã gửi tin nhắn (webhook, không trả về id)".to_string()
                    } else {
                        format!("tin nhắn {}", sent.id)
                    };
                    if !skipped.is_empty() {
                        detail.push_str(&format!(
                            " (tệp đính kèm bị bỏ qua: {})",
                            skipped.join(", ")
                        ));
                    }
                    let entry = SendLogEntry {
                        scenario_id: scenario.id.clone(),
                        scenario_name: scenario.name.clone(),
                        channel_id: log_channel.clone(),
                        status: "sent".into(),
                        detail,
                        message_id: sent.id.clone(),
                        destination_id: destination.map(|d| d.id.clone()).unwrap_or_default(),
                    };
                    self.write_log(&entry);
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
                            "defer: Discord giới hạn, thử lại sau {retry_after_secs}s (toàn cục={global})"
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
                        return Err("đã bấm dừng khẩn cấp trong lúc chờ hết giới hạn".into());
                    }
                    if attempt >= 3 {
                        return Err(format!(
                            "bị giới hạn liên tục (retry_after cuối {retry_after_secs}s)"
                        ));
                    }
                }
                Err(DiscordError::Unauthorized(m)) | Err(DiscordError::Forbidden(m)) => {
                    if let Some(dest) = destination {
                        let _ = self.store.set_destination_failure(
                            &dest.id,
                            FailureState::Paused401,
                            &format!("dừng ngay: {m}"),
                        );
                        crate::notify::play(crate::notify::SoundEvent::DestinationPaused);
                    }
                    return Err(format!("dừng ngay: {m}"));
                }
                Err(DiscordError::NotFound(m)) => {
                    // A gone webhook pauses the destination (persisted):
                    // further sends would only 404 again. The user
                    // resumes it from Destinations after fixing it.
                    if let Some(dest) = destination {
                        let _ = self.store.set_destination_failure(
                            &dest.id,
                            FailureState::Paused404,
                            &format!("dừng ngay: {m}"),
                        );
                        crate::notify::play(crate::notify::SoundEvent::DestinationPaused);
                    }
                    return Err(format!("dừng ngay: {m}"));
                }
                Err(DiscordError::Other(m)) => {
                    if attempt >= 3 {
                        return Err(format!("thất bại sau 3 lần thử: {m}"));
                    }
                    let backoff = 2u64.pow(attempt) * 2;
                    if self.sleep_interruptible(Duration::from_secs(backoff)).await {
                        return Err("đã bấm dừng khẩn cấp trong lúc chờ thử lại".into());
                    }
                }
            }
        }
    }

    /// Edit one message with the retry policy (idea #2). Returns
    /// `Ok(Some(id))` when edited, `Ok(None)` when the target is gone
    /// (404 — the caller falls back to a fresh send; the destination
    /// is NOT paused: a deleted message says nothing about the
    /// webhook), `Err` on other failures. Mirrors
    /// `send_message_with_retry` except: no auto-delete (deleting
    /// the living message would defeat edit mode), no destination
    /// pause on 404, and the log status is "edited" (counted as a
    /// delivery by the cap queries). Keep the two in sync.
    #[allow(clippy::too_many_arguments)]
    async fn edit_message_with_retry(
        &self,
        scenario: &Scenario,
        sender: &MessageSender,
        destination: Option<&Destination>,
        message_id: &str,
        clean: &str,
        embeds: &[crate::embed::Embed],
        antiban: &AntiBanConfig,
        rng: &mut StdRng,
    ) -> Result<Option<String>, String> {
        let log_channel = destination
            .map(|d| d.channel_id.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| scenario.channel_id.clone());
        let opts = crate::discord::SendOptions {
            allow_mass_mentions: scenario.allow_mass_mentions,
            silent: scenario.silent_send,
        };
        let mut attempt = 0;
        loop {
            attempt += 1;
            match sender
                .edit_message(&scenario.channel_id, message_id, clean, embeds, &opts)
                .await
            {
                Ok(sent) => {
                    *self.sent_today.lock().await += 1;
                    if let Some(dest) = destination {
                        self.last_sent
                            .lock()
                            .await
                            .insert(dest.id.clone(), Instant::now());
                        self.reset_dest_errors(&dest.id);
                    }
                    // A 204 has no message id: the edited message
                    // keeps its old id.
                    let id = if sent.id.is_empty() {
                        message_id.to_string()
                    } else {
                        sent.id.clone()
                    };
                    let entry = SendLogEntry {
                        scenario_id: scenario.id.clone(),
                        scenario_name: scenario.name.clone(),
                        channel_id: log_channel.clone(),
                        status: "edited".into(),
                        detail: format!("tin nhắn đã sửa {id}"),
                        message_id: id.clone(),
                        destination_id: destination.map(|d| d.id.clone()).unwrap_or_default(),
                    };
                    self.write_log(&entry);
                    crate::notify::play(crate::notify::SoundEvent::SendSuccess);
                    self.maybe_auto_break(antiban, rng).await?;
                    return Ok(Some(id));
                }
                Err(DiscordError::RateLimited {
                    retry_after_secs,
                    global,
                }) => {
                    tracing::warn!("Discord 429, waiting {retry_after_secs}s (global={global})");
                    if let Some(dest) = destination {
                        if global {
                            self.set_global_pause(retry_after_secs);
                        } else {
                            self.set_cooldown(&dest.id, retry_after_secs);
                        }
                        return Err(format!(
                            "defer: Discord giới hạn, thử lại sau {retry_after_secs}s (toàn cục={global})"
                        ));
                    }
                    let paused = if global {
                        self.sleep_interruptible(Duration::from_secs(3600)).await
                    } else {
                        self.sleep_interruptible(Duration::from_secs(retry_after_secs.max(1)))
                            .await
                    };
                    if paused {
                        return Err("đã bấm dừng khẩn cấp trong lúc chờ hết giới hạn".into());
                    }
                    if attempt >= 3 {
                        return Err(format!(
                            "bị giới hạn liên tục (retry_after cuối {retry_after_secs}s)"
                        ));
                    }
                }
                Err(DiscordError::Unauthorized(m)) | Err(DiscordError::Forbidden(m)) => {
                    if let Some(dest) = destination {
                        let _ = self.store.set_destination_failure(
                            &dest.id,
                            FailureState::Paused401,
                            &format!("dừng ngay: {m}"),
                        );
                        crate::notify::play(crate::notify::SoundEvent::DestinationPaused);
                    }
                    return Err(format!("dừng ngay: {m}"));
                }
                Err(DiscordError::NotFound(_)) => {
                    // The message is gone (deleted in Discord). Not
                    // a destination failure: fall back to a fresh
                    // send, decided by the caller.
                    return Ok(None);
                }
                Err(DiscordError::Other(m)) => {
                    if attempt >= 3 {
                        return Err(format!("thất bại sau 3 lần thử: {m}"));
                    }
                    let backoff = 2u64.pow(attempt) * 2;
                    if self.sleep_interruptible(Duration::from_secs(backoff)).await {
                        return Err("đã bấm dừng khẩn cấp trong lúc chờ thử lại".into());
                    }
                }
            }
        }
    }

    /// Auto Delete: delete the message after the
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
            return Err("đã bấm dừng khẩn cấp trong lúc chờ xóa".into());
        }
        match client
            .delete_message(&scenario.channel_id, message_id)
            .await
        {
            Ok(()) => {
                if let Ok(mut d) = self.deleted_today.lock() {
                    *d += 1;
                }
                self.write_log(&SendLogEntry {
                    scenario_id: scenario.id.clone(),
                    scenario_name: scenario.name.clone(),
                    channel_id: scenario.channel_id.clone(),
                    status: "deleted".into(),
                    detail: format!("đã xóa tin nhắn {message_id}"),
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

    /// Auto Break: after a random 15–25 sends, rest for
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
                return Err("đã bấm dừng khẩn cấp trong lúc nghỉ".into());
            }
        }
        Ok(())
    }

    /// Session-only guild cache (see the `guild_cache` field).
    /// Outer None = never resolved for this channel; inner None =
    /// a cached DM channel (no guild).
    fn cached_guild_id(&self, channel_id: &str) -> Option<Option<String>> {
        self.guild_cache.lock().ok()?.get(channel_id).cloned()
    }

    fn cache_guild_id(&self, channel_id: &str, guild_id: Option<String>) {
        if let Ok(mut cache) = self.guild_cache.lock() {
            cache.insert(channel_id.to_string(), guild_id);
        }
    }

    /// Run ALL `ActionSendCommand` nodes of a scenario, in node
    /// order, and return the last command's result (a message id for
    /// prefix commands, a summary for slash). Retry safety: a
    /// "defer:" error (rate limit / cooldown) means the run is
    /// retried later — safe only while nothing has run yet. Once a
    /// command already succeeded in this firing, a later command's
    /// "defer:" is demoted to a hard error (run consumed, failure
    /// row, no retry) so the earlier command is never sent twice.
    /// "skip:" / hard errors already consume the run, so they pass
    /// through unchanged.
    async fn send_commands(
        &self,
        scenario: &Scenario,
        cmd_nodes: &[&Node],
        sender: &MessageSender,
        username: &str,
        server_name: &str,
        antiban: &AntiBanConfig,
    ) -> Result<String, String> {
        let mut last_ok = String::new();
        let mut ran = 0usize;
        for node in cmd_nodes {
            let cfg = CommandConfig::from_node(node);
            match self
                .send_command(scenario, &cfg, sender, username, server_name, antiban)
                .await
            {
                Ok(summary) => {
                    last_ok = summary;
                    ran += 1;
                }
                Err(e) => {
                    return Err(demote_defer_after_partial(ran, e));
                }
            }
        }
        Ok(last_ok)
    }

    /// Execute one `ActionSendCommand` node (one config).
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
            return Err("chế độ Webhook không gửi được lệnh".into());
        };
        // Defensive: send_once already validates the scenario, but a
        // direct caller must never send a malformed command (H-CMD).
        cfg.validate()
            .map_err(|e| format!("lệnh không hợp lệ: {e}"))?;
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
        // Commands do not use durable counters: the empty map
        // leaves any `{{counter:...}}` tag literal.
        let rendered = render_template_with_vars(
            &composed,
            username,
            server_name,
            &scenario.channel_id,
            &vars,
            &HashMap::new(),
        );
        // Commands are user-token only: the defang stays on, exactly
        // as before (F-GUI-3 does not apply here).
        let (clean, warnings) = sanitize_content(&rendered, antiban, true);
        for w in &warnings {
            tracing::warn!("anti-ban: {w}");
        }
        if clean.trim().is_empty() {
            return Err("lệnh rỗng sau khi làm sạch".into());
        }
        let wait_secs = scenario.wait_seconds();
        if wait_secs > 0 {
            let _ = self
                .sleep_interruptible(Duration::from_secs(wait_secs))
                .await;
            if self.emergency_stop.load(Ordering::SeqCst) {
                return Err("đã bấm dừng khẩn cấp trong lúc chờ".into());
            }
        }
        self.wait_for_slot(&scenario.channel_id).await;
        if self.emergency_stop.load(Ordering::SeqCst) {
            return Err("đã bấm dừng khẩn cấp trong lúc chờ".into());
        }
        if antiban.auto_typing {
            let _ = client.send_typing(&scenario.channel_id).await;
            let typing = Duration::from_secs(antiban.typing_secs(&mut rng));
            if self.sleep_interruptible(typing).await {
                return Err("đã bấm dừng khẩn cấp trong lúc đang gõ".into());
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
            return Err("chế độ Webhook không gửi được lệnh".into());
        };
        let mut rng = StdRng::from_os_rng();
        let wait_secs = scenario.wait_seconds();
        if wait_secs > 0 {
            let _ = self
                .sleep_interruptible(Duration::from_secs(wait_secs))
                .await;
            if self.emergency_stop.load(Ordering::SeqCst) {
                return Err("đã bấm dừng khẩn cấp trong lúc chờ".into());
            }
        }
        self.wait_for_slot(&scenario.channel_id).await;
        if self.emergency_stop.load(Ordering::SeqCst) {
            return Err("đã bấm dừng khẩn cấp trong lúc chờ".into());
        }
        // Guild of the channel (None for DMs — the payload omits it).
        // Session cache first: a failed channel read must not fail
        // the whole send when a previous firing already resolved it.
        let guild_id = match self.cached_guild_id(&scenario.channel_id) {
            Some(g) => g,
            None => match client.get_channel(&scenario.channel_id).await {
                Ok(ch) => {
                    let g = ch.guild_id.clone();
                    self.cache_guild_id(&scenario.channel_id, g.clone());
                    g
                }
                Err(e) => match self.cached_guild_id(&scenario.channel_id) {
                    Some(g) => g,
                    None => {
                        return Err(format!("không đọc được kênh cho lệnh slash: {e}"));
                    }
                },
            },
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
                                "không tìm thấy lệnh slash /{} trong kênh này — dán Command ID thủ công trong phần sửa bước",
                                cfg.name.trim()
                            ));
                        }
                    }
                }
                Err(e) => {
                    return Err(format!(
                        "không tìm được lệnh ứng dụng ({e}) — dán Command ID thủ công trong phần sửa bước"
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
        // Options: the node's `SlashOption`s (nested for subcommands);
        // a bad leaf value fails here, before anything reaches Discord.
        let payload = build_interaction_payload(
            cfg.application_id.trim(),
            guild_id.as_deref(),
            &scenario.channel_id,
            &session,
            &make_nonce(),
            &command_id,
            cfg.name.trim(),
            &version,
            &cfg.options,
        )
        .map_err(|e| format!("lệnh slash không hợp lệ: {e}"))?;
        // Retry like a message send: 429 waits, 401/403 stop at once.
        let mut attempt = 0;
        loop {
            attempt += 1;
            match client.invoke_interaction(payload.clone()).await {
                Ok(()) => {
                    *self.sent_today.lock().await += 1;
                    self.write_log(&SendLogEntry {
                        scenario_id: scenario.id.clone(),
                        scenario_name: scenario.name.clone(),
                        channel_id: scenario.channel_id.clone(),
                        status: "sent".into(),
                        detail: format!("lệnh slash /{}", cfg.name.trim()),
                        message_id: String::new(),
                        destination_id: String::new(),
                    });
                    self.maybe_auto_break(antiban, &mut rng).await?;
                    return Ok(format!("lệnh slash /{}", cfg.name.trim()));
                }
                Err(DiscordError::RateLimited {
                    retry_after_secs,
                    global,
                }) => {
                    tracing::warn!(
                        "Discord trả 429 cho lệnh slash, chờ {retry_after_secs}s (toàn cục={global})"
                    );
                    let paused = if global {
                        self.sleep_interruptible(Duration::from_secs(3600)).await
                    } else {
                        self.sleep_interruptible(Duration::from_secs(retry_after_secs.max(1)))
                            .await
                    };
                    if paused {
                        return Err("đã bấm dừng khẩn cấp trong lúc chờ hết giới hạn".into());
                    }
                    if attempt >= 3 {
                        return Err(format!(
                            "bị giới hạn liên tục (retry_after cuối {retry_after_secs}s)"
                        ));
                    }
                }
                Err(DiscordError::Unauthorized(m))
                | Err(DiscordError::Forbidden(m))
                | Err(DiscordError::NotFound(m)) => {
                    return Err(format!("dừng ngay: {m}"));
                }
                Err(DiscordError::Other(m)) => {
                    if attempt >= 3 {
                        return Err(format!("thất bại sau 3 lần thử: {m}"));
                    }
                    let backoff = 2u64.pow(attempt) * 2;
                    if self.sleep_interruptible(Duration::from_secs(backoff)).await {
                        return Err("đã bấm dừng khẩn cấp trong lúc chờ thử lại".into());
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
    /// `resolve_sender` maps each scenario to its targets: one
    /// (sender, destination) pair in user-token mode, one pair per
    /// destination in webhook mode (broadcast, idea #1). `scenarios`
    /// is re-read each tick so edits in the UI take effect without
    /// restarting the engine. One loop only (atomic guard); targets
    /// are processed sequentially in a stable order, with a fixed
    /// 1-second stagger between webhook sends in one tick.
    pub async fn run_loop(
        self: Arc<Self>,
        resolve_sender: impl Fn(&Scenario) -> Vec<(MessageSender, Option<Destination>)>
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
        // Held for the whole loop body: `Drop` releases the claim and
        // clears `running` on every exit path, including a panic. A
        // panic used to wedge the engine until app restart — the claim
        // stayed held while no loop ran, so Start refused forever.
        // Deliberately fail-stop, not fail-continue: after a panic the
        // loop is dead and the user restarts it, rather than the loop
        // silently resuming with possibly partial send state.
        let _loop_guard = LoopGuard {
            engine: self.clone(),
        };
        self.running.store(true, Ordering::SeqCst);
        // Authoritative last-run state: scenario id -> (normalized
        // schedule, last scheduled time that fired). Persisted to
        // SQLite on every fire, so a restart resumes exactly where it
        // left off. The schedule is compared NORMALIZED, not by its
        // display label: reordering weekdays or retyping a cron
        // expression must not reset the anchor (duplicate send).
        let mut last_runs: HashMap<String, (Option<Schedule>, Option<DateTime<Utc>>)> =
            HashMap::new();
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
                    self.save_run_state(&scenario, "save scenario creation time");
                }
                let sched = scenario.schedule.as_ref().map(|s| s.normalized());
                let entry = last_runs
                    .entry(scenario.id.clone())
                    .or_insert_with(|| (sched.clone(), scenario.last_run_at));
                if entry.0 != sched {
                    entry.0 = sched;
                    entry.1 = None;
                    scenario.last_run_at = None;
                    self.save_run_state(&scenario, "save scenario after schedule change");
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
                // Broadcast (idea #1): one firing sends to every
                // resolved target — one sender in user-token mode,
                // one sender per destination in webhook mode. An
                // empty list means nothing resolved (no credential /
                // no usable destination): skip quietly, as before.
                let targets = resolve_sender(&scenario);
                if targets.is_empty() {
                    continue;
                }
                let any_webhook = targets.iter().any(|(_, d)| d.is_some());
                let mut results: Vec<TargetResult> = Vec::new();
                for (ti, (sender, destination)) in targets.iter().enumerate() {
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
                    // Fixed 1-second stagger between webhook sends
                    // in one firing (transparent, never random).
                    if ti > 0
                        && sender.is_webhook()
                        && self.sleep_interruptible(Duration::from_secs(1)).await
                    {
                        break;
                    }
                    // Template identity: per-destination in webhook mode
                    // (stored name, no network), global account identity
                    // in user mode (refreshed when still the fallback).
                    let (username, server_name) = if let Some(dest) = destination {
                        (dest.effective_username(), String::new())
                    } else {
                        if self.identity().await.0 == "you" {
                            if let MessageSender::User(client) = sender {
                                self.refresh_identity(client).await;
                            }
                        }
                        self.identity().await
                    };
                    let label = destination
                        .as_ref()
                        .map(|d| d.display_name())
                        .unwrap_or_else(|| "account".to_string());
                    let channel_id = destination
                        .as_ref()
                        .map(|d| d.channel_id.clone())
                        .filter(|c| !c.is_empty())
                        .unwrap_or_else(|| scenario.channel_id.clone());
                    let result = self
                        .send_once(
                            &scenario,
                            sender,
                            destination.as_ref(),
                            &username,
                            &server_name,
                        )
                        .await;
                    results.push(TargetResult {
                        label,
                        destination_id: destination
                            .as_ref()
                            .map(|d| d.id.clone())
                            .unwrap_or_default(),
                        channel_id,
                        edit_key: edit_key(destination.as_ref(), &scenario.channel_id),
                        result,
                    });
                }
                if results.is_empty() {
                    // Nothing was attempted (every target filtered
                    // out, or Emergency Stop during the stagger):
                    // do not consume the run.
                    continue;
                }
                match aggregate_broadcast(&results) {
                    BroadcastOutcome::Success => {
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
                        // Edit-in-place (idea #2): record the
                        // message id per target so the next firing
                        // edits it. Empty ids (webhook 204) are
                        // skipped — an unrecorded target posts new
                        // next time, honestly.
                        if scenario.edit_last_message {
                            for r in &results {
                                if let Ok(id) = &r.result {
                                    if !id.is_empty() {
                                        scenario
                                            .last_message_ids
                                            .insert(r.edit_key.clone(), id.clone());
                                    }
                                }
                            }
                        }
                        self.save_run_state(&scenario, "save scenario last-run after send");
                        // Partial failure: other targets succeeded, so
                        // the run is consumed and the engine error
                        // streak resets — but each failed target still
                        // gets its failure row and error counting, so
                        // Logs shows it and a sick destination still
                        // pauses itself.
                        self.log_broadcast_failures(&scenario, &results);
                        // Fixed 1-second stagger between webhook sends
                        // in one tick (transparent, never random).
                        if any_webhook && self.sleep_interruptible(Duration::from_secs(1)).await {
                            break;
                        }
                    }
                    BroadcastOutcome::Deferred(e) => {
                        // Short wait (interval / 429 cooldown / global
                        // pause): do NOT consume the run, count an
                        // error, or log a failure. The run retries on
                        // a later tick, slightly late.
                        tracing::info!("scenario {} deferred: {e}", scenario.name);
                    }
                    BroadcastOutcome::Skipped(e) => {
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
                        self.save_run_state(&scenario, "save scenario last-run after skip");
                    }
                    BroadcastOutcome::Invalid(e) => {
                        // A broken scenario is not a broken
                        // destination: consume the run (like a skip)
                        // so it is not retried every second, but do
                        // not count it toward the destination's error
                        // limit or the engine pause, and do not write
                        // a failure row — the destination did nothing
                        // wrong. The scenario stays enabled and will
                        // fail again at its next scheduled run until
                        // the user fixes it in the Builder.
                        tracing::info!("scenario {} invalid, run consumed: {e}", scenario.name);
                        let due_utc = due.with_timezone(&Utc);
                        entry.1 = Some(due_utc);
                        scenario.last_run_at = Some(due_utc);
                        if matches!(schedule, Schedule::Once { .. }) {
                            scenario.enabled = false;
                        }
                        self.save_run_state(&scenario, "save scenario last-run after invalid run");
                    }
                    BroadcastOutcome::Failed(e) => {
                        // Defense in depth: error strings must never
                        // carry credentials into tracing output, the
                        // database, or the UI. Scrub once here so all
                        // sinks below share the cleaned text.
                        let safe_err = crate::store::scrub_secrets(&e);
                        tracing::warn!("scenario {} failed: {safe_err}", scenario.name);
                        // One failure row per failed target (in the
                        // single-target case exactly the old single
                        // row), so Logs filters see every
                        // destination's failure. Details are
                        // truncated: Discord error bodies can be long,
                        // and the log is for diagnosis, not a
                        // transcript. Scrubbed before truncating so a
                        // URL straddling the cut cannot survive in part.
                        self.log_broadcast_failures(&scenario, &results);
                        crate::notify::play(crate::notify::SoundEvent::SendFailure);
                        consecutive_errors += 1;
                        // Spec §6: 3 consecutive errors pauses the engine.
                        // Interruptible: Emergency Stop must not wait
                        // out the full 10 minutes.
                        if consecutive_errors >= 3 {
                            tracing::error!("3 consecutive errors — pausing engine for 10 min");
                            // F-GUI-4: record the pause so the UI can
                            // show it instead of a misleading "Running".
                            self.set_error_pause(600);
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
        // Normal exit: the guard below releases the claim as it drops.
    }
}

/// Releases the engine-loop claim and clears `running` when dropped,
/// on every exit path including a panic inside `run_loop`.
struct LoopGuard {
    engine: Arc<Engine>,
}

impl Drop for LoopGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            tracing::error!("engine loop panicked; claim released so the engine can be restarted");
        }
        self.engine.running.store(false, Ordering::SeqCst);
        self.engine.release_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn target(label: &str, result: Result<String, String>) -> TargetResult {
        TargetResult {
            label: label.to_string(),
            destination_id: format!("dest-{label}"),
            channel_id: "chan-1".to_string(),
            edit_key: format!("dest-{label}"),
            result,
        }
    }

    #[test]
    fn broadcast_aggregation_folds_per_target_results() {
        // Any success wins, even with failures alongside.
        let mixed = vec![
            target("a", Ok("111".to_string())),
            target("b", Err("network error".to_string())),
            target("c", Err("skip: quiet window".to_string())),
        ];
        assert!(matches!(
            aggregate_broadcast(&mixed),
            BroadcastOutcome::Success
        ));
        // Defer beats hard failure: a retryable target must retry.
        let deferred = vec![
            target("a", Err("defer: cooling down".to_string())),
            target("b", Err("network error".to_string())),
        ];
        assert!(matches!(
            aggregate_broadcast(&deferred),
            BroadcastOutcome::Deferred(_)
        ));
        // Hard failure beats skip: it must surface and count.
        let failed = vec![
            target("a", Err("skip: cap reached".to_string())),
            target("b", Err("network error".to_string())),
        ];
        match aggregate_broadcast(&failed) {
            BroadcastOutcome::Failed(detail) => {
                assert!(detail.contains("a:"), "detail: {detail}");
                assert!(detail.contains("b:"), "detail: {detail}");
                assert!(detail.contains("cho 2 đích"), "detail: {detail}");
            }
            other => panic!(
                "expected Failed, got {}",
                match other {
                    BroadcastOutcome::Success => "Success",
                    BroadcastOutcome::Deferred(_) => "Deferred",
                    BroadcastOutcome::Skipped(_) => "Skipped",
                    BroadcastOutcome::Invalid(_) => "Invalid",
                    BroadcastOutcome::Failed(_) => "Failed",
                }
            ),
        }
        // All skipped -> Skipped; all invalid -> Invalid.
        let skipped = vec![target("a", Err("skip: cap".to_string()))];
        assert!(matches!(
            aggregate_broadcast(&skipped),
            BroadcastOutcome::Skipped(_)
        ));
        let invalid = vec![target("a", Err("invalid scenario: no name".to_string()))];
        assert!(matches!(
            aggregate_broadcast(&invalid),
            BroadcastOutcome::Invalid(_)
        ));
        // Single-target behaviour is unchanged: Ok -> Success,
        // hard Err -> Failed naming the target.
        let single_ok = vec![target("a", Ok("1".to_string()))];
        assert!(matches!(
            aggregate_broadcast(&single_ok),
            BroadcastOutcome::Success
        ));
        let single_err = vec![target("a", Err("boom".to_string()))];
        match aggregate_broadcast(&single_err) {
            BroadcastOutcome::Failed(detail) => assert!(detail.contains("a: boom")),
            _ => panic!("expected Failed"),
        }
    }

    #[test]
    fn demote_defer_after_partial_never_retries_a_ran_command() {
        // Nothing ran yet: a "defer:" passes through untouched, so
        // the run retries later (same as a single-command firing).
        assert_eq!(
            demote_defer_after_partial(0, "defer: cooling down".to_string()),
            "defer: cooling down"
        );
        // One command already ran: the "defer:" loses its prefix, so
        // the run is consumed as a hard failure instead of retrying
        // (which would send the first command twice).
        let demoted = demote_defer_after_partial(1, "defer: Discord giới hạn".to_string());
        assert!(
            !demoted.starts_with("defer:"),
            "demoted error must not retry: {demoted}"
        );
        assert!(demoted.contains("1 lệnh"), "detail: {demoted}");
        assert!(demoted.contains("Discord giới hạn"), "detail: {demoted}");
        // Hard errors and skips pass through unchanged either way.
        assert_eq!(
            demote_defer_after_partial(2, "network error".to_string()),
            "network error"
        );
        assert_eq!(
            demote_defer_after_partial(1, "skip: cap reached".to_string()),
            "skip: cap reached"
        );
    }

    #[test]
    fn load_attachments_skips_missing_and_oversize_files() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("nicord-attach-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let small = dir.join("hello.txt");
        std::fs::write(&small, b"hi").unwrap();
        let big = dir.join("big.bin");
        {
            let mut f = std::fs::File::create(&big).unwrap();
            f.write_all(&vec![
                0u8;
                (crate::discord::MAX_ATTACHMENT_BYTES + 1) as usize
            ])
            .unwrap();
        }
        let missing = dir.join("nope.png");
        let (files, skipped) = load_attachments(&[
            small.to_string_lossy().into_owned(),
            big.to_string_lossy().into_owned(),
            missing.to_string_lossy().into_owned(),
            String::new(),
        ]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "hello.txt");
        assert_eq!(files[0].content_type, "text/plain");
        assert_eq!(files[0].bytes, b"hi");
        assert_eq!(skipped.len(), 2);
        assert!(
            skipped.iter().any(|s| s.starts_with("big.bin")),
            "{skipped:?}"
        );
        assert!(
            skipped.iter().any(|s| s.starts_with("nope.png")),
            "{skipped:?}"
        );
        // Full paths never appear in skip reasons.
        for s in &skipped {
            assert!(!s.contains(dir.to_str().unwrap()), "{s}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_attachments_clamps_to_ten_files() {
        let dir = std::env::temp_dir().join(format!("nicord-attach-clamp-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut paths = Vec::new();
        for i in 0..12 {
            let p = dir.join(format!("f{i}.txt"));
            std::fs::write(&p, b"x").unwrap();
            paths.push(p.to_string_lossy().into_owned());
        }
        let (files, skipped) = load_attachments(&paths);
        assert_eq!(files.len(), 10);
        assert!(skipped.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn persist_error_is_remembered_until_a_save_succeeds() {
        let store = Arc::new(Store::open_memory().unwrap());
        let engine = Engine::new(store, AntiBanConfig::default(), 300, 5);
        assert!(engine.last_persist_error().is_none());
        engine.record_persist_error("test save", "disk full (simulated)");
        let err = engine.last_persist_error().unwrap();
        assert!(err.contains("test save"), "{err}");
        assert!(err.contains("disk full"), "{err}");
        // A successful run-state save clears it.
        let scenario = Scenario::new("t");
        engine.save_run_state(&scenario, "test save");
        assert!(engine.last_persist_error().is_none());
    }

    #[test]
    fn error_pause_remaining_tracks_the_pause() {
        // F-GUI-4: fresh engine = no pause; after set, the
        // remaining time is ~the full pause (a few ms of test
        // overhead at most); it expires by time comparison.
        let store = Arc::new(Store::open_memory().unwrap());
        let engine = Engine::new(store, AntiBanConfig::default(), 300, 5);
        assert!(engine.error_pause_remaining().is_none());
        engine.set_error_pause(600);
        let remaining = engine.error_pause_remaining().unwrap();
        assert!(remaining.as_secs() <= 600);
        assert!(remaining.as_secs() >= 590);
    }

    #[test]
    fn loop_guard_releases_on_panic() {
        let store = Arc::new(Store::open_memory().unwrap());
        let engine = Arc::new(Engine::new(store, AntiBanConfig::default(), 300, 5));
        assert!(engine.try_claim_loop());
        engine.running.store(true, Ordering::SeqCst);
        let engine2 = engine.clone();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = LoopGuard { engine: engine2 };
            panic!("simulated loop panic");
        }));
        assert!(caught.is_err());
        assert!(!engine.is_loop_active());
        assert!(!engine.running.load(Ordering::SeqCst));
        // The engine can be started again after the panic.
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
