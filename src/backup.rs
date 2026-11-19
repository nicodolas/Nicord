//! Config export/import for backup and cross-machine use.
//!
//! The problem: secrets in `config.json` and the destinations table
//! are stored machine-locally — either as OS-keyring references or
//! as blobs encrypted with a machine-tied key (see `secret_store`
//! and `crypto::machine_key`) — so copying those files to another
//! machine leaves the secrets unusable there. Portable exports
//! solve this by resolving every secret and re-encrypting it under
//! a passphrase-derived key (PBKDF2-HMAC-SHA256, 600k iterations,
//! AES-256-GCM). Import decrypts with the passphrase and stores
//! each secret locally again (keyring when available, else a
//! machine blob).
//!
//! Three modes:
//! - `portable`: secrets included, passphrase-encrypted. For moving
//!   to another machine. The file MUST be treated like a password
//!   vault — a weak passphrase means weak protection.
//! - `safe`: secrets omitted (destinations arrive without URLs,
//!   token/webhook fields empty). For sharing a setup or a public
//!   backup. The user re-enters credentials on the other machine.
//! - `local`: stored secret values copied as-is (keyring
//!   references or machine blobs). Only restorable on the SAME
//!   machine. Used for the automatic backup taken before an import
//!   replaces the current config.
//!
//! What is exported: AppConfig (minus secrets, which live in the
//! `secrets` block), destinations, scenarios, templates. What is
//! NOT exported: send logs (history, not config) and meta counters.
//! Scenario `variables` are exported as-is: if the user stored a
//! secret in a variable, it is in the file. That is documented in
//! the UI and in CONFIG-RULES.md.
//!
//! Import REPLACES the current config, destinations, scenarios,
//! and templates (it does not merge). A local-mode backup of the
//! current state is written first, and the UI requires explicit
//! confirmation. Invalid entries are skipped with warnings, never
//! silently dropped.

use crate::{
    crypto::{decrypt_with_key, encrypt_token, encrypt_with_key, pbkdf2_hmac_sha256, random_salt},
    destination::Destination,
    scenario::Scenario,
    secret_store::{
        destination_entry, load_secret, store_secret_with, Keyring, OsKeyring, ENTRY_ACCOUNT_TOKEN,
        ENTRY_LEGACY_WEBHOOK,
    },
    store::{AppConfig, Store, WebhookHealth},
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};

pub const EXPORT_FORMAT: &str = "nicord-export";
pub const EXPORT_VERSION: u32 = 1;
pub const EXPORT_KDF_ITERATIONS: u32 = 600_000;

/// Export modes (see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportMode {
    Portable,
    Safe,
    Local,
}

