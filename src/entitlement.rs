//! Pro license entitlement — verification, runtime state, and the keychain key.
//!
//! # License format
//!
//! ```text
//! HOBBES-PRO.<base64url(payload_json)>.<base64url(ed25519_signature)>
//! ```
//!
//! `payload_json` is `{"email": ..., "issued_at": <rfc3339>, "product": "pro"}`
//! and the signature is ed25519 over the **exact payload bytes** embedded in
//! the key (unpadded base64url throughout). Licenses are minted offline with
//! `scripts/mint_license/` (see its README); the app only ever verifies.
//!
//! # Swapping the public key
//!
//! The verifying key is embedded below as [`EMBEDDED_PUBLIC_KEY_B64`]. To
//! rotate it, run `cargo run -- keygen --force` inside `scripts/mint_license/`
//! and paste the constant it prints over the one here. All licenses minted
//! with the old private key stop verifying, so re-mint and re-issue.
//!
//! # Runtime state
//!
//! The verified license is cached in process-global state, set at startup
//! (from the `hobbes_license` keychain item, hydrated in `main.rs` alongside
//! the other secrets) and on key entry/removal in the settings About tab.
//! Feature gates call [`pro_active()`]; the About tab reads
//! [`current_license()`] for the "licensed to" line.
//!
//! Dev escape hatch: **debug builds only** honor `HOBBES_PRO_DEV=1` to force
//! `pro_active()`; release builds never consult the environment.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

/// The embedded ed25519 verifying key (unpadded base64url of the 32 raw
/// bytes). The matching private key lives ONLY in the operator's gitignored
/// `scripts/mint_license/keys/` directory — see module docs for rotation.
pub const EMBEDDED_PUBLIC_KEY_B64: &str = "3lAirn3vIB_Ud8w3aUZOgoTIrwfPtGy2vzSnSfb6sKk";

/// Leading tag of every license key.
const LICENSE_PREFIX: &str = "HOBBES-PRO";

/// Product string a valid Pro license must carry.
const PRODUCT_PRO: &str = "pro";

/// Keychain item name for the stored license key (P-011: accessed only via
/// SecretManager / `save_secret_to_keychain`). Listed in
/// `secret_types::KNOWN_KEYS` so it hydrates at startup with the rest.
pub const LICENSE_KEYCHAIN_KEY: &str = "hobbes_license";

/// The signed payload of a verified license.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicenseInfo {
    pub email: String,
    pub issued_at: String,
    pub product: String,
}

/// Why a license key failed verification.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LicenseError {
    #[error("That doesn't look like a Hobbes Pro license key.")]
    Malformed,
    #[error("Invalid license key — the signature doesn't check out.")]
    BadSignature,
    #[error("This key is for a different product ({0}), not Hobbes Pro.")]
    WrongProduct(String),
}

pub struct Entitlement;

impl Entitlement {
    /// Verify a license key against the embedded public key.
    pub fn verify(key: &str) -> Result<LicenseInfo, LicenseError> {
        let vk = embedded_verifying_key().ok_or(LicenseError::BadSignature)?;
        verify_with_key(key, &vk)
    }
}

/// Decode the embedded public key. `None` only if the constant is corrupt
/// (e.g. a bad paste during rotation) — verification then fails closed.
fn embedded_verifying_key() -> Option<VerifyingKey> {
    let bytes = URL_SAFE_NO_PAD.decode(EMBEDDED_PUBLIC_KEY_B64).ok()?;
    let arr: [u8; 32] = bytes.as_slice().try_into().ok()?;
    VerifyingKey::from_bytes(&arr).ok()
}

/// Core verification, parameterized on the verifying key so tests can use an
/// ephemeral keypair. The public API ([`Entitlement::verify`]) binds the
/// embedded key.
fn verify_with_key(key: &str, vk: &VerifyingKey) -> Result<LicenseInfo, LicenseError> {
    let mut parts = key.trim().split('.');
    let (prefix, payload_b64, sig_b64) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(p), Some(payload), Some(sig), None) => (p, payload, sig),
        _ => return Err(LicenseError::Malformed),
    };
    if prefix != LICENSE_PREFIX {
        return Err(LicenseError::Malformed);
    }

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| LicenseError::Malformed)?;
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| LicenseError::Malformed)?;
    let signature = Signature::from_slice(&sig_bytes).map_err(|_| LicenseError::Malformed)?;

    // Authenticate the payload bytes before trusting anything inside them.
    vk.verify(&payload_bytes, &signature)
        .map_err(|_| LicenseError::BadSignature)?;

    let info: LicenseInfo =
        serde_json::from_slice(&payload_bytes).map_err(|_| LicenseError::Malformed)?;
    if info.product != PRODUCT_PRO {
        return Err(LicenseError::WrongProduct(info.product));
    }
    Ok(info)
}

// ============================================================================
// RUNTIME STATE
// ============================================================================

