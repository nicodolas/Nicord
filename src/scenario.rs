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

use crate::preview::{char_count, DISCORD_CONTENT_LIMIT};
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
            Schedule::Weekly { weekdays, .. } => {
                if weekdays.is_empty() {
                    Err("weekly schedule needs at least one weekday".into())
                } else if weekdays.iter().any(|w| *w > 6) {
                    Err("weekly weekdays must be 0 (Mon) .. 6 (Sun)".into())
                } else {
                    Ok(())
                }
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
/// ActionSendEmbed (content sent as text), ActionSendCommand,
/// ConditionTimeWindow, UtilityWait, UtilityRandomPick, UtilitySetVariable.
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
    /// Send a Discord command: a prefix/text command (`!rank arg1`)
    /// or a slash command (`/daily`). See `CommandConfig`.
    ActionSendCommand,
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
                | NodeKind::ActionSendCommand
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
            NodeKind::ActionSendCommand => "Action: Send Command *",
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
            | NodeKind::ActionSendCommand
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

// ---------------------------------------------------------------------------
// Discord commands (`ActionSendCommand`): a prefix/text command or a
// slash command, configured per node — name, parameters, and (for slash)
// the bot's application ID. Maximally flexible by design: every field
// the user may need is a plain config value, never hardcoded.
// ---------------------------------------------------------------------------

/// Discord application-command option type codes.
/// 3 string, 4 integer, 5 boolean, 6 user, 7 channel, 8 role, 10 number.
pub fn slash_option_kind_label(kind: u8) -> &'static str {
    match kind {
        3 => "Chuỗi",
        4 => "Số nguyên",
        5 => "Đúng / Sai",
        6 => "Người dùng",
        7 => "Kênh",
        8 => "Vai trò",
        10 => "Số thực",
        _ => "Chuỗi",
    }
}

/// All option kinds the editor offers, in UI order.
pub const SLASH_OPTION_KINDS: &[(u8, &str)] = &[
    (3, "Chuỗi"),
    (4, "Số nguyên"),
    (10, "Số thực"),
    (5, "Đúng / Sai"),
    (6, "Người dùng"),
    (7, "Kênh"),
    (8, "Vai trò"),
];

/// One slash-command parameter (name + type + raw value; the value is
/// parsed per type when the interaction payload is built).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlashOption {
    #[serde(default)]
    pub name: String,
    /// Discord option type code (see `slash_option_kind_label`).
    #[serde(default = "default_slash_option_kind")]
    pub kind: u8,
    /// Raw value as typed by the user (number/bool parsed at send time;
    /// user/channel/role values are snowflake IDs as text).
    #[serde(default)]
    pub value: String,
}

impl Default for SlashOption {
    fn default() -> Self {
        Self {
            name: String::new(),
            // NOTE: a derived Default would give kind 0 (invalid type);
            // the serde default fn only applies on deserialization.
            kind: default_slash_option_kind(),
            value: String::new(),
        }
    }
}

fn default_slash_option_kind() -> u8 {
    3
}

/// Config of an `ActionSendCommand` node, stored in `Node.config`.
/// Unknown/missing fields fall back to defaults, so old scenarios
/// (and hand-edited JSON) still load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandConfig {
    /// "prefix" (default) or "slash".
    #[serde(default = "default_command_kind")]
    pub kind: String,
    /// Prefix for text commands, e.g. "!" (default) or "/" for bots
    /// that take slash-style text. Empty means "!".
    #[serde(default = "default_command_prefix")]
    pub prefix: String,
    /// Command name without any prefix, e.g. "rank" or "daily".
    #[serde(default)]
    pub name: String,
    /// Ordered arguments for a prefix command, e.g. ["arg1", "arg2"].
    #[serde(default)]
    pub args: Vec<String>,
    /// The bot's application ID (slash only, user-provided).
    #[serde(default)]
    pub application_id: String,
    /// Application command ID (slash only). Empty = auto-resolve via
    /// the channel's application-commands search; if that fails, the
    /// user must paste the ID manually.
    #[serde(default)]
    pub command_id: String,
    /// Application command version (slash only, optional).
    #[serde(default)]
    pub version: String,
    /// Slash command parameters.
    #[serde(default)]
    pub options: Vec<SlashOption>,
}

fn default_command_kind() -> String {
    "prefix".to_string()
}

fn default_command_prefix() -> String {
    "!".to_string()
}

