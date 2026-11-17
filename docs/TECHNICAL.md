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
| HTTP | reqwest 0.12 (`rustls-tls`) | Discord REST for scheduled sends; a gateway connection (`src/gateway.rs`) is used only to obtain a session ID for slash commands |
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
  discord.rs    # Discord REST client (user token) + shared error mapping
  webhook.rs    # Webhook mode: URL parsing, WebhookClient, MessageSender
  preview.rs    # message preview: markdown parsing, shortcodes, 2000-char limit
  destination.rs # Destination model (1 webhook = 1 channel, own settings)
  gateway.rs    # gateway client (slash session id, user-token mode only)
  spintax.rs    # spin-text + variation (user-token mode only)
  theme.rs      # dark theme, embedded fonts
  tray.rs       # Windows-only system tray
  autostart.rs  # OS autostart (Windows Run / Linux XDG)
  updater.rs    # in-app auto-update from GitHub Releases
  crypto.rs     # AES-256-GCM token/webhook encryption, machine-tied key
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

- Secrets (account token, webhook URLs) are stored through
  `src/secret_store.rs`, which prefers the OS keyring — Windows
  Credential Manager, macOS Keychain, Linux Secret Service
  (via the `keyring` crate) — and falls back to AES-256-GCM
  blobs encrypted with a machine-tied key when the keyring is
  unavailable or a write fails. A stored value is either empty,
  a `kr:<entry>` keyring reference (no secret material in the
  file), or a machine blob. The `kr:` prefix can never collide
  with a blob: base64 output contains no `:`.
- The machine key is derived from the machine ID + OS user, so a
  config file copied to another machine cannot be decrypted
  there. It is obfuscation against casual copying, not a
  keyring — the keyring is the real protection where available.
- At startup, existing blobs are migrated into the keyring when
  it is available, one entry per secret (`account-token`,
  `legacy-webhook`, `destination:<id>`). Migration verifies the
  keyring round-trip before replacing the blob; anything that
  cannot migrate stays a blob and keeps working. Deleting a
  destination removes its keyring entry too.
- A `kr:` reference whose entry cannot be read fails closed with
  a clear error ("cannot read secret from the OS keyring") and is
  treated as "no usable credential"; re-entering the secret always
  works (the write path falls back to a blob). No migration ever
  deletes a blob before its round-trip is verified, so there is
  no lockout path.
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
  staging dir with a progress bar; at most 100 MiB are accepted
  (`MAX_DOWNLOAD_BYTES` — a declared Content-Length above the cap
  is rejected before any byte is written, a body growing past it
  aborts mid-stream), and a failed download removes its partial
  file. Extraction uses the `zip` crate and rejects unsafe paths
  (`enclosed_name`, Zip Slip guard), with file-count/size limits
  (`MAX_ZIP_*`).
- Before anything is extracted or executed, the zip is verified
  against the release's `SHA256SUMS.txt` asset (SHA-256,
  `verify_download`): a missing checksum file, a missing entry, or
  a digest mismatch fails the update closed. The checksum file
  itself is capped at 64 KiB.
- The helper script (`.bat` on Windows, `.sh` on Linux) quotes
  every baked-in path: the `.sh` script single-quotes values
  (`sh_quote`, so `$`/backticks/newlines are literal), and the
  `.bat` script refuses install paths containing `"`, `%`, or
  line breaks (`validate_bat_value`, cmd.exe has no reliable
  escaping for them).
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

## CI & releases

- CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test`, and
  `cargo check --release` on **Linux, Windows, and macOS** for
  every push to `main`. The matrix exists because Windows-only
  code (`src/tray.rs`, `winreg`) is invisible to a Linux build;
  before the matrix, no automation ever compiled it. The first
  matrix run (run #56, for the Packages A–D tree) caught exactly
  this class of bug: `sh_quote` in `src/updater.rs` is only used
  by the `.sh` script branch, so Windows clippy failed it as
  dead code under `-D warnings`. It is now gated
  `#[cfg(any(not(target_os = "windows"), test))]`, mirroring
  `validate_bat_value`'s gate on the Windows side.
