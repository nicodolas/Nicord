# Nicord — Configuration persistence rules

> Standing rule (set by the project owner, 2026-11-17): **whenever a
> feature is added or updated, ask whether it needs to be saved in
> configuration. If the chance is over 20%, save it.** This document
> makes that rule operational: what counts as configuration, how to
> decide, where things live, and how export/import must stay in sync.

## 1. The 20% rule

For every new or changed feature, list the state it introduces
(settings, user choices, per-destination values, templates, UI
preferences, credentials, limits, schedules). For each item, estimate:

> "Will the user expect this to survive an app restart — or to be
> there when they move to another machine?"

- **Over 20% likely → persist it.** Do not wait for proof. A 1-in-5
  chance is enough; the cost of persisting is small, the cost of
  losing the user's setup is not.
- **Under 20% → keep it in memory**, and say why in the code comment
  or the change's docs (e.g. "session-only by design: live check
  result, refreshed on every open").

Examples of what passes the bar (persist): a new per-destination
setting, a new global limit, a UI preference (language, theme), a
template library, a schedule preset the user customized, an embed
the user composed, a friendly name, a quiet window.

Examples of what does not: the currently selected row, a transient
"checking…" status, a cached network response, a per-session
failure counter, the last window position (unless the user asks).

When in doubt, persist — and record the decision in the change's
docs so the next reader knows it was considered.

## 2. Where persisted state lives

| Kind | Where | Notes |
|---|---|---|
| Global settings (`AppConfig`) | `config.json` (atomic write, 0600 on Unix) | Every field needs `#[serde(default)]` (or a default fn) so old configs keep loading. Never remove a field without a migration note. |
| Destinations | SQLite `destinations` table (JSON per row) | URLs encrypted before writing. See `destination.rs`. |
| Scenarios | SQLite `scenarios` table (JSON per row) | Schema versioned (`schemaVersion`). Unknown fields must not break loading. |
| Templates | SQLite `templates` table | Name + content, no secrets. |
| Send logs | SQLite `send_logs` table | History, NOT config. Never exported. |
| Secrets | Encrypted, never plaintext | OS keyring first, machine-key blob fallback (`secret_store.rs`); passphrase key for portable export (`backup.rs`). Never logged, never shown in full, never in exports except encrypted. |

Rules for adding a persisted field:

1. Add a `#[serde(default)]` (or explicit default fn) — old files
   must load without it.
2. Choose a safe default (the behaviour before the feature existed,
   or the most conservative value).
3. Add it to the export format (see §3) if it belongs to the user's
   setup. If it must NOT be exported, document why here.
4. Add a test that an old file (without the field) still loads and
   that the new field round-trips.
5. Document it in `TECHNICAL.md` (and here, if it changes the rules).

Fields added after the initial rules, each evaluated under §1:

- `ui_scale`, `start_minimized`, `window_width`, `window_height`,
  `window_x`, `window_y` (`AppConfig`, Group 2): a chosen font
  size, start-minimized preference, and window placement are
  plainly expected to survive restarts — persisted. They ride
  along in exports as part of `AppConfig`; a foreign window
  geometry is clamped on load, so importing it on another
  machine is harmless.
- Deliberately NOT persisted: the Builder search query (a
  transient filter — nobody expects it back after a restart)
  and keyboard shortcuts (fixed bindings, no state to save).

## 3. Export / import must cover every persisted setting

The export format (`src/backup.rs`, `format: "nicord-export"`,
`version: 1`) is the user's setup in one file. **Every persisted
setting must appear in it**, unless it is explicitly excluded below.
When you add a persisted field, update `build_export` /
`apply_import` in the same change — a setting that survives a
restart but not a move to another machine is a bug under this rule.

Modes:

- **Portable** — secrets included, encrypted under a passphrase
  (PBKDF2-HMAC-SHA256, 600,000 iterations, AES-256-GCM). For moving
  to another machine. The file is a password vault: treat it like one.
- **Safe** — secrets omitted (destinations arrive without URLs,
  disabled). For sharing a setup. The user re-enters credentials.
- **Local** — stored secret values copied as-is (keyring
  references or machine blobs). Only restorable on the same
  machine. Used for the automatic backup taken before an import.

Explicitly NOT exported (and why):

- Send logs and meta counters — history, not setup.
- `autostart` OS hook state — the flag is exported, but the OS hook
  is not applied on import; the app re-reads the real OS state on
  startup, and the user toggles it in Settings.
- In-memory engine state (cooldowns, pauses that are not persisted,
  session counters) — by definition not configuration.

Import replaces the current setup (it does not merge). It validates
every entry, skips invalid ones with warnings (never silently), and
requires the engine to be stopped + explicit confirmation. A local
backup is written first. See `backup.rs` for the exact rules.

## 4. Secrets rules (non-negotiable)

- A secret is never stored in plaintext, never logged, never shown
  in full, never sent anywhere except its own Discord endpoint.
- Local storage goes through `secret_store`: the OS keyring first
  (Windows Credential Manager, macOS Keychain, Linux Secret
  Service), falling back to the machine-tied key
  (`crypto::machine_key`) when the keyring is unavailable. The
  machine key is obfuscation against casual copying, not a
  keyring — see `TECHNICAL.md` for the honest wording. Do not
  claim otherwise. A new secret MUST be stored via
  `secret_store::store_secret` with its own keyring entry name,
  never via `crypto::encrypt_token` directly.
- Portable storage uses a passphrase-derived key. A weak passphrase
  means weak protection; the UI requires at least 8 characters and
  says so plainly.
- Copying `config.json` / the database to another machine does NOT
  transfer secrets. That is by design (machine key), and the export
  feature exists precisely to solve it. Never "fix" it by weakening
  the machine key.
- If a feature introduces a new secret (a new token, URL, or key),
  it must be encrypted with the same rules, included in portable
  export, excluded from safe export, and scrubbed from logs/exports
  (`store::scrub_secrets`).

## 5. Checklist for a feature change

- [ ] Listed the feature's state; applied the 20% rule to each item.
- [ ] Persisted items have serde defaults + safe defaults + tests.
- [ ] Export/import updated (or the exclusion documented here).
- [ ] Secrets (if any) follow §4.
- [ ] Docs updated (`TECHNICAL.md`, and this file if the rules changed).
- [ ] Old configs / old export files still load (test or manual check).

## 6. Review questions (for the reviewer, human or AI)

1. Does this change add state the user would miss after a restart?
2. Does it survive export → import to another machine?
3. Does it break loading of an older config or export file?
4. Does any secret appear in plaintext, logs, or safe exports?
5. Is the decision recorded where the next reader will find it?
