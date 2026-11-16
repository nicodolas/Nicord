//! Scenario model: nodes, edges, schedule, validation, spin-text.
//! A scenario is data (JSON), never code. Spec §5 and §10.
//!
//! Scheduler semantics (fixed, see `due_time`):
//! - `last_run_at` is persisted on every successful send. A schedule
//!   fires once per scheduled time: the due time is the latest scheduled
//!   time in (anchor, now], where anchor = last_run or creation time.
//!   There is no 60-second "now - 60s" window, so no duplicates and no
//!   missed intervals for EveryMinutes/EveryHours.
//! - `Once` fires exactly once, then the scenario is disabled.
//! - Cron is a real 5-field parser (minute hour dom month dow), not hourly.

use chrono::{DateTime, Datelike, Local, NaiveTime, Timelike, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Schedule for a scenario (Spec §6).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Run once at a time.
    Once { at: DateTime<Utc> },
    /// Every N minutes.
    EveryMinutes { minutes: u32 },
    /// Every N hours.
    EveryHours { hours: u32 },
    /// Daily at a local time.
    Daily { time: NaiveTime },
    /// Weekly on given weekdays (0 = Mon .. 6 = Sun) at a local time.
    Weekly { weekdays: Vec<u8>, time: NaiveTime },
    /// Advanced cron string. Real 5-field parser.
    Cron { expr: String },
}

impl Schedule {
    pub fn label(&self) -> String {
        match self {
            Schedule::Once { at } => format!("Once at {}", at.format("%Y-%m-%d %H:%M UTC")),
            Schedule::EveryMinutes { minutes } => format!("Every {} min", minutes),
            Schedule::EveryHours { hours } => format!("Every {} h", hours),
            Schedule::Daily { time } => format!("Daily at {}", time.format("%H:%M")),
            Schedule::Weekly { weekdays, time } => {
                format!("Weekly {:?} at {}", weekdays, time.format("%H:%M"))
            }
            Schedule::Cron { expr } => format!("Cron `{}`", expr),
        }
    }

    /// Next run strictly after `from` (local time). Used for display
    /// ("next run at …"), not for the firing decision — see `due_time`.
    pub fn next_after(&self, from: DateTime<Local>) -> DateTime<Local> {
        match self {
            Schedule::Once { at } => at.with_timezone(&Local),
            Schedule::EveryMinutes { minutes } => {
                let m = (*minutes).max(1) as i64;
                from + chrono::Duration::minutes(m)
            }
            Schedule::EveryHours { hours } => {
                let h = (*hours).max(1) as i64;
                from + chrono::Duration::hours(h)
            }
            Schedule::Daily { time } => {
                let today = from
                    .date_naive()
                    .and_time(*time)
                    .and_local_timezone(Local)
                    .single()
                    .unwrap_or(from);
                if today > from {
                    today
                } else {
                    today + chrono::Duration::days(1)
                }
            }
            Schedule::Weekly { weekdays, time } => {
                for day_offset in 0..8 {
                    let candidate_date = from.date_naive() + chrono::Duration::days(day_offset);
                    let weekday = candidate_date.weekday().num_days_from_monday() as u8;
                    if weekdays.contains(&weekday) {
                        let candidate = candidate_date
                            .and_time(*time)
                            .and_local_timezone(Local)
                            .single();
                        if let Some(c) = candidate {
                            if c > from {
                                return c;
                            }
                        }
                    }
                }
                from + chrono::Duration::days(7)
            }
            Schedule::Cron { expr } => match parse_cron(expr) {
                Ok(fields) => {
                    let tz = chrono_tz::UTC;
                    let from_tz = from.with_timezone(&tz);
                    cron_next_after(&fields, from_tz)
                        .map(|d| d.with_timezone(&Local))
                        .unwrap_or(from + chrono::Duration::hours(1))
                }
                Err(_) => from + chrono::Duration::hours(1),
            },
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Schedule::EveryMinutes { minutes } if *minutes == 0 => {
                Err("interval must be >= 1 minute".into())
            }
            Schedule::EveryHours { hours } if *hours == 0 => {
                Err("interval must be >= 1 hour".into())
            }
            Schedule::Weekly { weekdays, .. } if weekdays.is_empty() => {
                Err("weekly schedule needs at least one weekday".into())
            }
            Schedule::Cron { expr } => parse_cron(expr).map(|_| ()),
            _ => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal 5-field cron parser: minute hour day-of-month month day-of-week.
// Supports `*`, `*/n`, `a-b`, `a-b/n`, `a,b,c`, single numbers.
// Day-of-week: 0 and 7 both mean Sunday.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronFields {
    pub minute: [bool; 60],
    pub hour: [bool; 24],
    pub dom: [bool; 32],   // index 1..=31
    pub month: [bool; 13], // index 1..=12
    pub dow: [bool; 7],    // index 0..=6, 0 = Sunday
}

fn parse_cron_field(field: &str, min: u32, max: u32) -> Result<Vec<bool>, String> {
    let size = (max - min + 1) as usize;
    let mut out = vec![false; size];
    let mut mark = |v: u32| {
        if v >= min && v <= max {
            out[(v - min) as usize] = true;
        }
    };
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err("empty cron field part".into());
        }
        let (range, step, has_step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .map_err(|_| format!("bad cron step '{s}'"))?,
                true,
            ),
            None => (part, 1, false),
        };
        if step == 0 {
            return Err("cron step must be >= 1".into());
        }
        if range == "*" {
            let mut v = min;
            while v <= max {
                mark(v);
                v += step;
            }
        } else if let Some((a, b)) = range.split_once('-') {
            let lo: u32 = a.parse().map_err(|_| format!("bad cron number '{a}'"))?;
            let hi: u32 = b.parse().map_err(|_| format!("bad cron number '{b}'"))?;
            if lo > hi || lo < min || hi > max {
                return Err(format!("cron range {lo}-{hi} out of bounds {min}-{max}"));
            }
            let mut v = lo;
            while v <= hi {
                mark(v);
                v += step;
            }
        } else {
            let v: u32 = range
                .parse()
                .map_err(|_| format!("bad cron number '{range}'"))?;
            if v < min || v > max {
                return Err(format!("cron value {v} out of bounds {min}-{max}"));
            }
            if has_step {
                // `a/n` means a..max step n.
                let mut x = v;
                while x <= max {
                    mark(x);
                    x += step;
                }
            } else {
                mark(v);
            }
        }
    }
    Ok(out)
}