/// The `secrets` block of a portable export. All values are
/// AES-GCM blobs under the passphrase-derived key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortableSecrets {
    pub kdf: String,
    pub iterations: u32,
    pub salt_b64: String,
    /// Portable-encrypted account token ("" when none stored).
    #[serde(default)]
    pub token: String,
    /// Portable-encrypted legacy webhook URL ("" when none stored).
    #[serde(default)]
    pub webhook_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportFile {
    pub format: String,
    pub version: u32,
    pub exported_at: String,
    pub mode: ExportMode,
    /// AppConfig with secret fields cleared (the real secrets live
    /// in `secrets` for portable/local, nowhere for safe).
    pub app_config: AppConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<PortableSecrets>,
    #[serde(default)]
    pub destinations: Vec<Destination>,
    #[serde(default)]
    pub scenarios: Vec<Scenario>,
    #[serde(default)]
    pub templates: Vec<TemplateEntry>,
    /// Durable template counters (idea #6). Not secrets: included
    /// in every mode. Missing in older files → empty (a fresh
    /// counter starts at 1). Import replaces the whole set.
    #[serde(default)]
    pub counters: std::collections::HashMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateEntry {
    pub name: String,
    pub content: String,
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub scenarios_imported: usize,
    pub scenarios_skipped: usize,
    pub destinations_imported: usize,
    pub destinations_skipped: usize,
    pub templates_imported: usize,
    pub warnings: Vec<String>,
}

/// Derive a 32-byte export key from a passphrase + salt.
fn export_key(passphrase: &str, salt: &[u8], iterations: u32) -> [u8; 32] {
    let dk = pbkdf2_hmac_sha256(passphrase.as_bytes(), salt, iterations, 32);
    let mut key = [0u8; 32];
    key.copy_from_slice(&dk);
    key
}

/// Build an export file from the current state. `passphrase` is
/// required for portable mode, ignored otherwise. The returned
/// warnings name database rows too corrupt to parse that were left
/// out of the export; the caller must show them.
pub fn build_export(
    store: &Store,
    config: &AppConfig,
    mode: ExportMode,
    passphrase: &str,
) -> Result<(ExportFile, Vec<String>), String> {
    build_export_with_iterations(store, config, mode, passphrase, EXPORT_KDF_ITERATIONS)
}

/// Build an export with an explicit KDF iteration count. Production
/// uses `EXPORT_KDF_ITERATIONS`; tests use a small count for speed.
/// The count is recorded in the file, and import honors it.
fn build_export_with_iterations(
    store: &Store,
    config: &AppConfig,
    mode: ExportMode,
    passphrase: &str,
    iterations: u32,
) -> Result<(ExportFile, Vec<String>), String> {
    if mode == ExportMode::Portable && passphrase.is_empty() {
        return Err("cần mật khẩu cho file xuất portable".into());
    }
    let mut app_config = config.clone();
    // Secrets never live in `app_config` inside the file.
    app_config.encrypted_token.clear();
    app_config.encrypted_webhook_url.clear();

    let (destinations, mut load_warnings) = store.load_destinations_with_warnings()?;
    let (scenarios, scenario_warnings) = store.load_scenarios_with_warnings()?;
    load_warnings.extend(scenario_warnings);
    let templates = store
        .load_templates()?
        .into_iter()
        .map(|(_, name, content)| TemplateEntry { name, content })
        .collect::<Vec<_>>();

    let (secrets, destinations) = match mode {
        ExportMode::Safe => {
            // Strip every destination URL; the user re-pastes them.
            let stripped: Vec<Destination> = destinations
                .into_iter()
                .map(|mut d| {
                    d.encrypted_url.clear();
                    d
                })
                .collect();
            (None, stripped)
        }
        ExportMode::Local => {
            // Machine-encrypted values copied as-is (same machine
            // restore only). Secrets block is not needed: the values
            // stay in their original fields.
            app_config.encrypted_token = config.encrypted_token.clone();
            app_config.encrypted_webhook_url = config.encrypted_webhook_url.clone();
            (None, destinations)
        }
        ExportMode::Portable => {
            let salt = random_salt();
            let key = export_key(passphrase, &salt, iterations);
            // Decrypt with the machine key, re-encrypt with the
            // export key. A secret that cannot be decrypted locally
            // (e.g. copied from another machine) is exported empty
            // with a warning in the report — never as a broken blob.
            let mut warnings = Vec::new();
            let token = portable_reencrypt(&config.encrypted_token, &key, &mut warnings, "token");
            let webhook_url = portable_reencrypt(
                &config.encrypted_webhook_url,
                &key,
                &mut warnings,
                "webhook URL",
            );
            let mut ported = Vec::with_capacity(destinations.len());
            for mut d in destinations {
                if d.encrypted_url.trim().is_empty() {
                    ported.push(d);
                    continue;
                }
                let re = portable_reencrypt(
                    &d.encrypted_url,
                    &key,
                    &mut warnings,
                    &format!("đích đến '{}'", d.display_name()),
                );
                d.encrypted_url = re;
                ported.push(d);
            }
            if !warnings.is_empty() {
                // Surface via a sentinel: the caller reads warnings
                // from the export result. For now, encode them as a
                // warning destination is wrong — instead, fail loudly
                // so the user knows the export is incomplete.
                return Err(format!(
                    "không xuất được các secret này (có thể thuộc về máy khác): {}",
                    warnings.join("; ")
                ));
            }
            let secrets = PortableSecrets {
                kdf: "pbkdf2-hmac-sha256".to_string(),
                iterations,
                salt_b64: B64.encode(salt),
                token,
                webhook_url,
            };
            (Some(secrets), ported)
        }
    };

    Ok((
        ExportFile {
            format: EXPORT_FORMAT.to_string(),
            version: EXPORT_VERSION,
            exported_at: chrono::Utc::now().to_rfc3339(),
            mode,
            app_config,
            secrets,
            destinations,
            scenarios,
            templates,
            counters: store.load_counters(),
        },
        load_warnings,
    ))
}

/// Resolve a stored secret (keyring reference or machine blob)
/// and re-encrypt it under the export key. Empty input stays empty.
/// Failures append a warning and yield an empty string (the caller
/// decides whether to abort).
fn portable_reencrypt(
    stored: &str,
    key: &[u8; 32],
    warnings: &mut Vec<String>,
    what: &str,
) -> String {
    if stored.trim().is_empty() {
        return String::new();
    }
    match load_secret(stored).and_then(|plain| encrypt_with_key(key, &plain)) {
        Ok(re) => re,
        Err(_) => {
            warnings.push(what.to_string());
            String::new()
        }
    }
}

/// Serialize an export file to pretty JSON.
pub fn export_to_json(file: &ExportFile) -> Result<String, String> {
    serde_json::to_string_pretty(file).map_err(|e| e.to_string())
}

/// Write a local-mode export of the current state to `path`
/// (machine-bound: only restorable on this machine), with
/// owner-only permissions on Unix. Returns the export warnings.
/// Shared by the pre-import backup and the scheduled automatic
/// backups so both write the same bytes the same way.
pub fn write_local_backup(
    store: &Store,
    config: &AppConfig,
    path: &std::path::Path,
) -> Result<Vec<String>, String> {
    let (file, warnings) = build_export(store, config, ExportMode::Local, "")?;
    let json = export_to_json(&file)?;
    std::fs::write(path, json).map_err(|e| e.to_string())?;
    crate::store::restrict_permissions(path);
    Ok(warnings)
}

/// Directory for scheduled automatic backups:
/// `<config_dir>/backups/`.
pub fn auto_backup_dir() -> std::path::PathBuf {
    crate::store::config_dir().join("backups")
}

/// Whether a scheduled backup is due: never run, an unparseable
/// timestamp, or the last run is at least `interval_days` old.
/// `interval_days == 0` means "due" (defensive: the UI clamps
/// the value to >= 1, but a hand-edited config must not wedge
/// the schedule silently).
pub fn auto_backup_due(
    last_at: &str,
    interval_days: u32,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if interval_days == 0 {
        return true;
    }
    let last = match chrono::DateTime::parse_from_rfc3339(last_at) {
        Ok(t) => t.with_timezone(&chrono::Utc),
        Err(_) => return true,
    };
    now - last >= chrono::Duration::days(i64::from(interval_days))
}

/// Write one scheduled backup and prune the directory to the
/// newest `keep` files (keep is clamped to >= 1). Returns the
/// written path. Pruning is best-effort: a failed listing or a
/// failed removal never fails the backup itself.
pub fn run_auto_backup(
    store: &Store,
    config: &AppConfig,
    keep: u32,
) -> Result<std::path::PathBuf, String> {
    let dir = auto_backup_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("nicord-backup-{ts}.json"));
    write_local_backup(store, config, &path)?;
    prune_auto_backups(&dir, keep.max(1) as usize);
    Ok(path)
}

