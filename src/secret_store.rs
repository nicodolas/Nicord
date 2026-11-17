//! Secret storage: OS keyring first, machine-encrypted file fallback.
//!
//! Every stored secret value is one of:
//!
//! - `""` (empty): no secret stored.
//! - `"kr:<entry>"`: the secret lives in the OS keyring (Windows
//!   Credential Manager, macOS Keychain, Linux Secret Service)
//!   under service `"Nicord"` and username `<entry>`. The stored
//!   string holds no secret material.
//! - anything else: a machine-encrypted blob, base64(nonce ||
//!   ciphertext) from `crypto.rs`.
//!
//! The `"kr:"` prefix can never collide with a real blob: base64
//! output contains no `':'`, so `strip_prefix("kr:")` only matches
//! references this module wrote.
//!
//! Reads (`load_secret`) resolve both forms. Writes
//! (`store_secret`) try the keyring first and fall back to a blob
//! when the keyring is unavailable or the write fails, so the app
//! keeps working exactly as before on machines without a
//! credential store (headless Linux, locked keychain). A one-time
//! migration (`migrate_secret`, run at startup) moves existing
//! blobs into the keyring when it is available, verifying the
//! round-trip before committing to the reference; anything it
//! cannot migrate stays a blob and keeps working.
//!
//! A `kr:` reference whose keyring entry cannot be read fails
//! closed with a clear error: the caller treats it as "no usable
//! credential" and the user can re-enter the secret (the write
//! path falls back to a blob, so re-entering always works). There
//! is no lockout: no migration ever deletes a blob before its
//! keyring round-trip is verified.
//!
//! Availability is probed via `keyring::Entry::store_status`,
//! which initializes the platform store once without prompting or
//! writing anything. The `Keyring` trait exists so tests can run
//! against an in-memory mock instead of the OS store; production
//! always uses `OsKeyring`.
//!
//! Threading: every OS-keyring operation runs on a fresh plain
//! thread, serialized behind one mutex (see `on_keyring_thread`).
//! The Linux backend (zbus blocking API) runs its own executor
//! and must not be called from an async context, but Nicord
//! resolves senders from inside the engine's async loop — a
//! dedicated thread is never in an async context, on any
//! platform. Operations are infrequent (at most one per scenario
//! per engine tick, plus explicit UI actions), so a thread spawn
//! per operation is negligible.

use crate::crypto::{decrypt_token, encrypt_token};

/// Keyring service name: every Nicord entry lives under it.
pub const KEYRING_SERVICE: &str = "Nicord";

/// Keyring entry for the account token (user-token mode).
pub const ENTRY_ACCOUNT_TOKEN: &str = "account-token";

/// Keyring entry for the legacy single webhook URL (config-level;
/// destinations have their own entries).
pub const ENTRY_LEGACY_WEBHOOK: &str = "legacy-webhook";

/// Keyring entry for one destination's webhook URL.
pub fn destination_entry(dest_id: &str) -> String {
    format!("destination:{dest_id}")
}

/// Prefix marking a stored value as a keyring reference.
const KEYRING_PREFIX: &str = "kr:";

/// Minimal keyring operations Nicord needs. `OsKeyring` talks to
/// the platform store; tests use an in-memory mock.
pub trait Keyring {
    fn available(&self) -> bool;
    fn get(&self, entry: &str) -> Result<String, String>;
    fn set(&self, entry: &str, secret: &str) -> Result<(), String>;
    fn delete(&self, entry: &str) -> Result<(), String>;
}

/// The OS credential store, via the `keyring` crate (v1 API).
pub struct OsKeyring;