impl Default for CommandConfig {
    fn default() -> Self {
        Self {
            kind: default_command_kind(),
            prefix: default_command_prefix(),
            name: String::new(),
            args: Vec::new(),
            application_id: String::new(),
            command_id: String::new(),
            version: String::new(),
            options: Vec::new(),
        }
    }
}

impl CommandConfig {
    /// Parse a node's config (tolerant: unknown fields ignored).
    pub fn from_node(node: &Node) -> Self {
        serde_json::from_value(node.config.clone()).unwrap_or_default()
    }

    pub fn is_slash(&self) -> bool {
        self.kind.trim().eq_ignore_ascii_case("slash")
    }

    /// Short display name for node lists: `!rank` or `/daily`.
    pub fn display_name(&self) -> String {
        let name = self.name.trim();
        if self.is_slash() {
            format!("/{name}")
        } else {
            format!("{}{name}", self.effective_prefix())
        }
    }

    /// The prefix actually used (empty config means "!").
    pub fn effective_prefix(&self) -> String {
        let p = self.prefix.trim();
        if p.is_empty() {
            "!".to_string()
        } else {
            p.to_string()
        }
    }

    /// Compose a prefix command's message text: `{prefix}{name}` plus
    /// non-empty args joined by spaces. Empty args are skipped.
    pub fn compose_prefix(&self) -> String {
        let mut out = format!("{}{}", self.effective_prefix(), self.name.trim());
        for a in &self.args {
            let a = a.trim();
            if !a.is_empty() {
                out.push(' ');
                out.push_str(a);
            }
        }
        out
    }

    /// Validate before running: a name is always required; slash also
    /// needs the bot's application ID. Command ID / version are
    /// optional (auto-resolved when empty).
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("Send Command node needs a command name".into());
        }
        if self.is_slash() && self.application_id.trim().is_empty() {
            return Err(
                "Slash command needs the bot's Application ID (paste it in the node editor)".into(),
            );
        }
        Ok(())
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

/// 0 = no per-scenario cap; the account/global cap applies (the
/// scheduler maps 0 to "unlimited" and takes the min with the
/// account/global cap). A non-zero value caps this one scenario
/// (per day) in addition to that cap.
fn default_daily_cap() -> u32 {
    0
}

impl Default for ScenarioLimits {
    /// Must match the serde default above: 0 (inherit the
    /// account/global cap). A derived-style Default of 300 used to
    /// make new scenarios behave differently from imported ones
    /// missing the field. Guarded by `default_matches_serde_default`.
    fn default() -> Self {
        Self {
            daily_cap: default_daily_cap(),
        }
    }
}

/// A full scenario (Spec §10). Stored as JSON, schemaVersion 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    /// Missing in older exports → 1 (current schema), so imports
    /// warn instead of failing on a missing version field.
    #[serde(rename = "schemaVersion", default = "default_schema_version")]
    pub schema_version: u32,
    /// Missing in hand-written files → "" (the importer always assigns
    /// a fresh UUID, so a missing id warns instead of failing).
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// Serializes as `channel_id`; also accepts the legacy camelCase
    /// `channelId` on read, so older exported JSON keeps its channel.
    #[serde(default, alias = "channelId")]
    pub channel_id: String,
    /// Destination id in Webhook mode (required when enabling).
    /// Ignored in user-token mode (which uses `channel_id`).
    /// Kept alongside `channel_id` so switching modes loses nothing.
    #[serde(default)]
    pub destination_id: Option<String>,
    /// When true (webhook mode), send from the destination's
    /// default messages instead of this scenario's own messages,
    /// even when the scenario has messages. When false, the
    /// destination defaults are only a fallback for an empty
    /// scenario message list (see the engine's fallback chain).
    #[serde(default)]
    pub use_destination_messages: bool,
    /// Optional rich embed attached to every message this scenario
    /// sends (Q8). None = plain text only. An embed may accompany
    /// empty content (Discord requires content OR embeds).
    #[serde(default)]
    pub embed: Option<crate::embed::Embed>,
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