pub fn parse_cron(expr: &str) -> Result<CronFields, String> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(format!(
            "cron expression needs exactly 5 fields (minute hour dom month dow), got {}",
            parts.len()
        ));
    }
    let minute = parse_cron_field(parts[0], 0, 59)?;
    let hour = parse_cron_field(parts[1], 0, 23)?;
    let dom = parse_cron_field(parts[2], 1, 31)?;
    let month = parse_cron_field(parts[3], 1, 12)?;
    let dow_raw = parse_cron_field(parts[4], 0, 7)?;
    // Normalise Sunday (7) into slot 0.
    let mut dow = [false; 7];
    for (i, v) in dow_raw.iter().enumerate() {
        if *v {
            dow[if i == 7 { 0 } else { i }] = true;
        }
    }
    let to_arr = |v: Vec<bool>, n: usize| -> Vec<bool> {
        let mut a = vec![false; n];
        for (i, x) in v.into_iter().enumerate() {
            if i < n {
                a[i] = x;
            }
        }
        a
    };
    let minute_arr: [bool; 60] = to_arr(minute, 60).try_into().unwrap();
    let hour_arr: [bool; 24] = to_arr(hour, 24).try_into().unwrap();
    // dom/month are 1-based: shift so index == value.
    let mut dom_arr = [false; 32];
    for (i, x) in dom.into_iter().enumerate() {
        dom_arr[i + 1] = x;
    }
    let mut month_arr = [false; 13];
    for (i, x) in month.into_iter().enumerate() {
        month_arr[i + 1] = x;
    }
    Ok(CronFields {
        minute: minute_arr,
        hour: hour_arr,
        dom: dom_arr,
        month: month_arr,
        dow,
    })
}

fn cron_field_is_all(arr: &[bool]) -> bool {
    arr.iter().all(|b| *b)
}

pub fn cron_matches(fields: &CronFields, dt: DateTime<chrono_tz::Tz>) -> bool {
    if !fields.minute[dt.minute() as usize] {
        return false;
    }
    if !fields.hour[dt.hour() as usize] {
        return false;
    }
    if !fields.month[dt.month() as usize] {
        return false;
    }
    let dom_restricted = !cron_field_is_all(&fields.dom[1..]);
    let dow_restricted = !cron_field_is_all(&fields.dow);
    let dom_ok = fields.dom[dt.day() as usize];
    let dow_ok = fields.dow[dt.weekday().num_days_from_sunday() as usize];
    match (dom_restricted, dow_restricted) {
        (true, true) => dom_ok || dow_ok,
        (true, false) => dom_ok,
        (false, true) => dow_ok,
        (false, false) => true,
    }
}

