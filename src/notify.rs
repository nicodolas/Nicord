//! Notification sounds (Q11): a short beep when a send fails, when a
//! destination auto-pauses, and (optionally) when a send succeeds.
//!
//! Best-effort and never blocking: playback runs on a detached
//! thread, failures are silent, and sounds are throttled so a
//! failing engine cannot beep every second. No audio files and no
//! new dependencies: the beep is a sine-wave WAV generated in code
//! and played with the platform's built-in player (PowerShell
//! `SoundPlayer` on Windows, `paplay`/`aplay` on Linux, `afplay` on
//! macOS). When no player exists, nothing happens — silently.
//!
//! Settings live in `AppConfig` (`sound_enabled`, `sound_on_success`)
//! and are mirrored into this module's process-wide cell by the UI,
//! because the engine runs on another thread and never sees the
//! config directly.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Minimum gap between two sounds, of any kind.
const MIN_GAP: Duration = Duration::from_secs(10);

/// What happened, for sound selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundEvent {
    SendSuccess,
    SendFailure,
    DestinationPaused,
}

impl SoundEvent {
    /// (frequency Hz, duration ms). Failures are low and longer;
    /// successes are a short high blip; pauses sit in between.
    fn tone(self) -> (u32, u32) {
        match self {
            Self::SendSuccess => (880, 120),
            Self::SendFailure => (330, 250),
            Self::DestinationPaused => (520, 180),
        }
    }
}

struct State {
    enabled: bool,
    on_success: bool,
    last_played: Option<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State {
    enabled: true,
    on_success: false,
    last_played: None,
});

/// Mirror the config's sound settings into the process-wide cell.
/// Called by the UI (startup + every frame, cheap) so engine threads
/// always see the current choice.
pub fn sync_settings(enabled: bool, on_success: bool) {
    if let Ok(mut s) = STATE.lock() {
        s.enabled = enabled;
        s.on_success = on_success;
    }
}

/// Pure gating rule, tested: sounds off → nothing; successes only
/// when the user asked for them; failures/pauses whenever on.
pub fn wanted(enabled: bool, on_success: bool, event: SoundEvent) -> bool {
    if !enabled {
        return false;
    }
    match event {
        SoundEvent::SendSuccess => on_success,
        SoundEvent::SendFailure | SoundEvent::DestinationPaused => true,
    }
}

/// Pure throttle rule, tested: play when never played, or when the
/// gap since the last sound has elapsed.
fn due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|l| now.duration_since(l) >= MIN_GAP)
}

/// Play the sound for `event` if settings allow and the throttle
/// has elapsed. Returns immediately; playback is detached.
pub fn play(event: SoundEvent) {
    let go = STATE.lock().map(|mut s| {
        if !wanted(s.enabled, s.on_success, event) || !due(s.last_played, Instant::now()) {
            return false;
        }
        s.last_played = Some(Instant::now());
        true
    });
    if go.unwrap_or(false) {
        spawn_beep(event.tone());
    }
}

/// Play a beep right now, ignoring settings and throttle. Used by
/// the Settings "Thử âm thanh" button — the user asked explicitly.
pub fn play_preview() {
    spawn_beep(SoundEvent::SendSuccess.tone());
}

fn spawn_beep((freq, ms): (u32, u32)) {
    std::thread::spawn(move || {
        let wav = beep_wav(freq, ms);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "nicord-beep-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        if std::fs::write(&path, &wav).is_err() {
            return;
        }
        play_file(&path);
        let _ = std::fs::remove_file(&path);
    });
}

/// Run the platform player and wait for it (we are already on a
/// detached thread). Every failure is swallowed: a missing player
/// must never surface as an error to the user.
fn play_file(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        let script = format!(
            "(New-Object Media.SoundPlayer '{}').PlaySync()",
            path.display()
        );
        let _ = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("afplay").arg(path).output();
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        for player in ["paplay", "aplay"] {
            if std::process::Command::new(player)
                .arg(path)
                .output()
                .is_ok()
            {
                break;
            }
        }
    }
}

/// Generate a mono 16-bit PCM WAV (8 kHz) with a sine tone.
/// Pure and tested: header magic, sizes, and sample count.
fn beep_wav(freq_hz: u32, duration_ms: u32) -> Vec<u8> {
    const SAMPLE_RATE: u32 = 8000;
    let samples = (SAMPLE_RATE * duration_ms / 1000).max(1);
    let data_bytes = samples * 2;
    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());
    for n in 0..samples {
        let t = n as f32 / SAMPLE_RATE as f32;
        // Gentle envelope: fade in/out over 10ms to avoid clicks.
        let fade = (n.min(80) as f32 / 80.0).min((samples - n).min(80) as f32 / 80.0);
        let v = (2.0 * std::f32::consts::PI * freq_hz as f32 * t).sin() * fade;
        out.extend_from_slice(&((v * i16::MAX as f32) as i16).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wanted_matrix() {
        assert!(!wanted(false, true, SoundEvent::SendFailure));
        assert!(!wanted(false, false, SoundEvent::DestinationPaused));
        assert!(wanted(true, false, SoundEvent::SendFailure));
        assert!(wanted(true, false, SoundEvent::DestinationPaused));
        assert!(!wanted(true, false, SoundEvent::SendSuccess));
        assert!(wanted(true, true, SoundEvent::SendSuccess));
    }

    #[test]
    fn throttle_due() {
        let now = Instant::now();
        assert!(due(None, now));
        assert!(!due(Some(now), now));
        assert!(due(Some(now - MIN_GAP - Duration::from_millis(1)), now));
        assert!(!due(Some(now - MIN_GAP + Duration::from_millis(1)), now));
    }

    #[test]
    fn wav_header_and_length() {
        let wav = beep_wav(440, 100);
        // 8000 Hz * 100ms = 800 samples * 2 bytes + 44 header bytes.
        assert_eq!(wav.len(), 44 + 1600);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        let data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap());
        assert_eq!(data_len, 1600);
        // Samples are not all silence.
        assert!(wav[44..].iter().any(|&b| b != 0));
    }
}
