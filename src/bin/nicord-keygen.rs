//! nicord-keygen — offline Ed25519 keypair generator + license signer.
//!
//! Nico runs this ONCE on a machine that never goes into the repo:
//!
//! ```text
//! # 1. Generate a keypair (private key stays OFFLINE, never commit it):
//! cargo run --bin nicord-keygen -- gen
//!
//! # 2. Paste the printed 32-byte public key into EMBEDDED_PUBLIC_KEY
//! #    in src/license.rs (as a Rust byte array).
//!
//! # 3. Issue a key for a buyer:
//! cargo run --bin nicord-keygen -- sign <PRIVATE_KEY_BASE64> pro 2027-01-01
//! #    (expires date optional; omit for a never-expiring key)
//! ```
//!
//! Key format (see src/license.rs): `base64(payload).base64(signature)`,
//! where payload is JSON `{"plan":"pro","expires_at":"...","key_version":1}`.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use std::process::exit;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Plan {
    Free,
    Pro,
    Team,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LicensePayload {
    plan: Plan,
    expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    machine_id: Option<String>,
    #[serde(default = "default_key_version")]
    key_version: u32,
}

fn default_key_version() -> u32 {
    1
}

fn usage() -> ! {
    eprintln!("Usage:");
    eprintln!("  nicord-keygen gen");
    eprintln!("  nicord-keygen sign <PRIVATE_KEY_BASE64> <free|pro|team> [EXPIRES_YYYY-MM-DD]");
    exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("gen") => {
            let signing = SigningKey::generate(&mut OsRng);
            let verifying = signing.verifying_key();
            let priv_b64 = B64.encode(signing.to_bytes());
            let pub_bytes = verifying.to_bytes();
            println!("Ed25519 keypair generated. KEEP THE PRIVATE KEY OFFLINE — never commit it.");
            println!();
            println!("PRIVATE_KEY_BASE64 (store in your password manager, offline):");
            println!("{priv_b64}");
            println!();
            println!("Paste this into EMBEDDED_PUBLIC_KEY in src/license.rs:");
            print!("pub const EMBEDDED_PUBLIC_KEY: [u8; 32] = [");
            for (i, b) in pub_bytes.iter().enumerate() {
                if i > 0 {
                    print!(", ");
                }
                print!("{b}");
            }
            println!("];");
            println!();
            println!(
                "Public key (base64, for reference): {}",
                B64.encode(pub_bytes)
            );
        }
        Some("sign") => {
            let priv_b64 = args.get(2).cloned().unwrap_or_else(|| usage());
            let plan_s = args.get(3).cloned().unwrap_or_else(|| usage());
            let plan = match plan_s.to_lowercase().as_str() {
                "free" => Plan::Free,
                "pro" => Plan::Pro,
                "team" => Plan::Team,
                _ => {
                    eprintln!("plan must be free, pro or team");
                    exit(2);
                }
            };
            let expires_at = args.get(4).map(|d| {
                let date = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap_or_else(|_| {
                    eprintln!("expiry must be YYYY-MM-DD, got '{d}'");
                    exit(2);
                });
                date.and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_local_timezone(Utc)
                    .single()
                    .unwrap()
            });
            let priv_bytes = B64.decode(&priv_b64).unwrap_or_else(|_| {
                eprintln!("private key is not valid base64");
                exit(2);
            });
            let signing = SigningKey::from_bytes(&priv_bytes.try_into().unwrap_or_else(|_| {
                eprintln!("private key must be 32 bytes");
                exit(2);
            }));
            let payload = LicensePayload {
                plan,
                expires_at,
                machine_id: None,
                key_version: 1,
            };
            let payload_bytes = serde_json::to_vec(&payload).expect("payload serialises");
            let sig: Signature = signing.sign(&payload_bytes);
            println!(
                "{}.{}",
                B64.encode(&payload_bytes),
                B64.encode(sig.to_bytes())
            );
            // Sanity-check: the key verifies against the paired public key.
            let verifying: VerifyingKey = signing.verifying_key();
            use ed25519_dalek::Verifier;
            assert!(
                verifying.verify(&payload_bytes, &sig).is_ok(),
                "self-check failed"
            );
            eprintln!("(verified against the paired public key — OK)");
        }
        _ => usage(),
    }
}