static PRO_ACTIVE: AtomicBool = AtomicBool::new(false);
static LICENSE: RwLock<Option<LicenseInfo>> = RwLock::new(None);

/// Is Pro unlocked right now? Cheap; callable from anywhere (components,
/// background tasks). Reflects the last verified license set via
/// [`hydrate_from_stored_key`] / [`activate`] / [`clear`].
///
/// Debug builds only: `HOBBES_PRO_DEV=1` forces `true`.
#[allow(dead_code)] // consumers are the Phase A/B Pro-gated surfaces
pub fn pro_active() -> bool {
    dev_flag_forced() || PRO_ACTIVE.load(Ordering::Relaxed)
}

/// The currently active verified license, if any (for the About tab's
/// "licensed to {email}" line).
pub fn current_license() -> Option<LicenseInfo> {
    LICENSE.read().expect("license lock poisoned").clone()
}

/// Mark a verified license active. Call only with the output of a successful
/// [`Entitlement::verify`].
pub fn activate(info: LicenseInfo) {
    *LICENSE.write().expect("license lock poisoned") = Some(info);
    PRO_ACTIVE.store(true, Ordering::Relaxed);
}

/// Drop any active license (key removed in settings).
pub fn clear() {
    *LICENSE.write().expect("license lock poisoned") = None;
    PRO_ACTIVE.store(false, Ordering::Relaxed);
}

/// Startup hydration: verify the keychain-stored license (if any) and set the
/// runtime state. Called from `main.rs` right after the secret cache loads.
pub fn hydrate_from_stored_key(stored: Option<&str>) {
    match stored {
        Some(key) => match Entitlement::verify(key) {
            Ok(info) => {
                tracing::info!("Pro license verified for {}", info.email);
                activate(info);
            }
            Err(e) => {
                tracing::warn!("Stored license key failed verification: {}", e);
                clear();
            }
        },
        None => clear(),
    }
}

