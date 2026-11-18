//! Token encryption: AES-256-GCM, key tied to this machine (Spec §9).
//! The token is NEVER stored in plaintext, NEVER logged, NEVER uploaded.
//!
//! User secrets are stored through `secret_store`, which prefers the
//! OS keyring and falls back to these machine-key functions. Use
//! `encrypt_token`/`decrypt_token` directly only for the fallback
//! path and for values that are not user secrets.

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

/// Encrypt with an explicit 32-byte key. Output: base64(nonce[12]
/// || ciphertext). Same format as `encrypt_token`, which delegates
/// here with the machine key. Portable exports use a
/// passphrase-derived key (see `pbkdf2_hmac_sha256`).
pub fn encrypt_with_key(key_bytes: &[u8; 32], plaintext: &str) -> Result<String, String> {
    let key = Key::<Aes256Gcm>::from_slice(key_bytes);
    let cipher = Aes256Gcm::new(key);
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| format!("mã hóa thất bại: {e}"))?;
    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&ct);
    Ok(B64.encode(blob))
}

/// Decrypt a blob produced by `encrypt_with_key` with the same key.
/// The error message never says which key was wrong beyond the
/// generic hint — a wrong passphrase and a corrupt file look the same.
pub fn decrypt_with_key(key_bytes: &[u8; 32], blob_b64: &str) -> Result<String, String> {
    let blob = B64
        .decode(blob_b64)
        .map_err(|e| format!("giá trị mã hóa không hợp lệ: {e}"))?;
    if blob.len() < 12 {
        return Err("giá trị mã hóa quá ngắn".into());
    }
    let (nonce_bytes, ct) = blob.split_at(12);
    let key = Key::<Aes256Gcm>::from_slice(key_bytes);
    let cipher = Aes256Gcm::new(key);
    let pt = cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ct)
        .map_err(|_| "giải mã thất bại — sai khóa hoặc dữ liệu hỏng".to_string())?;
    String::from_utf8(pt).map_err(|_| "giá trị đã giải mã không phải UTF-8 hợp lệ".to_string())
}

/// Encrypt a token with the machine key. Output: base64(nonce[12] || ciphertext).
pub fn encrypt_token(token: &str) -> Result<String, String> {
    encrypt_with_key(&machine_key(), token)
}

/// Decrypt a token produced by `encrypt_token`.
pub fn decrypt_token(blob_b64: &str) -> Result<String, String> {
    decrypt_with_key(&machine_key(), blob_b64).map_err(|e| {
        if e.contains("giải mã thất bại") {
            "giải mã thất bại — token đã được mã hóa trên máy khác".to_string()
        } else {
            e
        }
    })
}

/// HMAC-SHA256 (RFC 2104), implemented over `sha2` because the
/// project has no `hmac` dependency. Verified against RFC 4231
/// test vectors in the tests below.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let hash = Sha256::digest(key);
        key_block[..32].copy_from_slice(&hash);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0u8; BLOCK];
    let mut opad = [0u8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] = key_block[i] ^ 0x36;
        opad[i] = key_block[i] ^ 0x5c;
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    let out = outer.finalize();
    let mut result = [0u8; 32];
    result.copy_from_slice(&out);
    result
}

/// PBKDF2-HMAC-SHA256 (RFC 2898) with a fixed 32-byte output.
/// `iterations` must be > 0. Verified against RFC 6070 vectors.
/// Portable exports use 600,000 iterations (OWASP's recommendation
/// for PBKDF2-HMAC-SHA256); export/import are infrequent, so the
/// ~0.2s cost is acceptable.
pub fn pbkdf2_hmac_sha256(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out_len: usize,
) -> Vec<u8> {
    assert!(iterations > 0, "số vòng lặp PBKDF2 phải > 0");
    let mut out = Vec::with_capacity(out_len);
    let mut block_index: u32 = 1;
    while out.len() < out_len {
        // U1 = HMAC(password, salt || INT_32_BE(block_index))
        let mut msg = salt.to_vec();
        msg.extend_from_slice(&block_index.to_be_bytes());
        let mut u = hmac_sha256(password, &msg);
        let mut t = u;
        for _ in 1..iterations {
            u = hmac_sha256(password, &u);
            for i in 0..32 {
                t[i] ^= u[i];
            }
        }
        out.extend_from_slice(&t);
        block_index += 1;
    }
    out.truncate(out_len);
    out
}

/// Random salt for portable exports (16 bytes, base64 in the file).
pub fn random_salt() -> [u8; 16] {
    let mut salt = [0u8; 16];
    rand::rng().fill_bytes(&mut salt);
    salt
}

/// Show only the last 4 characters of a token, e.g. `****abcd`.
/// For an input of 4 chars or fewer (e.g. an already-truncated tail),
/// show those chars — they ARE the last 4.
/// Char-based, never byte-slicing: safe for multi-byte input.
pub fn mask_token(token: &str) -> String {
    let tail: String = token
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("****{tail}")
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
    fn hmac_sha256_matches_rfc4231() {
        // RFC 4231 Test Case 2: key "Jefe", data "what do ya want for
        // nothing?".
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // RFC 4231 Test Case 1: 20 bytes of 0x0b, "Hi There".
        let mac = hmac_sha256(&[0x0bu8; 20], b"Hi There");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn pbkdf2_matches_rfc6070() {
        // RFC 6070: password "password", salt "salt", 1 iteration.
        let dk = pbkdf2_hmac_sha256(b"password", b"salt", 1, 32);
        let hex: String = dk.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        // 2 iterations.
        let dk = pbkdf2_hmac_sha256(b"password", b"salt", 2, 32);
        let hex: String = dk.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
        // 4096 iterations.
        let dk = pbkdf2_hmac_sha256(b"password", b"salt", 4096, 32);
        let hex: String = dk.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    #[test]
    fn explicit_key_roundtrip_and_wrong_key_fails() {
        let key = [7u8; 32];
        let enc = encrypt_with_key(&key, "secret-value").unwrap();
        assert_eq!(decrypt_with_key(&key, &enc).unwrap(), "secret-value");
        assert!(decrypt_with_key(&[8u8; 32], &enc).is_err());
    }

    #[test]
    fn mask_shows_last_four_only() {
        assert_eq!(mask_token("abcdefgh"), "****efgh");
    }

    #[test]
    fn mask_handles_unicode_and_short_inputs() {
        assert_eq!(mask_token(""), "****");
        assert_eq!(mask_token("ab"), "****ab");
        // Last 4 chars, not last 4 bytes: must not panic and must not
        // split a multi-byte char.
        assert_eq!(mask_token("abéééé"), "****éééé");
        assert_eq!(mask_token("éééé"), "****éééé");
    }
}
