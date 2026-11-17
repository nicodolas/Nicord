//! Config export/import for backup and cross-machine use.
//!
//! The problem: secrets in `config.json` and the destinations table
//! are encrypted with a machine-tied key (see `crypto::machine_key`),
//! so copying those files to another machine leaves the secrets
//! undecryptable. Portable exports solve this by re-encrypting every
//! secret under a passphrase-derived key (PBKDF2-HMAC-SHA256,
//! 600k iterations, AES-256-GCM). Import decrypts with the
//! passphrase and re-encrypts with the LOCAL machine key.
//!
//! Three modes:
//! - `portable`: secrets included, passphrase-encrypted. For moving
//!   to another machine. The file MUST be treated like a password
//!   vault — a weak passphrase means weak protection.
//! - `safe`: secrets omitted (destinations arrive without URLs,
//!   token/webhook fields empty). For sharing a setup or a public
//!   backup. The user re-enters credentials on the other machine.
//! - `local`: secrets kept machine-encrypted (copied as-is). Only
//!   restorable on the SAME machine. Used for the automatic backup
//!   taken before an import replaces the current config.
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
    crypto::{
        decrypt_token, decrypt_with_key, encrypt_token, encrypt_with_key, pbkdf2_hmac_sha256,
        random_salt,
    },
    destination::Destination,
    scenario::Scenario,
    store::{AppConfig, Store},
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
/// required for portable mode, ignored otherwise.
pub fn build_export(
    store: &Store,
    config: &AppConfig,
    mode: ExportMode,
    passphrase: &str,
) -> Result<ExportFile, String> {
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
) -> Result<ExportFile, String> {
    if mode == ExportMode::Portable && passphrase.is_empty() {
        return Err("a passphrase is required for a portable export".into());
    }
    let mut app_config = config.clone();
    // Secrets never live in `app_config` inside the file.
    app_config.encrypted_token.clear();
    app_config.encrypted_webhook_url.clear();

    let destinations = store.load_destinations()?;
    let scenarios = store.load_scenarios()?;
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
                    &format!("destination '{}'", d.display_name()),
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
                    "cannot export these secrets (they may belong to another machine): {}",
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

    Ok(ExportFile {
        format: EXPORT_FORMAT.to_string(),
        version: EXPORT_VERSION,
        exported_at: chrono::Utc::now().to_rfc3339(),
        mode,
        app_config,
        secrets,
        destinations,
        scenarios,
        templates,
    })
}