/// Serializes all OS-keyring access: platform stores may misbehave
/// under concurrent access to one entry, and the operations are
/// infrequent enough that serializing costs nothing measurable.
static KEYRING_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` on a fresh plain thread, serialized behind
/// `KEYRING_MUTEX`, and return its result. A panic on the worker
/// thread becomes an error instead of crashing the caller: secret
/// reads fail closed and secret writes fall back to a blob.
fn on_keyring_thread<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let _guard = KEYRING_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::spawn(f)
        .join()
        .map_err(|_| "OS keyring operation failed (worker thread panicked)".to_string())
}

impl Keyring for OsKeyring {
    fn available(&self) -> bool {
        // Store initialization itself talks to the platform store
        // (on Linux, it connects to D-Bus), so it goes through the
        // keyring thread like every other operation.
        on_keyring_thread(|| keyring::Entry::store_status().is_ok()).unwrap_or(false)
    }

    fn get(&self, entry: &str) -> Result<String, String> {
        let entry = entry.to_string();
        on_keyring_thread(move || {
            let e = keyring::Entry::new(KEYRING_SERVICE, &entry).map_err(|e| e.to_string())?;
            e.get_password().map_err(|e| e.to_string())
        })?
    }

    fn set(&self, entry: &str, secret: &str) -> Result<(), String> {
        let entry = entry.to_string();
        let secret = secret.to_string();
        on_keyring_thread(move || {
            let e = keyring::Entry::new(KEYRING_SERVICE, &entry).map_err(|e| e.to_string())?;
            e.set_password(&secret).map_err(|e| e.to_string())
        })?
    }

    fn delete(&self, entry: &str) -> Result<(), String> {
        let entry = entry.to_string();
        on_keyring_thread(move || {
            let e = keyring::Entry::new(KEYRING_SERVICE, &entry).map_err(|e| e.to_string())?;
            e.delete_credential().map_err(|e| e.to_string())
        })?
    }
}

/// Resolve a stored secret to plaintext: keyring references via the
/// OS keyring, anything else as a machine-encrypted blob. Empty
/// input errors, matching `decrypt_token`'s behaviour.
pub fn load_secret(stored: &str) -> Result<String, String> {
    load_secret_with(stored, &OsKeyring)
}

pub fn load_secret_with(stored: &str, kr: &dyn Keyring) -> Result<String, String> {
    if let Some(entry) = stored.strip_prefix(KEYRING_PREFIX) {
        if entry.is_empty() {
            return Err("invalid keyring reference".to_string());
        }
        return kr
            .get(entry)
            .map_err(|e| format!("cannot read secret from the OS keyring: {e}"));
    }
    decrypt_token(stored)
}

/// Store a secret under `entry`, returning the value to persist:
/// a `kr:` reference when the keyring accepted it, otherwise a
/// machine-encrypted blob. Never fails closed on keyring trouble:
/// the secret must still be saved.
pub fn store_secret(entry: &str, plaintext: &str) -> Result<String, String> {
    store_secret_with(entry, plaintext, &OsKeyring)
}

pub fn store_secret_with(entry: &str, plaintext: &str, kr: &dyn Keyring) -> Result<String, String> {
    if kr.available() {
        match kr.set(entry, plaintext) {
            Ok(()) => return Ok(format!("{KEYRING_PREFIX}{entry}")),
            Err(e) => {
                tracing::warn!(
                    "keyring write failed ({e}); falling back to machine-encrypted storage"
                );
            }
        }
    }
    encrypt_token(plaintext)
}

/// Remove a stored secret from the keyring, if it is a reference.
/// Best-effort: errors are ignored (a stale entry is harmless, and
/// deletion must never block the UI flow that triggered it).
/// Blobs need no cleanup — deleting the row/file removes them.
pub fn delete_secret(stored: &str) {
    delete_secret_with(stored, &OsKeyring);
}

pub fn delete_secret_with(stored: &str, kr: &dyn Keyring) {
    if let Some(entry) = stored.strip_prefix(KEYRING_PREFIX) {
        if !entry.is_empty() {
            let _ = kr.delete(entry);
        }
    }
}

/// Migrate one stored blob into the keyring: returns the new
/// `kr:` reference when the secret was written AND read back
/// successfully, otherwise `None` (keep the current value).
/// Empty values and existing references are left alone, as are
/// blobs that cannot be decrypted (e.g. from another machine).
pub fn migrate_secret(stored: &str, entry: &str) -> Option<String> {
    migrate_secret_with(stored, entry, &OsKeyring)
}

pub fn migrate_secret_with(stored: &str, entry: &str, kr: &dyn Keyring) -> Option<String> {
    if stored.is_empty() || stored.starts_with(KEYRING_PREFIX) {
        return None;
    }
    if !kr.available() {
        return None;
    }
    let plain = decrypt_token(stored).ok()?;
    kr.set(entry, &plain).ok()?;
    if kr.get(entry).ok().as_deref() != Some(plain.as_str()) {
        let _ = kr.delete(entry);
        return None;
    }
    Some(format!("{KEYRING_PREFIX}{entry}"))
}

#[cfg(test)]
pub struct MockKeyring {
    available: bool,
    fail_sets: bool,
    map: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

#[cfg(test)]
impl MockKeyring {
    pub fn new(available: bool) -> Self {
        Self {
            available,
            fail_sets: false,
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn failing_sets() -> Self {
        Self {
            available: true,
            fail_sets: true,
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn contains(&self, entry: &str) -> bool {
        self.map.lock().unwrap().contains_key(entry)
    }
}

#[cfg(test)]
impl Keyring for MockKeyring {
    fn available(&self) -> bool {
        self.available
    }

    fn get(&self, entry: &str) -> Result<String, String> {
        self.map
            .lock()
            .unwrap()
            .get(entry)
            .cloned()
            .ok_or_else(|| "No matching credential found".to_string())
    }

    fn set(&self, entry: &str, secret: &str) -> Result<(), String> {
        if self.fail_sets {
            return Err("mock write failure".to_string());
        }
        self.map
            .lock()
            .unwrap()
            .insert(entry.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, entry: &str) -> Result<(), String> {
        self.map.lock().unwrap().remove(entry);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_fallback_roundtrip_when_keyring_unavailable() {
        let kr = MockKeyring::new(false);
        let stored = store_secret_with(ENTRY_ACCOUNT_TOKEN, "tok-123", &kr).unwrap();
        assert!(!stored.starts_with(KEYRING_PREFIX));
        assert!(!kr.contains(ENTRY_ACCOUNT_TOKEN));
        assert_eq!(load_secret_with(&stored, &kr).unwrap(), "tok-123");
    }

    #[test]
    fn keyring_roundtrip_when_available() {
        let kr = MockKeyring::new(true);
        let stored = store_secret_with(ENTRY_ACCOUNT_TOKEN, "tok-123", &kr).unwrap();
        assert_eq!(stored, format!("{KEYRING_PREFIX}{ENTRY_ACCOUNT_TOKEN}"));
        assert!(kr.contains(ENTRY_ACCOUNT_TOKEN));
        assert_eq!(load_secret_with(&stored, &kr).unwrap(), "tok-123");
        // The stored reference holds no secret material.
        assert!(!stored.contains("tok-123"));
    }

    #[test]
    fn keyring_write_failure_falls_back_to_blob() {
        let kr = MockKeyring::failing_sets();
        let stored = store_secret_with(ENTRY_ACCOUNT_TOKEN, "tok-123", &kr).unwrap();
        assert!(!stored.starts_with(KEYRING_PREFIX));
        assert_eq!(load_secret_with(&stored, &kr).unwrap(), "tok-123");
    }

    #[test]
    fn keyring_reference_fails_closed_when_unavailable_or_missing() {
        let kr = MockKeyring::new(false);
        let err = load_secret_with("kr:account-token", &kr).unwrap_err();
        assert!(err.contains("OS keyring"), "{err}");
        let kr = MockKeyring::new(true);
        let err = load_secret_with("kr:account-token", &kr).unwrap_err();
        assert!(err.contains("No matching credential"), "{err}");
        assert!(load_secret_with("kr:", &kr).is_err());
        assert!(load_secret_with("", &kr).is_err());
    }

    #[test]
    fn migration_moves_blob_to_keyring_and_verifies() {
        let kr = MockKeyring::new(true);
        let blob = encrypt_token("webhook-url").unwrap();
        let migrated = migrate_secret_with(&blob, ENTRY_LEGACY_WEBHOOK, &kr).unwrap();
        assert_eq!(migrated, format!("{KEYRING_PREFIX}{ENTRY_LEGACY_WEBHOOK}"));
        assert_eq!(load_secret_with(&migrated, &kr).unwrap(), "webhook-url");
        // Idempotent: references and empties are left alone.
        assert!(migrate_secret_with(&migrated, ENTRY_LEGACY_WEBHOOK, &kr).is_none());
        assert!(migrate_secret_with("", ENTRY_LEGACY_WEBHOOK, &kr).is_none());
        // Undecryptable blobs stay put.
        assert!(migrate_secret_with("not-a-blob!!!", ENTRY_LEGACY_WEBHOOK, &kr).is_none());
    }

    #[test]
    fn migration_is_noop_without_keyring() {
        let kr = MockKeyring::new(false);
        let blob = encrypt_token("webhook-url").unwrap();
        assert!(migrate_secret_with(&blob, ENTRY_LEGACY_WEBHOOK, &kr).is_none());
    }

    #[test]
    fn delete_secret_removes_keyring_entry_only() {
        let kr = MockKeyring::new(true);
        let stored = store_secret_with("destination:abc", "url", &kr).unwrap();
        delete_secret_with(&stored, &kr);
        assert!(!kr.contains("destination:abc"));
        // Deleting a blob is a harmless no-op.
        let blob = encrypt_token("x").unwrap();
        delete_secret_with(&blob, &kr);
    }

    #[tokio::test]
    async fn os_keyring_calls_from_async_context_do_not_panic() {
        // This test runs inside a tokio runtime: the OS-keyring
        // path must never panic here (operations run on a
        // dedicated thread). The result depends on the machine —
        // this VM has no Secret Service daemon, so reads fail —
        // but the calls must return either way.
        let _ = load_secret("kr:nicord-test-nonexistent-entry");
        let _ = OsKeyring.available();
    }

    #[test]
    fn destination_entries_are_namespaced() {
        assert_eq!(
            destination_entry("550e8400-e29b-41d4-a716-446655440000"),
            "destination:550e8400-e29b-41d4-a716-446655440000"
        );
    }
}
