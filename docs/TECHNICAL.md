# Nicord — Technical details

This document is for developers. End users: start from the main
[README](../README.md) instead.

## Stack

Nicord is written in **Rust + eframe/egui** (native, immediate-mode UI),
**no React, no Electron, no WebView**. One single binary, instant startup,
tens of MB of RAM. The interface is simple on purpose — speed over beauty.

| Layer | Choice | Why |
|---|---|---|
| Language | Rust 2021 | Single binary, no runtime, low RAM |
| UI | eframe 0.31 + egui 0.31 (`glow`) | Native immediate-mode |
| Async | tokio (multi-thread) | Send engine runs in background threads |
| HTTP | reqwest 0.12 (`rustls-tls`) | Discord REST only; no gateway needed for scheduled sends |
| Storage | rusqlite (bundled) + JSON config | Scenarios + logs in SQLite; config as versioned JSON |
| Token crypto | AES-256-GCM, machine-tied key | Token never in plaintext, never logged, never uploaded |
| Time | chrono + chrono-tz | Scheduling, sleep window, per-scenario timezones |

## Build & run

```bash
# Requires: Rust stable (via rustup)
cargo build --release
./target/release/nicord
```

On Windows: run `nicord.exe` after building. The app stores its config in
the OS config directory (`%APPDATA%/nicord` on Windows,
`~/.config/nicord` on Linux).

On Linux, install the system libraries egui/winit needs first:

```bash
sudo apt-get install -y libxcb1-dev libxkbcommon-dev libxkbcommon-x11-0 libxcb-xkb1 libgl1-mesa-dev libx11-dev
```

Development checks:

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test
cargo check
```

## Project structure

```
src/
  main.rs       # entry point, eframe window
  app.rs        # egui UI: Onboarding, Dashboard, Builder, Logs, Settings
  scenario.rs   # scenario model, schedule + cron parser, spin-text, validation
  scheduler.rs  # send engine, per-channel queue, retry, 429 handling
  discord.rs    # Discord REST client (user token)
  crypto.rs     # AES-256-GCM token encryption, machine-tied key
  store.rs      # SQLite (scenarios/logs/meta) + JSON config
docs/ARCHITECTURE.md
examples/scenario-example.json
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full data flow and a
per-module description.

## License keys — removed in v0.1.0

There is no license-key system anymore: Nicord is a single product for
everyone (1 channel, 300 messages/day, cap resets 00:00 UTC). The old
Ed25519 key verification (`src/license.rs`) and the offline keygen tool
(`src/bin/nicord-keygen.rs`) were removed in v0.1.0. They are recoverable
from git history if ever needed again — users must not know about
internal/dev mechanisms; they only receive the finished product.

## Anti-ban — 5 layers (reduces risk, does not remove it)

1. Spacing: minimum 10 seconds/message/channel (hard floor 5s, cannot be
   set lower).
2. Jitter: +5–25 random seconds per send.
3. Rate limits: respects 429 and `retry_after`; there is no option to
   disable this.
4. Content: rotation, spin-text, `@everyone`/`@here` blocked by default.
5. Rhythm: night sleep window, daily cap, 3-day warm-up at 30% of the cap.

There is no captcha solving, detection evasion, or bulk proxy rotation.

## Scheduler — fires exactly once per scheduled time

Each scenario records `last_run_at` (the last scheduled time it fired) in
SQLite after every successful send. The engine only sends when a scheduled
time falls in (last run, now] — so EveryHours/EveryMinutes fire on rhythm,
Once fires exactly once then disables itself, and Daily/Weekly never send
duplicates. A machine that was off does not bulk-send missed runs. Cron is
a real 5-field parser (minute hour day month weekday), not hourly.

Node execution: the scheduler runs Send Message / Send Embed nodes (first
non-empty content in node order wins over the message list), UtilityWait
(delay before sending), ConditionTimeWindow (skip outside the window),
UtilityRandomPick (rotation) and UtilitySetVariable (template vars). Any
other node kind is rejected by `Scenario::validate` with a clear error —
never silently ignored.

Warm-up / caps: the daily cap is 300/day for everyone, resetting at
00:00 UTC. The bucket is the UTC date everywhere — engine and persisted
send log — so changing the machine timezone cannot grant extra messages.
Warm-up is 30% (90/day) for the first 3 days since first run (from the
`meta.first_seen` row), additionally capped by a per-scenario limit when
one is set (0 = no per-scenario limit). "Sent today" is read from the
persisted send log, so it survives restarts. 1 distinct channel across
all enabled scenarios, enforced on enable, on channel change, on
import, and in the engine loop. Template vars (`{{username}}`,
`{{server}}`, `{{date}}`, `{{time}}`) use the real username / first
server name fetched once at engine start (`get_me` + `list_guilds`).

## Token security

- The Discord token is encrypted with AES-256-GCM before it is stored.
  The encryption key is derived from the machine ID + OS user, so a config
  file copied to another machine cannot be decrypted there.
- Only the last 4 characters of the token are stored for display
  (`token_last4`); the full token is never written to disk or logs in
  plaintext.
- Logs contain message IDs and status only — never the token.

## App icon

The icon source is `assets/icon.svg` (white speech bubble with a clock on
a blurple gradient). PNG sizes (64/128/256/512) and `icon.ico` for Windows
packaging are generated from it and live in `assets/`. The window icon is
embedded in the binary as base64 text (`assets/icon-256.b64`, decoded in
`src/main.rs`), so a checkout builds without extra steps.

## Known limitations

- **No system-tray icon / real autostart yet.** The autostart toggle in
  Settings is saved but not wired to the OS. Keep the window open
  (minimised is fine) for the engine to run.
- **Spike not run yet:** 7 days of stable sending on a spare account has
  not been proven — run it before releasing.
- Sending is REST-only (no gateway). The channel picker is wired (Load
  servers/channels), with a hand-typed channel ID as fallback.
- Cron schedules use the scenario timezone; double-check schedules that
  cross midnight.
