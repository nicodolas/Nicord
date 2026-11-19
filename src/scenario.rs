//! Scenario model: nodes, edges, schedule, validation, spin-text.
//! A scenario is data (JSON), never code. Spec §5 and §10.
//!
//! Scheduler semantics (fixed, see `due_time`):
//! - `last_run_at` is persisted on every successful send. A schedule
//!   fires once per scheduled time: the due time is the latest scheduled
//!   time in (anchor, now], where anchor = last_run or creation time.
//!   There is no 60-second "now - 60s" window, so no duplicates and no
//!   missed intervals for EveryMinutes/EveryHours/EverySeconds.
//! - `Once` fires exactly once, then the scenario is disabled.
//! - Cron is a real 5-field parser (minute hour dom month dow), not hourly.
//! - DST policy (L4, one policy for firing and display): an ambiguous
//!   wall-clock time fires once, at its FIRST occurrence; a nonexistent
//!   wall-clock time never fires. See `resolve_wall_time`.

use crate::preview::{char_count, DISCORD_CONTENT_LIMIT};
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, Timelike, Utc};
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
    /// Every N seconds (minimum `MIN_EVERY_SECONDS` — see below).
    EverySeconds { seconds: u32 },
    /// Daily at a local time.
    Daily { time: NaiveTime },
    /// Weekly on given weekdays (0 = Mon .. 6 = Sun) at a local time.
    Weekly { weekdays: Vec<u8>, time: NaiveTime },
    /// Advanced cron string. Real 5-field parser.
    Cron { expr: String },
}

/// Minimum interval for `Schedule::EverySeconds` (seconds).
///
/// The engine scans for due scenarios every second, so the scan is
/// not the binding constraint — the send path is: each destination
/// only sends once per `min_interval_secs` (default 60 s, hard floor
/// 5 s), and a run that fires early is deferred, not sent. Below
/// 60 s the schedule would promise a rate the send path cannot
/// keep (webhook mode would effectively send every 60 s), so 60 s
/// is the honest floor.
pub const MIN_EVERY_SECONDS: u32 = 60;