- Releases are cut from **tags on `main`** (no release branch):
  bump `version` in `Cargo.toml`, commit on `main`, then
  `git tag vX.Y.Z && git push origin vX.Y.Z`. The release workflow
  builds the Windows zip (`Nicord-vX.Y.Z-windows-x64.zip` — the
  name must match `expected_asset_name` in `src/updater.rs`) and
  publishes a `SHA256SUMS.txt` asset (`"<hash>  <filename>"`)
  that the in-app updater verifies before extracting.
- No release has been cut yet, so the release workflow has never
  run end to end. Cutting the first tag is an explicit decision,
  not a routine step.
- Linux/macOS release artifacts do not exist yet (the updater
  already looks for a `-linux-x64.zip` on Linux, so the flow works
  when one is published). Windows installer (MSI) is not set up.

## Webhook mode (official, recommended)

- Execute: `POST https://discord.com/api/webhooks/{id}/{token}?wait=true`
  with `{content, username?}`. `?wait=true` returns the created message
  (200 + JSON); without it Discord answers 204. No Authorization header:
  the URL token IS the credential.
- Inspect: `GET https://discord.com/api/webhooks/{id}/{token}` returns
  the webhook object (name, channel_id, guild_id). Used by "Kiểm tra
  webhook" and by the engine to seed `{{username}}` (`{{server}}` is
  empty in webhook mode).
- Limits: content ≤ 2000 chars (checked client-side). 429: respect
  `retry_after` (incl. `global`), same policy as user-token mode.
  404: the webhook is gone — stop at once, never retry
  (`DiscordError::NotFound`, shared error mapping in `discord.rs`).
- Parsing (`src/webhook.rs`, no `url` dependency): HTTPS only, host
  exactly `discord.com` / `discordapp.com`, path exactly
  `/api/webhooks/{numeric id}/{token}`. Errors never echo the input.
  `WebhookUrl` has a manual Debug impl and `masked_url()`; the full
  URL is never logged or displayed.
- Storage: `AppConfig.send_mode` (default `UserToken` for old configs),
  `encrypted_webhook_url` (same AES-GCM as the token),
  `webhook_id` / `webhook_name` (display only), `webhook_username`
  (optional override). Both credentials coexist; switching modes never
  deletes the other one.
- Engine: `MessageSender` (User / Webhook) in `webhook.rs`. Webhook
  mode skips spin-text/variation, typing, and auto-delete; command
  nodes are rejected by `Scenario::validate_for_webhook` and
  defensively in `send_once`. Channel rules are skipped (the URL is
  channel-bound). Daily cap, sleep window, and retry policy are shared.
- Validation: `Scenario::validate()` (user mode) vs
  `validate_for_webhook()` (no channel required, no commands). Enable
  and Import use the mode-aware check.

Manual webhook checklist (run on your own test server/webhook, not in CI):

1. Create a webhook in a test channel, paste the URL, "Kiểm tra
   webhook" shows the right name + channel.
2. Enable a message scenario, Start engine, verify the message arrives
   via the webhook with the webhook's name.
3. Delete/regenerate the webhook, verify the next send fails with a
   clear "webhook không tồn tại" error and does not loop.
4. Verify a command scenario is rejected on Enable in webhook mode,
   and that switching back to account mode restores command behavior.

## Quality of life (P0)

Implemented in single-destination form (the current app has one
webhook / one channel). Per-destination variants arrive with the
multi-destination design; destination clone/enable is deliberately
deferred to that work.