fn default_schema_version() -> u32 {
    1
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
            destination_id: None,
            use_destination_messages: false,
            embed: None,
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
    /// User-token mode: a destination channel is required and command
    /// nodes are allowed.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_inner(true, true, false)
    }

    /// Webhook-mode validation: no channel is required (the webhook
    /// URL is channel-bound), but command nodes are rejected —
    /// webhooks cannot send commands. All other checks are identical
    /// to `validate()`.
    pub fn validate_for_webhook(&self) -> Result<(), String> {
        self.validate_inner(false, false, true)
    }

    fn validate_inner(
        &self,
        require_channel: bool,
        allow_commands: bool,
        require_destination: bool,
    ) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("scenario name is empty".into());
        }
        // Fail-closed: an unknown timezone must not silently run in
        // UTC (tz_from_name falls back). Reject it at validation.
        if self.timezone.parse::<chrono_tz::Tz>().is_err() {
            return Err(format!(
                "unknown timezone '{}' — use an IANA name like Asia/Ho_Chi_Minh",
                self.timezone
            ));
        }
        if require_channel && self.channel_id.trim().is_empty() {
            return Err("no destination channel selected".into());
        }
        if require_destination
            && self
                .destination_id
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty()
        {
            return Err("no destination selected — pick a destination in the Builder".into());
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
                     Send Command, Time Window condition, Wait, Random Pick, Set Variable)",
                    node.kind.label()
                ));
            }
            if node.kind == NodeKind::ActionSendCommand {
                if !allow_commands {
                    return Err(
                        "webhook mode does not support Send Command nodes — use Send Message, or switch back to account mode"
                            .into(),
                    );
                }
                CommandConfig::from_node(node)
                    .validate()
                    .map_err(|e| format!("Send Command node {}: {e}", node.id))?;
            }
            // Time Window nodes fail CLOSED: a present-but-unparsable
            // start/end is a validation error, never a silent
            // whole-day default (which would send at any hour).
            if node.kind == NodeKind::ConditionTimeWindow {
                validate_time_window(node)
                    .map_err(|e| format!("Time Window node {}: {e}", node.id))?;
            }
        }
        if let Some(s) = &self.schedule {
            s.validate()?;
        }
        // Every Send Message node needs non-empty content (or a message list)
        // — unless the scenario runs a command instead: then the message
        // nodes are ignored and need no content.
        let has_command = self.command_node().is_some();
        for node in &self.nodes {
            if node.kind == NodeKind::ActionSendMessage || node.kind == NodeKind::ActionSendEmbed {
                let content = node_content(node);
                if content.trim().is_empty() && self.messages.is_empty() && !has_command {
                    return Err(format!(
                        "Send Message node {} has empty content and no message list",
                        node.id
                    ));
                }
            }
        }
        // Embed limits (Q8): validated in both modes, since both
        // transports send the same embed payload.
        if let Some(embed) = &self.embed {
            embed.validate()?;
        }
        // Discord rejects any message over 2000 characters, so fail fast
        // here with a clear error instead of failing on every send.
        // This checks the raw text (before template vars / spin-text):
        // conservative for templates that expand, exact for plain text.
        for (idx, msg) in self.messages.iter().enumerate() {
            if char_count(msg) > DISCORD_CONTENT_LIMIT {
                return Err(format!(
                    "message {} is {} characters, over Discord's 2000-character limit",
                    idx + 1,
                    char_count(msg)
                ));
            }
        }
        for node in &self.nodes {
            if node.kind == NodeKind::ActionSendMessage || node.kind == NodeKind::ActionSendEmbed {
                let content = node_content(node);
                if char_count(&content) > DISCORD_CONTENT_LIMIT {
                    return Err(format!(
                        "Send Message node {} content is {} characters, over Discord's 2000-character limit",
                        node.id,
                        char_count(&content)
                    ));
                }
            }
        }
        Ok(())
    }

    /// First `ActionSendCommand` node, if any. One scheduled firing
    /// runs exactly that node (and ignores message nodes) — running
    /// several commands per firing would duplicate earlier commands
    /// when a later one fails and the run is retried.
    pub fn command_node(&self) -> Option<&Node> {
        self.nodes
            .iter()
            .find(|n| n.kind == NodeKind::ActionSendCommand)
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

/// Validate a Time Window node's config (fail-closed). Absent
/// start/end means "whole day" and is valid; a present value that
/// does not parse (bad "HH:MM", hour > 23) is an error.
fn validate_time_window(node: &Node) -> Result<(), String> {
    for key in ["start", "end"] {
        if let Some(v) = node.config.get(key) {
            let s = v
                .as_str()
                .ok_or_else(|| format!("'{key}' must be an \"HH:MM\" string"))?;
            if parse_hhmm(s).is_none() {
                return Err(format!("'{key}' must be HH:MM (24h), got '{s}'"));
            }
        }
    }
    for key in ["start_hour", "end_hour"] {
        if let Some(v) = node.config.get(key) {
            let h = v
                .as_u64()
                .ok_or_else(|| format!("'{key}' must be a whole hour 0..=23"))?;
            if h > 23 {
                return Err(format!("'{key}' must be 0..=23, got {h}"));
            }
        }
    }
    Ok(())
}

/// (start_minutes, end_minutes) for a Time Window node. Accepts
/// start_hour/end_hour numbers or start/end "HH:MM" strings. Defaults
/// to the whole day (00:00–24:00) when unconfigured. Validation
/// (see `validate_time_window`) rejects malformed values before a
/// scenario can be enabled, so this never silently widens a window
/// the user mistyped.
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

/// Analyse a scenario JSON file before importing it (QoL import checks).
///
/// Returns `(scenario, warnings)`. `scenario` is `Some` only when the
/// file parses AND passes mode-aware validation. `warnings` is never
/// silent: missing fields (filled with defaults), secret-looking
/// content, and parse/validation failures are all reported with
/// enough detail to fix the file. Nothing is written here — the
/// caller decides whether to import.
///
/// Notes on rules that do NOT apply to scenario JSON: there are no
/// min/max field pairs in the Scenario schema (time windows with
/// start > end are valid overnight windows, not swapped), and the
/// importer always assigns a fresh UUID, so duplicate IDs cannot occur.
pub fn analyze_import(text: &str, is_webhook: bool) -> (Option<Scenario>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();

    // Secret scan (values are never included in warnings).
    if text.contains("api/webhooks/") {
        warnings.push(
            "File chứa URL webhook — hãy coi file này như file chứa mật khẩu, đừng chia sẻ."
                .to_string(),
        );
    }
    if text.contains("\"token\"") || text.contains("\"encrypted_token\"") {
        warnings.push(
            "File chứa trường token — kiểm tra kỹ nguồn gốc file trước khi import.".to_string(),
        );
    }

    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!(
                "JSON lỗi tại dòng {}, cột {}: {}",
                e.line(),
                e.column(),
                e
            ));
            return (None, warnings);
        }
    };
    let Some(obj) = value.as_object() else {
        warnings.push("File phải là một đối tượng JSON của scenario.".to_string());
        return (None, warnings);
    };
    // Missing-field warnings (serde fills defaults; the user should know).
    // `channelId` counts as `channel_id` (legacy alias).
    for key in [
        "schemaVersion",
        "id",
        "name",
        "enabled",
        "timezone",
        "destination_id",
        "schedule",
        "nodes",
        "edges",
        "variables",
        "limits",
        "messages",
    ] {
        if !obj.contains_key(key) {
            warnings.push(format!("Thiếu trường '{key}' — dùng giá trị mặc định."));
        }
    }
    if !obj.contains_key("channel_id") && !obj.contains_key("channelId") {
        warnings.push("Thiếu trường 'channel_id' — dùng giá trị mặc định.".to_string());
    }

    let scenario: Scenario = match serde_json::from_value(value) {
        Ok(s) => s,
        Err(e) => {
            warnings.push(format!("Không đọc được scenario: {e}"));
            return (None, warnings);
        }
    };
    // Import validation is relaxed for the destination: an imported
    // scenario arrives disabled, and the user picks its destination
    // in the Builder (a destination id from another machine is
    // meaningless here). Enable still requires a destination.
    let valid = if is_webhook {
        scenario.validate_inner(false, false, false)
    } else {
        scenario.validate()
    };
    if let Err(e) = valid {
        warnings.push(format!("Không hợp lệ: {e}"));
        return (None, warnings);
    }
    (Some(scenario), warnings)
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
    fn weekly_rejects_out_of_range_weekdays() {
        let time = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let bad = Schedule::Weekly {
            weekdays: vec![7],
            time,
        };
        assert!(bad.validate().is_err());
        let bad2 = Schedule::Weekly {
            weekdays: vec![0, 255],
            time,
        };
        assert!(bad2.validate().is_err());
        let good = Schedule::Weekly {
            weekdays: vec![0, 6],
            time,
        };
        assert!(good.validate().is_ok());
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
    fn prefix_command_composes_with_args_and_skips_empty() {
        let cfg = CommandConfig {
            name: "rank".into(),
            args: vec!["@nico".into(), "".into(), "  ".into(), "top".into()],
            ..Default::default()
        };
        assert_eq!(cfg.compose_prefix(), "!rank @nico top");
        assert_eq!(cfg.display_name(), "!rank");
        // No args: just prefix + name.
        let bare = CommandConfig {
            name: "daily".into(),
            ..Default::default()
        };
        assert_eq!(bare.compose_prefix(), "!daily");
        // Empty prefix falls back to "!"; a custom prefix is kept.
        let slashy = CommandConfig {
            prefix: "/".into(),
            name: "work".into(),
            ..Default::default()
        };
        assert_eq!(slashy.compose_prefix(), "/work");
        let no_prefix = CommandConfig {
            prefix: "   ".into(),
            name: "x".into(),
            ..Default::default()
        };
        assert_eq!(no_prefix.effective_prefix(), "!");
    }

    #[test]
    fn slash_command_config_validation() {
        // Prefix needs a name.
        let mut cfg = CommandConfig::default();
        assert!(cfg.validate().is_err());
        cfg.name = "rank".into();
        assert!(cfg.validate().is_ok());
        // Slash needs application_id + name; command_id/version optional.
        cfg.kind = "slash".into();
        assert!(cfg.validate().is_err());
        cfg.application_id = "123456789".into();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.display_name(), "/rank");
        assert!(cfg.is_slash());
    }

    #[test]
    fn command_node_wins_and_validates() {
        let mut s = Scenario::new("cmd");
        s.channel_id = "123".to_string();
        // Fresh scenario has a Send Message node + a message list, so it
        // validates; add a command node with an empty name -> error.
        let mut node = Node::new(NodeKind::ActionSendCommand);
        node.config = serde_json::json!({"kind": "prefix", "name": ""});
        s.nodes.push(node);
        assert!(s.validate().is_err());
        // Fill in the name: valid, and command_node() finds it.
        let cmd = s.command_node().unwrap();
        assert_eq!(CommandConfig::from_node(cmd).name, "");
        if let Some(n) = s
            .nodes
            .iter_mut()
            .find(|n| n.kind == NodeKind::ActionSendCommand)
        {
            n.config = serde_json::json!({"kind": "prefix", "name": "rank"});
        }
        assert!(s.validate().is_ok());
        assert_eq!(
            CommandConfig::from_node(s.command_node().unwrap()).display_name(),
            "!rank"
        );
        // A scenario with ONLY a command node (no messages at all) is
        // valid: the message-content check is skipped for commands.
        let mut s2 = Scenario::new("cmd2");
        s2.channel_id = "123".to_string();
        s2.messages.clear();
        s2.nodes.retain(|n| n.kind == NodeKind::TriggerSchedule);
        let mut node2 = Node::new(NodeKind::ActionSendCommand);
        node2.config =
            serde_json::json!({"kind": "slash", "name": "daily", "application_id": "999"});
        s2.nodes.push(node2);
        assert!(s2.validate().is_ok());
    }

    #[test]
    fn validation_rejects_overlong_messages() {
        let mut s = Scenario::new("long");
        s.channel_id = "123".to_string();
        s.messages = vec!["a".repeat(2001)];
        let err = s.validate().unwrap_err();
        assert!(err.contains("2000"), "unexpected error: {err}");
        s.messages = vec!["a".repeat(2000)];
        assert!(s.validate().is_ok());
    }

    #[test]
    fn analyze_import_warns_and_rejects_bad_files() {
        // Valid minimal file: warns about missing fields, still imports.
        let (sc, warns) = analyze_import(
            r#"{"name":"t","channel_id":"1","nodes":[{"id":"n1","type":"trigger_schedule","config":{}}],"messages":["hi"]}"#,
            false,
        );
        assert!(sc.is_some(), "warnings: {warns:?}");
        assert!(warns.iter().any(|w| w.contains("schemaVersion")));
        // Secret scan warns without including the URL.
        let (_, warns) = analyze_import(
            r#"{"name":"t","channel_id":"1","nodes":[],"messages":["see https://discord.com/api/webhooks/1/SECRET"]}"#,
            false,
        );
        assert!(warns.iter().any(|w| w.contains("mật khẩu")));
        assert!(!warns.join(" ").contains("SECRET"));
        // Bad JSON reports line/column and imports nothing.
        let (sc, warns) = analyze_import("{\n  bad", false);
        assert!(sc.is_none());
        assert!(warns.iter().any(|w| w.contains("dòng")));
        // Invalid scenario (empty name) is rejected with the reason.
        let (sc, warns) = analyze_import(
            r#"{"name":"","channel_id":"1","nodes":[],"messages":["hi"]}"#,
            false,
        );
        assert!(sc.is_none());
        assert!(warns.iter().any(|w| w.contains("Không hợp lệ")));
    }

    #[test]
    fn webhook_validation_allows_empty_channel_but_rejects_commands() {
        // Message scenario with no channel: user-mode validation fails,
        // webhook validation passes (the URL is channel-bound) once a
        // destination is selected; without one it fails.
        let mut s = Scenario::new("webhook");
        assert!(s.validate().is_err());
        assert!(s.validate_for_webhook().is_err());
        s.destination_id = Some("dest-1".to_string());
        assert!(s.validate_for_webhook().is_ok());
        // Adding a command node flips webhook validation to an error,
        // while user-mode validation (with a channel) still passes.
        let mut s2 = Scenario::new("webhook-cmd");
        s2.channel_id = "123".to_string();
        s2.destination_id = Some("dest-1".to_string());
        let mut node = Node::new(NodeKind::ActionSendCommand);
        node.config = serde_json::json!({"kind": "prefix", "name": "rank"});
        s2.nodes.push(node);
        assert!(s2.validate().is_ok());
        let err = s2.validate_for_webhook().unwrap_err();
        assert!(err.contains("webhook mode"), "unexpected error: {err}");
    }

    #[test]
    fn time_window_validation_fails_closed() {
        let mut s = Scenario::new("tw");
        s.channel_id = "1".to_string();
        // No Time Window node: fine.
        assert!(s.validate().is_ok());
        // Unconfigured window: whole day, fine.
        let mut node = Node::new(NodeKind::ConditionTimeWindow);
        node.config = serde_json::json!({});
        s.nodes.push(node);
        assert!(s.validate().is_ok());
        // Malformed start: rejected.
        s.nodes.last_mut().unwrap().config = serde_json::json!({"start": "25:00", "end": "06:00"});
        assert!(s.validate().is_err());
        // Overnight window: fine.
        s.nodes.last_mut().unwrap().config = serde_json::json!({"start": "22:00", "end": "06:00"});
        assert!(s.validate().is_ok());
        // Bad hour number: rejected.
        s.nodes.last_mut().unwrap().config = serde_json::json!({"start_hour": 24});
        assert!(s.validate().is_err());
    }

    #[test]
    fn slash_option_kind_labels() {
        assert_eq!(slash_option_kind_label(3), "Chuỗi");
        assert_eq!(slash_option_kind_label(4), "Số nguyên");
        assert_eq!(slash_option_kind_label(5), "Đúng / Sai");
        assert_eq!(slash_option_kind_label(6), "Người dùng");
        assert_eq!(slash_option_kind_label(7), "Kênh");
        assert_eq!(slash_option_kind_label(8), "Vai trò");
        assert_eq!(slash_option_kind_label(10), "Số thực");
        assert_eq!(slash_option_kind_label(99), "Chuỗi");
    }

    #[test]
    fn example_scenario_json_stays_valid() {
        let json = std::fs::read_to_string("examples/scenario-example.json")
            .expect("examples/scenario-example.json must exist");
        let s: Scenario =
            serde_json::from_str(&json).expect("example scenario must parse as a Scenario");
        assert_eq!(s.limits.daily_cap, 300);
        // The example keeps its channel field (a placeholder the user must
        // replace with a real channel ID) and is structurally valid.
        assert_eq!(s.channel_id, "REPLACE_WITH_YOUR_CHANNEL_ID");
        assert!(s.validate().is_ok());
    }

    #[test]
    fn limits_default_matches_serde_default() {
        // 0 = inherit the account/global cap, in both paths: a
        // scenario built in code and one imported without the field
        // must behave identically.
        let from_code = ScenarioLimits::default();
        let from_json: ScenarioLimits = serde_json::from_str("{}").unwrap();
        assert_eq!(from_code.daily_cap, 0);
        assert_eq!(from_json.daily_cap, 0);
    }

    #[test]
    fn scenario_accepts_camelcase_channel_id_alias() {
        let json = r#"{"schemaVersion":1,"id":"x","name":"t","channelId":"123","schedule":{"kind":"daily","time":"09:00:00"},"nodes":[{"id":"n1","type":"trigger_schedule"},{"id":"n2","type":"action_send_message","config":{"content":"hi"}}],"messages":["hi"]}"#;
        let s: Scenario = serde_json::from_str(json).unwrap();
        assert_eq!(s.channel_id, "123");
        assert!(s.validate().is_ok());
    }
}
