//! Anti-ban, 5 layers (Spec §7).
//! These REDUCE detection risk. They do NOT make self-botting legal or safe.
//! There is no way to guarantee an account will never be banned.

use chrono::{Local, Timelike};
use rand::Rng;
use serde::{Deserialize, Serialize};

/// The daily cap for every user: 300 messages/day, resetting at 00:00 UTC.
/// Warm-up: 30% (90/day) for the first 3 days since first run.
pub const DAILY_CAP: u32 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AntiBanConfig {
    /// Minimum seconds between two sends in the same channel (default 10).
    pub min_interval_secs: u64,
    /// Hard floor: never below this, even if the user asks (default 5).
    pub hard_floor_secs: u64,
    /// Random extra delay per send, seconds (default 5..=25).
    pub jitter_min_secs: u64,
    pub jitter_max_secs: u64,
    /// Max messages per day per account (300 for everyone).
    pub daily_cap: u32,
    /// Warm-up: first N days run at 30% of the cap.
    pub warmup_days: u32,
    /// Night sleep window, local hours [sleep_start, sleep_end).
    pub sleep_start_hour: u32,
    pub sleep_end_hour: u32,
    /// Block @everyone / @here unless the user explicitly confirms.
    pub block_everyone: bool,
    /// Smart Templates (Discord-BLUE): Spintax `{a|b}` + random message
    /// variation, applied after template vars are resolved (default off).
    pub smart_templates: bool,
    /// Auto Typing (Discord-BLUE): show "typing..." before sending
    /// (default off).
    pub auto_typing: bool,
    /// Typing indicator duration, random seconds in this range.
    pub typing_min_sec: u64,
    pub typing_max_sec: u64,
    /// Auto Break (Discord-BLUE): long rest after N sends (default off).
    pub auto_break: bool,
    /// Rest after a random count in this range of sends.
    pub break_after_min: u32,
    pub break_after_max: u32,
    /// Rest duration, random minutes in this range.
    pub break_duration_min: u32,
    pub break_duration_max: u32,
    /// Auto Delete (Discord-BLUE): delete the message after sending
    /// (default off).
    pub auto_delete: bool,
    /// Delay before deleting, milliseconds (0 = immediately).
    pub delete_delay_ms: u64,
}

impl Default for AntiBanConfig {
    fn default() -> Self {
        Self {
            min_interval_secs: 10,
            hard_floor_secs: 5,
            jitter_min_secs: 5,
            jitter_max_secs: 25,
            daily_cap: DAILY_CAP,
            warmup_days: 3,
            sleep_start_hour: 23,
            sleep_end_hour: 7,
            block_everyone: true,
            smart_templates: false,
            auto_typing: false,
            typing_min_sec: 2,
            typing_max_sec: 5,
            auto_break: false,
            break_after_min: 15,
            break_after_max: 25,
            break_duration_min: 10,
            break_duration_max: 30,
            auto_delete: false,
            delete_delay_ms: 0,
        }
    }
}

impl AntiBanConfig {
    /// Enforce the hard floor: effective interval is never below 5s.
    pub fn effective_interval_secs(&self) -> u64 {
        self.min_interval_secs.max(self.hard_floor_secs)
    }

    /// Random jitter for one send (5–25s by default).
    pub fn jitter_secs(&self, rng: &mut impl Rng) -> u64 {
        let lo = self.jitter_min_secs;
        let hi = self.jitter_max_secs.max(lo);
        rng.random_range(lo..=hi)
    }

    /// Random typing-indicator duration, seconds (2–5s by default).
    /// A reversed range is fixed by swapping, never by panicking.
    pub fn typing_secs(&self, rng: &mut impl Rng) -> u64 {
        let (lo, hi) = if self.typing_min_sec <= self.typing_max_sec {
            (self.typing_min_sec, self.typing_max_sec)
        } else {
            (self.typing_max_sec, self.typing_min_sec)
        };
        rng.random_range(lo..=hi)
    }

    /// Random send count after which an Auto Break starts (15–25 by
    /// default). At least 1, so a break can always trigger.
    pub fn break_after_target(&self, rng: &mut impl Rng) -> u32 {
        let lo = self.break_after_min.max(1);
        let hi = self.break_after_max.max(lo);
        rng.random_range(lo..=hi)
    }

    /// Random Auto Break duration, seconds (10–30 min by default).
    pub fn break_duration_secs(&self, rng: &mut impl Rng) -> u64 {
        let lo = self.break_duration_min.max(1);
        let hi = self.break_duration_max.max(lo);
        rng.random_range(lo..=hi) as u64 * 60
    }

    /// Is `now` inside the night sleep window?
    pub fn in_sleep_window(&self, now: chrono::DateTime<Local>) -> bool {
        let h = now.hour();
        if self.sleep_start_hour <= self.sleep_end_hour {
            h >= self.sleep_start_hour && h < self.sleep_end_hour
        } else {
            // Overnight window, e.g. 23:00–07:00.
            h >= self.sleep_start_hour || h < self.sleep_end_hour
        }
    }

    /// Effective daily cap during warm-up (30% for the first N days).
    /// `base_cap` is the account cap (300) — never hardcoded here, so the
    /// caller decides which cap applies.
    pub fn effective_daily_cap(&self, base_cap: u32, account_age_days: u32) -> u32 {
        if account_age_days < self.warmup_days {
            (base_cap as f64 * 0.3).ceil() as u32
        } else {
            base_cap
        }
    }
}

