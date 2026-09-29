use super::shared::INPUT;
use crate::settings::Settings;
use crate::SecretManagerTrait;
use dioxus::prelude::*;

/// License section on the About tab: current status (Free / Pro), paste-key
/// input with verify-on-save + inline error, and a remove-key button.
///
/// The key is verified with `entitlement::Entitlement::verify` BEFORE it is
/// stored; storage goes through the SecretManager helpers only (P-011) under
/// the `hobbes_license` keychain item, which hydrates at startup like the
/// other static secrets.
#[component]
pub(super) fn LicenseSection() -> Element {
    let settings = use_context::<Signal<Settings>>();
    let mut secret_manager = use_context::<Signal<crate::secret_manager::SecretManager>>();

    let mut key_input = use_signal(String::new);
    let mut license_error = use_signal(|| Option::<String>::None);
    // The verified license lives in process-global state (entitlement.rs), not
    // a signal — bump this after activate/remove so the status line re-reads it.
    let mut license_rev = use_signal(|| 0u32);

    let _ = license_rev();
    let current = crate::entitlement::current_license();

    let on_activate = move |_| {
        let key = key_input.peek().trim().to_string();
        if key.is_empty() {
            license_error.set(Some("Paste a license key first.".to_string()));
            return;
        }
        match crate::entitlement::Entitlement::verify(&key) {
            Ok(info) => {
                let use_biometric = settings.peek().use_biometric_storage();
                match crate::secret_manager::save_secret_to_keychain(
                    crate::entitlement::LICENSE_KEYCHAIN_KEY,
                    &key,
                    use_biometric,
                ) {
                    Ok(()) => {
                        secret_manager
                            .write()
                            .update_cache(crate::entitlement::LICENSE_KEYCHAIN_KEY.to_string(), key);
                        crate::entitlement::activate(info);
                        key_input.set(String::new());
                        license_error.set(None);
                        license_rev += 1;
                    }
                    Err(e) => {
                        tracing::error!("Failed to save license key to keychain: {}", e);
                        license_error.set(Some(format!("Could not store the key: {}", e)));
                    }
                }
            }
            Err(e) => license_error.set(Some(e.to_string())),
        }
    };

    let on_remove = move |_| {
        if let Err(e) = secret_manager
            .write()
            .delete(crate::entitlement::LICENSE_KEYCHAIN_KEY)
        {
            tracing::warn!("Failed to delete license key from keychain: {}", e);
        }
        crate::entitlement::clear();
        license_error.set(None);
        license_rev += 1;
    };

    rsx! {
        div {
            class: "border border-subtle rounded-lg mb-4",
            div {
                class: "p-4 bg-section rounded-lg",
                h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium", "License" }
                p { class: "text-sm text-fg-muted mb-3", "Unlock Fleet and other Pro features with a license key." }

                if let Some(info) = current {
                    div {
                        class: "flex items-center gap-2 mb-3",
                        span { class: "text-sm font-semibold text-green-400", "Pro" }
                        span { class: "text-sm text-fg-muted", "— licensed to {info.email}" }
                    }
                    button {
                        class: "border border-red-700/50 text-red-400 rounded px-3 py-1.5 text-sm",
                        onclick: on_remove,
                        "Remove license key"
                    }
                } else {
                    label { class: "block text-sm text-fg mb-1", "License Key" }
                    input {
                        class: INPUT,
                        r#type: "password",
                        placeholder: "HOBBES-PRO.…",
                        value: "{key_input}",
                        oninput: move |e| {
                            key_input.set(e.value());
                            if license_error.peek().is_some() {
                                license_error.set(None);
                            }
                        },
                    }
                    if let Some(err) = license_error() {
                        p { class: "text-sm text-red-400 mt-2", "{err}" }
                    }
                    button {
                        class: "mt-3 bg-btn-primary hover:bg-btn-primary-hover text-fg rounded px-3 py-1.5 text-sm",
                        onclick: on_activate,
                        "Activate"
                    }
                }
            }
        }
    }
}
