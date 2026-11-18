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

**Hardening audit (H-CMD, 2026-11-18).** The command paths were
audited end to end (routing, validation, prefix/slash sends,
gateway session handling, payload building, UI editor, test
send). Findings, all addressed or documented:

- Validation is fail-closed at three layers: scenario
  validation (name required; slash requires Application ID;
  command nodes rejected in webhook mode), `send_once`
  re-validates every scenario before firing, and `send_command`
  re-validates the config defensively. A malformed node config
  parses to the default (empty name), which validation rejects
  — a corrupt row can never send a bare prefix.
- Slash options are now validated: non-empty names, type codes
  limited to the supported set (3, 4, 5, 6, 7, 8, 10), no
  duplicate names (case-insensitive). Previously a bad option
  produced a malformed payload and a cryptic Discord 400.
  Prefix commands ignore options, so stale options on a prefix
  config do not fail validation. Note: a scenario saved with an
  empty option row now fails validation with a clear error
  instead of silently dropping that option — fill in or remove
  the row.
- Known limits (by design, unchanged): one firing runs only the
  FIRST command node; no subcommand/subcommand-group option
  types (1/2); no component interactions (the bot's reply
  buttons/selects cannot be clicked); option values that fail
  to parse fall back silently (bad integer → 0, bad number →
  0.0, unrecognized boolean → false); a failed channel read
  fails the whole slash send (no guild cache).
- Test coverage: validation, prefix composition, first-node
  routing, fail-closed malformed configs, interaction payload
  shapes (guild/DM, version, typed options), search-response
  parsing, option-value fallbacks, nonce shape, and gateway
  payload/sequence parsing. The live retry loop and gateway
  session flow have no unit tests (no mock client) and need
  the live checks below.
- **Live verification checklist (user, account mode):** one
  real prefix command; one real slash command with auto-resolved
  ID; one slash with a manually pasted Command ID; one slash
  with options of several types; one unknown command name (must
  fail with the "paste its Command ID" error); one run with the
  gateway unreachable (must fail with the gateway error while
  message sends keep working).

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
  Known limitation: the fallback key is a single SHA-256 with no
  KDF stretching, and on Windows (no machine ID file) a fixed
  constant stands in for the machine ID, so any local process
  that knows the username can derive it. Treat blob-fallback
  storage as protection against casual inspection only, never
  as confidentiality against a local attacker.
- Error strings never carry credentials: reqwest errors are
  formatted with the URL stripped (`reqwest_error` in
  `discord.rs`), because reqwest appends the request URL — which
  for webhooks contains the token — and the scheduler scrubs
  failure text before it reaches logs, the database, or the UI.
  The Logs screen scrubs on display too, for rows written before
  this boundary existed. If logs or a database were ever shared
  before this fix, regenerate the affected webhooks.
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

Measured on this Linux VM (debug build unless noted,
`cargo test -- --ignored`, harness tests in `src/store.rs` and
`src/app.rs`; re-run them after storage or runtime changes — they
print, never assert on timing):

- `count_sends_today()` over 10,000 log rows: **~3.3 ms/call**
  before Batch P (full scan, no index on `created_at`), **~1.2
  ms/call** after (debug harness, worst case: all 10,000 rows
  stamped today, so the count still visits 10k index entries).
  It used to run every frame in `update()` — ~3,600
  queries/minute at 60 fps. The Dashboard refreshes the count
  at most every 5 s (12/minute); the value is computed once at
  startup so the first frame is correct. The realistic shape
  (300 rows today out of 100k) measures 48 µs in release — see
  Batch P below.
- `tokio::runtime::Runtime::new()`: **~420 µs/call**. Eleven UI
  call sites each built a fresh runtime per click. They now share
  one process-wide runtime (`shared_runtime()` in `src/app.rs`,
  ~0.1 µs/call after warm-up). Safe because no call site blocks
  nested inside another `block_on` on it.

### Batch P (measured, overnight research)

A read-only harness compiled the real modules and measured the
exact store queries against 10k/100k/300k-row databases before
any change was made. Findings that drove the work:

- **No indexes existed**, and `send_logs` is never pruned, so the
  Dashboard's per-frame counts scanned the whole table: ~131 ms
  per frame at 100k rows (~1 year at the 300/day cap), ~383 ms
  at 300k. One sparse cron (`0 0 29 2 *`) added ~153 ms/frame
  on top: `next_after` walks minute-by-minute (~38 ms/call) and
  the UI called it ~4× per scenario per frame.