/// Layer 4 — content checks. Returns (cleaned_content, warnings).
/// @everyone / @here are stripped when blocked; over-long messages are cut.
///
/// `defang_mass_mentions` is false only in webhook mode when the
/// scenario explicitly allows mass mentions (idea #7): there,
/// Discord's official `allowed_mentions` mechanism governs, and
/// mangling the text would silently override the user's opt-in.
/// Every other path passes true — user-token mode always defangs
/// when blocked, exactly as before (F-GUI-3).
pub fn sanitize_content(
    content: &str,
    cfg: &AntiBanConfig,
    defang_mass_mentions: bool,
) -> (String, Vec<String>) {
    let mut warnings = Vec::new();
    let mut out = content.to_string();
    if defang_mass_mentions
        && cfg.block_everyone
        && (out.contains("@everyone") || out.contains("@here"))
    {
        out = out.replace("@everyone", "@\u{200b}everyone");
        out = out.replace("@here", "@\u{200b}here");
        warnings.push("@everyone/@here blocked by default (anti-ban layer 4)".to_string());
    }
    if out.chars().count() > 2000 {
        out = out.chars().take(2000).collect();
        warnings.push("message truncated to Discord's 2000-character limit".to_string());
    }
    // Repeated identical links are a classic spam signal.
    let link_count = out.matches("http").count();
    if link_count >= 3 {
        warnings.push(format!(
            "message contains {} links — high spam signal, consider fewer",
            link_count
        ));
    }
    if out.trim().is_empty() {
        warnings.push("message content is empty".to_string());
    }
    (out, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    #[test]
    fn hard_floor_never_below_five_seconds() {
        let cfg = AntiBanConfig {
            min_interval_secs: 1,
            ..Default::default()
        };
        assert_eq!(cfg.effective_interval_secs(), 5);
    }

    #[test]
    fn jitter_stays_in_range() {
        let cfg = AntiBanConfig::default();
        let mut rng = ChaCha8Rng::seed_from_u64(42);
        for _ in 0..50 {
            let j = cfg.jitter_secs(&mut rng);
            assert!((5..=25).contains(&j));
        }
    }

    #[test]
    fn everyone_is_blocked_by_default() {
        let cfg = AntiBanConfig::default();
        let (clean, warnings) = sanitize_content("hi @everyone", &cfg, true);
        assert!(!clean.contains("@everyone"));
        assert!(!warnings.is_empty());
    }

    #[test]
    fn everyone_survives_when_defang_is_off() {
        // F-GUI-3: webhook mode + explicit opt-in. The text keeps a
        // literal @everyone (Discord's allowed_mentions governs) and
        // no block warning is emitted.
        let cfg = AntiBanConfig::default();
        let (clean, warnings) = sanitize_content("hi @everyone and @here", &cfg, false);
        assert_eq!(clean, "hi @everyone and @here");
        assert!(warnings.is_empty());
    }

    #[test]
    fn default_cap_is_300_with_90_warmup() {
        let cfg = AntiBanConfig::default();
        assert_eq!(cfg.daily_cap, DAILY_CAP);
        assert_eq!(cfg.daily_cap, 300);
        // Warm-up: 30% of 300 = 90/day for the first 3 days.
        assert_eq!(cfg.effective_daily_cap(cfg.daily_cap, 0), 90);
        assert_eq!(cfg.effective_daily_cap(cfg.daily_cap, 2), 90);
        // Day 3 (age == warmup_days) is full cap.
        assert_eq!(cfg.effective_daily_cap(cfg.daily_cap, 3), 300);
        assert_eq!(cfg.effective_daily_cap(cfg.daily_cap, 10), 300);
    }

    #[test]
    fn warmup_caps_at_thirty_percent() {
        let cfg = AntiBanConfig::default();
        assert_eq!(cfg.effective_daily_cap(300, 0), 90);
        assert_eq!(cfg.effective_daily_cap(300, 10), 300);
    }

    #[test]
    fn new_blue_features_default_off() {
        let cfg = AntiBanConfig::default();
        assert!(!cfg.smart_templates);
        assert!(!cfg.auto_typing);
        assert!(!cfg.auto_break);
        assert!(!cfg.auto_delete);
        assert_eq!(cfg.delete_delay_ms, 0);
    }

    #[test]
    fn typing_secs_stays_in_range_and_survives_reversed_range() {
        let cfg = AntiBanConfig::default();
        let mut rng = ChaCha8Rng::seed_from_u64(7);
        for _ in 0..50 {
            assert!((2..=5).contains(&cfg.typing_secs(&mut rng)));
        }
        // Reversed min/max must not panic and still lands in range.
        let rev = AntiBanConfig {
            typing_min_sec: 9,
            typing_max_sec: 3,
            ..Default::default()
        };
        for _ in 0..20 {
            assert!((3..=9).contains(&rev.typing_secs(&mut rng)));
        }
    }

    #[test]
    fn break_target_and_duration_stay_in_range() {
        let cfg = AntiBanConfig::default();
        let mut rng = ChaCha8Rng::seed_from_u64(7);
        for _ in 0..50 {
            assert!((15..=25).contains(&cfg.break_after_target(&mut rng)));
            let d = cfg.break_duration_secs(&mut rng);
            assert!((10 * 60..=30 * 60).contains(&d));
        }
        // Zero bounds are clamped, never panic.
        let zero = AntiBanConfig {
            break_after_min: 0,
            break_after_max: 0,
            break_duration_min: 0,
            break_duration_max: 0,
            ..Default::default()
        };
        assert_eq!(zero.break_after_target(&mut rng), 1);
        assert_eq!(zero.break_duration_secs(&mut rng), 60);
    }
}
