//! Offline license minting helper for Hobbes Pro.
//!
//! Two commands:
//!   keygen [--force]     Generate the ed25519 signing keypair (once).
//!   mint --email <email> Sign a Pro license for the given email.
//!
//! The private key NEVER ships with the app and NEVER goes in git — it is
//! written to `keys/` next to this crate, which is gitignored. The matching
//! public key is embedded as a constant in `src/entitlement.rs` of the app.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use std::path::PathBuf;

const LICENSE_PREFIX: &str = "HOBBES-PRO";

fn keys_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("keys")
}

fn private_key_path() -> PathBuf {
    keys_dir().join("license_signing.key")
}

fn public_key_path() -> PathBuf {
    keys_dir().join("license_signing.pub")
}

fn keygen(force: bool) {
    let priv_path = private_key_path();
    if priv_path.exists() && !force {
        eprintln!(
            "Refusing to overwrite existing private key at {}.\n\
             Pass --force to regenerate (this invalidates ALL previously minted licenses\n\
             unless you also keep the old key).",
            priv_path.display()
        );
        std::process::exit(1);
    }

    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).expect("OS randomness unavailable");
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = signing_key.verifying_key();

    let priv_b64 = URL_SAFE_NO_PAD.encode(signing_key.to_bytes());
    let pub_b64 = URL_SAFE_NO_PAD.encode(verifying_key.to_bytes());

    std::fs::create_dir_all(keys_dir()).expect("create keys dir");
    std::fs::write(&priv_path, &priv_b64).expect("write private key");
    std::fs::write(public_key_path(), &pub_b64).expect("write public key");

    // Best-effort tighten permissions on the private key (unix only).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&priv_path, std::fs::Permissions::from_mode(0o600));
    }

    println!("Keypair written:");
    println!("  private: {}   (KEEP OFFLINE — gitignored)", priv_path.display());
    println!("  public:  {}", public_key_path().display());
    println!();
    println!("Paste this constant into src/entitlement.rs (replacing the existing one):");
    println!();
    println!("pub const EMBEDDED_PUBLIC_KEY_B64: &str = \"{}\";", pub_b64);
    println!();
    println!("Then rebuild the app. Licenses minted with the old key stop verifying.");
}

fn mint(email: &str) {
    let priv_path = private_key_path();
    let priv_b64 = std::fs::read_to_string(&priv_path).unwrap_or_else(|_| {
        eprintln!(
            "No private key at {} — run `cargo run -- keygen` first.",
            priv_path.display()
        );
        std::process::exit(1);
    });
    let seed_bytes = URL_SAFE_NO_PAD
        .decode(priv_b64.trim())
        .expect("private key file is not valid base64url");
    let seed: [u8; 32] = seed_bytes
        .as_slice()
        .try_into()
        .expect("private key must be exactly 32 bytes");
    let signing_key = SigningKey::from_bytes(&seed);

    let payload = serde_json::json!({
        "email": email,
        "issued_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "product": "pro",
    });
    let payload_bytes = serde_json::to_vec(&payload).expect("serialize payload");
    let signature = signing_key.sign(&payload_bytes);

    let license = format!(
        "{}.{}.{}",
        LICENSE_PREFIX,
        URL_SAFE_NO_PAD.encode(&payload_bytes),
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    );
    println!("{}", license);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("keygen") => {
            let force = args.iter().any(|a| a == "--force");
            keygen(force);
        }
        Some("mint") => {
            let email = args
                .iter()
                .position(|a| a == "--email")
                .and_then(|i| args.get(i + 1))
                .unwrap_or_else(|| {
                    eprintln!("Usage: mint --email <email>");
                    std::process::exit(1);
                });
            mint(email);
        }
        _ => {
            eprintln!("Usage:");
            eprintln!("  cargo run -- keygen [--force]");
            eprintln!("  cargo run -- mint --email <email>");
            std::process::exit(1);
        }
    }
}