/// Plain-words rendering of a second count, e.g. 3600 -> "1 giờ",
/// 5400 -> "1 giờ 30 phút", 90 -> "1 phút 30 giây". Used by the
/// schedule editor's "Tùy chỉnh" hint so users never have to do
/// the conversion themselves.
pub fn describe_seconds(seconds: u32) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h} giờ"));
    }
    if m > 0 {
        parts.push(format!("{m} phút"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(format!("{s} giây"));
    }
    parts.join(" ")
}

impl Schedule {
    pub fn label(&self) -> String {
        match self {
            Schedule::Once { at } => format!("Một lần lúc {}", at.format("%Y-%m-%d %H:%M UTC")),
            Schedule::EveryMinutes { minutes } => format!("Mỗi {} phút", minutes),
            Schedule::EveryHours { hours } => format!("Mỗi {} giờ", hours),
            Schedule::EverySeconds { seconds } => format!("Cứ mỗi {} giây", seconds),
            Schedule::Daily { time } => format!("Hàng ngày lúc {}", time.format("%H:%M")),
            Schedule::Weekly { weekdays, time } => {
                const NAMES: [&str; 7] = ["T2", "T3", "T4", "T5", "T6", "T7", "CN"];
                let days: Vec<&str> = weekdays
                    .iter()
                    .map(|d| NAMES.get(*d as usize).copied().unwrap_or("?"))
                    .collect();
                format!("Hàng tuần {} lúc {}", days.join(", "), time.format("%H:%M"))
            }
            Schedule::Cron { expr } => format!("Cron `{}`", expr),
        }
    }

    /// Next run strictly after `from`, computed in the scenario's
    /// timezone and returned in local time for display. Used for
    /// display ("next run at …"), not for the firing decision — see
    /// `due_time`, which uses the same timezone. Computing in Local
    /// (or UTC for cron) showed the wrong time whenever the scenario
    /// timezone differed from the machine's.
    pub fn next_after(&self, from: DateTime<Local>, timezone: &str) -> DateTime<Local> {
        let tz = tz_from_name(timezone);
        let from_tz = from.with_timezone(&tz);
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
            Schedule::EverySeconds { seconds } => {
                // Absolute-duration semantics, like EveryMinutes /
                // EveryHours: N real seconds after `from`, whatever
                // the wall clock does across a DST transition.
                let s = (*seconds).max(MIN_EVERY_SECONDS) as i64;
                from + chrono::Duration::seconds(s)
            }
            Schedule::Daily { time } => {
                // Same DST policy as the firing decision (L4):
                // ambiguous resolves to the first occurrence,
                // nonexistent skips to the next day. Resolving
                // per date (instead of adding 24h to today's
                // occurrence) also keeps the wall time exact
                // across a DST transition.
                for day_offset in 0..8 {
                    let candidate_date = from_tz.date_naive() + chrono::Duration::days(day_offset);
                    if let Some(c) = resolve_wall_time(candidate_date, *time, tz) {
                        if c > from_tz {
                            return c.with_timezone(&Local);
                        }
                    }
                }
                (from_tz + chrono::Duration::days(1)).with_timezone(&Local)
            }
            Schedule::Weekly { weekdays, time } => {
                for day_offset in 0..8 {
                    let candidate_date = from_tz.date_naive() + chrono::Duration::days(day_offset);
                    let weekday = candidate_date.weekday().num_days_from_monday() as u8;
                    if weekdays.contains(&weekday) {
                        let candidate = resolve_wall_time(candidate_date, *time, tz);
                        if let Some(c) = candidate {
                            if c > from_tz {
                                return c.with_timezone(&Local);
                            }
                        }
                    }
                }
                from + chrono::Duration::days(7)
            }
            Schedule::Cron { expr } => match parse_cron(expr) {
                Ok(fields) => cron_next_after(&fields, from_tz)
                    .map(|d| d.with_timezone(&Local))
                    .unwrap_or(from + chrono::Duration::hours(1)),
                Err(_) => from + chrono::Duration::hours(1),
            },
        }
    }

    /// Canonical form for change detection: weekday order and cron
    /// whitespace do not change what the schedule MEANS, so they
    /// must not reset the last-run anchor (which would re-fire the
    /// most recent past occurrence — a duplicate send).
    pub fn normalized(&self) -> Schedule {
        match self {
            Schedule::Weekly { weekdays, time } => {
                let mut w = weekdays.clone();
                w.sort_unstable();
                w.dedup();
                Schedule::Weekly {
                    weekdays: w,
                    time: *time,
                }
            }
            Schedule::Cron { expr } => Schedule::Cron {
                expr: expr.split_whitespace().collect::<Vec<_>>().join(" "),
            },
            other => other.clone(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Schedule::EveryMinutes { minutes } if *minutes == 0 => {
                Err("khoảng cách phải >= 1 phút".into())
            }
            Schedule::EveryHours { hours } if *hours == 0 => {
                Err("khoảng cách phải >= 1 giờ".into())
            }
            Schedule::EverySeconds { seconds } if *seconds < MIN_EVERY_SECONDS => Err(format!(
                "mỗi lần gửi cách nhau ít nhất {MIN_EVERY_SECONDS} giây"
            )),
            Schedule::Weekly { weekdays, .. } => {
                if weekdays.is_empty() {
                    Err("lịch hàng tuần cần ít nhất một ngày trong tuần".into())
                } else if weekdays.iter().any(|w| *w > 6) {
                    Err("ngày trong tuần phải là 0 (T2) .. 6 (CN)".into())
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
            return Err("trường cron rỗng".into());
        }
        let (range, step, has_step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .map_err(|_| format!("bước nhảy cron '{s}' không hợp lệ"))?,
                true,
            ),
            None => (part, 1, false),
        };
        if step == 0 {
            return Err("bước nhảy cron phải >= 1".into());
        }
        if range == "*" {
            let mut v = min;
            while v <= max {
                mark(v);
                // Saturating: a huge step must stop the scan, not
                // panic (debug) or wrap to wrong minutes (release).
                v = v.saturating_add(step);
            }
        } else if let Some((a, b)) = range.split_once('-') {
            let lo: u32 = a
                .parse()
                .map_err(|_| format!("số cron '{a}' không hợp lệ"))?;
            let hi: u32 = b
                .parse()
                .map_err(|_| format!("số cron '{b}' không hợp lệ"))?;
            if lo > hi || lo < min || hi > max {
                return Err(format!("khoảng cron {lo}-{hi} ngoài giới hạn {min}-{max}"));
            }
            let mut v = lo;
            while v <= hi {
                mark(v);
                v = v.saturating_add(step);
            }
        } else {
            let v: u32 = range
                .parse()
                .map_err(|_| format!("số cron '{range}' không hợp lệ"))?;
            if v < min || v > max {
                return Err(format!("giá trị cron {v} ngoài giới hạn {min}-{max}"));
            }
            if has_step {
                // `a/n` means a..max step n.
                let mut x = v;
                while x <= max {
                    mark(x);
                    x = x.saturating_add(step);
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
            "biểu thức cron cần đúng 5 trường (phút giờ ngày tháng thứ), nhận được {}",
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

/// Resolve a wall-clock time in a timezone under Nicord's single
/// DST policy (L4), shared by the firing decision (`due_time`)
/// and the display computation (`next_after`):
///
/// - An ambiguous wall-clock time (one that occurs twice when
///   clocks fall back) resolves to its FIRST occurrence: the
///   schedule fires once, at the first.
/// - A nonexistent wall-clock time (one skipped when clocks
///   spring forward) resolves to nothing: it never fires.
///
/// Before L4, Daily/Weekly skipped ambiguous days entirely
/// (`.single()`), while the cron path could fire twice in an
/// ambiguous hour — two distinct instants share the wall time,
/// and the last-run anchor only suppresses the earlier one.
/// The cron minute-walk cannot use this helper (it visits
/// absolute instants, not wall times), so it applies the same
/// policy via `is_second_occurrence` instead.
pub(crate) fn resolve_wall_time(
    date: NaiveDate,
    time: NaiveTime,
    tz: chrono_tz::Tz,
) -> Option<DateTime<chrono_tz::Tz>> {
    match date.and_time(time).and_local_timezone(tz) {
        chrono::LocalResult::Single(t) => Some(t),
        chrono::LocalResult::Ambiguous(early, _late) => Some(early),
        chrono::LocalResult::None => None,
    }
}

/// True when `dt` is the second occurrence of an ambiguous
/// wall-clock time (L4 policy: fire at the first only). Used by
/// the cron backward walk in `due_time`, which visits absolute
/// instants and would otherwise match both occurrences.
fn is_second_occurrence(dt: DateTime<chrono_tz::Tz>, tz: chrono_tz::Tz) -> bool {
    matches!(
        dt.naive_local().and_local_timezone(tz),
        chrono::LocalResult::Ambiguous(early, _) if early != dt
    )
}

/// First cron match strictly after `from` (scans at most ~366 days).
/// The forward scan implements the L4 DST policy naturally: it
/// reaches the first occurrence of an ambiguous wall time first,
/// and a nonexistent wall time is never an instant, so it never
/// matches.
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
        Schedule::EverySeconds { seconds } => {
            let interval = (*seconds).max(MIN_EVERY_SECONDS) as i64;
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
                    // L4 DST policy: skip the second occurrence
                    // of an ambiguous wall time — it fires once,
                    // at the first. Without this, the walk fires
                    // at the first occurrence, then fires again
                    // when it reaches the second (a different
                    // instant, so the anchor cannot suppress it).
                    if !is_second_occurrence(cand, tz) {
                        return Some(cand.with_timezone(&Local));
                    }
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
    // L4: clamp the scan to the last 7 days + today (8 dates).
    // Daily/Weekly schedules recur within every 7-day window,
    // so the latest occurrence in (anchor, now] is always inside
    // it — the window's 8th date covers a weekday whose time has
    // not occurred yet today (its latest occurrence is exactly
    // 7 days back). An ancient anchor (long-off machine, old
    // import) used to scan every day since the anchor, on every
    // engine tick. The `> anchor` filter below still applies.
    let end = now.date_naive();
    let mut date = anchor.date_naive().max(end - chrono::Duration::days(7));
    while date <= end {
        if let Some(wds) = weekdays {
            let wd = date.weekday().num_days_from_monday() as u8;
            if !wds.contains(&wd) {
                date += chrono::Duration::days(1);
                continue;
            }
        }
        if let Some(cand) = resolve_wall_time(date, time, tz)
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
                | NodeKind::UtilityCallApi
        )
    }
    #[allow(dead_code)]
    pub fn is_mvp(&self) -> bool {
        self.is_executed()
    }
    // Plain Vietnamese, no unexplained markers: the old " *" / " %"
    // suffixes were stale (they no longer matched `is_executed()`)
    // and nothing in the UI ever explained them. "Call API" is
    // "Lấy dữ liệu" everywhere the user can see it.
    pub fn label(&self) -> &'static str {
        match self {
            NodeKind::TriggerSchedule => "Kích hoạt: Theo lịch",
            NodeKind::TriggerManual => "Kích hoạt: Thủ công",
            NodeKind::TriggerMessageReceived => "Kích hoạt: Nhận tin nhắn",
            NodeKind::TriggerMemberJoin => "Kích hoạt: Thành viên mới",
            NodeKind::ActionSendMessage => "Hành động: Gửi tin nhắn",
            NodeKind::ActionSendEmbed => "Hành động: Gửi embed",
            NodeKind::ActionSendCommand => "Hành động: Gửi lệnh",
            NodeKind::ActionSendFile => "Hành động: Gửi file",
            NodeKind::ActionAddReaction => "Hành động: Thêm reaction",
            NodeKind::ActionReply => "Hành động: Trả lời",
            NodeKind::ActionDeleteMessage => "Hành động: Xóa tin nhắn",
            NodeKind::ConditionKeyword => "Điều kiện: Từ khóa",
            NodeKind::ConditionTimeWindow => "Điều kiện: Khung giờ",
            NodeKind::ConditionRandom => "Điều kiện: Ngẫu nhiên",
            NodeKind::ConditionRole => "Điều kiện: Vai trò",
            NodeKind::ConditionVariable => "Điều kiện: Biến",
            NodeKind::UtilityWait => "Tiện ích: Chờ",
            NodeKind::UtilityRandomPick => "Tiện ích: Chọn ngẫu nhiên",
            NodeKind::UtilitySetVariable => "Tiện ích: Đặt biến",
            NodeKind::UtilityLoop => "Tiện ích: Lặp",
            NodeKind::UtilityCallApi => "Tiện ích: Lấy dữ liệu",
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
/// 1 subcommand, 2 subcommand group, 3 string, 4 integer,
/// 5 boolean, 6 user, 7 channel, 8 role, 9 mentionable, 10 number.
/// 11 (attachment) is NOT supported: the interaction endpoint sends
/// JSON only, and attachments would need a multipart upload.
pub fn slash_option_kind_label(kind: u8) -> &'static str {
    match kind {
        1 => "Lệnh con",
        2 => "Nhóm lệnh con",
        3 => "Chuỗi",
        4 => "Số nguyên",
        5 => "Đúng / Sai",
        6 => "Người dùng",
        7 => "Kênh",
        8 => "Vai trò",
        9 => "Nhắc đến",
        10 => "Số thực",
        _ => "Chuỗi",
    }
}

/// True for container kinds (subcommand / subcommand group): they
/// carry nested options and no value of their own.
pub fn is_container_option_kind(kind: u8) -> bool {
    matches!(kind, 1 | 2)
}

/// True for leaf kinds (a real parameter with a value).
pub fn is_leaf_option_kind(kind: u8) -> bool {
    matches!(kind, 3..=10)
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
    (9, "Nhắc đến"),
    (1, "Lệnh con"),
    (2, "Nhóm lệnh con"),
];

/// Leaf-only kinds: the editor offers these inside a subcommand.
/// A group only offers kind 1 (see `SLASH_OPTION_KINDS`).
pub const SLASH_LEAF_OPTION_KINDS: &[(u8, &str)] = &[
    (3, "Chuỗi"),
    (4, "Số nguyên"),
    (10, "Số thực"),
    (5, "Đúng / Sai"),
    (6, "Người dùng"),
    (7, "Kênh"),
    (8, "Vai trò"),
    (9, "Nhắc đến"),
];

/// One slash-command parameter (name + type + raw value; the value is
/// parsed per type when the interaction payload is built). Kinds 1
/// (subcommand) and 2 (subcommand group) are containers: they carry
/// nested `options` and no value of their own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlashOption {
    #[serde(default)]
    pub name: String,
    /// Discord option type code (see `slash_option_kind_label`).
    #[serde(default = "default_slash_option_kind")]
    pub kind: u8,
    /// Raw value as typed by the user (number/bool parsed at send time;
    /// user/channel/role values are snowflake IDs as text). Must be
    /// empty for container kinds (1/2).
    #[serde(default)]
    pub value: String,
    /// Nested options for a subcommand (kind 1) or group (kind 2).
    /// Empty for leaf kinds. `serde(default)` keeps configs saved
    /// before nesting existed parsing as empty.
    #[serde(default)]
    pub options: Vec<SlashOption>,
}

impl Default for SlashOption {
    fn default() -> Self {
        Self {
            name: String::new(),
            // NOTE: a derived Default would give kind 0 (invalid type);
            // the serde default fn only applies on deserialization.
            kind: default_slash_option_kind(),
            value: String::new(),
            options: Vec::new(),
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
    /// optional (auto-resolved when empty). Slash options are checked
    /// too (H-CMD): fail fast with a clear error instead of a cryptic
    /// Discord 400 for a malformed payload.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("Node Gửi lệnh cần tên lệnh".into());
        }
        if self.is_slash() && self.application_id.trim().is_empty() {
            return Err("Lệnh slash cần Application ID của bot (dán vào phần sửa bước)".into());
        }
        // Prefix commands ignore options, so stale options on a prefix
        // config must not fail it — only slash options are checked.
        if self.is_slash() {
            validate_slash_options(&self.options)?;
        }
        Ok(())
    }
}

/// Validate one level of slash options, recursively: names non-empty
/// and unique among siblings (case-insensitive), kinds known,
/// containers (1/2) carry ≥1 child and no value, a subcommand's
/// children are leaf kinds only, a group's children are subcommands
/// only, leaf kinds carry no children, and leaf number/bool values
/// must parse (fail fast with the parameter name instead of sending
/// a silent 0/false at fire time).
fn validate_slash_options(options: &[SlashOption]) -> Result<(), String> {
    let mut seen: Vec<String> = Vec::new();
    for opt in options {
        let name = opt.name.trim();
        if name.is_empty() {
            return Err("Lệnh slash có tham số tên trống — điền tên hoặc xóa hàng đó".into());
        }
        let key = name.to_lowercase();
        if seen.contains(&key) {
            return Err(format!("Lệnh slash có tên tham số trùng '{name}'"));
        }
        seen.push(key);
        if opt.kind == 11 {
            return Err(format!(
                "Tham số slash '{name}' là loại đính kèm (11) — chưa hỗ trợ, vì gửi lệnh đi kèm file cần tải lên đa phần (multipart) mà đường gửi tương tác hiện chỉ gửi JSON"
            ));
        }
        if is_container_option_kind(opt.kind) {
            if !opt.value.trim().is_empty() {
                return Err(format!(
                    "Lệnh con '{name}' không nhận giá trị — giá trị nằm ở tham số con bên trong"
                ));
            }
            if opt.options.is_empty() {
                let what = if opt.kind == 1 {
                    "Lệnh con"
                } else {
                    "Nhóm lệnh con"
                };
                return Err(format!("{what} '{name}' cần ít nhất một tham số con"));
            }
            if opt.kind == 1 {
                for child in &opt.options {
                    if is_container_option_kind(child.kind) {
                        return Err(format!(
                            "Trong lệnh con '{name}' chỉ được tham số thường — không được lồng lệnh con trong lệnh con"
                        ));
                    }
                }
            } else {
                for child in &opt.options {
                    if child.kind != 1 {
                        return Err(format!(
                            "Trong nhóm '{name}' chỉ được lệnh con (loại 1) — tham số thường phải nằm trong lệnh con"
                        ));
                    }
                }
            }
            validate_slash_options(&opt.options)?;
        } else if is_leaf_option_kind(opt.kind) {
            if !opt.options.is_empty() {
                return Err(format!(
                    "Tham số '{name}' là loại thường nên không được có tham số con — chuyển nó sang Lệnh con hoặc xóa các hàng con"
                ));
            }
            // Fail fast on unparseable leaf values (same check the
            // payload builder runs at send time).
            crate::discord::slash_option_value(opt.kind, name, &opt.value)?;
        } else {
            return Err(format!(
                "Tham số slash '{name}' có mã loại không hỗ trợ {}",
                opt.kind
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// External data (`UtilityCallApi`, idea #5): fetch JSON over HTTPS and
// feed one field into a template variable. The URL is a secret (it
// may carry an API key): only a secret-store reference is kept in
// the node config, never the plaintext URL.
// ---------------------------------------------------------------------------

/// Configuration for a `UtilityCallApi` node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiCallConfig {
    /// Secret-store reference for the URL (`kr:` ref or encrypted
    /// blob). Empty = not configured yet.
    #[serde(default)]
    pub url_ref: String,
    /// Host part of the URL, stored in plaintext for display (the
    /// host is not secret; the full URL may carry an API key).
    #[serde(default)]
    pub url_host: String,
    /// Template variable name fed by the fetch (`{{name}}`).
    #[serde(default)]
    pub variable: String,
    /// Dot path into the JSON response (`weather.temp`, array
    /// indices as numbers: `items.0.name`).
    #[serde(default)]
    pub field_path: String,
    /// Per-fetch timeout in seconds (1–60, default 10).
    #[serde(default = "default_api_timeout")]
    pub timeout_secs: u64,
    /// Failure policy: "skip" (default — skip the turn with a log
    /// row) or "empty" (send with an empty value).
    #[serde(default)]
    pub on_failure: String,
}

fn default_api_timeout() -> u64 {
    crate::apidata::DEFAULT_API_TIMEOUT_SECS
}

impl Default for ApiCallConfig {
    fn default() -> Self {
        Self {
            url_ref: String::new(),
            url_host: String::new(),
            variable: String::new(),
            field_path: String::new(),
            timeout_secs: default_api_timeout(),
            on_failure: "skip".to_string(),
        }
    }
}

impl ApiCallConfig {
    /// Parse a node's config (tolerant: unknown fields ignored).
    /// A malformed config parses as the default, which fails
    /// validation (empty URL/variable) — fail-closed, like
    /// `CommandConfig::from_node`.
    pub fn from_node(node: &Node) -> Self {
        serde_json::from_value(node.config.clone()).unwrap_or_default()
    }

    /// Effective failure policy: "empty" only when explicitly set;
    /// anything else (including "") means "skip".
    pub fn effective_on_failure(&self) -> &'static str {
        if self.on_failure.trim() == "empty" {
            "empty"
        } else {
            "skip"
        }
    }

    /// Structural validation. The URL itself is NOT resolved here:
    /// validation runs on the UI thread and must not touch the
    /// keyring; the scheme + allowlist are checked at fetch time
    /// (fail-closed), and the editor checks the scheme when the
    /// URL is pasted.
    pub fn validate(&self) -> Result<(), String> {
        if self.url_ref.trim().is_empty() {
            return Err(
                "Bước lấy dữ liệu chưa có địa chỉ đã lưu — dán địa chỉ https trong phần sửa bước"
                    .into(),
            );
        }
        if !valid_counter_name(self.variable.trim()) {
            return Err(
                "Tên biến của bước lấy dữ liệu phải dài 1–64 ký tự: chữ, số, '_' hoặc '-'".into(),
            );
        }
        let path = self.field_path.trim();
        if path.is_empty() {
            return Err(
                "Đường dẫn trường của bước lấy dữ liệu đang trống — vd: weather.temp".into(),
            );
        }
        if path.starts_with('.')
            || path.ends_with('.')
            || path.split('.').any(|seg| seg.trim().is_empty())
        {
            return Err(format!(
                "Đường dẫn trường '{path}' của bước lấy dữ liệu sai định dạng — dùng tên cách nhau bằng dấu chấm như weather.temp"
            ));
        }
        if self.timeout_secs < 1 || self.timeout_secs > 60 {
            return Err("Thời gian chờ của bước lấy dữ liệu phải là 1–60 giây".into());
        }
        let policy = self.on_failure.trim();
        if !policy.is_empty() && policy != "skip" && policy != "empty" {
            return Err("Chính sách lỗi của bước lấy dữ liệu phải là 'skip' hoặc 'empty'".into());
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
    /// Destination ids in Webhook mode (broadcast, idea #1): one
    /// firing sends to EVERY listed destination, each with its own
    /// fault isolation (a failure in one never blocks the others).
    /// At least one is required when enabling in webhook mode.
    /// Ignored in user-token mode (which uses `channel_id`).
    /// Kept alongside `channel_id` so switching modes loses nothing.
    ///
    /// Migration: older files stored ONE id under the key
    /// `"destination_id"` (a string). The `alias` + custom
    /// deserializer below reads that form into this vec, so old
    /// scenarios, exports, and backups keep working; new files
    /// serialize only `"destination_ids"`.
    #[serde(
        default,
        alias = "destination_id",
        deserialize_with = "de_destination_ids"
    )]
    pub destination_ids: Vec<String>,
    /// When true (webhook mode), send from the destination's
    /// default messages instead of this scenario's own messages,
    /// even when the scenario has messages. When false, the
    /// destination defaults are only a fallback for an empty
    /// scenario message list (see the engine's fallback chain).
    #[serde(default)]
    pub use_destination_messages: bool,
    /// Rich embeds attached to every message this scenario sends
    /// (Q8, extended by idea #8 to up to 10). Empty = plain text
    /// only. An embed may accompany empty content (Discord
    /// requires content OR embeds).
    ///
    /// Migration: older files stored ONE embed under the key
    /// `"embed"` (an object, or null). The `alias` + custom
    /// deserializer below reads that form into this vec, so old
    /// scenarios, exports, and backups keep working; new files
    /// serialize only `"embeds"`.
    #[serde(default, alias = "embed", deserialize_with = "de_embeds")]
    pub embeds: Vec<crate::embed::Embed>,
    /// When false (default), mass pings are blocked: the send
    /// payload carries `allowed_mentions: {"parse": ["users"]}`,
    /// so @everyone/@here/role mentions render as plain text
    /// while individual user mentions still ping. When true, the
    /// field is omitted and Discord parses everything.
    #[serde(default)]
    pub allow_mass_mentions: bool,
    /// When true, the SUPPRESS_NOTIFICATIONS flag is set: the
    /// message arrives without pushing a notification.
    #[serde(default)]
    pub silent_send: bool,
    /// Optional poll attached to every message this scenario
    /// sends (idea #3). None = no poll. A poll may accompany
    /// empty content (Discord requires content OR embeds OR a
    /// poll). Results are not readable via webhook — see poll.rs.
    #[serde(default)]
    pub poll: Option<crate::poll::Poll>,
    /// Edit-in-place mode (idea #2): when true, each firing edits
    /// the scenario's previous message (one per target) instead of
    /// posting a new one. The first firing posts new and records
    /// the message id; a deleted target falls back to a new send.
    /// Edits carry content/embeds/mentions/flags only — polls and
    /// attachments ride the first send, never an edit.
    #[serde(default)]
    pub edit_last_message: bool,
    /// File attachments (idea #4): local paths attached to every
    /// fresh send (never to edits). Only the PATHS are stored —
    /// the files themselves are read at send time and are NOT
    /// included in exports/backups, so a moved file means a
    /// re-picked path on the new machine. A missing or oversize
    /// file is skipped with a warning in the log; the message
    /// still goes out.
    #[serde(default)]
    pub attachments: Vec<String>,
    /// Last message id sent per target, for edit-in-place mode:
    /// keyed by destination id in webhook mode, by channel id in
    /// user-token mode. Updated by the engine loop after each
    /// successful send; the Builder can clear it ("forget") so the
    /// next firing posts new. Exported/imported with the scenario;
    /// a stale id (message deleted) falls back to a new send with
    /// an honest log — never an error spiral.
    #[serde(default)]
    pub last_message_ids: std::collections::HashMap<String, String>,
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

/// Deserialize `Scenario::embeds` from either the old form (a
/// single embed object under `"embed"`, or null) or the new form
/// (an array under `"embeds"`). A missing key never reaches here
/// (the field's `#[serde(default)]` applies instead).
fn de_embeds<'de, D>(deserializer: D) -> Result<Vec<crate::embed::Embed>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Order matters: `Many` BEFORE `One`. Serde structs accept
    // seq form, and `Embed` has defaults on every field — so an
    // empty array `[]` would otherwise match `One` as a default
    // embed instead of the empty list.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        Empty,
        Many(Vec<crate::embed::Embed>),
        One(crate::embed::Embed),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::Empty => Vec::new(),
        OneOrMany::Many(embeds) => embeds,
        OneOrMany::One(embed) => vec![embed],
    })
}

/// Deserialize `Scenario::destination_ids` from either the old form
/// (one id string under `"destination_id"`) or the new form (an
/// array under `"destination_ids"`). A missing key never reaches
/// here (the field's `#[serde(default)]` applies instead). Empty
/// and duplicate ids are dropped: an empty id can never resolve,
/// and a duplicate would send the same message twice to one channel.
fn de_destination_ids<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        Empty,
        Many(Vec<String>),
        One(String),
    }
    let ids = match OneOrMany::deserialize(deserializer)? {
        OneOrMany::Empty => Vec::new(),
        OneOrMany::Many(ids) => ids,
        OneOrMany::One(id) => vec![id],
    };
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        let id = id.trim().to_string();
        if !id.is_empty() && !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
}

impl Scenario {
    /// First destination id, if any — the "primary" destination used
    /// by displays that show one destination (health, simulation).
    /// Broadcast sends to all ids; this is display-only.
    pub fn primary_destination_id(&self) -> Option<&str> {
        self.destination_ids.first().map(|s| s.as_str())
    }

    pub fn new(name: &str) -> Self {
        Self {
            schema_version: 1,
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            enabled: false,
            timezone: default_timezone(),
            channel_id: String::new(),
            destination_ids: Vec::new(),
            use_destination_messages: false,
            embeds: Vec::new(),
            allow_mass_mentions: false,
            silent_send: false,
            poll: None,
            edit_last_message: false,
            attachments: Vec::new(),
            last_message_ids: std::collections::HashMap::new(),
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
            messages: vec!["Xin chào từ Nicord!".to_string()],
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
            return Err("tên kịch bản đang trống".into());
        }
        // Fail-closed: an unknown timezone must not silently run in
        // UTC (tz_from_name falls back). Reject it at validation.
        if self.timezone.parse::<chrono_tz::Tz>().is_err() {
            return Err(format!(
                "múi giờ '{}' không rõ — dùng tên IANA như Asia/Ho_Chi_Minh",
                self.timezone
            ));
        }
        if require_channel && self.channel_id.trim().is_empty() {
            return Err("chưa chọn kênh đích".into());
        }
        if require_destination {
            if !self.destination_ids.iter().any(|id| !id.trim().is_empty()) {
                return Err(
                    "chưa chọn đích đến — chọn ít nhất một đích đến trong phần Soạn kịch bản"
                        .into(),
                );
            }
            // A duplicate id would send twice to one channel. The
            // deserializer dedupes on read, but a hand-built list can
            // still carry one — fail closed with a clear error.
            let mut seen: Vec<&str> = Vec::new();
            for id in &self.destination_ids {
                let id = id.trim();
                if id.is_empty() {
                    continue;
                }
                if seen.contains(&id) {
                    return Err(
                        "cùng một đích đến được chọn hai lần — mỗi đích đến chỉ chọn một lần"
                            .into(),
                    );
                }
                seen.push(id);
            }
        }
        let triggers = self
            .nodes
            .iter()
            .filter(|n| n.kind.group() == "Trigger")
            .count();
        if triggers != 1 {
            return Err(format!(
                "một kịch bản phải bắt đầu với đúng một Kích hoạt, đang có {}",
                triggers
            ));
        }
        for node in &self.nodes {
            if !node.kind.is_executed() {
                return Err(format!(
                    "Bước '{}' chưa được làm trong phiên bản này — hãy xóa nó \
                     (hỗ trợ: kích hoạt Theo lịch/Thủ công, Gửi tin nhắn, Gửi embed, \
                     Gửi lệnh, điều kiện Khung giờ, Chờ, Chọn ngẫu nhiên, Đặt biến, Lấy dữ liệu)",
                    node.kind.label()
                ));
            }
            if node.kind == NodeKind::ActionSendCommand {
                if !allow_commands {
                    return Err(
                        "chế độ Webhook không hỗ trợ bước Gửi lệnh — dùng Gửi tin nhắn, hoặc chuyển lại chế độ Tài khoản"
                            .into(),
                    );
                }
                CommandConfig::from_node(node)
                    .validate()
                    .map_err(|e| format!("Bước Gửi lệnh {}: {e}", node.id))?;
            }
            // Time Window nodes fail CLOSED: a present-but-unparsable
            // start/end is a validation error, never a silent
            // whole-day default (which would send at any hour).
            if node.kind == NodeKind::ConditionTimeWindow {
                validate_time_window(node)
                    .map_err(|e| format!("Bước Khung giờ {}: {e}", node.id))?;
            }
            if node.kind == NodeKind::UtilityCallApi {
                ApiCallConfig::from_node(node)
                    .validate()
                    .map_err(|e| format!("Bước lấy dữ liệu {}: {e}", node.id))?;
            }
        }
        // Two Call API nodes feeding the same variable would make
        // the template value depend on node order — reject it.
        {
            let mut seen: Vec<String> = Vec::new();
            for node in &self.nodes {
                if node.kind != NodeKind::UtilityCallApi {
                    continue;
                }
                let name = ApiCallConfig::from_node(node).variable.trim().to_string();
                if name.is_empty() {
                    continue;
                }
                if seen.contains(&name) {
                    return Err(format!(
                        "hai bước lấy dữ liệu cùng đưa vào biến '{{{name}}}' — đổi tên một cái"
                    ));
                }
                seen.push(name);
            }
        }
        if let Some(s) = &self.schedule {
            s.validate()?;
        }
        // Every Send Message node needs non-empty content (or a message list)
        // — unless the scenario runs a command instead: then the message
        // nodes are ignored and need no content.
        let has_command = self.command_node().is_some();
        // Edit-in-place (idea #2) edits messages; a command is sent
        // fresh every time and ignores the mode entirely, so the
        // combination is rejected instead of silently doing nothing.
        if self.edit_last_message && has_command {
            return Err(
                "chế độ sửa tin nhắn cũ không áp dụng cho bước Gửi lệnh — xóa bước lệnh hoặc tắt chế độ này"
                    .into(),
            );
        }
        for node in &self.nodes {
            if node.kind == NodeKind::ActionSendMessage || node.kind == NodeKind::ActionSendEmbed {
                let content = node_content(node);
                // A poll alone satisfies the node: Discord accepts
                // a message that is only a poll.
                if content.trim().is_empty()
                    && self.messages.is_empty()
                    && !has_command
                    && self.poll.is_none()
                {
                    return Err(format!(
                        "Node Gửi tin nhắn {} không có nội dung và không có danh sách tin nhắn",
                        node.id
                    ));
                }
            }
        }
        // Embed limits (Q8 / idea #8): validated in both modes,
        // since both transports send the same embed payload.
        // The 6000-char total is enforced across all embeds.
        crate::embed::validate_embeds(&self.embeds)?;
        // Poll limits (idea #3): validated in both modes, since
        // both transports send the same poll payload.
        if let Some(poll) = &self.poll {
            poll.validate()?;
        }
        // Attachments (idea #4): at most 10 files, no empty paths.
        // Existence and size are checked at send time (files can
        // move after saving), with a warning in the Builder when a
        // path is missing.
        if self.attachments.len() > crate::discord::MAX_ATTACHMENTS {
            return Err(format!(
                "{} tệp đính kèm, vượt giới hạn {} file của Discord",
                self.attachments.len(),
                crate::discord::MAX_ATTACHMENTS
            ));
        }
        for (idx, path) in self.attachments.iter().enumerate() {
            if path.trim().is_empty() {
                return Err(format!("tệp đính kèm {} có đường dẫn trống", idx + 1));
            }
        }
        // Discord rejects any message over 2000 characters, so fail fast
        // here with a clear error instead of failing on every send.
        // This checks the raw text (before template vars / spin-text):
        // conservative for templates that expand, exact for plain text.
        for (idx, msg) in self.messages.iter().enumerate() {
            // An empty entry would fail on every firing (the send
            // path rejects empty content after sanitising) and count
            // toward error pauses — reject it here, once, instead.
            if msg.trim().is_empty() {
                return Err(format!("tin nhắn {} đang trống", idx + 1));
            }
            if char_count(msg) > DISCORD_CONTENT_LIMIT {
                return Err(format!(
                    "tin nhắn {} dài {} ký tự, vượt giới hạn 2000 ký tự của Discord",
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
                        "nội dung bước Gửi tin nhắn {} dài {} ký tự, vượt giới hạn 2000 ký tự của Discord",
                        node.id,
                        char_count(&content)
                    ));
                }
            }
        }
        Ok(())
    }

    /// First `ActionSendCommand` node, if any. Kept for validation
    /// (message nodes are ignored when a command exists) — the
    /// scheduler runs ALL command nodes via `command_nodes`.
    pub fn command_node(&self) -> Option<&Node> {
        self.nodes
            .iter()
            .find(|n| n.kind == NodeKind::ActionSendCommand)
    }

    /// All `ActionSendCommand` nodes, in node order. One scheduled
    /// firing runs all of them in order (see `Engine::send_commands`
    /// in scheduler.rs for the retry-safety rule).
    pub fn command_nodes(&self) -> Vec<&Node> {
        self.nodes
            .iter()
            .filter(|n| n.kind == NodeKind::ActionSendCommand)
            .collect()
    }

    /// Delete one message from the rotation. Returns false when the
    /// index is out of range (stale UI state must not panic).
    pub fn remove_message(&mut self, idx: usize) -> bool {
        if idx >= self.messages.len() {
            return false;
        }
        self.messages.remove(idx);
        true
    }

    /// Move one message one step up (`up = true`) or down. Returns
    /// false at the ends or for an out-of-range index.
    pub fn move_message(&mut self, idx: usize, up: bool) -> bool {
        if idx >= self.messages.len() {
            return false;
        }
        let target = if up {
            idx.checked_sub(1)
        } else {
            idx.checked_add(1)
        };
        let Some(target) = target else {
            return false;
        };
        if target >= self.messages.len() {
            return false;
        }
        self.messages.swap(idx, target);
        true
    }

    /// Delete one workflow step by id. The only trigger step cannot
    /// be deleted: validation requires exactly one trigger, and the
    /// schedule itself is edited in the schedule section, not by
    /// removing its step. Returns Err with the reason when the step
    /// cannot be deleted.
    pub fn remove_node(&mut self, id: &str) -> Result<(), String> {
        let Some(pos) = self.nodes.iter().position(|n| n.id == id) else {
            return Err("Không tìm thấy bước này.".to_string());
        };
        let is_only_trigger = self.nodes[pos].kind.group() == "Trigger"
            && self
                .nodes
                .iter()
                .filter(|n| n.kind.group() == "Trigger")
                .count()
                == 1;
        if is_only_trigger {
            return Err(
                "Không xóa được bước kích hoạt duy nhất — kịch bản cần đúng một bước kích hoạt. Muốn đổi lịch, sửa ở phần Lịch bên dưới."
                    .to_string(),
            );
        }
        self.nodes.remove(pos);
        Ok(())
    }

    /// Move one workflow step one step up (`up = true`) or down.
    /// Moving the trigger step is harmless: firing uses the
    /// `schedule` field, and only the order of the action steps
    /// affects a run. Returns false at the ends or when the id is
    /// unknown.
    pub fn move_node(&mut self, id: &str, up: bool) -> bool {
        let Some(pos) = self.nodes.iter().position(|n| n.id == id) else {
            return false;
        };
        let target = if up {
            pos.checked_sub(1)
        } else {
            pos.checked_add(1)
        };
        let Some(target) = target else {
            return false;
        };
        if target >= self.nodes.len() {
            return false;
        }
        self.nodes.swap(pos, target);
        true
    }

    /// Pick the next message to send, rotating without repeating the last one.
    /// Returns (index, message). `last_index` is the index sent last time.
    /// Empty/whitespace-only entries are never picked (validation
    /// rejects them, but corrupt rows bypass validation): the
    /// returned index is the ORIGINAL position, so no-repeat
    /// tracking stays consistent. None when no usable message exists.
    pub fn pick_message_with_index(
        &self,
        last_index: Option<usize>,
        rng: &mut impl Rng,
    ) -> Option<(usize, String)> {
        let candidates: Vec<usize> = self
            .messages
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
            return Some((idx, self.messages[idx].clone()));
        }
        let mut idx = candidates[rng.random_range(0..candidates.len())];
        if Some(idx) == last_index {
            let pos = candidates.iter().position(|&c| c == idx).unwrap_or(0);
            idx = candidates[(pos + 1) % candidates.len()];
        }
        Some((idx, self.messages[idx].clone()))
    }

    /// First non-empty content from ActionSendMessage / ActionSendEmbed
    /// nodes, in node order. None if no node carries content.
    /// The embeds this scenario actually sends: non-empty ones
    /// only, in order. Empty embeds (e.g. a half-filled composer
    /// draft) are dropped rather than sent — Discord would 400.
    pub fn active_embeds(&self) -> Vec<crate::embed::Embed> {
        self.embeds
            .iter()
            .filter(|e| !e.is_empty())
            .cloned()
            .collect()
    }

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
    /// Saturating: crafted node configs with huge `seconds` values
    /// must not overflow the sum (debug panic / release wrap).
    pub fn wait_seconds(&self) -> u64 {
        let total: u64 = self
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::UtilityWait)
            .filter_map(|n| n.config.get("seconds").and_then(|v| v.as_u64()))
            .fold(0u64, |acc, v| acc.saturating_add(v));
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
                    "ngoài khung giờ của bước Khung giờ {} ({}–{}) — bỏ qua",
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
                .ok_or_else(|| format!("'{key}' phải là chuỗi \"HH:MM\""))?;
            if parse_hhmm(s).is_none() {
                return Err(format!("'{key}' phải là HH:MM (24h), nhận được '{s}'"));
            }
        }
    }
    for key in ["start_hour", "end_hour"] {
        if let Some(v) = node.config.get(key) {
            let h = v
                .as_u64()
                .ok_or_else(|| format!("'{key}' phải là số giờ nguyên 0..=23"))?;
            if h > 23 {
                return Err(format!("'{key}' phải là 0..=23, nhận được {h}"));
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

/// Whether `name` is a valid counter name: 1–64 chars of
/// letters, digits, `_`, `-`. Anything else is left unrendered
/// (literal text stays in the message), never an error.
pub fn valid_counter_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Names of durable counters referenced by `template`
/// (`{{counter:name}}`), deduplicated, in first-use order.
/// Invalid names and unterminated tags are skipped.
pub fn counter_names(template: &str) -> Vec<String> {
    const TAG: &str = "{{counter:";
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find(TAG) {
        let after = &rest[start + TAG.len()..];
        let Some(end) = after.find("}}") else {
            break;
        };
        let name = &after[..end];
        if valid_counter_name(name) && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
        rest = &after[end + 2..];
    }
    names
}

/// Render `{{days_until:YYYY-MM-DD}}` tags: whole days from
/// `today` until the date, clamped at 0 (a past date renders 0,
/// so countdowns never go negative). Unparseable dates are left
/// literal and the scan continues past them.
fn render_days_until(mut text: String, today: NaiveDate) -> String {
    const TAG: &str = "{{days_until:";
    let mut search_from = 0;
    while let Some(rel) = text[search_from..].find(TAG) {
        let start = search_from + rel;
        let Some(close_rel) = text[start..].find("}}") else {
            break;
        };
        let close = start + close_rel;
        let date_str = &text[start + TAG.len()..close];
        match NaiveDate::parse_from_str(date_str.trim(), "%Y-%m-%d") {
            Ok(date) => {
                let days = (date - today).num_days().max(0).to_string();
                text.replace_range(start..close + 2, &days);
                search_from = start + days.len();
            }
            Err(_) => {
                search_from = close + 2;
            }
        }
    }
    text
}

/// Render `{{username}}`, `{{server}}`, `{{channel}}`, `{{date}}`,
/// `{{time}}`, `{{weekday}}`, `{{week_number}}`,
/// `{{days_until:YYYY-MM-DD}}`, `{{counter:name}}` (from
/// `counters`; names absent from the map stay literal), plus any
/// custom `{{name}}` variables. Unknown tags are left as-is:
/// rendering never fails, it just leaves what it cannot resolve.
pub fn render_template_with_vars(
    template: &str,
    username: &str,
    server: &str,
    channel: &str,
    vars: &HashMap<String, String>,
    counters: &HashMap<String, u64>,
) -> String {
    let now = Local::now();
    let mut out = template
        .replace("{{username}}", username)
        .replace("{{server}}", server)
        .replace("{{channel}}", channel)
        .replace("{{date}}", &now.format("%Y-%m-%d").to_string())
        .replace("{{time}}", &now.format("%H:%M").to_string())
        .replace("{{weekday}}", &now.format("%A").to_string())
        .replace("{{week_number}}", &now.format("%V").to_string());
    out = render_days_until(out, now.date_naive());
    for (name, value) in counters {
        out = out.replace(&format!("{{{{counter:{name}}}}}"), &value.to_string());
    }
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
    // `destination_id` (one string) is the legacy form of
    // `destination_ids` (array); either one counts as present.
    if !obj.contains_key("destination_ids") && !obj.contains_key("destination_id") {
        warnings.push("Thiếu trường 'destination_ids' — dùng giá trị mặc định.".to_string());
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
    // Attachments (idea #4): only paths travel with the file — the
    // files themselves stay on the exporting machine.
    if !scenario.attachments.is_empty() {
        warnings.push(format!(
            "Kịch bản có {} tệp đính kèm — chỉ đường dẫn được lưu, hãy kiểm tra file còn tồn tại trên máy này.",
            scenario.attachments.len()
        ));
    }
    // Call API URLs (idea #5) are per-machine secrets: the exported
    // node carries only a reference, so the URL must be re-entered
    // on this machine before the node can fetch.
    let api_nodes = scenario
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::UtilityCallApi)
        .filter(|n| !ApiCallConfig::from_node(n).url_ref.trim().is_empty())
        .count();
    if api_nodes > 0 {
        warnings.push(format!(
            "Kịch bản có {api_nodes} bước lấy dữ liệu — địa chỉ URL là bí mật riêng của máy xuất, hãy nhập lại URL trên máy này trước khi chạy."
        ));
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
        // Fixed UTC instants: deterministic on any machine timezone.
        let from = Utc
            .with_ymd_and_hms(2026, 11, 17, 10, 0, 0)
            .unwrap()
            .with_timezone(&Local);
        let next = s.next_after(from, "UTC");
        assert!(next > from);
        assert_eq!(next.with_timezone(&Utc).hour(), 9);
        assert_eq!(next.with_timezone(&Utc).day(), 18);
    }

    #[test]
    fn next_after_uses_scenario_timezone() {
        let s = Schedule::Daily {
            time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        };
        // 10:00 UTC is 05:00 in New York (EST): the next 09:00 NY
        // run is 14:00 UTC the same day — not tomorrow 09:00 UTC,
        // which the old Local-based computation returned.
        let from = Utc
            .with_ymd_and_hms(2026, 11, 17, 10, 0, 0)
            .unwrap()
            .with_timezone(&Local);
        let next = s.next_after(from, "America/New_York");
        let expected = Utc
            .with_ymd_and_hms(2026, 11, 17, 14, 0, 0)
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(next, expected);
    }

    #[test]
    fn every_seconds_serde_round_trip_and_old_config_still_parses() {
        let s = Schedule::EverySeconds { seconds: 90 };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"every_seconds\""), "{json}");
        let back: Schedule = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        // Old configs never mention the new variant — they must
        // still parse (backward compatibility is a serde default
        // here, but pin it so a future refactor cannot break it).
        let old: Schedule =
            serde_json::from_str(r#"{"kind":"every_minutes","minutes":30}"#).unwrap();
        assert_eq!(old, Schedule::EveryMinutes { minutes: 30 });
        let bad: Result<Schedule, _> =
            serde_json::from_str(r#"{"kind":"every_seconds","seconds":30}"#);
        // Parses fine (validation, not serde, enforces the floor).
        assert_eq!(bad.unwrap(), Schedule::EverySeconds { seconds: 30 });
    }

    #[test]
    fn describe_seconds_plain_words() {
        assert_eq!(describe_seconds(3600), "1 giờ");
        assert_eq!(describe_seconds(5400), "1 giờ 30 phút");
        assert_eq!(describe_seconds(90), "1 phút 30 giây");
        assert_eq!(describe_seconds(60), "1 phút");
        assert_eq!(describe_seconds(3661), "1 giờ 1 phút 1 giây");
        assert_eq!(describe_seconds(45), "45 giây");
        assert_eq!(describe_seconds(0), "0 giây");
        assert_eq!(
            Schedule::EverySeconds { seconds: 90 }.label(),
            "Cứ mỗi 90 giây"
        );
    }

    #[test]
    fn every_seconds_next_after_and_due_time() {
        let s = Schedule::EverySeconds { seconds: 90 };
        let from = Utc
            .with_ymd_and_hms(2026, 11, 17, 10, 0, 0)
            .unwrap()
            .with_timezone(&Local);
        let next = s.next_after(from, "Asia/Ho_Chi_Minh");
        let expected = Utc
            .with_ymd_and_hms(2026, 11, 17, 10, 1, 30)
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(next, expected);
        // due_time: anchored at creation, fires once 90 s have passed.
        let created = Utc.with_ymd_and_hms(2026, 11, 17, 10, 0, 0).unwrap();
        let early = Utc
            .with_ymd_and_hms(2026, 11, 17, 10, 0, 30)
            .unwrap()
            .with_timezone(&Local);
        assert!(due_time(&s, None, Some(created), early, "UTC").is_none());
        let due = Utc
            .with_ymd_and_hms(2026, 11, 17, 10, 1, 30)
            .unwrap()
            .with_timezone(&Local);
        assert!(due_time(&s, None, Some(created), due, "UTC").is_some());
    }

    #[test]
    fn every_seconds_next_after_crosses_spring_forward() {
        // Absolute-duration semantics: 3600 s after 01:30 EST is
        // 03:30 EDT — the wall clock jumps the 02:00-03:00 gap,
        // the instant is exactly one hour later.
        let s = Schedule::EverySeconds { seconds: 3600 };
        let from = Utc
            .with_ymd_and_hms(2026, 3, 8, 6, 30, 0)
            .unwrap()
            .with_timezone(&Local);
        let next = s.next_after(from, "America/New_York");
        let expected = Utc
            .with_ymd_and_hms(2026, 3, 8, 7, 30, 0)
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(next, expected);
    }

    #[test]
    fn every_seconds_validate_rejects_below_minimum() {
        assert!(Schedule::EverySeconds { seconds: 0 }.validate().is_err());
        assert!(Schedule::EverySeconds { seconds: 59 }.validate().is_err());
        assert!(Schedule::EverySeconds { seconds: 60 }.validate().is_ok());
        assert!(Schedule::EverySeconds { seconds: 3600 }.validate().is_ok());
    }

    #[test]
    fn normalized_ignores_weekday_order_and_cron_whitespace() {
        let a = Schedule::Weekly {
            weekdays: vec![1, 2],
            time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        };
        let b = Schedule::Weekly {
            weekdays: vec![2, 1, 1],
            time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        };
        assert_eq!(a.normalized(), b.normalized());
        let c = Schedule::Cron {
            expr: "0 9 * * *".to_string(),
        };
        let d = Schedule::Cron {
            expr: "  0   9  *   *  *  ".to_string(),
        };
        assert_eq!(c.normalized(), d.normalized());
        // A real change still differs.
        let e = Schedule::Cron {
            expr: "0 10 * * *".to_string(),
        };
        assert_ne!(c.normalized(), e.normalized());
    }

    #[test]
    fn validate_rejects_empty_messages() {
        let mut s = Scenario::new("t");
        s.channel_id = "123".to_string();
        s.messages = vec!["hi".to_string(), "   ".to_string()];
        assert!(s.validate().is_err());
        s.messages = vec!["".to_string()];
        assert!(s.validate().is_err());
    }

    #[test]
    fn poll_alone_satisfies_validation_and_invalid_poll_fails() {
        // A scenario with no messages but a valid poll validates:
        // Discord accepts a poll-only message.
        let mut s = Scenario::new("poll-only");
        s.messages.clear();
        s.poll = Some(crate::poll::Poll {
            question: "Q?".to_string(),
            answers: vec!["A".to_string(), "B".to_string()],
            duration_hours: 24,
            allow_multiselect: false,
        });
        s.channel_id = "111".to_string();
        assert!(s.validate().is_ok());
        // An invalid poll fails validation.
        s.poll.as_mut().unwrap().answers.clear();
        assert!(s.validate().is_err());
    }

    #[test]
    fn embeds_migrate_from_old_single_embed_form() {
        // Old files: one embed object under "embed".
        let old = r#"{"name":"s","embed":{"title":"Hi"}}"#;
        let s: Scenario = serde_json::from_str(old).unwrap();
        assert_eq!(s.embeds.len(), 1);
        assert_eq!(s.embeds[0].title, "Hi");
        // Old files: explicit null (Option<Embed>::None).
        let old_null = r#"{"name":"s","embed":null}"#;
        let s: Scenario = serde_json::from_str(old_null).unwrap();
        assert!(s.embeds.is_empty());
        // Missing key entirely.
        let s: Scenario = serde_json::from_str(r#"{"name":"s"}"#).unwrap();
        assert!(s.embeds.is_empty());
        // Empty array: the empty list, NOT one default embed
        // (serde structs accept seq form — variant order in
        // de_embeds guards this).
        let s: Scenario = serde_json::from_str(r#"{"name":"s","embeds":[]}"#).unwrap();
        assert!(s.embeds.is_empty());
        // New files: an array under "embeds", serialized back under
        // "embeds" only — never "embed".
        let new = r#"{"name":"s","embeds":[{"title":"A"},{"title":"B"}]}"#;
        let s: Scenario = serde_json::from_str(new).unwrap();
        assert_eq!(s.embeds.len(), 2);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"embeds\""));
        assert!(!json.contains("\"embed\":"));
        // active_embeds drops empty drafts.
        let mut s = Scenario::new("t");
        s.embeds = vec![
            crate::embed::Embed::default(),
            crate::embed::Embed {
                title: "Real".to_string(),
                ..crate::embed::Embed::default()
            },
        ];
        assert_eq!(s.active_embeds().len(), 1);
    }

    #[test]
    fn counter_names_extracts_valid_deduped_names() {
        assert_eq!(
            counter_names("Day {{counter:day}} of {{counter:day}} and {{counter:week-1}}"),
            vec!["day".to_string(), "week-1".to_string()]
        );
        assert!(counter_names("no counters here").is_empty());
        // Invalid names are skipped, not errors.
        assert!(counter_names("{{counter:}} {{counter:has space}} {{counter:a/b}}").is_empty());
        // Unterminated tag: nothing extracted, no panic.
        assert!(counter_names("{{counter:never").is_empty());
        assert!(!valid_counter_name(""));
        assert!(!valid_counter_name("a".repeat(65).as_str()));
        assert!(valid_counter_name("series_2-A"));
    }

    #[test]
    fn render_template_resolves_new_builtins() {
        let vars: HashMap<String, String> = HashMap::new();
        let mut counters: HashMap<String, u64> = HashMap::new();
        counters.insert("day".to_string(), 3);
        let now = Local::now();
        let out = render_template_with_vars(
            "Today is {{weekday}}, week {{week_number}}, in {{days_until:2999-01-01}} days, day {{counter:day}} of {{counter:missing}}.",
            "u",
            "s",
            "c",
            &vars,
            &counters,
        );
        assert!(out.contains(&format!("Today is {}", now.format("%A"))));
        assert!(out.contains(&format!("week {}", now.format("%V"))));
        let expected_days = (NaiveDate::from_ymd_opt(2999, 1, 1).unwrap() - now.date_naive())
            .num_days()
            .max(0);
        assert!(out.contains(&format!("in {expected_days} days")));
        assert!(out.contains("day 3 of {{counter:missing}}"));
        // Past dates clamp at 0; unparseable dates stay literal.
        let out = render_template_with_vars(
            "{{days_until:2000-01-01}} and {{days_until:not-a-date}} and {{days_until:2026-13-40}}",
            "u",
            "s",
            "c",
            &vars,
            &counters,
        );
        assert!(out.starts_with("0 and {{days_until:not-a-date}}"));
        assert!(out.contains("{{days_until:2026-13-40}}"));
    }

    #[test]
    fn picker_never_returns_empty_messages() {
        use rand::{rngs::StdRng, SeedableRng};
        let mut s = Scenario::new("t");
        s.messages = vec!["".to_string(), "hi".to_string(), "  ".to_string()];
        let mut rng = StdRng::seed_from_u64(1);
        for _ in 0..20 {
            let (idx, msg) = s.pick_message_with_index(None, &mut rng).unwrap();
            assert_eq!(idx, 1);
            assert_eq!(msg, "hi");
        }
        s.messages = vec!["".to_string(), " ".to_string()];
        assert!(s.pick_message_with_index(None, &mut rng).is_none());
    }

    #[test]
    fn wait_seconds_saturates_instead_of_overflowing() {
        let mut s = Scenario::new("t");
        let mut n1 = Node::new(NodeKind::UtilityWait);
        n1.config = serde_json::json!({ "seconds": u64::MAX });
        let mut n2 = Node::new(NodeKind::UtilityWait);
        n2.config = serde_json::json!({ "seconds": u64::MAX });
        s.nodes = vec![n1, n2];
        // Saturating sum, then the 300s cap — never a wrap to a
        // small number, never a debug panic.
        assert_eq!(s.wait_seconds(), 300);
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
    fn daily_fires_once_at_first_occurrence_in_ambiguous_hour() {
        // 2026-11-01: clocks fall back in New York; 01:30 occurs
        // twice (05:30 UTC as EDT, 06:30 UTC as EST). L4 policy:
        // fire once, at the first.
        let sched = Schedule::Daily {
            time: NaiveTime::from_hms_opt(1, 30, 0).unwrap(),
        };
        let anchor = Utc
            .with_ymd_and_hms(2026, 10, 31, 6, 0, 0)
            .single()
            .unwrap();
        // Between the two occurrences: due at the FIRST one.
        let now = local(2026, 11, 1, 6, 0); // 01:00 EST
        let due = due_time(&sched, Some(anchor), None, now, "America/New_York").unwrap();
        let first = Utc
            .with_ymd_and_hms(2026, 11, 1, 5, 30, 0)
            .single()
            .unwrap();
        assert_eq!(due.with_timezone(&Utc), first);
        // After firing at the first occurrence, the second must
        // not fire again.
        let now = local(2026, 11, 1, 7, 0); // 02:00 EST
        assert!(due_time(&sched, Some(first), None, now, "America/New_York").is_none());
    }

    #[test]
    fn daily_skips_nonexistent_time_in_spring_gap() {
        // 2026-03-08: clocks spring forward in New York; 02:30
        // does not exist. L4 policy: never fires.
        let sched = Schedule::Daily {
            time: NaiveTime::from_hms_opt(2, 30, 0).unwrap(),
        };
        // Anchor after 03-07's 02:30 EST (07:30 UTC).
        let anchor = Utc.with_ymd_and_hms(2026, 3, 7, 8, 0, 0).single().unwrap();
        let now = local(2026, 3, 8, 12, 0);
        assert!(due_time(&sched, Some(anchor), None, now, "America/New_York").is_none());
    }

    #[test]
    fn cron_fires_once_at_first_occurrence_in_ambiguous_hour() {
        // Regression: the backward minute-walk used to return
        // the second occurrence, and then fire again at the
        // first — two sends for one wall-clock time.
        let sched = Schedule::Cron {
            expr: "30 1 * * *".to_string(),
        };
        let anchor = Utc
            .with_ymd_and_hms(2026, 10, 31, 6, 0, 0)
            .single()
            .unwrap();
        let now = local(2026, 11, 1, 7, 0); // after both occurrences
        let due = due_time(&sched, Some(anchor), None, now, "America/New_York").unwrap();
        let first = Utc
            .with_ymd_and_hms(2026, 11, 1, 5, 30, 0)
            .single()
            .unwrap();
        assert_eq!(due.with_timezone(&Utc), first);
        // After firing at the first, no second fire.
        assert!(due_time(&sched, Some(first), None, now, "America/New_York").is_none());
    }

    #[test]
    fn cron_skips_nonexistent_time_in_spring_gap() {
        let sched = Schedule::Cron {
            expr: "30 2 * * *".to_string(),
        };
        let anchor = Utc.with_ymd_and_hms(2026, 3, 7, 8, 0, 0).single().unwrap();
        let now = local(2026, 3, 8, 12, 0);
        assert!(due_time(&sched, Some(anchor), None, now, "America/New_York").is_none());
    }

    #[test]
    fn wall_clock_scan_handles_ancient_anchor() {
        // L4 clamp: a 1970 anchor must not scan ~20k days; the
        // latest occurrence is still found.
        let sched = Schedule::Daily {
            time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        };
        let anchor = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).single().unwrap();
        let now = local(2026, 11, 18, 10, 0);
        let due = due_time(&sched, Some(anchor), None, now, "UTC").unwrap();
        assert_eq!(
            due.with_timezone(&Utc),
            Utc.with_ymd_and_hms(2026, 11, 18, 9, 0, 0)
                .single()
                .unwrap()
        );
    }

    #[test]
    fn weekly_clamp_keeps_occurrence_exactly_seven_days_back() {
        // Monday 09:00; now is Monday 08:00, so the latest
        // occurrence is exactly 7 days back — the clamp window
        // must include it. (2026-11-16 and 2026-11-23 are
        // Mondays; Monday is weekday 0.)
        let sched = Schedule::Weekly {
            weekdays: vec![0],
            time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        };
        let anchor = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).single().unwrap();
        let now = local(2026, 11, 23, 8, 0);
        let due = due_time(&sched, Some(anchor), None, now, "UTC").unwrap();
        assert_eq!(
            due.with_timezone(&Utc),
            Utc.with_ymd_and_hms(2026, 11, 16, 9, 0, 0)
                .single()
                .unwrap()
        );
    }

    #[test]
    fn next_after_ambiguous_hour_returns_first_occurrence() {
        let s = Schedule::Daily {
            time: NaiveTime::from_hms_opt(1, 30, 0).unwrap(),
        };
        let from = local(2026, 11, 1, 4, 0); // 00:00 EDT
        let next = s.next_after(from, "America/New_York");
        assert_eq!(
            next.with_timezone(&Utc),
            Utc.with_ymd_and_hms(2026, 11, 1, 5, 30, 0)
                .single()
                .unwrap()
        );
        // Between the two occurrences: the day's fire has
        // passed; next is tomorrow's 01:30 EST.
        let from = local(2026, 11, 1, 6, 0); // 01:00 EST
        let next = s.next_after(from, "America/New_York");
        assert_eq!(
            next.with_timezone(&Utc),
            Utc.with_ymd_and_hms(2026, 11, 2, 6, 30, 0)
                .single()
                .unwrap()
        );
    }

    #[test]
    fn next_after_skips_spring_gap_day() {
        let s = Schedule::Daily {
            time: NaiveTime::from_hms_opt(2, 30, 0).unwrap(),
        };
        let from = local(2026, 3, 8, 5, 0); // 00:00 EST
        let next = s.next_after(from, "America/New_York");
        assert_eq!(
            next.with_timezone(&Utc),
            Utc.with_ymd_and_hms(2026, 3, 9, 6, 30, 0).single().unwrap()
        );
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

    fn cmd_opt(name: &str, kind: u8, value: &str) -> SlashOption {
        SlashOption {
            name: name.to_string(),
            kind,
            value: value.to_string(),
            options: Vec::new(),
        }
    }

    fn cmd_container(name: &str, kind: u8, children: Vec<SlashOption>) -> SlashOption {
        SlashOption {
            name: name.to_string(),
            kind,
            value: String::new(),
            options: children,
        }
    }

    #[test]
    fn slash_option_validation_rejects_bad_options() {
        // H-CMD: slash options fail fast with clear errors.
        let mut cfg = CommandConfig {
            kind: "slash".into(),
            name: "daily".into(),
            application_id: "123456789".into(),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
        // Empty option name.
        cfg.options = vec![SlashOption {
            name: "  ".into(),
            ..Default::default()
        }];
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("tên trống"), "unexpected error: {err}");
        // Unsupported type codes.
        for bad in [0u8, 12, 255] {
            cfg.options = vec![cmd_opt("count", bad, "1")];
            let err = cfg.validate().unwrap_err();
            assert!(
                err.contains("mã loại không hỗ trợ"),
                "kind {bad}: unexpected error: {err}"
            );
        }
        // Attachment (11) is blocked with its own message.
        cfg.options = vec![cmd_opt("file", 11, "x")];
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("chưa hỗ trợ"), "unexpected error: {err}");
        // Mentionable (9) is accepted.
        cfg.options = vec![cmd_opt("target", 9, "123")];
        assert!(cfg.validate().is_ok());
        // Duplicate names, including a case variant.
        cfg.options = vec![cmd_opt("count", 4, "1"), cmd_opt("Count", 4, "2")];
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("trùng"), "unexpected error: {err}");
        // Every supported leaf kind passes together.
        cfg.options = vec![
            cmd_opt("text", 3, "hi"),
            cmd_opt("n", 4, "5"),
            cmd_opt("f", 10, "2.5"),
            cmd_opt("b", 5, "true"),
            cmd_opt("u", 6, "123"),
            cmd_opt("ch", 7, "456"),
            cmd_opt("r", 8, "789"),
            cmd_opt("m", 9, "999"),
        ];
        assert!(cfg.validate().is_ok());
        // Prefix commands ignore options: junk options must not fail.
        cfg.kind = "prefix".into();
        cfg.options = vec![SlashOption {
            name: "".into(),
            kind: 0,
            value: "".into(),
            options: vec![],
        }];
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn slash_subcommand_nesting_validates() {
        let mut cfg = CommandConfig {
            kind: "slash".into(),
            name: "music".into(),
            application_id: "123456789".into(),
            ..Default::default()
        };
        // Valid: subcommand with leaf options + group of subcommands.
        cfg.options = vec![
            cmd_container("play", 1, vec![cmd_opt("song", 3, "x")]),
            cmd_container(
                "admin",
                2,
                vec![cmd_container(
                    "set",
                    1,
                    vec![cmd_opt("key", 3, "volume"), cmd_opt("value", 4, "7")],
                )],
            ),
        ];
        assert!(cfg.validate().is_ok());
        // A container with no children fails.
        cfg.options = vec![cmd_container("play", 1, vec![])];
        let err = cfg.validate().unwrap_err();
        assert!(
            err.contains("ít nhất một tham số con"),
            "unexpected error: {err}"
        );
        // A container carrying a value fails.
        cfg.options = vec![SlashOption {
            name: "play".into(),
            kind: 1,
            value: "x".into(),
            options: vec![cmd_opt("song", 3, "x")],
        }];
        let err = cfg.validate().unwrap_err();
        assert!(
            err.contains("không nhận giá trị"),
            "unexpected error: {err}"
        );
        // A group may only contain subcommands (no leaf directly).
        cfg.options = vec![cmd_container("admin", 2, vec![cmd_opt("key", 3, "x")])];
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("chỉ được lệnh con"), "unexpected error: {err}");
        // A subcommand may not nest another container.
        cfg.options = vec![cmd_container(
            "play",
            1,
            vec![cmd_container("deep", 1, vec![cmd_opt("x", 3, "y")])],
        )];
        let err = cfg.validate().unwrap_err();
        assert!(
            err.contains("chỉ được tham số thường"),
            "unexpected error: {err}"
        );
        // A leaf carrying children fails.
        cfg.options = vec![SlashOption {
            name: "count".into(),
            kind: 4,
            value: "1".into(),
            options: vec![cmd_opt("x", 3, "y")],
        }];
        let err = cfg.validate().unwrap_err();
        assert!(
            err.contains("không được có tham số con"),
            "unexpected error: {err}"
        );
        // Duplicate names inside a subcommand fail too.
        cfg.options = vec![cmd_container(
            "play",
            1,
            vec![cmd_opt("song", 3, "a"), cmd_opt("Song", 3, "b")],
        )];
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("trùng"), "unexpected error: {err}");
        // A bad leaf value fails fast, naming the parameter.
        cfg.options = vec![cmd_opt("count", 4, "abc")];
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("count"), "unexpected error: {err}");
        assert!(err.contains("số nguyên"), "unexpected error: {err}");
    }

    #[test]
    fn command_node_returns_first_command_node_only() {
        // H-CMD: `command_node()` finds the first command node, and
        // `command_nodes()` returns all of them in node order (one
        // firing runs all of them).
        let mut s = Scenario::new("cmd");
        assert!(s.command_node().is_none());
        let mut first = Node::new(NodeKind::ActionSendCommand);
        first.config = serde_json::json!({"kind": "prefix", "name": "first"});
        let mut second = Node::new(NodeKind::ActionSendCommand);
        second.config = serde_json::json!({"kind": "prefix", "name": "second"});
        s.nodes.push(first);
        s.nodes.push(second);
        let found = s.command_node().unwrap();
        assert_eq!(CommandConfig::from_node(found).name, "first");
        let all = s.command_nodes();
        assert_eq!(all.len(), 2);
        assert_eq!(CommandConfig::from_node(all[0]).name, "first");
        assert_eq!(CommandConfig::from_node(all[1]).name, "second");
    }

    #[test]
    fn message_remove_and_move() {
        // Builder Group 1: one message can be deleted or moved;
        // the ends and stale indices are safe no-ops.
        let mut s = Scenario::new("m");
        s.messages = vec!["a".into(), "b".into(), "c".into()];
        assert!(!s.move_message(0, true));
        assert!(!s.move_message(2, false));
        assert!(!s.move_message(9, true));
        assert!(s.move_message(0, false));
        assert_eq!(s.messages, vec!["b", "a", "c"]);
        assert!(s.move_message(2, true));
        assert_eq!(s.messages, vec!["b", "c", "a"]);
        assert!(!s.remove_message(9));
        assert!(s.remove_message(1));
        assert_eq!(s.messages, vec!["b", "a"]);
    }

    #[test]
    fn node_remove_refuses_only_trigger_and_moves() {
        // Builder Group 1: the only trigger step cannot be deleted
        // (validation needs exactly one), other steps can; moving
        // swaps neighbours and stops at the ends.
        let mut s = Scenario::new("n"); // TriggerSchedule + SendMessage
        let trigger_id = s.nodes[0].id.clone();
        let msg_id = s.nodes[1].id.clone();
        assert!(s.remove_node(&trigger_id).is_err());
        assert_eq!(s.nodes.len(), 2);
        assert!(s.remove_node(&msg_id).is_ok());
        assert_eq!(s.nodes.len(), 1);
        assert!(s.remove_node("no-such-id").is_err());
        s.nodes.push(Node::new(NodeKind::UtilityWait));
        s.nodes.push(Node::new(NodeKind::UtilityWait));
        let last = s.nodes[2].id.clone();
        assert!(s.move_node(&last, true));
        assert_eq!(s.nodes[1].id, last);
        assert!(s.move_node(&last, true));
        assert_eq!(s.nodes[0].id, last);
        assert!(!s.move_node(&last, true));
        assert!(!s.move_node("no-such-id", false));
    }

    #[test]
    fn workflow_labels_are_plain_vietnamese() {
        // Builder Group 2: no unexplained "*" / "%" markers, and
        // "Call API" is "Lấy dữ liệu" everywhere the user sees it.
        for kind in [
            NodeKind::TriggerSchedule,
            NodeKind::TriggerManual,
            NodeKind::TriggerMessageReceived,
            NodeKind::TriggerMemberJoin,
            NodeKind::ActionSendMessage,
            NodeKind::ActionSendEmbed,
            NodeKind::ActionSendCommand,
            NodeKind::ActionSendFile,
            NodeKind::ActionAddReaction,
            NodeKind::ActionReply,
            NodeKind::ActionDeleteMessage,
            NodeKind::ConditionKeyword,
            NodeKind::ConditionTimeWindow,
            NodeKind::ConditionRandom,
            NodeKind::ConditionRole,
            NodeKind::ConditionVariable,
            NodeKind::UtilityWait,
            NodeKind::UtilityRandomPick,
            NodeKind::UtilitySetVariable,
            NodeKind::UtilityLoop,
            NodeKind::UtilityCallApi,
        ] {
            let label = kind.label();
            assert!(!label.ends_with(" *"), "{label}");
            assert!(!label.ends_with(" %"), "{label}");
            assert!(!label.contains("Call API"), "{label}");
        }
        assert_eq!(NodeKind::UtilityCallApi.label(), "Tiện ích: Lấy dữ liệu");
    }

    #[test]
    fn malformed_command_config_fails_closed() {
        // H-CMD: from_node tolerates malformed JSON by returning the
        // default config — and the default fails validation, so a
        // corrupt row can never send (no bare "!" message).
        let mut node = Node::new(NodeKind::ActionSendCommand);
        node.config = serde_json::json!({"kind": 123, "name": ["not", "a", "string"]});
        let cfg = CommandConfig::from_node(&node);
        assert_eq!(cfg.name, "");
        assert!(cfg.validate().is_err());
        // A slash config missing name + application ID also fails.
        node.config = serde_json::json!({"kind": "slash"});
        assert!(CommandConfig::from_node(&node).validate().is_err());
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
    fn edit_mode_fields_default_off_and_round_trip() {
        let fresh = Scenario::new("e");
        assert!(!fresh.edit_last_message);
        assert!(fresh.last_message_ids.is_empty());
        let mut s = Scenario::new("e2");
        s.edit_last_message = true;
        s.last_message_ids
            .insert("dest-1".to_string(), "111".to_string());
        s.last_message_ids
            .insert("chan-9".to_string(), "222".to_string());
        let json = serde_json::to_string(&s).unwrap();
        let back: Scenario = serde_json::from_str(&json).unwrap();
        assert!(back.edit_last_message);
        assert_eq!(
            back.last_message_ids.get("dest-1").map(String::as_str),
            Some("111")
        );
        assert_eq!(
            back.last_message_ids.get("chan-9").map(String::as_str),
            Some("222")
        );
        // Old files without the fields read as off/empty.
        let old: Scenario = serde_json::from_str(r#"{"name":"o"}"#).unwrap();
        assert!(!old.edit_last_message);
        assert!(old.last_message_ids.is_empty());
        // Edit mode + a command node is rejected (the command path
        // ignores the mode, so the combination must not enable).
        let mut cmd = Scenario::new("cmd");
        cmd.channel_id = "123".to_string();
        cmd.edit_last_message = true;
        let mut node = Node::new(NodeKind::ActionSendCommand);
        node.config = serde_json::json!({"kind": "prefix", "name": "rank"});
        cmd.nodes.push(node);
        let err = cmd.validate().unwrap_err();
        assert!(err.contains("sửa tin nhắn cũ"), "unexpected error: {err}");
        cmd.edit_last_message = false;
        assert!(cmd.validate().is_ok());
    }

    #[test]
    fn api_call_config_validation_and_duplicates() {
        // A fresh node (empty config) fails closed.
        let node = Node::new(NodeKind::UtilityCallApi);
        let cfg = ApiCallConfig::from_node(&node);
        assert!(cfg.validate().is_err());
        // Malformed config parses as the default -> fails closed.
        let mut bad = Node::new(NodeKind::UtilityCallApi);
        bad.config = serde_json::json!({"timeout_secs": "not-a-number"});
        assert!(ApiCallConfig::from_node(&bad).validate().is_err());
        // A complete config validates.
        let mut good = Node::new(NodeKind::UtilityCallApi);
        good.config = serde_json::json!({
            "url_ref": "blob:xxx",
            "url_host": "api.example.com",
            "variable": "temp",
            "field_path": "weather.temp",
            "timeout_secs": 10,
            "on_failure": "skip"
        });
        assert!(ApiCallConfig::from_node(&good).validate().is_ok());
        // Each rule, broken one at a time.
        let mut broken = ApiCallConfig::from_node(&good);
        broken.variable = "bad name!".to_string();
        assert!(broken.validate().is_err());
        broken = ApiCallConfig::from_node(&good);
        broken.field_path = "weather..temp".to_string();
        assert!(broken.validate().is_err());
        broken = ApiCallConfig::from_node(&good);
        broken.timeout_secs = 0;
        assert!(broken.validate().is_err());
        broken = ApiCallConfig::from_node(&good);
        broken.timeout_secs = 61;
        assert!(broken.validate().is_err());
        broken = ApiCallConfig::from_node(&good);
        broken.on_failure = "retry".to_string();
        assert!(broken.validate().is_err());
        // Duplicate variable names across nodes are rejected.
        let mut s = Scenario::new("api");
        s.channel_id = "123".to_string();
        s.nodes.push(good.clone());
        let mut second = Node::new(NodeKind::UtilityCallApi);
        second.config = serde_json::json!({
            "url_ref": "blob:yyy",
            "variable": "temp",
            "field_path": "other",
            "timeout_secs": 10
        });
        s.nodes.push(second);
        let err = s.validate().unwrap_err();
        assert!(err.contains("cùng đưa vào biến"), "unexpected error: {err}");
    }

    #[test]
    fn attachment_validation_enforces_count_and_paths() {
        let mut s = Scenario::new("a");
        s.channel_id = "123".to_string();
        assert!(s.validate().is_ok());
        s.attachments = vec!["".to_string()];
        assert!(s.validate().is_err());
        s.attachments = (0..11).map(|i| format!("/tmp/f{i}.png")).collect();
        let err = s.validate().unwrap_err();
        assert!(err.contains("11 tệp đính kèm"), "unexpected error: {err}");
        s.attachments = (0..10).map(|i| format!("/tmp/f{i}.png")).collect();
        assert!(s.validate().is_ok());
        // Paths round-trip through JSON (the files themselves are
        // never stored).
        let json = serde_json::to_string(&s).unwrap();
        let back: Scenario = serde_json::from_str(&json).unwrap();
        assert_eq!(back.attachments.len(), 10);
        assert_eq!(back.attachments[0], "/tmp/f0.png");
    }

    #[test]
    fn destination_ids_migration_reads_old_and_new_forms() {
        // Old form: one string under "destination_id".
        let old: Scenario =
            serde_json::from_str(r#"{"name":"o","destination_id":"dest-1"}"#).unwrap();
        assert_eq!(old.destination_ids, vec!["dest-1"]);
        assert_eq!(old.primary_destination_id(), Some("dest-1"));
        // New form: array under "destination_ids".
        let new: Scenario =
            serde_json::from_str(r#"{"name":"n","destination_ids":["a","b"]}"#).unwrap();
        assert_eq!(new.destination_ids, vec!["a", "b"]);
        // Null / missing -> empty.
        let null: Scenario =
            serde_json::from_str(r#"{"name":"z","destination_ids":null}"#).unwrap();
        assert!(null.destination_ids.is_empty());
        let missing: Scenario = serde_json::from_str(r#"{"name":"m"}"#).unwrap();
        assert!(missing.destination_ids.is_empty());
        // Empty strings and duplicates are dropped on read.
        let messy: Scenario =
            serde_json::from_str(r#"{"name":"x","destination_ids":["a","","a"," b ","b"]}"#)
                .unwrap();
        assert_eq!(messy.destination_ids, vec!["a", "b"]);
        // New files serialize only "destination_ids".
        let json = serde_json::to_string(&new).unwrap();
        assert!(json.contains("destination_ids"));
        assert!(!json.contains("destination_id\""), "json: {json}");
    }

    #[test]
    fn broadcast_validation_requires_one_and_rejects_duplicates() {
        let mut s = Scenario::new("b");
        assert!(s.validate_for_webhook().is_err());
        s.destination_ids = vec!["a".to_string(), "a".to_string()];
        let err = s.validate_for_webhook().unwrap_err();
        assert!(err.contains("hai lần"), "unexpected error: {err}");
        s.destination_ids = vec!["a".to_string(), "b".to_string()];
        assert!(s.validate_for_webhook().is_ok());
        // User mode ignores the list entirely (channel governs).
        s.destination_ids = vec!["a".to_string(), "a".to_string()];
        s.channel_id = "123".to_string();
        assert!(s.validate().is_ok());
    }

    #[test]
    fn webhook_validation_allows_empty_channel_but_rejects_commands() {
        // Message scenario with no channel: user-mode validation fails,
        // webhook validation passes (the URL is channel-bound) once a
        // destination is selected; without one it fails.
        let mut s = Scenario::new("webhook");
        assert!(s.validate().is_err());
        assert!(s.validate_for_webhook().is_err());
        s.destination_ids = vec!["dest-1".to_string()];
        assert!(s.validate_for_webhook().is_ok());
        // Adding a command node flips webhook validation to an error,
        // while user-mode validation (with a channel) still passes.
        let mut s2 = Scenario::new("webhook-cmd");
        s2.channel_id = "123".to_string();
        s2.destination_ids = vec!["dest-1".to_string()];
        let mut node = Node::new(NodeKind::ActionSendCommand);
        node.config = serde_json::json!({"kind": "prefix", "name": "rank"});
        s2.nodes.push(node);
        assert!(s2.validate().is_ok());
        let err = s2.validate_for_webhook().unwrap_err();
        assert!(err.contains("chế độ Webhook"), "unexpected error: {err}");
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
        assert_eq!(slash_option_kind_label(1), "Lệnh con");
        assert_eq!(slash_option_kind_label(2), "Nhóm lệnh con");
        assert_eq!(slash_option_kind_label(3), "Chuỗi");
        assert_eq!(slash_option_kind_label(4), "Số nguyên");
        assert_eq!(slash_option_kind_label(5), "Đúng / Sai");
        assert_eq!(slash_option_kind_label(6), "Người dùng");
        assert_eq!(slash_option_kind_label(7), "Kênh");
        assert_eq!(slash_option_kind_label(8), "Vai trò");
        assert_eq!(slash_option_kind_label(9), "Nhắc đến");
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
    fn cron_huge_step_marks_only_the_first_value() {
        // A step near u32::MAX used to panic in debug builds and wrap
        // to wrong minutes in release builds. Saturating addition
        // stops the scan after the first value.
        let fields = parse_cron("30-59/4294967290 * * * *").unwrap();
        assert!(fields.minute[30]);
        assert_eq!(fields.minute.iter().filter(|m| **m).count(), 1);
        let fields = parse_cron("*/4294967290 * * * *").unwrap();
        assert!(fields.minute[0]);
        assert_eq!(fields.minute.iter().filter(|m| **m).count(), 1);
    }

    #[test]
    fn scenario_accepts_camelcase_channel_id_alias() {
        let json = r#"{"schemaVersion":1,"id":"x","name":"t","channelId":"123","schedule":{"kind":"daily","time":"09:00:00"},"nodes":[{"id":"n1","type":"trigger_schedule"},{"id":"n2","type":"action_send_message","config":{"content":"hi"}}],"messages":["hi"]}"#;
        let s: Scenario = serde_json::from_str(json).unwrap();
        assert_eq!(s.channel_id, "123");
        assert!(s.validate().is_ok());
    }
}