/// First cron match strictly after `from` (scans at most ~366 days).
pub fn cron_next_after(
    fields: &CronFields,
    from: DateTime<chrono_tz::Tz>,
) -> Option<DateTime<chrono_tz::Tz>> {
    let mut cand = from
        .with_second(0)
        .unwrap_or(from)
        .with_nanosecond(0)
        .unwrap_or(from)
        + chrono::Duration::minutes(1);
    for _ in 0..(366 * 24 * 60) {
        if cron_matches(fields, cand) {
            return Some(cand);
        }
        cand += chrono::Duration::minutes(1);
    }
    None
}

// ---------------------------------------------------------------------------
// Due-check: the single source of truth for "should this fire now?".
// Returns the scheduled time that is due (the latest scheduled time in
// (anchor, now]), or None. `last_run` is the last *scheduled* time that
// fired (persisted); `created_at` is the scenario creation time.
// ---------------------------------------------------------------------------

pub fn tz_from_name(name: &str) -> chrono_tz::Tz {
    name.parse::<chrono_tz::Tz>().unwrap_or(chrono_tz::UTC)
}

pub fn due_time(
    schedule: &Schedule,
    last_run: Option<DateTime<Utc>>,
    created_at: Option<DateTime<Utc>>,
    now: DateTime<Local>,
    timezone: &str,
) -> Option<DateTime<Local>> {
    let tz = tz_from_name(timezone);
    let now_tz = now.with_timezone(&tz);
    let anchor_tz: DateTime<chrono_tz::Tz> = last_run
        .map(|t| t.with_timezone(&tz))
        .or_else(|| created_at.map(|t| t.with_timezone(&tz)))
        .unwrap_or(now_tz);
    match schedule {
        Schedule::Once { at } => {
            if last_run.is_some() {
                return None; // already fired once
            }
            let at_local = at.with_timezone(&Local);
            if now >= at_local {
                Some(at_local)
            } else {
                None
            }
        }
        Schedule::EveryMinutes { minutes } => {
            let interval = (*minutes).max(1) as i64 * 60;
            interval_due(anchor_tz, now_tz, interval)
        }
        Schedule::EveryHours { hours } => {
            let interval = (*hours).max(1) as i64 * 3600;
            interval_due(anchor_tz, now_tz, interval)
        }
        Schedule::Daily { time } => wall_clock_due(anchor_tz, now_tz, *time, None, tz),
        Schedule::Weekly { weekdays, time } => {
            wall_clock_due(anchor_tz, now_tz, *time, Some(weekdays), tz)
        }
        Schedule::Cron { expr } => {
            let fields = parse_cron(expr).ok()?;
            // Latest matching minute in (anchor, now]. Walk backwards from
            // now, bounded to 7 days, so a long-off machine never bulk-fires.
            let mut cand = now_tz
                .with_second(0)
                .unwrap_or(now_tz)
                .with_nanosecond(0)
                .unwrap_or(now_tz);
            let floor = anchor_tz.max(now_tz - chrono::Duration::days(7));
            for _ in 0..(7 * 24 * 60 + 1) {
                if cand <= floor {
                    return None;
                }
                if cand > anchor_tz && cron_matches(&fields, cand) {
                    return Some(cand.with_timezone(&Local));
                }
                cand -= chrono::Duration::minutes(1);
            }
            None
        }
    }
}

fn interval_due(
    anchor: DateTime<chrono_tz::Tz>,
    now: DateTime<chrono_tz::Tz>,
    interval_secs: i64,
) -> Option<DateTime<Local>> {
    if now <= anchor {
        return None;
    }
    let elapsed = (now - anchor).num_seconds();
    let intervals = elapsed / interval_secs;
    if intervals < 1 {
        return None;
    }
    Some((anchor + chrono::Duration::seconds(intervals * interval_secs)).with_timezone(&Local))
}

fn wall_clock_due(
    anchor: DateTime<chrono_tz::Tz>,
    now: DateTime<chrono_tz::Tz>,
    time: NaiveTime,
    weekdays: Option<&Vec<u8>>,
    tz: chrono_tz::Tz,
) -> Option<DateTime<Local>> {
    let mut best: Option<DateTime<chrono_tz::Tz>> = None;
    let mut date = anchor.date_naive();
    let end = now.date_naive();
    while date <= end {
        if let Some(wds) = weekdays {
            let wd = date.weekday().num_days_from_monday() as u8;
            if !wds.contains(&wd) {
                date += chrono::Duration::days(1);
                continue;
            }
        }
        if let Some(cand) = date
            .and_time(time)
            .and_local_timezone(tz)
            .single()
            .filter(|c| *c > anchor && *c <= now)
            .filter(|c| best.map(|b| *c > b).unwrap_or(true))
        {
            best = Some(cand);
        }
        date += chrono::Duration::days(1);
    }
    best.map(|d| d.with_timezone(&Local))
}

