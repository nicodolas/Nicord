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

## Discord-BLUE features (ported, all optional, all off by default)

Ported from bluemanhst/Discord-BLUE (Python/tkinter) into Rust. The
multi-account / multi-channel model was NOT ported — Nicord stays
1 account, 1 channel, 300 messages/day.

- **Smart Templates** (`src/spintax.rs`): Spintax `{a|b|c}` picks one
  option at random; `{...}` groups without `|` and `{{template}}` vars
  pass through untouched. Variation randomly inserts an emoji, doubles
  a space, or flips one word's case (each ~15%, so most messages stay
  natural). Applied after template vars are resolved, before sending.
  BLUE's abbreviation mutation was not ported (Vietnamese-slang
  specific, would mangle ordinary words).
- **Auto Typing** (`DiscordClient::send_typing`): POST
  `/channels/{id}/typing`, then wait a random 2–5s before sending.
  A typing failure never blocks the send; the wait aborts on
  Emergency Stop.
- **Auto Break** (scheduler): after a random 15–25 sends, rest for a
  random 10–30 minutes (interruptible). Counter and target reset after
  every break.
- **Auto Delete** (`DiscordClient::delete_message`): after a successful
  send, wait `delete_delay_ms` (0 = immediately, interruptible), then
  DELETE the message (204 = success). Failures are only logged; the
  send already succeeded. Deleted count shows on the Dashboard.
- **Token Validator** (Settings → "Kiểm tra token"): calls the existing
  `get_me_typed()` and shows username, user ID, email, email-verified,
  2FA, Nitro tier. 401 → "Token không hợp lệ hoặc đã hết hạn". The
  token itself is never shown.
- **Dashboard stats**: success rate % (sent / sent+failed, session),
  send rate (sent/minute since engine start), deleted count (session),
  and "Export thống kê (.json)" writing a stats JSON to the config dir
  (same pattern as scenario export).
- **System tray** (Windows only, `src/tray.rs`, `tray-icon` crate):
  closing the window hides Nicord to the tray instead of quitting;
  tray menu "Hiện Nicord" / "Thoát", left-click shows the window.
  The tray code is `#[cfg(target_os = "windows")]` — Linux builds
  never compile it (Linux is check-only; Windows is the release
  target). The tray icon reuses the embedded `assets/icon-256.b64`.
- **Real autostart** (`src/autostart.rs`): Windows Run key
  `HKCU\...\CurrentVersion\Run` (`winreg` crate, Windows only);
  Linux XDG desktop entry `~/.config/autostart/nicord.desktop`.
  On startup the toggle reflects the actual OS state.

Anti-ban settings are persisted in `config.json` (`AppConfig.antiban`)
and applied live to the running engine on Settings Save — no restart
needed.

## Discord commands (`ActionSendCommand`)

A scenario node that sends a Discord command instead of a plain
message. Two kinds, configured per node in the Builder (click the
node to edit it):

- **Prefix / text command** (`!rank`, `!daily arg1 arg2`): fields
  `prefix` (default `!`), `name`, `args` (ordered list). Composed
  text is `{prefix}{name}` + args joined by spaces (empty args
  skipped), then sent as a normal message through the existing
  pipeline (daily cap, slot + jitter, typing, retry, auto-delete,
  auto-break). Template vars (`{{username}}`…) are resolved; Smart
  Templates variation is NOT applied — it would corrupt a command.
- **Slash command** (`/daily`): fields `application_id` (the bot's
  application ID, required), `name`, `command_id` (optional),
  `version` (optional), `options` (list of name/type/value; types:
  3 string, 4 integer, 5 boolean, 6 user, 7 channel, 8 role,
  10 number). Sent via `POST /interactions` (type 2) with
  `guild_id` (resolved from the channel via `GET /channels/{id}`;
  omitted for DMs), `session_id`, and a snowflake nonce.

