//! Anti-ban, 5 layers (Spec §7).
//! These REDUCE detection risk. They do NOT make self-botting legal or safe.
//! There is no way to guarantee an account will never be banned.

use chrono::{Local, Timelike};
use rand::Rng;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntiBanConfig {
    /// Minimum seconds between two sends in the same channel (default 10).
    pub min_interval_secs: u64,
    /// Hard floor: never below this, even if the user asks (default 5).
    pub hard_floor_secs: u64,
    /// Random extra delay per send, seconds (default 5..=25).
    pub jitter_min_secs: u64,
    pub jitter_max_secs: u64,
    /// Max messages per day per account (Free default 100).
    pub daily_cap: u32,
    /// Warm-up: first N days run at 30% of the cap.
    pub warmup_days: u32,
    /// Night sleep window, local hours [sleep_start, sleep_end).
    pub sleep_start_hour: u32,
    pub sleep_end_hour: u32,
    /// Block @everyone / @here unless the user explicitly confirms.
    pub block_everyone: bool,
}

impl Default for AntiBanConfig {
    fn default() -> Self {
        Self {
            min_interval_secs: 10,
            hard_floor_secs: 5,
            jitter_min_secs: 5,
            jitter_max_secs: 25,
            daily_cap: 100,
            warmup_days: 3,
            sleep_start_hour: 23,
            sleep_end_hour: 7,
            block_everyone: true,
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
    /// `base_cap` is the plan/scenario cap (Free 100, Pro 1000) — never
    /// hardcoded here, so the caller decides which cap applies.
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
pub fn sanitize_content(content: &str, cfg: &AntiBanConfig) -> (String, Vec<String>) {
    let mut warnings = Vec::new();
    let mut out = content.to_string();
    if cfg.block_everyone && (out.contains("@everyone") || out.contains("@here")) {
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
        let (clean, warnings) = sanitize_content("hi @everyone", &cfg);
        assert!(!clean.contains("@everyone"));
        assert!(!warnings.is_empty());
    }

    #[test]
    fn warmup_caps_at_thirty_percent() {
        let cfg = AntiBanConfig::default();
        assert_eq!(cfg.effective_daily_cap(100, 0), 30);
        assert_eq!(cfg.effective_daily_cap(100, 10), 100);
        // Pro base cap also warms up at 30%.
        assert_eq!(cfg.effective_daily_cap(1000, 0), 300);
        assert_eq!(cfg.effective_daily_cap(1000, 10), 1000);
    }
}
