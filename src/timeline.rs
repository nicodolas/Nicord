//! Upcoming-send computation for the Dashboard timeline and the
//! Builder schedule simulation. Pure functions over schedules,
//! destinations, and already-counted send totals — no sending, no
//! mutation, no database access. The engine is the only thing that
//! fires; this module only predicts what it will do, using the same
//! `next_after` the "next run" lines use and the same skip checks
//! the engine applies (destination state, quiet window, daily cap,
//! time-window nodes).
//!
//! Predictions are honest about their limits: the daily-cap check
//! only applies to runs dated today (future days' counts cannot be
//! known), 429 cooldowns and the night sleep window are not
//! predicted, and interval schedules are projected forward from
//! `now` rather than from the last fired run.

use std::collections::HashMap;

use chrono::{DateTime, Local, Timelike, Utc};

use crate::destination::Destination;
use crate::scenario::{Scenario, Schedule};

/// One upcoming run in the Dashboard timeline.
#[derive(Debug, Clone)]
pub struct UpcomingRun {
    pub at: DateTime<Local>,
    pub scenario_id: String,
    pub scenario_name: String,
    /// Destination display name, or "" in user-token mode.
    pub destination_name: String,
    /// Why this run will be skipped, or `None` when it should send.
    pub blocked: Option<String>,
}

/// One simulated run in the Builder schedule preview.
#[derive(Debug, Clone)]
pub struct SimulatedRun {
    pub at: DateTime<Local>,
    pub blocked: Option<String>,
}

/// Future run times for one schedule, strictly after `from`, up to
/// `limit` runs or `horizon_end`, whichever comes first. `Once`
/// yields at most one run, only when still in the future. Iteration
/// steps one second past each yielded run: every schedule kind
/// produces minute-aligned times, so stepping a second past a run
/// can never re-yield it, and the `next <= cursor` guard stops the
/// loop even if a schedule misbehaves.
pub fn future_runs(
    schedule: &Schedule,
    timezone: &str,
    from: DateTime<Local>,
    limit: usize,
    horizon_end: DateTime<Local>,
) -> Vec<DateTime<Local>> {
    let mut runs = Vec::new();
    let mut cursor = from;
    for _ in 0..limit {
        let next = schedule.next_after(cursor, timezone);
        if next <= cursor || next > horizon_end {
            break;
        }
        runs.push(next);
        cursor = next + chrono::Duration::seconds(1);
    }
    runs
}

/// Why a run at `at` will be skipped, mirroring the engine's
/// ahead-of-time checks. `None` means the run should send.
/// `sent_today_for_dest` is the destination's persisted sent-today
/// count; it is only consulted for runs dated today (UTC).
pub fn blocked_reason(
    scenario: &Scenario,
    destination: Option<&Destination>,
    is_webhook: bool,
    at: DateTime<Local>,
    sent_today_for_dest: u32,
    global_daily_cap: u32,
) -> Option<String> {
    if is_webhook {
        let Some(dest) = destination else {
            return Some("chưa gắn đích đến".to_string());
        };
        if !dest.enabled {
            return Some(format!("đích đến '{}' đang tắt", dest.display_name()));
        }
        if dest.failure_state.is_paused() {
            return Some(format!(
                "đích đến '{}' đang tạm dừng ({})",
                dest.display_name(),
                dest.failure_state.label()
            ));
        }
        if dest.encrypted_url.trim().is_empty() {
            return Some(format!(
                "đích đến '{}' chưa có URL webhook",
                dest.display_name()
            ));
        }
        let minutes = at.hour() * 60 + at.minute();
        if dest.is_quiet_at(minutes) {
            let window = match (&dest.quiet_start, &dest.quiet_end) {
                (Some(s), Some(e)) => format!("{s}–{e}"),
                _ => "giờ yên tĩnh".to_string(),
            };
            return Some(format!("giờ yên tĩnh của đích đến ({window})"));
        }
        // Daily cap: only runs dated today can be judged, and only
        // against the persisted count the engine itself uses.
        if at.with_timezone(&Utc).date_naive() == Utc::now().date_naive() {
            let effective_cap = if dest.daily_cap == 0 {
                global_daily_cap
            } else {
                dest.daily_cap
            };
            if sent_today_for_dest >= effective_cap {
                return Some(format!(
                    "đạt giới hạn ngày của đích đến ({sent_today_for_dest}/{effective_cap})"
                ));
            }
        }
    }
    // Time-window nodes: reuse the engine's own check at the run
    // time. Scenarios without such nodes always pass.
    if let Err(reason) = scenario.check_time_windows(at) {
        return Some(reason);
    }
    None
}