- **R1 — indexes + range rewrite.** Three indexes
  (`(status, created_at)`, `(destination_id, status, created_at)`,
  `(scenario_id, id)`), created idempotently on open, plus
  rewriting `substr(created_at,1,10) = today` to a half-open
  `[today, tomorrow)` range (`utc_today_bounds`, equivalent for
  every stored string). Measured at 100k rows: the today-count
  18.8 ms → 48 µs (390×), per-destination count 17 ms → 54 µs,
  `MAX(created_at)` 17.3 ms → 11 µs. The rewrite is essential:
  the index alone only gave 1.5×. One-time migration ~0.9 s at
  100k rows; DB 19.6 → 32.8 MB.
- **R2 — next-run cache.** `NextRunCache` in `app.rs`: 60 s TTL,
  keyed by scenario + schedule + timezone (edits invalidate),
  past values recomputed at once. Display only — the engine
  fires via `due_time`. The schedule editor still calls
  `next_after` directly (draft schedule, one call per frame).
- **R3 — WAL + `synchronous=FULL`.** Each `log_send` commit cost
  ~90 ms on this VM's storage (fsync per commit in DELETE mode).
  WAL removes the per-commit fsync; FULL keeps the same crash
  durability as before (a power loss cannot lose the last
  scenario edit). Export/import never copy the raw DB file, so
  the sidecars are safe. Absolute milliseconds on a Windows SSD
  will differ; only the mechanism transfers.
- **R4 — `query_logs` restructure.** The always-true keyword
  clause defeated index use; omitting it when the keyword is
  empty (identical result set) cut a rare-scenario filter from
  12.8 ms to 0.29 ms at 100k rows. Keyword search itself
  (39 ms at 100k) has no B-tree fix and was left alone.
- **R5 — hygiene.** `load_destination(id)` is a direct
  `WHERE id=?` lookup; the Logs screen loads destinations once
  per frame instead of twice.
- **Deliberately not optimized** (measured fast enough):
  per-tick scenario clone (44 µs), `load_config` + decrypt per
  resolution (~30 µs, live-reload kept), fresh `reqwest::Client`
  per send (9 µs CPU; TLS reuse unmeasured, no claim made),
  `due_time` (≤651 µs on a background thread), unfiltered
  `query_logs`, CSV export query.

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
  rows. Import-replace runs as one atomic `Store::replace_all`
  transaction (Batch B1) instead of load-then-delete, so corrupt
  rows cannot survive a "replace everything" import and a failed
  import leaves the old data untouched.
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

## DST policy + wall-clock scan clamp (L4)

One DST policy, shared by the firing decision (`due_time`)
and the display computation (`next_after`), implemented in
`resolve_wall_time` / `is_second_occurrence` (`scenario.rs`):

- An ambiguous wall-clock time (occurs twice when clocks fall
  back) fires once, at its FIRST occurrence.
- A nonexistent wall-clock time (skipped when clocks spring
  forward) never fires; display moves to the next valid day.

Before this fix the two paths disagreed: Daily/Weekly skipped
ambiguous days entirely (`.single()` returned `None`), while
the cron backward minute-walk could fire twice in an ambiguous
hour (two distinct instants share the wall time, and the
last-run anchor only suppresses the earlier one). The cron
walk now skips the second occurrence; the forward scan used
for display already returned the first occurrence. `next_after`
for Daily now resolves per date instead of adding 24h to
today's occurrence, so the wall time stays exact across a
transition.