- **Preview + counter (Builder).** `src/preview.rs` renders
  Discord markdown (`**`, `*`, `_`, `~~`, `__`, `` ` ``) and expands
  `:shortcode:` emoji for display only — the sent text keeps exactly
  what was typed. Live `N/2000` counter; Add is disabled and Enable
  is blocked when any message exceeds 2000 characters
  (`Scenario::validate` checks raw length, both modes).
- **Destination health.** `AppConfig.webhook_health` +
  `webhook_last_checked_at` persist the last check result (Ok /
  NotFound / Forbidden / NetworkError / Unknown). A 404 during a
  send pauses the webhook in the engine (`Engine::webhook_paused`);
  a successful re-check from Settings clears the pause. Rate limits
  never overwrite health. Dashboard shows a destination card with
  health, last check, last send, and persisted failures today.
- **Validated import.** `analyze_import` (scenario.rs) scans for
  secret-looking content, reports JSON errors with line/column,
  warns about missing fields (filled with defaults), and rejects
  invalid scenarios with the reason. Warnings are shown in the
  Builder editor. Missing `schemaVersion`/`id` now default instead
  of failing deserialization (importer assigns a fresh UUID).
- **Logs.** Failures are now persisted (`status = 'failed'`,
  detail truncated to 1000 chars) so Logs can filter by status.
  `Store::query_logs` combines keyword (LIKE-escaped) + status +
  scenario filters. CSV export uses RFC 4180 quoting and scrubs
  webhook URLs (`scrub_secrets`).
- **Clone + friendly names.** Builder "Clone" duplicates a scenario
  (fresh id, disabled, no run history). `webhook_friendly_name`
  (Settings) overrides the webhook's own name everywhere a
  destination is displayed (`AppConfig::webhook_display_name`).

## Destinations (multi-channel, Webhook mode only)

One destination = one official webhook = one channel. Each
destination owns its settings: friendly name, encrypted URL, display
metadata from Discord, enable flag, username/avatar overrides,
optional thread id (stored, not yet used for sending), daily cap,
minimum interval, quiet window, and default messages. Failure state
is persisted (`Ok` / `Paused404` / `Paused401` / `PausedErrors`), so
a paused destination stays paused across restarts until resumed.

- **Storage:** `destinations` table in SQLite (JSON per row, URL
  encrypted before writing). `AppConfig` adds `global_daily_cap`
  (default 300), `max_destinations` (default 10, hard ceiling 20),
  `consecutive_error_limit` (default 5), and the
  `destinations_migrated` flag. Legacy single-webhook fields are
  kept (not deleted) and dual-written into the destinations table
  when a URL is saved from onboarding/Settings.
- **Migration:** on startup, `ensure_destinations_migrated` creates
  one destination from the legacy URL (encrypted bytes copied
  as-is, never decrypted) and assigns every scenario's
  `destination_id` to it when the mode is Webhook. Idempotent and
  lossless; existing destinations are never duplicated.
- **Engine:** one loop (guard kept). Per scenario, the sender is
  resolved from the scenario's destination. Pre-send checks:
  enabled, not paused, has a URL, quiet window (fail-closed),
  per-destination 429 cooldown, per-destination daily cap (DB
  count by channel id), global daily cap. Short waits return
  `defer:` errors (run NOT consumed, no failure logged); caps,
  quiet windows, and pauses return `skip:` errors (run consumed,
  no failure). A 429 records a per-destination cooldown (or a
  global pause when `global`), never shortened. 404/401 persist
  a pause for that destination only. 5 consecutive errors
  (configurable) pause only that destination. A fixed 1-second
  stagger separates webhook sends in one tick. Content fallback:
  node message → scenario messages → destination default messages.
- **UI:** Destinations screen (list + editor + delete with scenario
  reassignment + clone starting without a URL). Builder picks one
  destination per scenario (enabled destinations only). Dashboard
  shows a card per destination plus a global-pause banner. Logs
  filter by destination (channel id). `thread_id` is validated
  (digits only) but not used for sending until verified.
- **User-token mode is unchanged:** single-channel rule, no
  destinations, no shared multi-channel send path.

## Quality of life (P1)

- **Templates + fallback (Q6).** Shared template library
  (`templates` table: name + content, no secrets). Content
  fallback chain in the engine: node message → scenario
  messages → destination default messages. A scenario can
  explicitly use the destination's messages instead of its own
  (`use_destination_messages`). The Builder shows the content
  source and, when unambiguous, the exact next message; rotation
  is random (never repeats the last pick), so with several
  candidates the UI says so instead of predicting.
- **Schedules (Q7).** The Builder now edits schedules directly:
  Daily/Weekly/Every N minutes/Every N hours/Cron, plus presets
  (30 min, hourly, 8:00, 20:00, weekdays 9:00, weekend 10:00)
  and a timezone field with common IANA names. Time Window
  nodes support overnight windows and fail closed: a present
  but unparsable window is a validation error, never a silent
  whole-day default. Unknown timezones are rejected (the
  previous silent UTC fallback is gone). Dashboard destination
  cards show each destination's next run with its timezone.
- **Embeds (Q8).** One rich embed per scenario, edited in the
  Builder (title, description, author, color, up to 25 fields,
  footer) with live limit checks and a preview. Limits follow
  Discord's official docs (verified 2026-11-17): title 256,
  description 4096, field name 256 / value 1024, footer 2048,
  author 256, total ≤ 6000. The color range 0..=0xFFFFFF is an
  app decision (docs say only "integer"). Embeds send via both
  transports; empty content is allowed with an embed.
- **Language (Q9).** Vietnamese (default) + English, switched
  instantly from Settings. Coverage is the core UI (navigation,
  titles, section titles, common buttons); body copy and status
  messages stay Vietnamese until their keys are added
  (incremental — see `src/i18n.rs`).
- **Emoji picker (Q10).** Offline grid (same characters as the
  preview shortcode table) inserts the real Unicode character,
  plus custom emoji markup paste (`<:name:id>`). Custom emoji
  only render in Discord when the guild has them.
- **Thread sending.** `thread_id` is now sent as a query param
  on webhook execute (verified: the thread must belong to the
  webhook's channel; Discord auto-unarchives it). Locked-thread
  behaviour is unverified — test on a real thread first.
- **Verified Discord facts (2026-11-17, official docs).** Max
  15 webhooks/channel, 1000/guild. Rate limits are per-webhook
  buckets ("may calculate limits independently"); `retry_after`
  is float seconds (Nicord ceils it, never shortens it). A 404
  means "do not attempt to use it again" — hence the persisted
  destination pause. Username override length is undocumented;
  Nicord limits it to 80 chars as a precaution (app decision).

## Config export / import (cross-machine moves)

Settings → "Sao lưu & khôi phục cấu hình" exports the whole
setup — config, destinations, scenarios, templates — as one
versioned JSON file (`format: "nicord-export"`, `version: 1`).
Send logs are history, not config, and are never exported.

- **Portable** (default): secrets included, encrypted under a
  passphrase — PBKDF2-HMAC-SHA256, 600,000 iterations, AES-256-GCM
  (`src/crypto.rs`, hand-written HMAC/PBKDF2, verified against RFC
  4231 / RFC 6070 vectors). Use this to move to another machine.
  The file is a password vault: a weak passphrase means weak
  protection, and a forgotten passphrase means the file cannot
  be opened.
- **Safe**: secrets omitted. Destinations arrive without URLs and
  disabled; the user re-enters credentials on the new machine.
  Safe for sharing a setup.
- **Local**: stored secret values copied as-is (keyring
  references or machine blobs). Restorable only on the same
  machine. Used for the automatic backup taken before every
  import.

Why this exists: secrets are stored machine-locally (OS-keyring
references or blobs encrypted with a machine-tied key), so
copying `config.json` / the database to another machine leaves
them unusable there. Portable export resolves and re-encrypts
them under the passphrase instead. Exporting a secret that cannot
be resolved locally aborts the export rather than writing a
broken file.

Import **replaces** the current setup (never merges). It requires
the engine to be stopped + an explicit confirmation, writes a
local backup first (aborting if the backup cannot be written),
validates every entry, and skips invalid ones with warnings —
never silently. A wrong passphrase fails before anything changes.
More than 20 destinations in a file is rejected (hard ceiling).

Persistence rule for future features: if a feature's state has
over a 20% chance of needing to survive a restart or a move,
persist it — and include it in export/import. Full rules:
`docs/CONFIG-RULES.md`.

## Quality of life (P2)

- **Notification sounds (Q11, `src/notify.rs`).** A short beep on
  send failure, destination auto-pause, and (opt-in) send success.
  No audio files or new dependencies: a sine-wave WAV is generated
  in code and played with the platform's built-in player
  (PowerShell `SoundPlayer` on Windows, `paplay`/`aplay` on Linux,
  `afplay` on macOS). Playback runs on a detached thread, failures
  are silent, and sounds are throttled to one per 10 s so a failing
  engine cannot beep every second. Defaults: on for failures/pauses,
  off for successes. Settings live in `AppConfig` and are mirrored
  into the module for engine threads. Unverified on real
  Windows/macOS machines (no GUI run here).
- **Themes (Q12, `src/theme.rs`).** Dark (original) and Light
  palettes behind one `Palette` struct; `ThemeChoice` is stored as
  a code string in config like the language, applied instantly
  from Settings and at startup. The current palette is process-wide
  (`palette()`), read by every helper and screen — the old
  per-color constants are gone, so a missed call site fails to
  compile instead of staying dark.
- **Manual delete (Q13).** One sent message at a time, from the
  Logs screen, webhook mode only, with an inline confirmation.
  Nothing automatic calls it: no engine, schedule, or bulk path.
  The send history stores the Discord `message_id` and the
  `destination_id` per row (new `send_logs` columns, added to older
  databases by a guarded `ALTER TABLE` migration; old rows read
  back as ""). Delete goes to the exact webhook that sent the
  message (destination must still exist with a URL; the id must be
  digits-only before it enters the URL path). Every deletion writes
  an audit row (status `deleted`); a 404 is reported honestly as
  "already deleted" and audited as such. Verified Discord facts
  used: message delete returns 204 and supports `thread_id`.
- **Startup health report (Q15, `src/health.rs`).** Offline checks
  over the loaded config, destinations, and scenarios, shown once
  on the Dashboard until dismissed: no webhook URL, all
  destinations disabled, paused destinations, zero global cap,
  destination count over the limit, enabled scenarios that fail
  validation, no token in user-token mode, no scenarios yet, and a
  backup reminder when no export was ever made (tracked via
  `AppConfig.last_export_at`, set on every successful export). No
  network calls; each issue points at the screen where it is
  fixed. Pure functions, unit-tested.
- **Bug found by the health checks: `AppConfig::default()`.**
  `AppConfig` derived `Default`, so a fresh or corrupt config
  silently got `global_daily_cap = 0`, `max_destinations = 0`,
  `consecutive_error_limit = 0`, and update checks off — while the
  serde defaults for missing fields said 300 / 10 / 5 / on. The
  struct now has a manual `Default` using the same default
  functions, guarded by `default_matches_serde_defaults`, which
  compares `Default` against deserializing `{}`.

## Daily-use improvements (Group 2)

- **Builder search + bulk actions.** The scenario list has a
  search field (case-insensitive substring on name or channel
  id) and Enable-all / Disable-all buttons. Bulk actions apply
  to exactly the visible (filtered) scenarios — what you see is
  what you act on. Enabling reuses the single-toggle validation
  (`try_enable_scenario`): an invalid scenario stays disabled
  and is reported, the rest still go through; in user-token mode
  the 1-channel rule still applies per scenario. The search query
  is session-only (transient UI state, deliberately not
  persisted — see `docs/CONFIG-RULES.md`); the enabled flags
  themselves were already persisted per scenario.
- **Keyboard shortcuts (`handle_shortcuts`).** Ctrl/Cmd+S saves
  scenarios + config, Ctrl/Cmd+N creates a scenario (and opens
  the Builder), Ctrl/Cmd+1..5 switches Dashboard / Builder /
  Destinations / Logs / Settings, plain Esc dismisses the inline
  delete confirmations and the emoji picker. Shortcuts are
  ignored on the Onboarding screen so none can bypass the ToS
  gate. Bindings are fixed (no state to persist); the list is
  shown in Settings.
- **Window geometry memory + start-minimized.** The last sane
  window rect is remembered and restored at startup (clamped by
  `clamp_window_geometry` in `main.rs`: size 640..3840 ×
  480..2160, positions kept as-is for multi-monitor setups,
  non-finite values fall back to the default). Only
  non-minimized, non-maximized rects are recorded, so a
  maximized window does not restore as a giant normal window.
  The rect is written to config on exit (`on_exit`) and on
  hide-to-tray — never per-frame. Settings has a reset button
  (default size + position 100,100) for a lost window, and a
  start-minimized toggle (the minimize command is sent on the
  first frame; egui 0.31 has no start-minimized viewport
  option, so the window may flash once). Geometry fields live
  in `AppConfig` (0.0 = unset) and ride along in exports,
  harmlessly clamped on load.
- **Tray menu (Windows only, `src/tray.rs`).** The menu gained
  "Mở Dashboard" (show + switch to the Dashboard) and an engine
  pause/resume item whose label follows the engine state
  ("Dừng engine" / "Tiếp tục engine", synced every frame, only
  touching the menu on change). Tray labels stay Vietnamese,
  matching the existing items.
- **Font size (`UiScale` in `src/theme.rs`).** Small / Medium /
  Large (factors 0.875 / 1.0 / 1.15) multiply every text size;
  spacing and padding stay fixed so layouts never break. Stored
  as a code string in config like theme/language, applied
  instantly from Settings and at startup.

## Performance notes

Measured on this Linux VM (debug build, `cargo test -- --ignored`,
harness tests in `src/store.rs` and `src/app.rs`; re-run them after
storage or runtime changes — they print, never assert on timing):

- `count_sends_today()` over 10,000 log rows: **~3.3 ms/call**
  (full scan, no index on `created_at`). It used to run every frame
  in `update()` — ~3,600 queries/minute at 60 fps. The Dashboard
  now refreshes the count at most every 5 s (12/minute); the value
  is computed once at startup so the first frame is correct.
- `tokio::runtime::Runtime::new()`: **~420 µs/call**. Eleven UI
  call sites each built a fresh runtime per click. They now share
  one process-wide runtime (`shared_runtime()` in `src/app.rs`,
  ~0.1 µs/call after warm-up). Safe because no call site blocks
  nested inside another `block_on` on it.
- Not optimized: nothing else was measured, so nothing else was
  changed. In particular the per-frame `load_destinations()` calls
  in the Logs filter and the engine's per-tick queries were left
  alone — measure before touching them.

## Correctness fixes (Package A)

A batch of small, independently verified fixes for behaviours found
by reading the code (no new features):

- **Test send targets the selected scenario.** The Dashboard "Test
  send" button used to send the first scenario, ignoring the
  Builder selection. It now sends the selected scenario when one is
  selected and in range, else the first — and the button names the
  target scenario, so a REAL message never goes out by surprise.
- **Corrupt rows warn instead of vanishing.** `load_scenarios` and
  `load_destinations` silently dropped rows that failed to parse.
  Both now have `*_with_warnings` variants returning one warning
  per bad row (row id + truncated parse error). Startup collects
  them into the health report; exports name them in the status
  message; the automatic pre-import backup warns when it skipped
  rows. Import-replace deletes whole tables (`delete_all_*`)
  instead of load-then-delete, so corrupt rows cannot survive a
  "replace everything" import.
- **Per-scenario cap default unified.** `ScenarioLimits::default()`
  was 300 while the serde default for a missing `daily_cap` was 0
  ("inherit the account/global cap"), so new and imported scenarios
  behaved differently. Both are now 0, matching the scheduler (0
  maps to "unlimited", min-ed with the account/global cap) and
  guarded by `limits_default_matches_serde_default`. Effective
  behaviour is unchanged at the default caps.
- **Dashboard success rate is fully persisted.** It used to mix the
  persisted sent-today count with a session-only failure counter,
  so restarting the app reset the rate to 100%. Both sides now come
  from the send log (`count_status_today`). The engine's
  session-only failure counter was removed with it; the "Deleted"
  card stays honestly session-only.
- **Engine reads credentials fresh.** `start_engine` captured a
  config clone, so token/webhook changes needed an engine restart.
  The sender resolver now reloads the config from disk on every
  resolution (destinations already resolved fresh from the store),
  so Settings edits apply to a running engine.
- **SQLite busy timeout.** The file database now waits up to 5 s
  for a lock (`busy_timeout`), so a second Nicord instance holding
  the database no longer makes writes fail instantly with
  "database is locked".

## Known limitations

- **System tray is Windows-only.** On Linux the tray checkbox is
  hidden and closing the window still quits. Autostart works on both
  Windows (Run key) and Linux (XDG desktop entry).
- **Locked-thread behaviour is unverified.** `thread_id` sends are
  enabled (same-channel threads auto-unarchive per Discord's
  docs), but sending into a locked thread has not been tested.
- **Spike not run yet:** 7 days of stable sending on a spare account has
  not been proven — run it before releasing.
- **Platform testing:** the GUI has only been exercised on Linux, and
  never in this headless environment. Windows/macOS builds compile in
  CI (Linux-only CI job), but real-machine behaviour there — tray,
  autostart, sounds — is unverified. The README does not carry this
  kind of note by design (brand + features only); it lives here.
- Sending is REST-based; a gateway connection is used only to obtain
  a session ID for slash commands (`src/gateway.rs`). The channel
  picker is wired (Load servers/channels), with a hand-typed channel
  ID as fallback.
- The machine-tied token key is obfuscation against casual config
  copying, not protection against local malware or other local users
  (the machine ID is world-readable on Linux; on Windows a constant
  fallback is used). An OS-keyring migration is future work; do not
  change the key derivation without a migration plan, or existing
  stored tokens will stop decrypting.
- Cron schedules use the scenario timezone; double-check schedules that
  cross midnight.
