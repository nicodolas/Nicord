# Nicord — Architecture

## Decisions (locked, 16 Nov 2026)

- **Name:** Nicord. **Form:** desktop app, local-first, no login flow.
- **Target:** a channel in a Discord server (not Group DM).
- **NOT a Bot API bot.** Sends as the user's own Discord account
  (user-account / self-bot automation). Users paste their own Discord token
  themselves, like a dev token — that is their responsibility.
- **Single product, no tiers, no license keys.** 1 channel,
  300 messages/day, cap resets 00:00 UTC. (The license-key system was
  removed in v0.1.0; it is recoverable from git history.)
- **Stack:** Rust + eframe/egui. **No React, no Electron, no WebView.**
  Priority: fast, light, low RAM — not beauty.

## Stack

| Layer | Choice | Why |
|---|---|---|
| Language | Rust 2021, edition 2021 | Single binary, no runtime, low RAM |
| UI | eframe 0.31 + egui 0.31 (`glow`) | Native immediate-mode, tens of MB RAM, instant startup |
| Async | tokio (multi-thread) | Send engine runs in background threads |
| HTTP | reqwest 0.12 (`rustls-tls`) | Discord REST only; no gateway needed for scheduled sends |
| Storage | rusqlite (bundled) + JSON config | Scenarios + logs in SQLite; config as versioned JSON |
| Token crypto | AES-256-GCM, machine-tied key | Token never in plaintext, never logged, never uploaded |
| Time | chrono | Scheduling, sleep window, next-run display |

## Data flow (one send)

1. Scheduler tick (1s) finds a due scenario.
2. Job enters the per-channel queue; waits for the minimum interval + jitter.
3. Engine renders the template (`{{username}}`, `{{date}}`, …), picks a
   spin-text variant, sanitises content (blocks `@everyone` by default).
4. `POST /channels/{id}/messages` with the user token.
5. On 429: wait exactly `retry_after`, back off; on 401/403: stop at once.
6. Log: time, channel, scenario, status, error code — **never the token**.

## Modules

- `main.rs` — eframe window (1100×700), tracing init, store open.
- `app.rs` — egui screens. Onboarding shows the red ToS warning first and
  requires an explicit accept checkbox before token entry.
- `scenario.rs` — scenario JSON (schemaVersion 1), schedules, node kinds,
  validation, spin-text, message rotation.
- `scheduler.rs` — `Engine`: per-channel `last_sent`, daily cap, sleep
  window, retry (3×, exponential backoff), global-429 pause (1h),
  3-consecutive-errors pause (10 min), emergency stop.
- `discord.rs` — REST client: `send_message`, `get_me`, `list_guilds`,
  `list_channels`. `parse_retry_after` for 429 bodies.
- `crypto.rs` — `machine_key()` = SHA-256(`nicord-machine-key-v1` +
  `/etc/machine-id` + OS user); AES-256-GCM encrypt/decrypt; `mask_token`.
- `store.rs` — SQLite tables `scenarios` / `send_logs`; JSON `AppConfig`.

## License keys — removed in v0.1.0

The license-key system (`src/license.rs`, `src/bin/nicord-keygen.rs`)
was removed in v0.1.0 when Nicord became a single product with no tiers.
It is recoverable from git history if ever needed again.

## Scheduler — firing exactly once per scheduled time (fixed)

`scenario::due_time(schedule, last_run, created_at, now, timezone)`
returns the latest scheduled time in (anchor, now], where anchor is the
last run or the creation time. `last_run_at` is persisted to SQLite on
every successful send (and on intentional skips), so restarts can neither
duplicate nor lose a run. `Once` fires once, then the scenario is
disabled and persisted as disabled. Missed runs while off are not
bulk-sent. Cron is a real 5-field parser (`parse_cron` in scenario.rs),
not hourly. `Schedule::next_after` is display-only ("next run at …").

Node execution: the scheduler runs Send Message / Send Embed nodes
(first non-empty content in node order wins over the message list),
UtilityWait (delay before sending), ConditionTimeWindow (skip outside
the window), UtilityRandomPick (rotation) and UtilitySetVariable
(template vars). Any other node kind is rejected by `Scenario::validate`
with a clear error — never silently ignored.

Warm-up / caps: the daily cap is 300/day for everyone, resetting at
00:00 UTC (the UTC date is the bucket key everywhere, so changing the
machine timezone cannot grant extra messages), with warm-up (30%, i.e.
90/day, for the first 3 days since first run, from the `meta.first_seen`
row), additionally capped by a per-scenario limit when one is set (0 = no
per-scenario limit). "Sent today" is read from the persisted send log, so
it survives restarts. Rotation keeps the last message index per scenario
in engine memory. 1 distinct channel across all enabled scenarios,
enforced on enable, on channel change, on import, and in the engine loop. Template vars use the real
username / first server name fetched once at engine start (`get_me` +
`list_guilds`); "you"/"server" is only a logged fallback.

## Anti-ban (Spec §7) — summary

1. Spacing: min 10s/message/channel, hard floor 5s.
2. Jitter: +5–25s random per send.
3. Rate limits: respect 429/`retry_after`; cannot be disabled.
4. Content: rotation, spin-text, `@everyone` blocked by default.
5. Rhythm: night sleep window, daily cap, 3-day warm-up at 30%.

These reduce risk only. They do not make self-botting compliant.

## What is still a stub / next

- **Spike (Spec §11, week 1):** prove on a spare account — token login,
  list guilds/channels, send stably for 7 days. If this fails, re-evaluate
  the whole self-bot direction.
- Channel picker is wired (Load servers/channels in the Builder), with a
  hand-typed channel ID as a fallback.
- Cron is a real 5-field parser now (see above); the timezone of a cron
  schedule is the scenario timezone.
- **Tray / autostart: NOT implemented.** There is no system-tray icon;
  the app runs while its window is open (minimise keeps the engine
  running). The Settings autostart toggle is saved but the OS hook is
  not wired. The README says this plainly instead of claiming tray support.
- Release packaging: Windows installer (MSI via `cargo-packager` or WiX)
  is not yet set up.
- Export/import of scenarios (JSON file) and log filter/export (CSV)
  are in the Builder / Logs screens.