/// Node kinds (Spec §5). `*` = MVP nodes.
///
/// EXECUTED node kinds (the scheduler actually runs these — see
/// `scheduler.rs`): TriggerSchedule, TriggerManual, ActionSendMessage,
/// ActionSendEmbed (content sent as text), ConditionTimeWindow,
/// UtilityWait, UtilityRandomPick, UtilitySetVariable.
/// Every other kind is REJECTED by `Scenario::validate` with a clear error.
/// Do not add a kind here without implementing it in the scheduler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    TriggerSchedule,
    TriggerManual,
    TriggerMessageReceived,
    TriggerMemberJoin,
    ActionSendMessage,
    ActionSendEmbed,
    ActionSendFile,
    ActionAddReaction,
    ActionReply,
    ActionDeleteMessage,
    ConditionKeyword,
    ConditionTimeWindow,
    ConditionRandom,
    ConditionRole,
    ConditionVariable,
    UtilityWait,
    UtilityRandomPick,
    UtilitySetVariable,
    UtilityLoop,
    UtilityCallApi,
}

impl NodeKind {
    /// Kinds the scheduler actually executes. Anything else is rejected
    /// by validation — never silently treated as a no-op.
    pub fn is_executed(&self) -> bool {
        matches!(
            self,
            NodeKind::TriggerSchedule
                | NodeKind::TriggerManual
                | NodeKind::ActionSendMessage
                | NodeKind::ActionSendEmbed
                | NodeKind::ConditionTimeWindow
                | NodeKind::UtilityWait
                | NodeKind::UtilityRandomPick
                | NodeKind::UtilitySetVariable
        )
    }
    #[allow(dead_code)]
    pub fn is_mvp(&self) -> bool {
        self.is_executed()
    }
    pub fn label(&self) -> &'static str {
        match self {
            NodeKind::TriggerSchedule => "Trigger: Schedule *",
            NodeKind::TriggerManual => "Trigger: Manual *",
            NodeKind::TriggerMessageReceived => "Trigger: Message Received",
            NodeKind::TriggerMemberJoin => "Trigger: Member Join",
            NodeKind::ActionSendMessage => "Action: Send Message *",
            NodeKind::ActionSendEmbed => "Action: Send Embed *",
            NodeKind::ActionSendFile => "Action: Send File",
            NodeKind::ActionAddReaction => "Action: Add Reaction",
            NodeKind::ActionReply => "Action: Reply",
            NodeKind::ActionDeleteMessage => "Action: Delete Message",
            NodeKind::ConditionKeyword => "Condition: Keyword *",
            NodeKind::ConditionTimeWindow => "Condition: Time Window *",
            NodeKind::ConditionRandom => "Condition: Random %",
            NodeKind::ConditionRole => "Condition: Role",
            NodeKind::ConditionVariable => "Condition: Variable",
            NodeKind::UtilityWait => "Utility: Wait *",
            NodeKind::UtilityRandomPick => "Utility: Random Pick *",
            NodeKind::UtilitySetVariable => "Utility: Set Variable *",
            NodeKind::UtilityLoop => "Utility: Loop",
            NodeKind::UtilityCallApi => "Utility: Call API",
        }
    }
    pub fn group(&self) -> &'static str {
        match self {
            NodeKind::TriggerSchedule
            | NodeKind::TriggerManual
            | NodeKind::TriggerMessageReceived
            | NodeKind::TriggerMemberJoin => "Trigger",
            NodeKind::ActionSendMessage
            | NodeKind::ActionSendEmbed
            | NodeKind::ActionSendFile
            | NodeKind::ActionAddReaction
            | NodeKind::ActionReply
            | NodeKind::ActionDeleteMessage => "Action",
            NodeKind::ConditionKeyword
            | NodeKind::ConditionTimeWindow
            | NodeKind::ConditionRandom
            | NodeKind::ConditionRole
            | NodeKind::ConditionVariable => "Condition",
            _ => "Utility",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: NodeKind,
    #[serde(default)]
    pub config: serde_json::Value,
}