/// Keep the newest `keep` `nicord-backup-*.json` files in `dir`
/// (filenames sort chronologically), removing the rest.
/// Best-effort: every failure is ignored.
fn prune_auto_backups(dir: &std::path::Path, keep: usize) {
    let mut names: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .map(|n| {
                        n.to_string_lossy().starts_with("nicord-backup-")
                            && n.to_string_lossy().ends_with(".json")
                    })
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => return,
    };
    names.sort();
    if names.len() > keep {
        for old in &names[..names.len() - keep] {
            let _ = std::fs::remove_file(old);
        }
    }
}

/// Parse and validate an export file's envelope (format/version).
/// Returns the parsed file; secret decryption happens in `apply_import`.
pub fn parse_export(text: &str) -> Result<ExportFile, String> {
    let file: ExportFile = serde_json::from_str(text)
        .map_err(|e| format!("không phải file xuất Nicord hợp lệ: {e}"))?;
    if file.format != EXPORT_FORMAT {
        return Err(format!(
            "định dạng xuất '{}' không rõ (cần '{EXPORT_FORMAT}')",
            file.format
        ));
    }
    if file.version != EXPORT_VERSION {
        return Err(format!(
            "phiên bản xuất {} không được hỗ trợ (app này đọc phiên bản {EXPORT_VERSION})",
            file.version
        ));
    }
    Ok(file)
}

/// Apply an import, REPLACING the current config/destinations/
/// scenarios/templates. The caller must have taken a backup first
/// and obtained the user's confirmation. `passphrase` is required
/// for portable mode. Returns the new AppConfig + a report.
/// The caller saves the config and reloads its scenario list.
pub fn apply_import(
    store: &Store,
    file: &ExportFile,
    passphrase: &str,
) -> Result<(AppConfig, ImportReport), String> {
    apply_import_with(store, file, passphrase, &OsKeyring)
}

/// Where a pending portable secret belongs, for the phase-3
/// keyring upgrade.
enum SecretSlot {
    ConfigToken,
    ConfigWebhook,
    Destination(String),
}