/// Next `limit` runs for one scenario (saved or draft), each with
/// its blocked reason. The simulation horizon is 30 days: far
/// enough for monthly and sparse cron schedules to show a run.
pub fn simulate_scenario(
    scenario: &Scenario,
    destination: Option<&Destination>,
    is_webhook: bool,
    sent_today_for_dest: u32,
    global_daily_cap: u32,
    now: DateTime<Local>,
    limit: usize,
) -> Vec<SimulatedRun> {
    let Some(schedule) = scenario.schedule.as_ref() else {
        return Vec::new();
    };
    let horizon = now + chrono::Duration::days(30);
    future_runs(schedule, &scenario.timezone, now, limit, horizon)
        .into_iter()
        .map(|at| SimulatedRun {
            blocked: blocked_reason(
                scenario,
                destination,
                is_webhook,
                at,
                sent_today_for_dest,
                global_daily_cap,
            ),
            at,
        })
        .collect()
}

/// The timeline: upcoming runs across enabled scenarios within
/// `horizon_days`, merged, sorted by time, capped at `limit`
/// entries. `sent_today_by_dest` maps destination id → persisted
/// sent-today count (built by the caller, once per refresh).
#[allow(clippy::too_many_arguments)]
pub fn upcoming_runs(
    scenarios: &[Scenario],
    destinations: &[Destination],
    is_webhook: bool,
    sent_today_by_dest: &HashMap<String, u32>,
    global_daily_cap: u32,
    now: DateTime<Local>,
    horizon_days: i64,
    per_scenario: usize,
    limit: usize,
) -> Vec<UpcomingRun> {
    let horizon_end = now + chrono::Duration::days(horizon_days);
    let mut runs: Vec<UpcomingRun> = Vec::new();
    for scenario in scenarios {
        if !scenario.enabled {
            continue;
        }
        let Some(schedule) = scenario.schedule.as_ref() else {
            continue;
        };
        let destination = scenario
            .destination_id
            .as_deref()
            .and_then(|id| destinations.iter().find(|d| d.id == id));
        let sent = destination
            .map(|d| sent_today_by_dest.get(&d.id).copied().unwrap_or(0))
            .unwrap_or(0);
        for at in future_runs(schedule, &scenario.timezone, now, per_scenario, horizon_end) {
            runs.push(UpcomingRun {
                blocked: blocked_reason(
                    scenario,
                    destination,
                    is_webhook,
                    at,
                    sent,
                    global_daily_cap,
                ),
                at,
                scenario_id: scenario.id.clone(),
                scenario_name: scenario.name.clone(),
                destination_name: destination.map(|d| d.display_name()).unwrap_or_default(),
            });
        }
    }
    runs.sort_by(|a, b| a.at.cmp(&b.at).then(a.scenario_name.cmp(&b.scenario_name)));
    runs.truncate(limit);
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
            .with_timezone(&Local)
    }

    fn daily_at(h: u32, m: u32) -> Schedule {
        Schedule::Daily {
            time: chrono::NaiveTime::from_hms_opt(h, m, 0).unwrap(),
        }
    }

    #[test]
    fn future_runs_daily_steps_day_by_day() {
        let runs = future_runs(
            &daily_at(9, 0),
            "UTC",
            utc(2026, 11, 17, 8, 0),
            3,
            utc(2026, 11, 30, 0, 0),
        );
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0], utc(2026, 11, 17, 9, 0));
        assert_eq!(runs[1], utc(2026, 11, 18, 9, 0));
        assert_eq!(runs[2], utc(2026, 11, 19, 9, 0));
    }

    #[test]
    fn future_runs_interval_steps_by_the_interval() {
        let runs = future_runs(
            &Schedule::EveryMinutes { minutes: 30 },
            "UTC",
            utc(2026, 11, 17, 8, 0),
            3,
            utc(2026, 11, 18, 0, 0),
        );
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0], utc(2026, 11, 17, 8, 30));
        // Each step advances by the interval plus the 1-second
        // iteration step (duration assertions: timezone-proof).
        let step = chrono::Duration::minutes(30) + chrono::Duration::seconds(1);
        assert_eq!(runs[1] - runs[0], step);
        assert_eq!(runs[2] - runs[1], step);
    }

    #[test]
    fn future_runs_once_yields_once_and_only_in_the_future() {
        let future = Schedule::Once {
            at: Utc.with_ymd_and_hms(2026, 12, 1, 9, 0, 0).unwrap(),
        };
        let runs = future_runs(
            &future,
            "UTC",
            utc(2026, 11, 17, 8, 0),
            5,
            utc(2026, 12, 31, 0, 0),
        );
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0], utc(2026, 12, 1, 9, 0));
        let past = Schedule::Once {
            at: Utc.with_ymd_and_hms(2026, 11, 1, 9, 0, 0).unwrap(),
        };
        let runs = future_runs(
            &past,
            "UTC",
            utc(2026, 11, 17, 8, 0),
            5,
            utc(2026, 12, 31, 0, 0),
        );
        assert!(runs.is_empty());
    }

    #[test]
    fn future_runs_cron_and_horizon() {
        let runs = future_runs(
            &Schedule::Cron {
                expr: "0 9 * * *".to_string(),
            },
            "UTC",
            utc(2026, 11, 17, 8, 0),
            5,
            utc(2026, 11, 18, 12, 0),
        );
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0], utc(2026, 11, 17, 9, 0));
        assert_eq!(runs[1], utc(2026, 11, 18, 9, 0));
    }

    fn scenario_with(schedule: Schedule) -> Scenario {
        let mut s = Scenario::new("t");
        s.enabled = true;
        s.schedule = Some(schedule);
        s.timezone = "UTC".to_string();
        s
    }

    #[test]
    fn blocked_reason_covers_destination_state_quiet_and_cap() {
        let s = scenario_with(daily_at(9, 0));
        // No destination in webhook mode.
        assert!(blocked_reason(&s, None, true, utc(2026, 11, 17, 9, 0), 0, 300).is_some());
        // User mode without a destination is fine.
        assert!(blocked_reason(&s, None, false, utc(2026, 11, 17, 9, 0), 0, 300).is_none());

        let mut dest = Destination::new("D");
        dest.encrypted_url = "blob".to_string();
        dest.daily_cap = 50;
        // Healthy destination, run dated two days out: not blocked.
        // Fixed UTC dates, so the test cannot flake near midnight.
        let today_noon = Utc::now()
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        let future = (today_noon + chrono::Duration::days(2)).with_timezone(&Local);
        assert!(blocked_reason(&s, Some(&dest), true, future, 0, 300).is_none());

        dest.enabled = false;
        assert!(blocked_reason(&s, Some(&dest), true, future, 0, 300)
            .unwrap()
            .contains("đang tắt"));
        dest.enabled = true;

        dest.quiet_start = Some("22:00".to_string());
        dest.quiet_end = Some("06:00".to_string());
        // A run at 23:00 local is inside the quiet window. Use a
        // fixed local wall time: construct via Local directly.
        let night = Local.with_ymd_and_hms(2026, 11, 17, 23, 0, 0).unwrap();
        let reason = blocked_reason(&s, Some(&dest), true, night, 0, 300).unwrap();
        assert!(reason.contains("yên tĩnh"), "{reason}");
        let day = Local.with_ymd_and_hms(2026, 11, 17, 10, 0, 0).unwrap();
        assert!(blocked_reason(&s, Some(&dest), true, day, 0, 300).is_none());
        dest.quiet_start = None;
        dest.quiet_end = None;

        // Cap: a run dated today at the cap is blocked; the same
        // count does not block a run dated two days out.
        let today_run = today_noon.with_timezone(&Local);
        assert!(blocked_reason(&s, Some(&dest), true, today_run, 50, 300)
            .unwrap()
            .contains("giới hạn"));
        assert!(blocked_reason(&s, Some(&dest), true, future, 50, 300).is_none());
    }

    #[test]
    fn upcoming_runs_merges_sorts_and_limits() {
        let mut a = scenario_with(daily_at(9, 0));
        a.name = "A".to_string();
        let mut b = scenario_with(daily_at(8, 0));
        b.name = "B".to_string();
        let mut disabled = scenario_with(daily_at(7, 0));
        disabled.enabled = false;
        let runs = upcoming_runs(
            &[a, b, disabled],
            &[],
            false,
            &HashMap::new(),
            300,
            utc(2026, 11, 17, 6, 0),
            2,
            5,
            3,
        );
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].scenario_name, "B");
        assert_eq!(runs[1].scenario_name, "A");
        assert_eq!(runs[2].scenario_name, "B");
        assert!(runs.iter().all(|r| r.blocked.is_none()));
    }
}