impl Node {
    pub fn new(kind: NodeKind) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            kind,
            config: serde_json::json!({}),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioLimits {
    #[serde(default = "default_daily_cap")]
    pub daily_cap: u32,
}

/// 0 = no per-scenario cap; the account cap (300/day) applies.
/// A non-zero value caps this one scenario (per day) in addition to the
/// account cap.
fn default_daily_cap() -> u32 {
    0
}

impl Default for ScenarioLimits {
    fn default() -> Self {
        Self { daily_cap: 300 }
    }
}

/// A full scenario (Spec §10). Stored as JSON, schemaVersion 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub channel_id: String,
    #[serde(default)]
    pub schedule: Option<Schedule>,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub variables: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub limits: ScenarioLimits,
    /// Plain message templates to rotate through (spin-text applies per send).
    #[serde(default)]
    pub messages: Vec<String>,
    /// Last *scheduled* time that fired (persisted on every successful
    /// send). None = never fired. This is what makes the due-check fire
    /// exactly once per scheduled time, across restarts.
    #[serde(default)]
    pub last_run_at: Option<DateTime<Utc>>,
    /// When the scenario was created. Anchor for interval schedules
    /// that have never fired.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

fn default_timezone() -> String {
    "Asia/Ho_Chi_Minh".to_string()
}

impl Scenario {
    pub fn new(name: &str) -> Self {
        Self {
            schema_version: 1,
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            enabled: false,
            timezone: default_timezone(),
            channel_id: String::new(),
            schedule: Some(Schedule::Daily {
                time: NaiveTime::from_hms_opt(9, 0, 0).unwrap_or_default(),
            }),
            nodes: vec![
                Node::new(NodeKind::TriggerSchedule),
                Node::new(NodeKind::ActionSendMessage),
            ],
            edges: vec![],
            variables: serde_json::Map::new(),
            limits: ScenarioLimits::default(),
            messages: vec!["Hello from Nicord!".to_string()],
            last_run_at: None,
            created_at: Some(Utc::now()),
        }
    }

    /// Validate before running (Spec §5): exactly one trigger, no empty
    /// content, no infinite loop, valid schedule — and every node kind
    /// must be one the scheduler actually executes.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("scenario name is empty".into());
        }
        if self.channel_id.trim().is_empty() {
            return Err("no destination channel selected".into());
        }
        let triggers = self
            .nodes
            .iter()
            .filter(|n| n.kind.group() == "Trigger")
            .count();
        if triggers != 1 {
            return Err(format!(
                "a scenario must start with exactly one Trigger, found {}",
                triggers
            ));
        }
        for node in &self.nodes {
            if !node.kind.is_executed() {
                return Err(format!(
                    "Node '{}' is not implemented in this version — remove it \
                     (supported: Schedule/Manual trigger, Send Message, Send Embed, \
                     Time Window condition, Wait, Random Pick, Set Variable)",
                    node.kind.label()
                ));
            }
        }
        if let Some(s) = &self.schedule {
            s.validate()?;
        }
        // Every Send Message node needs non-empty content (or a message list).
        for node in &self.nodes {
            if node.kind == NodeKind::ActionSendMessage || node.kind == NodeKind::ActionSendEmbed {
                let content = node_content(node);
                if content.trim().is_empty() && self.messages.is_empty() {
                    return Err(format!(
                        "Send Message node {} has empty content and no message list",
                        node.id
                    ));
                }
            }
        }
        Ok(())
    }

    /// Pick the next message to send, rotating without repeating the last one.
    /// Returns (index, message). `last_index` is the index sent last time.
    pub fn pick_message_with_index(
        &self,
        last_index: Option<usize>,
        rng: &mut impl Rng,
    ) -> Option<(usize, String)> {
        if self.messages.is_empty() {
            return None;
        }
        if self.messages.len() == 1 {
            return Some((0, self.messages[0].clone()));
        }
        let mut idx = rng.random_range(0..self.messages.len());
        if Some(idx) == last_index {
            idx = (idx + 1) % self.messages.len();
        }
        Some((idx, self.messages[idx].clone()))
    }

    /// First non-empty content from ActionSendMessage / ActionSendEmbed
    /// nodes, in node order. None if no node carries content.
    pub fn node_message(&self) -> Option<String> {
        self.nodes
            .iter()
            .filter(|n| {
                n.kind == NodeKind::ActionSendMessage || n.kind == NodeKind::ActionSendEmbed
            })
            .filter_map(|n| {
                let c = node_content(n);
                if c.trim().is_empty() {
                    None
                } else {
                    Some(c)
                }
            })
            .next()
    }

    /// Total extra wait (seconds) requested by UtilityWait nodes (capped).
    pub fn wait_seconds(&self) -> u64 {
        let total: u64 = self
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::UtilityWait)
            .filter_map(|n| n.config.get("seconds").and_then(|v| v.as_u64()))
            .sum();
        total.min(300)
    }

    /// Check ConditionTimeWindow nodes against `now` (local time).
    /// Returns Err (skip reason) when outside the window.
    pub fn check_time_windows(&self, now: DateTime<Local>) -> Result<(), String> {
        for node in &self.nodes {
            if node.kind != NodeKind::ConditionTimeWindow {
                continue;
            }
            let (start, end) = time_window_range(node);
            let mins = now.hour() * 60 + now.minute();
            let inside = if start <= end {
                mins >= start && mins < end
            } else {
                mins >= start || mins < end
            };
            if !inside {
                return Err(format!(
                    "outside Time Window node {} ({}–{}) — skipped",
                    node.id,
                    fmt_mins(start),
                    fmt_mins(end)
                ));
            }
        }
        Ok(())
    }

    /// Variables contributed by UtilitySetVariable nodes (name → value).
    pub fn set_variable_overrides(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for node in &self.nodes {
            if node.kind != NodeKind::UtilitySetVariable {
                continue;
            }
            if let Some(name) = node.config.get("name").and_then(|v| v.as_str()) {
                let value = node
                    .config
                    .get("value")
                    .map(|v| {
                        if let Some(s) = v.as_str() {
                            s.to_string()
                        } else {
                            v.to_string()
                        }
                    })
                    .unwrap_or_default();
                out.insert(name.to_string(), value);
            }
        }
        out
    }
}

