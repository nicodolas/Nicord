//! Token encryption: AES-256-GCM, key tied to this machine (Spec §9).
//! The token is NEVER stored in plaintext, NEVER logged, NEVER uploaded.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Key, Nonce,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Derive a 32-byte machine key from /etc/machine-id (or a fallback),
/// the OS user, and a Nicord-specific salt. The key never leaves this machine.
pub fn machine_key() -> [u8; 32] {
    let machine_id = std::fs::read_to_string("/etc/machine-id")
        .unwrap_or_else(|_| "nicord-fallback-machine".to_string());
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let mut hasher = Sha256::new();
    hasher.update(b"nicord-machine-key-v1");
    hasher.update(machine_id.trim().as_bytes());
    hasher.update(user.as_bytes());
    let out = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out[..32]);
    key
}

/// Encrypt a token. Output: base64(nonce[12] || ciphertext).
pub fn encrypt_token(token: &str) -> Result<String, String> {
    let machine = machine_key();
    let key = Key::<Aes256Gcm>::from_slice(&machine);
    let cipher = Aes256Gcm::new(key);
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, token.as_bytes())
        .map_err(|e| format!("encryption failed: {e}"))?;
    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&ct);
    Ok(B64.encode(blob))
}

/// Decrypt a token produced by `encrypt_token`.
pub fn decrypt_token(blob_b64: &str) -> Result<String, String> {
    let blob = B64
        .decode(blob_b64)
        .map_err(|e| format!("invalid encrypted token: {e}"))?;
    if blob.len() < 12 {
        return Err("encrypted token too short".into());
    }
    let (nonce_bytes, ct) = blob.split_at(12);
    let machine = machine_key();
    let key = Key::<Aes256Gcm>::from_slice(&machine);
    let cipher = Aes256Gcm::new(key);
    let pt = cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ct)
        .map_err(|_| "decryption failed — token was encrypted on another machine".to_string())?;
    String::from_utf8(pt).map_err(|_| "decrypted token is not valid UTF-8".to_string())
}

/// Show only the last 4 characters of a token, e.g. `****abcd`.
/// For an input of 4 chars or fewer (e.g. an already-truncated tail),
/// show those chars — they ARE the last 4.
pub fn mask_token(token: &str) -> String {
    if token.len() <= 4 {
        return format!("****{token}");
    }
    format!("****{}", &token[token.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let token = "test-token-1234567890";
        let enc = encrypt_token(token).unwrap();
        assert_ne!(enc, token);
        assert_eq!(decrypt_token(&enc).unwrap(), token);
    }

    #[test]
    fn mask_shows_last_four_only() {
        assert_eq!(mask_token("abcdefgh"), "****efgh");
    }
}