/// Decrypt a machine-encrypted value and re-encrypt it under the
/// export key. Empty input stays empty. Failures append a warning
/// and yield an empty string (the caller decides whether to abort).
fn portable_reencrypt(
    machine_blob: &str,
    key: &[u8; 32],
    warnings: &mut Vec<String>,
    what: &str,
) -> String {
    if machine_blob.trim().is_empty() {
        return String::new();
    }
    match decrypt_token(machine_blob).and_then(|plain| encrypt_with_key(key, &plain)) {
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

/// Parse and validate an export file's envelope (format/version).
/// Returns the parsed file; secret decryption happens in `apply_import`.
pub fn parse_export(text: &str) -> Result<ExportFile, String> {
    let file: ExportFile =
        serde_json::from_str(text).map_err(|e| format!("not a valid Nicord export file: {e}"))?;
    if file.format != EXPORT_FORMAT {
        return Err(format!(
            "unknown export format '{}' (expected '{EXPORT_FORMAT}')",
            file.format
        ));
    }
    if file.version != EXPORT_VERSION {
        return Err(format!(
            "unsupported export version {} (this app reads version {EXPORT_VERSION})",
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
    let mut report = ImportReport::default();
    let mut config = file.app_config.clone();

    // Portable mode: derive the key ONCE (PBKDF2 is deliberately
    // slow) and reuse it for the secrets block + every destination.
    // The iteration count comes from the file, bounded to reject
    // denial-of-service values (a hostile file asking for billions
    // of iterations must not hang the app).
    let portable_key: Option<[u8; 32]> = if file.mode == ExportMode::Portable {
        let secrets = file
            .secrets
            .as_ref()
            .ok_or("portable export is missing its secrets block")?;
        if secrets.kdf != "pbkdf2-hmac-sha256" {
            return Err(format!("unsupported KDF '{}'", secrets.kdf));
        }
        if !(1..=2_000_000).contains(&secrets.iterations) {
            return Err(format!(
                "unsupported KDF iteration count {}",
                secrets.iterations
            ));
        }
        if passphrase.is_empty() {
            return Err("a passphrase is required to import a portable export".into());
        }
        let salt = B64
            .decode(&secrets.salt_b64)
            .map_err(|_| "portable export has an invalid salt".to_string())?;
        Some(export_key(passphrase, &salt, secrets.iterations))
    } else {
        None
    };

    // Secrets: resolve to machine-encrypted values (or empty).
    match file.mode {
        ExportMode::Safe => {
            config.encrypted_token.clear();
            config.encrypted_webhook_url.clear();
        }
        ExportMode::Local => {
            // Values are already machine-encrypted for this machine.
            // Keep them as-is (they came from `app_config`).
        }
        ExportMode::Portable => {
            let secrets = file.secrets.as_ref().unwrap();
            let key = portable_key.as_ref().unwrap();
            config.encrypted_token = portable_decrypt(&secrets.token, key, "account token")?;
            config.encrypted_webhook_url =
                portable_decrypt(&secrets.webhook_url, key, "webhook URL")?;
        }
    }

    // Destinations: validate each; skip invalid ones with warnings.
    let mut imported_dests: Vec<Destination> = Vec::new();
    for mut d in file.destinations.clone() {
        // Portable: re-encrypt the URL under the machine key
        // (reusing the key derived once above).
        if file.mode == ExportMode::Portable && !d.encrypted_url.trim().is_empty() {
            let key = portable_key.as_ref().unwrap();
            match portable_decrypt(&d.encrypted_url, key, &d.display_name()) {
                Ok(machine_blob) => d.encrypted_url = machine_blob,
                Err(e) => {
                    report.destinations_skipped += 1;
                    report
                        .warnings
                        .push(format!("Destination '{}' skipped: {e}", d.display_name()));
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
                .push(format!("Destination '{}' skipped: {e}", d.display_name()));
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
            "export contains {} destinations, over the hard ceiling of {}",
            imported_dests.len(),
            crate::destination::MAX_DESTINATIONS_HARD
        ));
    }

    // Scenarios: validate each (destination requirement relaxed —
    // the user picks destinations after import, and ids from the
    // export are preserved so references usually stay valid).
    // Validation uses the user-mode rules for structure + the
    // webhook rules minus the destination requirement; the
    // simplest correct check is: name/timezone/schedule/nodes
    // valid AND (if it has commands, it is a user-mode scenario).
    // Reuse `validate()` for structure, but allow empty channels
    // (webhook scenarios have none): validate a clone with a
    // placeholder channel when the original has none.
    let mut imported_scenarios: Vec<Scenario> = Vec::new();
    for s in file.scenarios.clone() {
        let mut probe = s.clone();
        if probe.channel_id.trim().is_empty() {
            probe.channel_id = "import-placeholder".to_string();
        }
        if let Err(e) = probe.validate() {
            report.scenarios_skipped += 1;
            report
                .warnings
                .push(format!("Scenario '{}' skipped: {e}", s.name));
            continue;
        }
        imported_scenarios.push(s);
    }

    // Templates: name + content only; content limit enforced.
    let mut imported_templates: Vec<TemplateEntry> = Vec::new();
    for t in &file.templates {
        if t.name.trim().is_empty() {
            report
                .warnings
                .push("Template with an empty name skipped.".to_string());
            continue;
        }
        if crate::preview::over_limit(&t.content) {
            report.warnings.push(format!(
                "Template '{}' skipped: over the 2000-character limit.",
                t.name
            ));
            continue;
        }
        imported_templates.push(t.clone());
    }
    report.templates_imported = imported_templates.len();

    // Apply the replacement: delete existing rows, insert imported.
    // Order matters: scenarios reference destinations by id, but
    // there is no foreign key, so any order works. Destinations
    // first, so a failure mid-way leaves the old scenarios pointing
    // at destinations that still exist (they are replaced right after).
    let existing_dests = store.load_destinations()?;
    for d in &existing_dests {
        store.delete_destination(&d.id)?;
    }
    for d in &imported_dests {
        store.save_destination(d)?;
    }
    report.destinations_imported = imported_dests.len();

    let existing_scenarios = store.load_scenarios()?;
    for s in &existing_scenarios {
        store.delete_scenario(&s.id)?;
    }
    for s in &imported_scenarios {
        store.save_scenario(s)?;
    }
    report.scenarios_imported = imported_scenarios.len();

    let existing_templates = store.load_templates()?;
    for (id, _, _) in &existing_templates {
        store.delete_template(id)?;
    }
    for t in &imported_templates {
        store.save_template(&t.name, &t.content)?;
    }

    // Mark migration done: the imported state IS the destination
    // state (no legacy migration should run over it).
    config.destinations_migrated = true;
    Ok((config, report))
}

/// Decrypt a portable-encrypted value and re-encrypt it under the
/// machine key. Empty input stays empty. A wrong passphrase fails
/// here with a clear, secret-free error.
fn portable_decrypt(blob: &str, key: &[u8; 32], what: &str) -> Result<String, String> {
    if blob.trim().is_empty() {
        return Ok(String::new());
    }
    let plain = decrypt_with_key(key, blob)
        .map_err(|_| format!("cannot decrypt {what} — wrong passphrase or corrupt export file"))?;
    encrypt_token(&plain).map_err(|e| format!("cannot re-encrypt {what}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let file = build_export(&store, &cfg, ExportMode::Safe, "").unwrap();
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
        let file =
            build_export_with_iterations(&store, &cfg, ExportMode::Portable, "correct horse", 1000)
                .unwrap();
        assert!(file.secrets.is_some());
        // Wrong passphrase fails cleanly, changing nothing.
        assert!(apply_import(&store, &file, "wrong").is_err());
        // Right passphrase restores machine-encrypted values.
        let (imported, report) = apply_import(&store, &file, "correct horse").unwrap();
        assert_eq!(report.destinations_imported, 1);
        assert_eq!(decrypt_token(&imported.encrypted_token).unwrap(), "tok-abc");
        let dests = store.load_destinations().unwrap();
        assert_eq!(
            decrypt_token(&dests[0].encrypted_url).unwrap(),
            "https://discord.com/api/webhooks/9/xyz"
        );
    }

    #[test]
    fn import_skips_invalid_entries_with_warnings() {
        let store = test_store();
        let cfg = AppConfig::default();
        let mut bad = Destination::new("");
        bad.name = String::new();
        let mut file = build_export(&store, &cfg, ExportMode::Safe, "").unwrap();
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
        let mut file =
            build_export(&test_store(), &AppConfig::default(), ExportMode::Safe, "").unwrap();
        file.version = 99;
        let json = export_to_json(&file).unwrap();
        assert!(parse_export(&json).is_err());
    }
}