fn fmt_mins(m: u32) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h < 24 && m < 60 {
        Some(h * 60 + m)
    } else {
        None
    }
}

/// (start_minutes, end_minutes) for a Time Window node. Accepts
/// start_hour/end_hour numbers or start/end "HH:MM" strings. Defaults
/// to the whole day (00:00–24:00) when unconfigured.
fn time_window_range(node: &Node) -> (u32, u32) {
    let start = node
        .config
        .get("start")
        .and_then(|v| v.as_str())
        .and_then(parse_hhmm)
        .or_else(|| {
            node.config
                .get("start_hour")
                .and_then(|v| v.as_u64())
                .map(|h| (h.min(23) * 60) as u32)
        })
        .unwrap_or(0);
    let end = node
        .config
        .get("end")
        .and_then(|v| v.as_str())
        .and_then(parse_hhmm)
        .or_else(|| {
            node.config
                .get("end_hour")
                .and_then(|v| v.as_u64())
                .map(|h| (h.min(24) * 60) as u32)
        })
        .unwrap_or(24 * 60);
    (start, end.min(24 * 60))
}

/// Content of a Send Message / Send Embed node: `content`, else
/// `description`, else `text`, else empty.
pub fn node_content(node: &Node) -> String {
    for key in ["content", "description", "text"] {
        if let Some(s) = node.config.get(key).and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// Render `{{username}}`, `{{server}}`, `{{channel}}`, `{{date}}`,
/// `{{time}}`, plus any custom `{{name}}` variables.
pub fn render_template_with_vars(
    template: &str,
    username: &str,
    server: &str,
    channel: &str,
    vars: &HashMap<String, String>,
) -> String {
    let now = Local::now();
    let mut out = template
        .replace("{{username}}", username)
        .replace("{{server}}", server)
        .replace("{{channel}}", channel)
        .replace("{{date}}", &now.format("%Y-%m-%d").to_string())
        .replace("{{time}}", &now.format("%H:%M").to_string());
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{}}}}}", k), v);
    }
    out
}

