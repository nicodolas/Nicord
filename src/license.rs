//! License keys (Spec §8).
//! Format: `base64(payload).base64(signature)` — payload is JSON, signed by
//! Nico's Ed25519 private key (kept offline). The app embeds ONLY the public
//! key and verifies offline. A key unlocks features; it does NOT contain the
//! Discord token and does NOT log the user in.
//!
//! HONEST LIMIT (must stay in README too): with open-source client code, a
//! determined user can rebuild the app and skip this check. The key is for
//! honest buyers; the real paid value is good UX, updates, templates, and
//! future server-side services (cloud runner, sync), not this check alone.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// **TO_REPLACE** — generate a real Ed25519 keypair offline with
/// `cargo run --bin nicord-keygen` (see docs/ARCHITECTURE.md) and paste the
/// 32-byte public key here as a byte array. Until then every key is rejected.
pub const EMBEDDED_PUBLIC_KEY: [u8; 32] = [0u8; 32];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Plan {
    Free,
    Pro,
    Team,
}

#[allow(dead_code)]
impl Plan {
    pub fn max_channels(&self) -> usize {
        match self {
            Plan::Free => 1,
            Plan::Pro => 20,
            Plan::Team => 20,
        }
    }
    pub fn max_running_scenarios(&self) -> usize {
        match self {
            Plan::Free => 1,
            Plan::Pro => 20,
            Plan::Team => 20,
        }
    }
    pub fn max_accounts(&self) -> usize {
        match self {
            Plan::Free => 1,
            Plan::Pro => 3,
            Plan::Team => 5,
        }
    }
    pub fn daily_cap(&self) -> u32 {
        match self {
            Plan::Free => 100,
            Plan::Pro | Plan::Team => 1000,
        }
    }
    pub fn has_advanced_cron(&self) -> bool {
        matches!(self, Plan::Pro | Plan::Team)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicensePayload {
    pub plan: Plan,
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub machine_id: Option<String>,
    #[serde(default = "default_key_version")]
    pub key_version: u32,
}

fn default_key_version() -> u32 {
    1
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LicenseInfo {
    pub plan: Plan,
    pub expires_at: Option<DateTime<Utc>>,
    /// True when expired but inside the 7-day grace period (Spec §8).
    pub in_grace_period: bool,
}

pub fn is_placeholder_key() -> bool {
    EMBEDDED_PUBLIC_KEY == [0u8; 32]
}

/// Verify a license key offline. Returns the plan, or an error string.
/// Expired keys fall back to Free (with a 7-day grace period for running
/// scenarios); the app is never hard-locked by a bad key.
pub fn verify_license_key(key: &str) -> Result<LicenseInfo, String> {
    if is_placeholder_key() {
        return Err(
            "no real public key embedded yet (EMBEDDED_PUBLIC_KEY is a placeholder); \
             generate a keypair and replace it — see docs/ARCHITECTURE.md"
                .to_string(),
        );
    }
    let (payload_b64, sig_b64) = key
        .trim()
        .split_once('.')
        .ok_or_else(|| "license key must be base64(payload).base64(signature)".to_string())?;
    let payload_bytes = B64
        .decode(payload_b64)
        .map_err(|_| "license payload is not valid base64".to_string())?;
    let sig_bytes = B64
        .decode(sig_b64)
        .map_err(|_| "license signature is not valid base64".to_string())?;
    let verifying_key = VerifyingKey::from_bytes(&EMBEDDED_PUBLIC_KEY)
        .map_err(|e| format!("embedded public key is invalid: {e}"))?;
    let signature = Signature::from_bytes(
        &sig_bytes
            .try_into()
            .map_err(|_| "license signature must be 64 bytes".to_string())?,
    );
    verifying_key
        .verify(&payload_bytes, &signature)
        .map_err(|_| "license signature does not match — invalid key".to_string())?;
    let payload: LicensePayload = serde_json::from_slice(&payload_bytes)
        .map_err(|e| format!("license payload is not valid JSON: {e}"))?;
    if payload.key_version == 0 {
        return Err("license key version is revoked".to_string());
    }
    let now = Utc::now();
    let in_grace_period = match payload.expires_at {
        Some(exp) if exp < now => {
            // 7-day grace period after expiry.
            now.signed_duration_since(exp).num_days() <= 7
        }
        _ => false,
    };
    if let Some(exp) = payload.expires_at {
        if exp < now && !in_grace_period {
            return Err("license key expired (grace period over) — falling back to Free".into());
        }
    }
    Ok(LicenseInfo {
        plan: payload.plan,
        expires_at: payload.expires_at,
        in_grace_period,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_key_rejects_everything() {
        assert!(is_placeholder_key());
        assert!(verify_license_key("abc.def").is_err());
    }

    #[test]
    fn malformed_keys_are_rejected() {
        // With a placeholder key this fails at the placeholder step; the
        // format check is still exercised once a real key is embedded.
        assert!(verify_license_key("not-a-key").is_err());
        assert!(verify_license_key("").is_err());
    }

    #[test]
    fn free_plan_limits_match_spec() {
        assert_eq!(Plan::Free.max_channels(), 1);
        assert_eq!(Plan::Free.max_running_scenarios(), 1);
        assert_eq!(Plan::Free.daily_cap(), 100);
        assert!(!Plan::Free.has_advanced_cron());
        assert!(Plan::Pro.has_advanced_cron());
    }
}