/// Import with an injectable keyring (tests use a mock).
///
/// Ordering is the safety property here, in three phases:
///
/// 1. Pure validation + decryption to memory. Every fallible check
///    (KDF, passphrase, destination/scenario/template validation,
///    the destination ceiling) runs BEFORE any write, and portable
///    secrets are decrypted to plaintext in memory only, then held
///    as machine blobs. Nothing in this phase touches the keyring
///    or the database, so a failure here provably changes nothing.
/// 2. Atomic database replacement: one transaction replaces all
///    three tables (see `Store::replace_all`).
/// 3. Best-effort keyring upgrade (portable mode only): each secret
///    is stored under its keyring entry and the row/config field is
///    updated to the returned form. A failure here keeps the blob
///    form with a warning — it can never fail the import, because
///    the database already holds valid stored forms.
///
/// The previous code stored config secrets into the keyring BEFORE
/// the ceiling check and the database replacement, so a rejected
/// import silently swapped the live credentials while reporting
/// failure — and the pre-import backup (which stores only `kr:`
/// references) could not restore them.
fn apply_import_with(
    store: &Store,
    file: &ExportFile,
    passphrase: &str,
    kr: &dyn Keyring,
) -> Result<(AppConfig, ImportReport), String> {
    let mut report = ImportReport::default();
    let mut config = file.app_config.clone();

    // Phase 0: key derivation. No side effects; the iteration count
    // comes from the file, bounded to reject denial-of-service
    // values (a hostile file asking for billions of iterations must
    // not hang the app).
    let portable_key: Option<[u8; 32]> = if file.mode == ExportMode::Portable {
        let secrets = file
            .secrets
            .as_ref()
            .ok_or("file xuất portable thiếu khối secret")?;
        if secrets.kdf != "pbkdf2-hmac-sha256" {
            return Err(format!("KDF '{}' không được hỗ trợ", secrets.kdf));
        }
        if !(1..=2_000_000).contains(&secrets.iterations) {
            return Err(format!(
                "số vòng lặp KDF {} không được hỗ trợ",
                secrets.iterations
            ));
        }
        if passphrase.is_empty() {
            return Err("cần mật khẩu để nhập file xuất portable".into());
        }
        let salt = B64
            .decode(&secrets.salt_b64)
            .map_err(|_| "file xuất portable có salt không hợp lệ".to_string())?;
        Some(export_key(passphrase, &salt, secrets.iterations))
    } else {
        None
    };

    // Secrets decrypted in phase 1, awaiting the phase-3 upgrade:
    // (keyring entry, plaintext, slot).
    let mut pending_secrets: Vec<(String, String, SecretSlot)> = Vec::new();

    // Phase 1a: config secrets -> plaintext in memory (+ blob forms).
    match file.mode {
        ExportMode::Safe => {
            config.encrypted_token.clear();
            config.encrypted_webhook_url.clear();
            // Stale metadata must not survive: health "good" for a
            // webhook with no credential would be a lie.
            config.webhook_id.clear();
            config.webhook_name.clear();
            config.webhook_health = WebhookHealth::default();
            config.webhook_last_checked_at.clear();
        }
        ExportMode::Local => {
            // Values are already stored for this machine (keyring
            // references or machine blobs). Keep them as-is (they
            // came from `app_config`).
        }
        ExportMode::Portable => {
            let (Some(secrets), Some(key)) = (file.secrets.as_ref(), portable_key.as_ref()) else {
                return Err("file xuất portable thiếu khối secret".into());
            };
            let token_plain = portable_decrypt(&secrets.token, key, "token tài khoản")?;
            if !token_plain.is_empty() {
                pending_secrets.push((
                    ENTRY_ACCOUNT_TOKEN.to_string(),
                    token_plain.clone(),
                    SecretSlot::ConfigToken,
                ));
            }
            config.encrypted_token = blob_form(&token_plain)?;
            let url_plain = portable_decrypt(&secrets.webhook_url, key, "webhook URL")?;
            if !url_plain.is_empty() {
                pending_secrets.push((
                    ENTRY_LEGACY_WEBHOOK.to_string(),
                    url_plain.clone(),
                    SecretSlot::ConfigWebhook,
                ));
            }
            config.encrypted_webhook_url = blob_form(&url_plain)?;
        }
    }

    // Phase 1b: destinations. Validate each; skip invalid ones with
    // warnings. Duplicate ids keep the first entry (last-write-wins
    // would silently miscount the report).
    let mut imported_dests: Vec<Destination> = Vec::new();
    let mut seen_dest_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for mut d in file.destinations.clone() {
        if !seen_dest_ids.insert(d.id.clone()) {
            report.destinations_skipped += 1;
            report.warnings.push(format!(
                "Đích đến '{}' bị bỏ qua: id trùng trong file xuất.",
                d.display_name()
            ));
            continue;
        }
        // Portable: decrypt the URL to plaintext in memory (per-URL
        // failure skips only that destination); the blob form goes
        // into the row, the plaintext waits for phase 3.
        if file.mode == ExportMode::Portable && !d.encrypted_url.trim().is_empty() {
            let Some(key) = portable_key.as_ref() else {
                return Err("file xuất portable thiếu khối secret".into());
            };
            match portable_decrypt(&d.encrypted_url, key, &d.display_name()) {
                Ok(plain) => {
                    if !plain.is_empty() {
                        pending_secrets.push((
                            destination_entry(&d.id),
                            plain.clone(),
                            SecretSlot::Destination(d.id.clone()),
                        ));
                    }
                    d.encrypted_url = blob_form(&plain)?;
                }
                Err(e) => {
                    report.destinations_skipped += 1;
                    report
                        .warnings
                        .push(format!("Đích đến '{}' bị bỏ qua: {e}", d.display_name()));
                    continue;
                }
            }
        }
        if file.mode == ExportMode::Safe {
            d.encrypted_url.clear();
            d.enabled = false;
        }
        if let Err(e) = d.validate() {
            report.destinations_skipped += 1;
            report
                .warnings
                .push(format!("Đích đến '{}' bị bỏ qua: {e}", d.display_name()));
            continue;
        }
        // A destination without a URL can never be enabled.
        if d.encrypted_url.trim().is_empty() {
            d.enabled = false;
        }
        imported_dests.push(d);
    }
    if imported_dests.len() > crate::destination::MAX_DESTINATIONS_HARD {
        return Err(format!(
            "file xuất chứa {} đích đến, vượt trần cứng {}",
            imported_dests.len(),
            crate::destination::MAX_DESTINATIONS_HARD
        ));
    }

    // Phase 1c: scenarios. Validate each (destination requirement
    // relaxed — the user picks destinations after import, and ids
    // from the export are preserved so references usually stay
    // valid). Validation uses the user-mode rules for structure +
    // the webhook rules minus the destination requirement; the
    // simplest correct check is: name/timezone/schedule/nodes
    // valid AND (if it has commands, it is a user-mode scenario).
    // Reuse `validate()` for structure, but allow empty channels
    // (webhook scenarios have none): validate a clone with a
    // placeholder channel when the original has none. References to
    // destinations that were not imported are cleared with a
    // warning — after replacement those destinations are gone, so
    // the scenario would silently never fire.
    let dest_ids: std::collections::HashSet<&str> =
        imported_dests.iter().map(|d| d.id.as_str()).collect();
    let mut imported_scenarios: Vec<Scenario> = Vec::new();
    let mut seen_scenario_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for mut s in file.scenarios.clone() {
        if !seen_scenario_ids.insert(s.id.clone()) {
            report.scenarios_skipped += 1;
            report.warnings.push(format!(
                "Kịch bản '{}' bị bỏ qua: id trùng trong file xuất.",
                s.name
            ));
            continue;
        }
        let mut probe = s.clone();
        if probe.channel_id.trim().is_empty() {
            probe.channel_id = "import-placeholder".to_string();
        }
        if let Err(e) = probe.validate() {
            report.scenarios_skipped += 1;
            report
                .warnings
                .push(format!("Kịch bản '{}' bị bỏ qua: {e}", s.name));
            continue;
        }
        // Drop destination ids that were not imported (broadcast
        // lists keep the ids that did arrive); warn so the user
        // can re-pick the missing ones.
        let before = s.destination_ids.len();
        s.destination_ids
            .retain(|id| dest_ids.contains(id.as_str()));
        let dropped = before - s.destination_ids.len();
        if dropped > 0 {
            report.warnings.push(format!(
                "Kịch bản '{}' dùng tới {dropped} đích đến không được nhập; các đích đến đó đã bị xóa khỏi kịch bản.",
                s.name
            ));
        }
        // Call API URLs (idea #5) are per-machine secrets: the
        // exported node carries only a reference, which will not
        // resolve on this machine. The scenario still imports —
        // the node fails closed at send time — but warn so the
        // user re-enters the URL before relying on it.
        let api_nodes = s
            .nodes
            .iter()
            .filter(|n| n.kind == crate::scenario::NodeKind::UtilityCallApi)
            .filter(|n| {
                !crate::scenario::ApiCallConfig::from_node(n)
                    .url_ref
                    .trim()
                    .is_empty()
            })
            .count();
        if api_nodes > 0 {
            report.warnings.push(format!(
                "Kịch bản '{}' có {api_nodes} bước lấy dữ liệu: địa chỉ của chúng là bí mật gắn với máy đã xuất — nhập lại từng địa chỉ trên máy này.",
                s.name
            ));
        }
        imported_scenarios.push(s);
    }

    // Phase 1d: templates. Name + content only; content limit
    // enforced.
    let mut imported_templates: Vec<TemplateEntry> = Vec::new();
    for t in &file.templates {
        if t.name.trim().is_empty() {
            report
                .warnings
                .push("Mẫu không có tên đã bị bỏ qua.".to_string());
            continue;
        }
        if crate::preview::over_limit(&t.content) {
            report.warnings.push(format!(
                "Mẫu '{}' bị bỏ qua: vượt giới hạn 2000 ký tự.",
                t.name
            ));
            continue;
        }
        imported_templates.push(t.clone());
    }

    // Phase 2: atomic replacement. One transaction replaces all
    // three tables, so a failure mid-way cannot leave a
    // half-replaced database. Deletion is table-wide, not
    // load-then-delete: rows too corrupt to parse must not survive
    // a "replace everything" import.
    store.replace_all(&imported_dests, &imported_scenarios, &imported_templates)?;
    // Counters replace with the rest of the setup (same phase: a
    // DB error here fails the import before any secret is stored).
    store.save_counters(&file.counters)?;
    report.destinations_imported = imported_dests.len();
    report.scenarios_imported = imported_scenarios.len();
    report.templates_imported = imported_templates.len();

    // Phase 3: keyring upgrade (portable only; the pending list is
    // empty for other modes). Best-effort: a failure keeps the blob
    // form with a warning — the database already holds valid stored
    // forms, so this can never fail the import.
    for (entry, plain, slot) in &pending_secrets {
        let stored = match store_secret_with(entry, plain, kr) {
            Ok(stored) => stored,
            Err(e) => {
                report.warnings.push(format!(
                    "Secret của '{entry}' giữ nguyên dạng blob mã hóa theo máy: {e}"
                ));
                continue;
            }
        };
        match slot {
            SecretSlot::ConfigToken => config.encrypted_token = stored,
            SecretSlot::ConfigWebhook => config.encrypted_webhook_url = stored,
            SecretSlot::Destination(id) => {
                if let Some(d) = imported_dests.iter_mut().find(|d| &d.id == id) {
                    d.encrypted_url = stored;
                    if let Err(e) = store.save_destination(d) {
                        report.warnings.push(format!(
                            "Đích đến '{}' giữ nguyên URL mã hóa theo máy: {e}",
                            d.display_name()
                        ));
                    }
                }
            }
        }
    }

    // Mark migration done: the imported state IS the destination
    // state (no legacy migration should run over it).
    config.destinations_migrated = true;
    Ok((config, report))
}