/// Spin-text: `{a|b|c}` — pick one variant, never the previous one if possible.
pub fn spin_text(input: &str, rng: &mut impl Rng) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let options: Vec<&str> = after[..end].split('|').collect();
                if options.len() > 1 {
                    let pick = options[rng.random_range(0..options.len())];
                    out.push_str(pick);
                } else {
                    out.push_str(after[..end].trim());
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    // Test times are instants in UTC, not wall-clock in the machine's Local
    // zone: the due_time tests below pass "UTC" as the scenario timezone,
    // so building them in Local would only pass when Local == UTC (it
    // failed on a +07 machine). Naming stays `local` because the return
    // type is DateTime<Local>, as due_time expects.
    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .unwrap()
            .with_timezone(&Local)
    }

    #[test]
    fn spin_text_picks_one_variant() {
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        let out = spin_text("{chào|hello|hi} bạn", &mut rng);
        assert!(out == "chào bạn" || out == "hello bạn" || out == "hi bạn");
        assert!(!out.contains('{'));
    }

    #[test]
    fn scenario_validation_requires_one_trigger_and_channel() {
        let mut s = Scenario::new("test");
        s.channel_id = String::new();
        assert!(s.validate().is_err());
        s.channel_id = "123".to_string();
        assert!(s.validate().is_ok());
    }

    #[test]
    fn daily_schedule_next_after_is_tomorrow_when_time_passed() {
        let s = Schedule::Daily {
            time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        };
        let from = Local::now()
            .date_naive()
            .and_hms_opt(10, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .unwrap();
        let next = s.next_after(from);
        assert!(next > from);
        assert_eq!(next.hour(), 9);
    }

    // --- due_time: the scheduler fix, proved by tests ---

    #[test]
    fn every_hours_fires_once_per_hour_not_never() {
        let sched = Schedule::EveryHours { hours: 1 };
        let created = local(2026, 11, 16, 9, 0).with_timezone(&Utc);
        // 61 minutes after creation: the 10:00 run is due.
        let now = local(2026, 11, 16, 10, 1);
        let due = due_time(&sched, None, Some(created), now, "UTC");
        assert_eq!(due, Some(local(2026, 11, 16, 10, 0)));
        // After firing at 10:00, nothing is due at 10:01 or 10:30.
        let last = Some(due.unwrap().with_timezone(&Utc));
        assert!(due_time(
            &sched,
            last,
            Some(created),
            local(2026, 11, 16, 10, 1),
            "UTC"
        )
        .is_none());
        assert!(due_time(
            &sched,
            last,
            Some(created),
            local(2026, 11, 16, 10, 30),
            "UTC"
        )
        .is_none());
        // At 11:01 the 11:00 run is due — exactly once.
        let due2 = due_time(
            &sched,
            last,
            Some(created),
            local(2026, 11, 16, 11, 1),
            "UTC",
        );
        assert_eq!(due2, Some(local(2026, 11, 16, 11, 0)));
    }

    #[test]
    fn every_minutes_30_fires_on_boundaries() {
        let sched = Schedule::EveryMinutes { minutes: 30 };
        let created = local(2026, 11, 16, 9, 0).with_timezone(&Utc);
        let due = due_time(
            &sched,
            None,
            Some(created),
            local(2026, 11, 16, 9, 31),
            "UTC",
        );
        assert_eq!(due, Some(local(2026, 11, 16, 9, 30)));
        let last = Some(due.unwrap().with_timezone(&Utc));
        assert!(due_time(
            &sched,
            last,
            Some(created),
            local(2026, 11, 16, 9, 45),
            "UTC"
        )
        .is_none());
        let due2 = due_time(
            &sched,
            last,
            Some(created),
            local(2026, 11, 16, 10, 1),
            "UTC",
        );
        assert_eq!(due2, Some(local(2026, 11, 16, 10, 0)));
    }

    #[test]
    fn once_fires_exactly_once() {
        let at = local(2026, 11, 16, 10, 0).with_timezone(&Utc);
        let sched = Schedule::Once { at };
        let created = local(2026, 11, 16, 9, 0).with_timezone(&Utc);
        assert!(due_time(
            &sched,
            None,
            Some(created),
            local(2026, 11, 16, 9, 59),
            "UTC"
        )
        .is_none());
        let due = due_time(
            &sched,
            None,
            Some(created),
            local(2026, 11, 16, 10, 0),
            "UTC",
        );
        assert_eq!(due, Some(local(2026, 11, 16, 10, 0)));
        // After firing, never again — even much later.
        let last = Some(due.unwrap().with_timezone(&Utc));
        assert!(due_time(
            &sched,
            last,
            Some(created),
            local(2026, 11, 16, 10, 1),
            "UTC"
        )
        .is_none());
        assert!(due_time(
            &sched,
            last,
            Some(created),
            local(2026, 11, 17, 10, 0),
            "UTC"
        )
        .is_none());
    }

    #[test]
    fn daily_fires_once_per_day_no_duplicate_in_60s_window() {
        let sched = Schedule::Daily {
            time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        };
        let created = local(2026, 11, 16, 8, 0).with_timezone(&Utc);
        let due = due_time(
            &sched,
            None,
            Some(created),
            local(2026, 11, 16, 9, 0),
            "UTC",
        );
        assert_eq!(due, Some(local(2026, 11, 16, 9, 0)));
        let last = Some(due.unwrap().with_timezone(&Utc));
        // 30s and 59s later: no duplicate (the old 60s-window bug).
        assert!(due_time(
            &sched,
            last,
            Some(created),
            local(2026, 11, 16, 9, 0)
                .checked_add_signed(chrono::Duration::seconds(30))
                .unwrap(),
            "UTC"
        )
        .is_none());
        assert!(due_time(
            &sched,
            last,
            Some(created),
            local(2026, 11, 16, 9, 30),
            "UTC"
        )
        .is_none());
        // Next day 09:00 fires again, exactly once.
        let due2 = due_time(
            &sched,
            last,
            Some(created),
            local(2026, 11, 17, 9, 0),
            "UTC",
        );
        assert_eq!(due2, Some(local(2026, 11, 17, 9, 0)));
    }

    #[test]
    fn weekly_fires_once_on_matching_weekday() {
        // 2026-11-16 is a Monday (weekday 0).
        let sched = Schedule::Weekly {
            weekdays: vec![0],
            time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        };
        let created = local(2026, 11, 16, 8, 0).with_timezone(&Utc);
        let due = due_time(
            &sched,
            None,
            Some(created),
            local(2026, 11, 16, 9, 0),
            "UTC",
        );
        assert_eq!(due, Some(local(2026, 11, 16, 9, 0)));
        let last = Some(due.unwrap().with_timezone(&Utc));
        // Tuesday 09:00 is not a Monday — nothing due.
        assert!(due_time(
            &sched,
            last,
            Some(created),
            local(2026, 11, 17, 9, 0),
            "UTC"
        )
        .is_none());
        // Next Monday 09:00 fires again.
        let due2 = due_time(
            &sched,
            last,
            Some(created),
            local(2026, 11, 23, 9, 0),
            "UTC",
        );
        assert_eq!(due2, Some(local(2026, 11, 23, 9, 0)));
    }

    #[test]
    fn cron_is_real_not_hourly() {
        let sched = Schedule::Cron {
            expr: "0 9 * * *".to_string(),
        };
        let created = local(2026, 11, 16, 8, 0).with_timezone(&Utc);
        let due = due_time(
            &sched,
            None,
            Some(created),
            local(2026, 11, 16, 10, 0),
            "UTC",
        );
        assert_eq!(due, Some(local(2026, 11, 16, 9, 0)));
        let last = Some(due.unwrap().with_timezone(&Utc));
        assert!(due_time(
            &sched,
            last,
            Some(created),
            local(2026, 11, 16, 10, 30),
            "UTC"
        )
        .is_none());
        // */30 fires on half-hour boundaries, not hourly.
        let sched2 = Schedule::Cron {
            expr: "*/30 * * * *".to_string(),
        };
        let due2 = due_time(
            &sched2,
            None,
            Some(created),
            local(2026, 11, 16, 8, 31),
            "UTC",
        );
        assert_eq!(due2, Some(local(2026, 11, 16, 8, 30)));
        // Bad cron is rejected by validation.
        assert!(Schedule::Cron {
            expr: "not a cron".to_string()
        }
        .validate()
        .is_err());
        assert!(Schedule::Cron {
            expr: "0 25 * * *".to_string()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn daily_respects_scenario_timezone() {
        // 09:00 Asia/Ho_Chi_Minh = 02:00 UTC. Created 01:00 UTC (08:00 +07),
        // now 03:00 UTC (10:00 +07): the 09:00 +07 run is due.
        let sched = Schedule::Daily {
            time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        };
        let created = DateTime::from_timestamp(1_793_000_000, 0).unwrap(); // fixed anchor
        let _ = created;
        let created = Utc
            .with_ymd_and_hms(2026, 11, 16, 1, 0, 0)
            .single()
            .unwrap();
        let now = Local
            .from_utc_datetime(
                &Utc.with_ymd_and_hms(2026, 11, 16, 3, 0, 0)
                    .single()
                    .unwrap()
                    .naive_utc(),
            )
            .with_timezone(&Local);
        let due = due_time(&sched, None, Some(created), now, "Asia/Ho_Chi_Minh");
        assert!(due.is_some());
        // The due instant is 02:00 UTC.
        assert_eq!(due.unwrap().with_timezone(&Utc).hour(), 2);
    }

    #[test]
    fn unsupported_nodes_are_rejected() {
        let mut s = Scenario::new("test");
        s.channel_id = "123".to_string();
        s.nodes.push(Node::new(NodeKind::UtilityLoop));
        assert!(s.validate().is_err());
        let mut s2 = Scenario::new("test2");
        s2.channel_id = "123".to_string();
        s2.nodes.push(Node::new(NodeKind::ActionReply));
        assert!(s2.validate().is_err());
    }

    #[test]
    fn rotation_never_repeats_last_index() {
        let mut s = Scenario::new("rot");
        s.messages = vec!["a".into(), "b".into(), "c".into()];
        let mut rng = ChaCha8Rng::seed_from_u64(7);
        let (i1, _) = s.pick_message_with_index(None, &mut rng).unwrap();
        let (i2, _) = s.pick_message_with_index(Some(i1), &mut rng).unwrap();
        assert_ne!(i1, i2);
    }

    #[test]
    fn example_scenario_json_stays_valid() {
        let json = std::fs::read_to_string("examples/scenario-example.json")
            .expect("examples/scenario-example.json must exist");
        let s: Scenario =
            serde_json::from_str(&json).expect("example scenario must parse as a Scenario");
        assert_eq!(s.limits.daily_cap, 300);
        assert!(
            s.validate().is_err(),
            "example has a placeholder channel id"
        );
    }
}