Slash command ID / version resolution: config first, else the
channel's application-commands search (`GET
/channels/{id}/application-commands/search`) by name (preferring the
configured application). If that fails, the send fails with a clear
log error telling the user to paste the Command ID manually — manual
entry is the reliable path.

Gateway (`src/gateway.rs`, `tokio-tungstenite` + rustls, same TLS
stack as reqwest — no OpenSSL): Discord requires a valid gateway
`session_id` for slash invocations, so Nicord runs a minimal gateway
client — Identify with intents 0, HELLO / heartbeat / READY only, no
event handling. It connects LAZILY (only when a slash command fires)
as a background task owned by the engine, reconnects with backoff,
and stops on Emergency Stop / engine shutdown. If no session appears
within 20s, the send falls back to a one-shot `connect_once`
(connect → READY → close). A failed gateway never breaks message
sending: slash fails with a clear message while prefix commands keep
working. REST-only operation is unchanged when no slash commands
exist.

Slash invocations count toward the 300/day cap and `sent_today`
(logged with status `sent`, detail `slash /name`) and run the Auto
Break layer. One scheduled firing runs exactly the FIRST command
node in a scenario (message nodes are ignored when a command node
exists) — running several commands per firing would duplicate earlier
commands when a later one fails and the run is retried.

What the user must provide for slash: the bot's Application ID
(from the bot's Discord developer page / the app's info); the
Command ID only if auto-resolve fails. The gateway token is the same
stored token (Identify reuses it in-memory; never logged).

## Scheduler — fires exactly once per scheduled time

Each scenario records `last_run_at` (the last scheduled time it fired) in
SQLite after every successful send. The engine only sends when a scheduled
time falls in (last run, now] — so EveryHours/EveryMinutes fire on rhythm,
Once fires exactly once then disables itself, and Daily/Weekly never send
duplicates. A machine that was off does not bulk-send missed runs. Cron is
a real 5-field parser (minute hour day month weekday), not hourly.

Node execution: the scheduler runs Send Message / Send Embed nodes (first
non-empty content in node order wins over the message list), Send Command
nodes (first command node wins over message nodes), UtilityWait
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

## Font and theme (v0.1.0 redesign)

- **Font: Inter** (SIL OFL 1.1, by Rasmus Andersson — the open font
  deliberately designed to match Apple San Francisco's look and metrics,
  with full Vietnamese support). Apple's own SF Pro is proprietary and
  licensed for Apple platforms only, so it cannot legally be bundled into
  a Windows app; Inter is the correct legal equivalent. Regular and
  SemiBold, subsetted with `pyftsubset` (fontTools) to the glyphs the
  UI needs: Latin + full Vietnamese diacritics (U+1EA0–U+1EF9,
  precomposed) + common punctuation (339 glyphs; the only missing
  codepoint from the round-1 set is U+00AD soft hyphen, which Inter
  does not ship and is invisible anyway). Coverage verified with
  fontTools: zero missing glyphs for Vietnamese; the subset has **no
  arrows, checkmarks or warning glyphs**, so UI text avoids them (use
  ASCII `->` or drawn shapes instead). Source: the official rsms/inter
  GitHub release (Inter 4.1, `extras/ttf/` static instances).
- The subsets are embedded as **base64 text** — `assets/font-inter-regular.b64`
  (49,052 chars) and `assets/font-inter-semibold.b64`
  (50,192 chars) — decoded in `src/main.rs` and installed as egui's
  default proportional font (SemiBold for headings/buttons). No binary
  `.ttf` is committed, because the GitHub write route is text-only
  (same trick as the icon). Binary cost: ~+74 KB total (smaller than
  round 1's ~+92 KB).
- **Theme (`src/theme.rs`):** Discord-dark palette — background
  `#1E1F22`, sidebar `#161719`, cards `#2B2D31`, borders `#3F4147`,
  blurple accent `#5865F2` (light accent `#7B61FF`), danger `#ED4245`,
  success `#57F287`, warning `#FAA81A`, text `#F2F3F5` / secondary
  `#B5BAC1` / muted `#80848E`. Sizes: heading 24, section 17, body and
  buttons 15, small/monospace 13, big numbers 28 (all SemiBold where
  emphasised). Layout: 208 px left sidebar (brand, nav, "Today" status
  card, Emergency Stop at the bottom on every screen) + main column
  with a title, Vietnamese subtitle and the limit line
  "1 channel • 300 messages/day • resets 00:00 UTC". No extra images,
  no idle animations.

## App icon

The icon source is `assets/icon.svg` (white speech bubble with a clock on
a blurple gradient). PNG sizes (64/128/256/512) and `icon.ico` for Windows
packaging are generated from it and live in `assets/`. The window icon is
embedded in the binary as base64 text (`assets/icon-256.b64`, decoded in
`src/main.rs`), so a checkout builds without extra steps.

## Auto-update (in-app)

`src/updater.rs`. On startup (background thread, never blocks the UI)
the app GETs the public GitHub Releases API
(`api.github.com/repos/nicodolas/Nicord/releases/latest`,
unauthenticated, User-Agent only — no user data sent) and compares
the latest tag with `env!("CARGO_PKG_VERSION")`. A newer release
opens a dialog: "Cập nhật ngay" / "Để sau". Nothing installs
silently. A 404 (no releases yet) or any API/network error is
treated as "no update" and logged quietly — no error dialog.

- Asset: `Nicord-v{tag}-windows-x64.zip` on Windows
  (`-linux-x64.zip` on Linux), exact name first, then any `.zip`
  for the platform; a wrong-platform file is never picked.
- Download streams with reqwest (`stream` feature) into a temp
  staging dir with a progress bar; extraction uses the `zip` crate
  and rejects unsafe paths (`enclosed_name`, Zip Slip guard).
- Install (Windows): a running exe cannot overwrite itself, so a
  helper `.bat` (`.sh` on Linux) waits for Nicord's PID to exit,
  backs the old exe up to `Nicord.exe.bak`, copies the staged files
  over the install folder, deletes the staging dir and relaunches
  Nicord. The engine is stopped cleanly (`stop_all`) before the app
  quits. `config.json` / `nicord.db` live in the OS config dir
  (`%APPDATA%\nicord` / `~/.config/nicord`), never next to the exe,
  so an update cannot touch user data.
- Settings has a "Cập nhật" card: "Kiểm tra cập nhật" button +
  auto-check toggle (default ON, persisted in `config.json` as
  `auto_check_updates`).

## Known limitations

- **System tray is Windows-only.** On Linux the tray checkbox is
  hidden and closing the window still quits. Autostart works on both
  Windows (Run key) and Linux (XDG desktop entry).
- **Spike not run yet:** 7 days of stable sending on a spare account has
  not been proven — run it before releasing.
- Sending is REST-only (no gateway). The channel picker is wired (Load
  servers/channels), with a hand-typed channel ID as fallback.
- Cron schedules use the scenario timezone; double-check schedules that
  cross midnight.