/// Decrypt a portable-encrypted value to plaintext. Empty input
/// stays empty. A wrong passphrase fails here with a clear,
/// secret-free error. Pure: no keyring, no database.
fn portable_decrypt(blob: &str, key: &[u8; 32], what: &str) -> Result<String, String> {
    if blob.trim().is_empty() {
        return Ok(String::new());
    }
    decrypt_with_key(key, blob)
        .map_err(|_| format!("không giải mã được {what} — sai mật khẩu hoặc file xuất bị hỏng"))
}

/// Blob form of a plaintext secret: empty stays empty, else a
/// machine-encrypted blob (always a valid stored form; the phase-3
/// upgrade replaces it with a keyring reference when available).
fn blob_form(plain: &str) -> Result<String, String> {
    if plain.is_empty() {
        Ok(String::new())
    } else {
        encrypt_token(plain)
    }
}

/// Maximum import-file size. Exports are JSON config; anything
/// larger is a mistake or hostile, and reading it unbounded spikes
/// memory before serde rejects it.
pub const MAX_IMPORT_BYTES: usize = 16 * 1024 * 1024;

/// Read an import file with a size bound. Used for both config
/// imports and single-scenario imports.
pub fn read_import_file(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    file.take((MAX_IMPORT_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.len() > MAX_IMPORT_BYTES {
        return Err(format!(
            "file nhập lớn hơn giới hạn {} MiB",
            MAX_IMPORT_BYTES / (1024 * 1024)
        ));
    }
    String::from_utf8(buf).map_err(|_| "file nhập không phải UTF-8 hợp lệ".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::encrypt_token;

    fn test_store() -> Store {
        Store::open_memory().unwrap()
    }

    #[test]
    fn safe_export_strips_secrets() {
        let store = test_store();
        let cfg = AppConfig {
            encrypted_token: encrypt_token("tok-123").unwrap(),
            ..AppConfig::default()
        };
        let mut d = Destination::new("D1");
        d.encrypted_url = encrypt_token("https://discord.com/api/webhooks/1/abc").unwrap();
        store.save_destination(&d).unwrap();
        let (file, warnings) = build_export(&store, &cfg, ExportMode::Safe, "").unwrap();
        assert!(warnings.is_empty());
        assert!(file.app_config.encrypted_token.is_empty());
        assert!(file.secrets.is_none());
        assert!(file.destinations[0].encrypted_url.is_empty());
        // The secret values never appear in the serialized file.
        let json = export_to_json(&file).unwrap();
        assert!(!json.contains("tok-123"));
        assert!(!json.contains("webhooks/1/abc"));
    }

    #[test]
    fn portable_roundtrip_restores_secrets() {
        let store = test_store();
        let cfg = AppConfig {
            encrypted_token: encrypt_token("tok-abc").unwrap(),
            ..AppConfig::default()
        };
        let mut d = Destination::new("D1");
        d.encrypted_url = encrypt_token("https://discord.com/api/webhooks/9/xyz").unwrap();
        store.save_destination(&d).unwrap();
        // Low iteration count for test speed; import honors the
        // count recorded in the file.
        let (file, warnings) =
            build_export_with_iterations(&store, &cfg, ExportMode::Portable, "correct horse", 1000)
                .unwrap();
        assert!(warnings.is_empty());
        assert!(file.secrets.is_some());
        // Wrong passphrase fails cleanly, changing nothing.
        assert!(apply_import(&store, &file, "wrong").is_err());
        // Right passphrase restores machine-encrypted values.
        let (imported, report) = apply_import(&store, &file, "correct horse").unwrap();
        assert_eq!(report.destinations_imported, 1);
        assert_eq!(load_secret(&imported.encrypted_token).unwrap(), "tok-abc");
        let dests = store.load_destinations().unwrap();
        assert_eq!(
            load_secret(&dests[0].encrypted_url).unwrap(),
            "https://discord.com/api/webhooks/9/xyz"
        );
    }

    #[test]
    fn import_skips_invalid_entries_with_warnings() {
        let store = test_store();
        let cfg = AppConfig::default();
        let mut bad = Destination::new("");
        bad.name = String::new();
        let (mut file, _) = build_export(&store, &cfg, ExportMode::Safe, "").unwrap();
        file.destinations = vec![bad];
        let mut bad_scenario = Scenario::new("");
        bad_scenario.name = String::new();
        file.scenarios = vec![bad_scenario];
        let (_, report) = apply_import(&store, &file, "").unwrap();
        assert_eq!(report.destinations_skipped, 1);
        assert_eq!(report.scenarios_skipped, 1);
        assert_eq!(report.warnings.len(), 2);
        assert!(store.load_destinations().unwrap().is_empty());
    }

    #[test]
    fn parse_export_rejects_wrong_format_and_version() {
        assert!(parse_export("{}").is_err());
        let (mut file, _) =
            build_export(&test_store(), &AppConfig::default(), ExportMode::Safe, "").unwrap();
        file.version = 99;
        let json = export_to_json(&file).unwrap();
        assert!(parse_export(&json).is_err());
    }

    #[test]
    fn failed_import_changes_nothing_on_keyring_or_db() {
        use crate::secret_store::{Keyring, MockKeyring};
        let store = test_store();
        // Existing state that must survive a failed import.
        let existing = Destination::new("Existing");
        store.save_destination(&existing).unwrap();
        let mut scenario = Scenario::new("Existing scenario");
        scenario.channel_id = "111".to_string();
        store.save_scenario(&scenario).unwrap();
        store.save_template("t1", "hello").unwrap();
        // Live keyring entries that must not be overwritten.
        let kr = MockKeyring::new(true);
        kr.set(ENTRY_ACCOUNT_TOKEN, "OLD-TOKEN").unwrap();
        kr.set(ENTRY_LEGACY_WEBHOOK, "OLD-URL").unwrap();
        // A portable export whose config secrets decrypt fine but
        // which fails the destination ceiling: the old code stored
        // the config secrets into the keyring before this check.
        let cfg = AppConfig {
            encrypted_token: encrypt_token("NEW-TOKEN").unwrap(),
            encrypted_webhook_url: encrypt_token("https://discord.com/api/webhooks/1/new").unwrap(),
            ..AppConfig::default()
        };
        let (mut file, _) =
            build_export_with_iterations(&store, &cfg, ExportMode::Portable, "correct horse", 1000)
                .unwrap();
        for i in 0..20 {
            file.destinations
                .push(Destination::new(&format!("Extra {i}")));
        }
        assert_eq!(file.destinations.len(), 21);
        let err = apply_import_with(&store, &file, "correct horse", &kr).unwrap_err();
        assert!(err.contains("trần cứng"), "unexpected error: {err}");
        // Keyring untouched.
        assert_eq!(kr.get(ENTRY_ACCOUNT_TOKEN).unwrap(), "OLD-TOKEN");
        assert_eq!(kr.get(ENTRY_LEGACY_WEBHOOK).unwrap(), "OLD-URL");
        // Database untouched.
        assert_eq!(store.load_destinations().unwrap().len(), 1);
        assert_eq!(store.load_scenarios().unwrap().len(), 1);
        assert_eq!(store.load_templates().unwrap().len(), 1);
    }

    #[test]
    fn successful_portable_import_upgrades_secrets_to_keyring() {
        use crate::secret_store::{load_secret_with, Keyring, MockKeyring};
        let store = test_store();
        let cfg = AppConfig {
            encrypted_token: encrypt_token("tok-abc").unwrap(),
            encrypted_webhook_url: encrypt_token("https://discord.com/api/webhooks/1/abc").unwrap(),
            ..AppConfig::default()
        };
        let mut d = Destination::new("D1");
        d.encrypted_url = encrypt_token("https://discord.com/api/webhooks/9/xyz").unwrap();
        store.save_destination(&d).unwrap();
        let (file, _) =
            build_export_with_iterations(&store, &cfg, ExportMode::Portable, "correct horse", 1000)
                .unwrap();
        let kr = MockKeyring::new(true);
        let (imported, report) = apply_import_with(&store, &file, "correct horse", &kr).unwrap();
        assert!(report.warnings.is_empty());
        // Keyring now holds the imported secrets under the fixed entries.
        assert_eq!(kr.get(ENTRY_ACCOUNT_TOKEN).unwrap(), "tok-abc");
        assert_eq!(
            kr.get(ENTRY_LEGACY_WEBHOOK).unwrap(),
            "https://discord.com/api/webhooks/1/abc"
        );
        // The returned config + rows resolve through the keyring.
        assert_eq!(
            load_secret_with(&imported.encrypted_token, &kr).unwrap(),
            "tok-abc"
        );
        let dests = store.load_destinations().unwrap();
        assert_eq!(dests.len(), 1);
        assert_eq!(
            load_secret_with(&dests[0].encrypted_url, &kr).unwrap(),
            "https://discord.com/api/webhooks/9/xyz"
        );
        assert_eq!(
            kr.get(&destination_entry(&dests[0].id)).unwrap(),
            "https://discord.com/api/webhooks/9/xyz"
        );
    }

    #[test]
    fn portable_import_without_keyring_keeps_blobs() {
        use crate::secret_store::{load_secret_with, MockKeyring};
        let store = test_store();
        let cfg = AppConfig {
            encrypted_token: encrypt_token("tok-abc").unwrap(),
            ..AppConfig::default()
        };
        let (file, _) =
            build_export_with_iterations(&store, &cfg, ExportMode::Portable, "correct horse", 1000)
                .unwrap();
        let kr = MockKeyring::new(false);
        let (imported, _) = apply_import_with(&store, &file, "correct horse", &kr).unwrap();
        assert!(!imported.encrypted_token.starts_with("kr:"));
        assert_eq!(
            load_secret_with(&imported.encrypted_token, &kr).unwrap(),
            "tok-abc"
        );
    }

    #[test]
    fn import_clears_dangling_destination_ids() {
        let store = test_store();
        let (mut file, _) =
            build_export(&store, &AppConfig::default(), ExportMode::Safe, "").unwrap();
        let mut s = Scenario::new("Orphaned");
        s.channel_id = "111".to_string();
        s.destination_ids = vec!["no-such-destination".to_string()];
        file.scenarios = vec![s];
        let (_, report) = apply_import(&store, &file, "").unwrap();
        let loaded = store.load_scenarios().unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].destination_ids.is_empty());
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("không được nhập")));
    }

    #[test]
    fn safe_import_clears_webhook_metadata() {
        let store = test_store();
        let (mut file, _) =
            build_export(&store, &AppConfig::default(), ExportMode::Safe, "").unwrap();
        file.app_config.webhook_id = "123".to_string();
        file.app_config.webhook_name = "Hook".to_string();
        file.app_config.webhook_health = WebhookHealth::Ok;
        file.app_config.webhook_last_checked_at = "2026-01-01".to_string();
        let (imported, _) = apply_import(&store, &file, "").unwrap();
        assert!(imported.webhook_id.is_empty());
        assert!(imported.webhook_name.is_empty());
        assert!(matches!(imported.webhook_health, WebhookHealth::Unknown));
        assert!(imported.webhook_last_checked_at.is_empty());
    }

    #[test]
    fn import_dedupes_duplicate_ids() {
        let store = test_store();
        let (mut file, _) =
            build_export(&store, &AppConfig::default(), ExportMode::Safe, "").unwrap();
        let d = Destination::new("Dup");
        file.destinations = vec![d.clone(), d];
        let mut s = Scenario::new("Dup scenario");
        s.channel_id = "111".to_string();
        file.scenarios = vec![s.clone(), s];
        let (_, report) = apply_import(&store, &file, "").unwrap();
        assert_eq!(report.destinations_imported, 1);
        assert_eq!(report.scenarios_imported, 1);
        assert_eq!(report.destinations_skipped, 1);
        assert_eq!(report.scenarios_skipped, 1);
    }

    #[test]
    fn export_import_carries_counters_and_old_files_still_parse() {
        let store = test_store();
        let mut counters = std::collections::HashMap::new();
        counters.insert("day".to_string(), 7);
        store.save_counters(&counters).unwrap();
        let cfg = AppConfig::default();
        let (file, _) = build_export(&store, &cfg, ExportMode::Local, "").unwrap();
        assert_eq!(file.counters.get("day"), Some(&7));
        // Import into a fresh store replaces the counter set.
        let fresh = test_store();
        fresh
            .save_counters(&{
                let mut old = std::collections::HashMap::new();
                old.insert("stale".to_string(), 99);
                old
            })
            .unwrap();
        apply_import(&fresh, &file, "").unwrap();
        assert_eq!(fresh.get_counter("day"), 7);
        assert_eq!(fresh.get_counter("stale"), 1);
        // An export file without the counters field (older version)
        // still parses, with an empty counter set.
        let json = serde_json::json!({
            "format": EXPORT_FORMAT,
            "version": EXPORT_VERSION,
            "exported_at": "2026-01-01T00:00:00+00:00",
            "mode": "local",
            "app_config": AppConfig::default(),
            "destinations": [],
            "scenarios": [],
            "templates": []
        });
        let file = parse_export(&serde_json::to_string(&json).unwrap()).unwrap();
        assert!(file.counters.is_empty());
    }

    #[test]
    fn auto_backup_due_covers_never_recent_old_and_garbage() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-18T00:00:00+00:00")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(auto_backup_due("", 1, now));
        assert!(auto_backup_due("not a timestamp", 1, now));
        assert!(!auto_backup_due("2026-11-17T12:00:00+00:00", 1, now));
        assert!(auto_backup_due("2026-11-17T00:00:00+00:00", 1, now));
        assert!(!auto_backup_due("2026-11-17T00:00:00+00:00", 7, now));
        assert!(auto_backup_due("2026-11-17T12:00:00+00:00", 0, now));
    }

    #[test]
    fn prune_auto_backups_keeps_newest_and_ignores_other_files() {
        let dir = std::env::temp_dir().join(format!("nicord-prune-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "nicord-backup-20260101-000000.json",
            "nicord-backup-20260102-000000.json",
            "nicord-backup-20260103-000000.json",
            "keep-me.txt",
        ] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        prune_auto_backups(&dir, 2);
        assert!(!dir.join("nicord-backup-20260101-000000.json").exists());
        assert!(dir.join("nicord-backup-20260102-000000.json").exists());
        assert!(dir.join("nicord-backup-20260103-000000.json").exists());
        assert!(dir.join("keep-me.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_local_backup_roundtrips_through_parse_export() {
        let store = test_store();
        let cfg = AppConfig::default();
        let path =
            std::env::temp_dir().join(format!("nicord-local-backup-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let warnings = write_local_backup(&store, &cfg, &path).unwrap();
        assert!(warnings.is_empty());
        let text = std::fs::read_to_string(&path).unwrap();
        let file = parse_export(&text).unwrap();
        assert_eq!(file.format, EXPORT_FORMAT);
        assert_eq!(file.version, EXPORT_VERSION);
        let _ = std::fs::remove_file(&path);
    }
}