/// Debug-build-only dev escape hatch. Release builds never read the env var.
#[allow(dead_code)] // reached via pro_active(); see its allow note
fn dev_flag_forced() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var("HOBBES_PRO_DEV").is_ok_and(|v| v == "1")
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn test_keypair() -> (SigningKey, VerifyingKey) {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let vk = sk.verifying_key();
        (sk, vk)
    }

    /// Mirror of the mint helper: sign a payload and assemble the key string.
    fn mint(sk: &SigningKey, email: &str, product: &str) -> String {
        let payload = serde_json::json!({
            "email": email,
            "issued_at": "2026-08-25T00:00:00Z",
            "product": product,
        });
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let sig = sk.sign(&payload_bytes);
        format!(
            "{}.{}.{}",
            LICENSE_PREFIX,
            URL_SAFE_NO_PAD.encode(&payload_bytes),
            URL_SAFE_NO_PAD.encode(sig.to_bytes())
        )
    }

    #[test]
    fn mint_verify_round_trip() {
        let (sk, vk) = test_keypair();
        let key = mint(&sk, "user@example.com", "pro");
        let info = verify_with_key(&key, &vk).expect("round-trip must verify");
        assert_eq!(info.email, "user@example.com");
        assert_eq!(info.product, "pro");
        assert_eq!(info.issued_at, "2026-08-25T00:00:00Z");
    }

    #[test]
    fn tampered_payload_rejected() {
        let (sk, vk) = test_keypair();
        let key = mint(&sk, "user@example.com", "pro");
        let parts: Vec<&str> = key.split('.').collect();
        // Swap the email inside the signed payload; signature stays the same.
        let payload = URL_SAFE_NO_PAD.decode(parts[1]).unwrap();
        let tampered_json = String::from_utf8(payload)
            .unwrap()
            .replace("user@example.com", "evil@example.com");
        let tampered = format!(
            "{}.{}.{}",
            parts[0],
            URL_SAFE_NO_PAD.encode(tampered_json.as_bytes()),
            parts[2]
        );
        assert_eq!(
            verify_with_key(&tampered, &vk),
            Err(LicenseError::BadSignature)
        );
    }

    #[test]
    fn wrong_product_rejected() {
        let (sk, vk) = test_keypair();
        // Correctly signed, but not a "pro" license.
        let key = mint(&sk, "user@example.com", "trial");
        assert_eq!(
            verify_with_key(&key, &vk),
            Err(LicenseError::WrongProduct("trial".into()))
        );
    }

    #[test]
    fn wrong_signing_key_rejected() {
        let (_, vk) = test_keypair();
        let other_sk = SigningKey::from_bytes(&[7u8; 32]);
        let key = mint(&other_sk, "user@example.com", "pro");
        assert_eq!(verify_with_key(&key, &vk), Err(LicenseError::BadSignature));
    }

    #[test]
    fn malformed_keys_rejected() {
        let (sk, vk) = test_keypair();
        let good = mint(&sk, "user@example.com", "pro");
        let parts: Vec<&str> = good.split('.').collect();

        // Missing parts
        assert_eq!(verify_with_key("", &vk), Err(LicenseError::Malformed));
        assert_eq!(
            verify_with_key("HOBBES-PRO", &vk),
            Err(LicenseError::Malformed)
        );
        assert_eq!(
            verify_with_key(&format!("HOBBES-PRO.{}", parts[1]), &vk),
            Err(LicenseError::Malformed)
        );
        // Extra part
        assert_eq!(
            verify_with_key(&format!("{}.extra", good), &vk),
            Err(LicenseError::Malformed)
        );
        // Wrong prefix
        assert_eq!(
            verify_with_key(&format!("HOBBES-FREE.{}.{}", parts[1], parts[2]), &vk),
            Err(LicenseError::Malformed)
        );
        // Bad base64 in payload and in signature (`%` is not in the alphabet)
        assert_eq!(
            verify_with_key(&format!("HOBBES-PRO.%%%.{}", parts[2]), &vk),
            Err(LicenseError::Malformed)
        );
        assert_eq!(
            verify_with_key(&format!("HOBBES-PRO.{}.%%%", parts[1]), &vk),
            Err(LicenseError::Malformed)
        );
        // Valid base64 but not a 64-byte signature
        assert_eq!(
            verify_with_key(&format!("HOBBES-PRO.{}.AAAA", parts[1]), &vk),
            Err(LicenseError::Malformed)
        );
    }

    #[test]
    fn signed_garbage_json_is_malformed() {
        let (sk, vk) = test_keypair();
        // A validly signed payload that isn't the expected JSON shape.
        let payload = b"not json at all";
        let sig = sk.sign(payload);
        let key = format!(
            "{}.{}.{}",
            LICENSE_PREFIX,
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(sig.to_bytes())
        );
        assert_eq!(verify_with_key(&key, &vk), Err(LicenseError::Malformed));
    }

    #[test]
    fn embedded_key_api_rejects_foreign_and_garbage_keys() {
        // Garbage → Malformed via the public (embedded-key) API.
        assert_eq!(
            Entitlement::verify("HOBBES-PRO"),
            Err(LicenseError::Malformed)
        );
        // Well-formed but signed by a different key → BadSignature.
        let (sk, _) = test_keypair();
        let foreign = mint(&sk, "user@example.com", "pro");
        assert_eq!(
            Entitlement::verify(&foreign),
            Err(LicenseError::BadSignature)
        );
    }

    /// A license actually produced by `scripts/mint_license` must verify
    /// against the embedded public key — ties the shipped constant to the
    /// operator's real keypair. Regenerate this fixture after key rotation
    /// (`cargo run -- mint --email dustin@clearmirror.ai` in the helper).
    #[test]
    fn real_minted_license_verifies_with_embedded_key() {
        let minted = "HOBBES-PRO.eyJlbWFpbCI6ImR1c3RpbkBjbGVhcm1pcnJvci5haSIsImlzc3VlZF9hdCI6IjIwMjYtMDgtMjVUMjA6NTI6NTdaIiwicHJvZHVjdCI6InBybyJ9.HP0veIeVHkhKw5hSngTtl9tEl15hf8UmCVlNbUozMGKbp258q9Wg1Ph-qT60zYL_QrjtLy8cAIcKy6CTY_dpAQ";
        let info = Entitlement::verify(minted).expect("real minted license must verify");
        assert_eq!(info.email, "dustin@clearmirror.ai");
        assert_eq!(info.product, "pro");
    }

    /// All process-global state (activate/clear/pro_active and the dev env
    /// flag) is exercised in this single test so parallel tests never race on
    /// the statics.
    #[test]
    fn global_state_and_dev_flag() {
        // Dev flag: honored only under debug_assertions.
        // SAFETY: single-threaded with respect to env access — no other test
        // in this binary reads or writes the environment.
        unsafe { std::env::set_var("HOBBES_PRO_DEV", "1") };
        #[cfg(debug_assertions)]
        assert!(pro_active(), "debug builds must honor HOBBES_PRO_DEV=1");
        #[cfg(not(debug_assertions))]
        assert!(
            !pro_active(),
            "release builds must NEVER honor HOBBES_PRO_DEV"
        );
        unsafe { std::env::remove_var("HOBBES_PRO_DEV") };

        // Activate / clear round-trip.
        assert!(!pro_active());
        assert_eq!(current_license(), None);
        let info = LicenseInfo {
            email: "user@example.com".into(),
            issued_at: "2026-08-25T00:00:00Z".into(),
            product: "pro".into(),
        };
        activate(info.clone());
        assert!(pro_active());
        assert_eq!(current_license(), Some(info));
        clear();
        assert!(!pro_active());
        assert_eq!(current_license(), None);

        // Hydration path: bad stored key clears, good state only via verify.
        hydrate_from_stored_key(Some("HOBBES-PRO.garbage.garbage"));
        assert!(!pro_active());
        hydrate_from_stored_key(None);
        assert!(!pro_active());
    }
}