`wall_clock_due` also clamps its day-scan to the last 7 days +
today (8 dates, matching the cron path's 7-day clamp). Daily
and Weekly schedules recur within every 7-day window, so the
latest occurrence in (anchor, now] is always inside it — the
8th date covers a weekday whose time has not occurred yet
today. An ancient anchor (long-off machine, old import) used
to scan every day since the anchor on every engine tick.

## Run-state persistence failures (M2)

The engine's run loop used to ignore `save_scenario` failures
after a send/skip: a failed last-run save means a restart
re-fires the run (a duplicate send), with no warning anywhere.
All five run-loop save sites now go through
`Engine::save_run_state`, which logs the failure loudly and
remembers it; the Dashboard health card shows the last
persistence error live until a later save succeeds. Send-log
writes go through `Engine::write_log`, which logs failures
(history loss cannot cause a duplicate send, so it stays out
of the health card).

## Scheduled polls (idea #3)

One poll per scenario (`src/poll.rs`), sent with every message
in both modes — the `poll` field is accepted by the
execute-webhook body and the create-message body alike.
Limits (official docs): question ≤ 300 chars, 2–10 answers,
each ≤ 55 chars, duration 1–168 h. A poll may accompany empty
content: validation accepts a poll-only scenario, and the
engine's content picking falls through to empty text when no
message source has content but a poll is present. Results are
NOT readable via webhook (and the user-token path does not
read messages), so the composer states that results are visible
only inside Discord. One real test message is recommended to
confirm the payload shape, as with the other webhook behaviors.

Composer-draft fix (found while building this): the Q8 embed
composer saved only when the embed validated — but a fresh
draft is empty, so the enable checkbox unchecked itself on the
next frame and the composer could never be entered. Both
composers now save drafts as edited; validation gates
enabling/sending (via `validate_inner` and the engine's
send-time validation), not the editor.

## Richer embeds (idea #8)

- **Up to 10 embeds per scenario** (Discord's limit). The model
  moved from `Scenario.embed: Option<Embed>` to `embeds:
  Vec<Embed>`; old files (one object under `"embed"`, or null)
  migrate automatically via a serde alias + custom deserializer
  (`de_embeds` — variant order matters: the array form is tried
  before the single-object form, because serde structs accept
  seq form and `[]` would otherwise parse as one default
  embed). New files serialize only `"embeds"`. The 6000-char
  total is enforced ACROSS all embeds by `validate_embeds`, in
  addition to each embed's own limits. Empty embeds are dropped
  at send time (`active_embeds`) rather than 400-ing.
- **New embed fields:** title link `url`, large `image_url`,
  small `thumbnail_url` — all https-only (same rule as
  destination avatars), empty = absent. An image-only embed
  counts as non-empty (it renders). URLs are not counted in the
  6000-char total (Discord counts title/description/fields/
  footer/author only).
- **No timestamp field, deliberately:** a fixed timestamp on a
  recurring message would go stale; a send-time stamp belongs
  in the send layer, not the stored model.
- The composer edits a list (add/remove per embed, shared
  field editor + preview), shows the combined total, and
  fail-closes on save when the set is invalid.

## Drip series & template variables (idea #6)

New template variables, rendered by `render_template_with_vars`
(unknown tags stay literal — rendering never fails):

- `{{counter:name}}` — a durable counter stored in the `meta`
  table (`counter:<name>`). The stored value IS what renders;
  it starts at 1 and increments by 1 after each successful send
  that used it (failed/deferred runs never advance the series).
  Names are 1–64 chars of letters, digits, `_`, `-`; anything
  else stays literal. The Builder shows the counters a scenario
  uses, with current values and a reset (delete the row → the
  next render starts at 1 again). Counters are exported with
  the setup in every mode and replaced on import — a series
  survives a move to another machine.
- `{{weekday}}` (full English name), `{{week_number}}` (ISO
  week, 01–53), `{{days_until:YYYY-MM-DD}}` (whole days from
  today, clamped at 0; unparseable dates stay literal).
  Like `{{date}}`/`{{time}}`, these use the machine's local
  time, not the scenario timezone (documented behavior).

Counters render in message sends only: the command path passes
an empty counter map, so a `{{counter:...}}` tag in a command
stays literal and never increments.

## Mention control & silent send (idea #7)

Two per-scenario settings, stored on the scenario (persisted and
exported with it, serde defaults keep old scenarios working):

- **Block mass pings (default).** Unless the scenario opts in,
  every send carries `allowed_mentions: {"parse": ["users"]}`:
  @everyone/@here/role mentions render as plain text and ping
  nobody, while individual user mentions still ping. Opting in
  omits the field (Discord's default: everything parses). The
  Builder warns when a scenario's messages contain @everyone/
  @here while blocking is on.
- **Silent send.** Sets the SUPPRESS_NOTIFICATIONS flag
  (`1 << 12`): the message arrives without pushing a
  notification.

Both fields ride the same `SendOptions` in webhook mode and
user-token mode (the execute-webhook body and the create-message
body accept the same fields). Implemented from Discord's
official message-object documentation; one real test message
is still recommended to confirm Discord honors `allowed_mentions`
via webhook execute, as with the other webhook behaviors.

**Text defang vs. opt-in (F-GUI-3).** The scheduler's content
check (`sanitize_content`, anti-ban layer 4) used to defang
`@everyone`/`@here` with a zero-width space in every mode,
which silently overrode an explicit opt-in: the text no longer
contained a literal mention, so Discord had nothing to parse
even with `allowed_mentions` omitted. Since the GUI-fix batch,
the defang is skipped only in webhook mode when the scenario
opts in — there, the official `allowed_mentions` mechanism is
the single control. User-token mode and the command path keep
the defang exactly as before.

## Maintenance (Group 3)

- **Scheduled automatic backups.** On by default: every
  `auto_backup_interval_days` (default 1), Nicord writes a
  local-mode export (same bytes as the pre-import backup, via
  `backup::write_local_backup`) to `<config_dir>/backups/` and
  prunes to the newest `auto_backup_keep` (default 7) files.
  The due check runs at most every 60 s from `update()`; the
  backup itself is a small export and runs inline. Failures are
  logged (`tracing::warn!`), not shown — a backup failing every
  60 s must not spam the status line; the Settings card shows
  the last successful time (`last_auto_backup_at`), so a stale
  timestamp is the visible signal. "Sao lưu ngay" runs the same
  path on demand and reports in the status line. Like all local
  backups, automatic backups are machine-bound: they restore
  only on the machine that wrote them.
- **Log retention.** `log_retention_days` (default 90, 0 =
  keep forever). `Store::prune_logs` deletes rows dated before
  the UTC cutoff, once at startup and on demand from Settings
  (the button reports how many rows were deleted). Deleted
  pages are reused by SQLite; the file does not shrink (no
  VACUUM — a large database would freeze the UI at startup).
  Pruning never touches schedules, destinations, or today's
  statistics. The send log grows unboundedly without it
  (~100k rows ≈ one year at the 300/day cap), which is what
  made the Batch P indexes necessary.
- **Log detail view.** The "Chi tiết" button on a log row opens
  a detail card above the list: the full detail text (selectable,
  secret-scrubbed on display) plus message id, destination id,
  and channel id. Esc closes it. Session-only state.
- **i18n.** The new Group 1/3 strings are keyed in both
  languages. Pre-existing section titles and body copy remain
  Vietnamese-only: completing them is incremental work (add a
  key, then use `t()` at the call site), deliberately not
  churned in this batch.

## Seeing the future (Group 1)

Three computed views — no new persisted state (the >20% rule was
evaluated: everything here is derived from schedules, destinations,
and the send log, so nothing is saved or exported):

- **Upcoming-sends timeline** (Dashboard). The next 20 runs across
  enabled scenarios within 7 days, each with its destination and
  a blocked-run warning when the run will be skipped. Clicking
  "Sửa" jumps to the scenario in the Builder. Computation lives
  in `src/timeline.rs` (`future_runs` iterates `next_after`;
  `blocked_reason` mirrors the engine's ahead-of-time skip
  checks: destination state, quiet window, today's daily cap,
  time-window nodes). Refreshed at most every 30 s — a sparse
  cron costs ~38 ms per forward step, so the timeline must not
  recompute every frame.
- **7-day chart** (Dashboard). Sent vs failed bars per day from
  one `counts_by_day` query (range-bounded, index-friendly),
  hand-drawn with the egui painter; no chart dependency. The
  `deleted` audit rows are excluded.
- **Schedule simulation** (Builder schedule editor). The next 10
  runs of the DRAFT schedule with the same blocked-run warnings,
  cached by draft key (schedule + timezone + scenario) so edits
  recompute once, not per frame.

Honest limits, stated in the UI: the daily-cap warning only
applies to runs dated today (future counts cannot be known);
429 cooldowns and the night sleep window are not predicted;
interval schedules are projected forward from now rather than
from the last fired run, so the first projected run can differ
from the engine's next firing by less than one interval.

## Correctness fixes (Batch B2)

A second batch from the overnight bug hunt (no new features).
Items marked [test] have a regression test; the engine-loop items
are covered by code review (the loop is not unit-testable).

- **Per-destination caps count by destination, not channel**
  [test]. `count_sends_today_for_channel` silently skipped the
  cap when `channel_id` was empty, misattributed caps when the id
  was stale, and merged two destinations in one channel into one
  bucket. Counting is now by `destination_id`
  (`count_sends_today_for_destination`), unconditionally.
- **Empty messages rejected and never picked** [test]. Validation
  checked only the 2000-char limit, so a crafted import could
  plant an empty message that the fallback chain would "send"
  (a failure log + error counting every run). Both validators
  reject empty/whitespace entries, and both message pickers skip
  them, so corrupt rows cannot produce an empty send either.
- **Displayed "next run" uses the scenario timezone** [test].
  `Schedule::next_after` computed Daily/Weekly in local time and
  Cron in UTC while the engine fires in the scenario timezone,
  so the UI showed a different time than the actual send. The
  display path now takes the scenario timezone and computes in
  it, matching `due_time`.
- **Schedule-change detection compares schedules, not labels**
  [test]. The engine remembered `schedule.label()`; reordering
  weekdays or changing cron whitespace reset the last-run
  anchor and could duplicate a send. It now compares
  `Schedule::normalized()` values.
- **Quiet hours and Time Windows agree** [test]. The destination
  quiet window was end-inclusive, treated start==end as one
  quiet minute, and failed OPEN on a one-sided setting; Time
  Window nodes were end-exclusive with start==end never inside.
  Both are now end-exclusive, start==end means never, and a
  one-sided or unparseable quiet window fails CLOSED (quiet).
- **Re-enabling a fired Once scenario works** [test]. `due_time`
  never fires a Once scenario with a `last_run_at`, so
  re-enabling one silently did nothing. Enabling now clears the
  anchor for Once schedules only; recurring schedules keep it.
- **Destination migration assigns scenarios even when
  destinations already exist** [test]. The assignment loop only
  ran on the fresh-migration path; scenarios left unassigned
  could never be enabled in webhook mode.
- **Startup never overwrites an unreadable config.** `load_config`
  collapsed missing/corrupt/transiently-unreadable into defaults,
  and startup saves could then destroy a good config file.
  `load_config_status` distinguishes Fresh/Loaded/Unreadable;
  startup skips both config saves when unreadable and the health
  report warns about it.
- **Keyring operations time out after 10 s** [test]. A hung
  platform store used to block the keyring thread forever (join
  with no timeout). Operations now use a channel +
  `recv_timeout`; a timeout degrades like any other keyring
  failure (reads fail closed, writes fall back to a blob).
- **Validation failures no longer blame the destination.** The
  engine loop counted a broken scenario's validation error
  toward the destination's error limit and the 3-error engine
  pause. Such failures now consume the run like a skip: no
  counters, no failure row. Failure rows also log the
  destination's channel in webhook mode (they used the empty
  scenario channel, so the Logs destination filter missed them).
- **Updater: macOS asset name, version-bound fallback, safe
  swap** [test]. macOS used to look for a Linux asset name; it
  now looks for `Nicord-v{version}-macos-arm64.zip`, and the
  platform-zip fallback requires the version in the name (no
  silent downgrades). The helper scripts now swap via a temp
  name + rename and check every critical step (a failed swap
  keeps the old exe, the `.bak`, and the staging dir, and never
  relaunches); a zip with the exe in a subdirectory is refused;
  `launch_helper_and_exit` reports a spawn failure instead of
  exiting anyway; the release-JSON response is size-capped.
- **Small items** [test]. `wait_seconds` sums saturating (no
  overflow on crafted configs); `store_secret` never writes an
  empty secret to the keyring; the legacy-webhook dual-write
  clears a persisted destination pause only when the URL was
  actually resolved and re-stored.
- **Known and documented, not changed.** `{{date}}`/`{{time}}`
  template variables render in the machine's local timezone,
  not the scenario timezone (changing the signature through the
  render path was judged too invasive for the value; the
  schedule itself already respects the scenario timezone).

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

## GUI fixes from live verification (F-GUI-1, F-GUI-2, F-GUI-4, F-GUI-5)

Found by running the real binary under Xvfb and driving it
(2026-11-18, screenshots in the verification notes, not the repo):

- **F-GUI-1:** the health card was computed once at startup, so
  after onboarding saved a webhook it still showed the
  startup-time token-mode warning. All three onboarding exits
  (webhook save, token save, skip) now call `refresh_health()`
  before showing the Dashboard.
- **F-GUI-2:** the Builder search row reserved a fixed 170 px
  for the two bulk buttons, which they overflow at the default
  window width (the second button slid under the editor panel).
  The search field now takes a full row, with the buttons on
  the row below — robust at any width, language, or UI scale.
- **F-GUI-4:** the 10-minute engine pause after 3 consecutive
  errors was invisible (Dashboard still said "Running"). The
  engine now records the pause end (`error_pause_until`,
  runtime-only: a restart resets the breaker), and the sidebar
  + Dashboard engine card show "Paused (m:ss)" with a warning
  dot while it lasts.
- **F-GUI-5:** Dashboard, Destinations, and Settings had no
  vertical `ScrollArea`, so at the default 1100x700 window
  their lower content (Actions row, timeline, lower Settings
  cards) was unreachable without resizing. The three screens
  are now wrapped in a `ScrollArea` at the `CentralPanel`
  dispatch (distinct `id_salt`s); Builder and Logs already
  scroll internally, and Onboarding is left unwrapped because
  it centers from `available_height`.
