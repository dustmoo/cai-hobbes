use crate::services::gemini_models::validate_gemini_api_key;
use crate::settings::{is_sandboxed, KeychainStorageMode, Settings, SettingsManager, CURRENT_TOS_VERSION};
use dioxus::prelude::*;
use dioxus_desktop::use_window;

/// Onboarding step in the 2-step flow
#[derive(Clone, Copy, PartialEq)]
enum OnboardingStep {
    /// User must accept Terms of Service
    TosAcceptance,
    /// User must provide API key
    ApiKeySetup,
    /// User declined TOS - cannot proceed
    Declined,
}

/// Embedded Terms of Service content
const TOS_CONTENT: &str = r#"HOBBES TERMS OF SERVICE
Version 1.0 | Effective Date: February 2026

By using Hobbes ("the Software"), you agree to these terms.

1. ACCEPTANCE OF TERMS
By clicking "I Accept" or using the Software, you agree to be bound by these Terms of Service and our Privacy Policy.

2. LICENSE GRANT
Clear Mirror LLC grants you a limited, non-exclusive, non-transferable license to use the Software for personal or internal business purposes, subject to these terms.

3. AI-GENERATED CONTENT DISCLAIMER
The Software uses third-party AI services (including Google Gemini) to generate responses. You acknowledge that:
• AI-generated content may be inaccurate, incomplete, or inappropriate
• You are solely responsible for reviewing and verifying any AI output before use
• Clear Mirror LLC does not guarantee the accuracy, reliability, or suitability of AI-generated content for any purpose

4. YOUR RESPONSIBILITIES
You agree to:
• Provide your own API keys for third-party services
• Not use the Software for any unlawful purpose
• Not attempt to reverse engineer, modify, or redistribute the Software
• Take responsibility for all actions performed through the Software

5. DATA & PRIVACY
• All conversation data is stored locally on your device
• API keys are stored in your system keychain
• We do not collect telemetry or transmit your data to our servers
• Third-party AI providers may process your prompts according to their terms

6. DISCLAIMER OF WARRANTIES
THE SOFTWARE IS PROVIDED "AS IS" WITHOUT WARRANTY OF ANY KIND. CLEAR MIRROR LLC DISCLAIMS ALL WARRANTIES, EXPRESS OR IMPLIED, INCLUDING MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE, AND NON-INFRINGEMENT.

7. LIMITATION OF LIABILITY
IN NO EVENT SHALL CLEAR MIRROR LLC BE LIABLE FOR ANY INDIRECT, INCIDENTAL, SPECIAL, CONSEQUENTIAL, OR PUNITIVE DAMAGES, OR ANY LOSS OF PROFITS OR REVENUE, WHETHER INCURRED DIRECTLY OR INDIRECTLY, OR ANY LOSS OF DATA, USE, GOODWILL, OR OTHER INTANGIBLE LOSSES.

8. BETA SOFTWARE
You acknowledge this Software is in beta. Features may change, and bugs may exist. Your use during beta helps improve the product.

9. TERMINATION
We may terminate or suspend your access immediately, without prior notice, for any breach of these Terms.

10. CHANGES TO TERMS
We reserve the right to modify these terms. Continued use after changes constitutes acceptance.

11. GOVERNING LAW
These terms are governed by the laws of the State of California, without regard to conflict of law principles.

Contact: support@clearmirror.ai
"#;

#[component]
pub fn Onboarding(needs_onboarding: Memo<bool>) -> Element {
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();

    // Determine initial step based on TOS acceptance status
    let initial_step = {
        let current_settings = settings.read();
        let tos_accepted = current_settings
            .tos_accepted_version
            .as_ref()
            .map(|v| v == CURRENT_TOS_VERSION)
            .unwrap_or(false);
        if tos_accepted {
            OnboardingStep::ApiKeySetup
        } else {
            OnboardingStep::TosAcceptance
        }
    };

    let mut current_step = use_signal(|| initial_step);
    let mut tos_checkbox_checked = use_signal(|| false);
    let mut gemini_api_key = use_signal(String::new);
    let mut keychain_mode = use_signal(KeychainStorageMode::default);
    let mut error_message = use_signal(String::new);
    let mut success_message = use_signal(String::new);
    let mut is_validating = use_signal(|| false);

    let sandboxed = is_sandboxed();

    // Handler for accepting TOS
    let accept_tos = move |_| {
        // Save TOS acceptance to settings
        let mut current_settings = settings.read().clone();
        current_settings.tos_accepted_version = Some(CURRENT_TOS_VERSION.to_string());

        if let Err(e) = settings_manager.read().save(&current_settings) {
            error_message.set(format!("Failed to save TOS acceptance: {}", e));
            return;
        }

        settings.set(current_settings);
        current_step.set(OnboardingStep::ApiKeySetup);
    };

    // Handler for declining TOS
    let decline_tos = move |_| {
        current_step.set(OnboardingStep::Declined);
    };

    // Handler for "Go Back" from declined state
    let go_back_to_tos = move |_| {
        current_step.set(OnboardingStep::TosAcceptance);
        tos_checkbox_checked.set(false);
    };

    // Handler for quitting the app (declined TOS)
    let window = use_window();
    let quit_app = move |_| {
        window.close();
    };


    let save_settings = move |_| {
        if gemini_api_key.read().is_empty() {
            error_message.set("Gemini API Key cannot be empty.".to_string());
            return;
        }

        let api_key = gemini_api_key.read().clone();
        let use_biometric =
            is_sandboxed() && *keychain_mode.read() == KeychainStorageMode::Biometric;
        let selected_mode = if is_sandboxed() {
            keychain_mode.read().clone()
        } else {
            KeychainStorageMode::LocalKeychain
        };

        spawn(async move {
            is_validating.set(true);
            error_message.set("".to_string());

            match validate_gemini_api_key(&api_key).await {
                Ok(()) => {
                    let save_result = tokio::task::spawn_blocking({
                        let api_key = api_key.clone();
                        move || {
                            if use_biometric {
                                crate::secret_manager::set_generic_password_with_biometric_protection(
                                    "api_key", &api_key,
                                )
                                .or_else(|e| {
                                    if let crate::secret_manager::KeychainError::SecurityError(
                                        -34018,
                                    ) = e
                                    {
                                        crate::secret_manager::set_generic_password(
                                            "api_key", &api_key,
                                        )
                                    } else {
                                        Err(e)
                                    }
                                })
                            } else {
                                crate::secret_manager::set_generic_password("api_key", &api_key)
                            }
                        }
                    })
                    .await;

                    if let Err(e) = save_result
                        .unwrap_or(Err(crate::secret_manager::KeychainError::SecurityError(-1)))
                    {
                        error_message.set(format!("Failed to save API key: {}", e));
                        is_validating.set(false);
                        return;
                    }

                    let mut current_settings = settings.read().clone();
                    current_settings.gemini_config.api_key = Some(api_key);
                    current_settings.keychain_storage_mode = selected_mode;

                    if let Err(e) = settings_manager.read().save(&current_settings) {
                        error_message.set(format!("Failed to save settings: {}", e));
                        is_validating.set(false);
                        return;
                    }

                    settings.set(current_settings);
                    success_message.set("Configuration saved!".to_string());
                }
                Err(validation_error) => {
                    error_message.set(format!("Invalid API key: {}", validation_error));
                }
            }
            is_validating.set(false);
        });
    };

    match *current_step.read() {
        OnboardingStep::TosAcceptance => {
            rsx! {
                div {
                    class: "px-8 py-10 bg-section rounded-lg shadow-lg max-w-lg flex flex-col gap-y-4 min-h-[550px]",
                    // Header
                    div {
                        class: "text-center",
                        h1 { class: "text-2xl font-bold mb-2", "Welcome to Hobbes" }
                        p { class: "text-fg-muted", "Please review and accept our Terms of Service to continue." }
                    }

                    // Scrollable TOS content
                    div {
                        class: "flex-grow bg-app border border-subtle rounded-lg p-4 overflow-y-auto max-h-[300px] text-sm text-fg-muted whitespace-pre-wrap font-mono",
                        "{TOS_CONTENT}"
                    }

                    // Checkbox
                    div {
                        class: "flex items-center gap-3 py-2",
                        input {
                            r#type: "checkbox",
                            id: "tos-checkbox",
                            class: "w-5 h-5 rounded border-primary-600 bg-input focus:ring-primary-500 cursor-pointer",
                            checked: *tos_checkbox_checked.read(),
                            oninput: move |e| {
                                if let Ok(checked) = e.value().parse::<bool>() {
                                    tos_checkbox_checked.set(checked);
                                }
                            }
                        }
                        label {
                            r#for: "tos-checkbox",
                            class: "text-sm text-fg cursor-pointer select-none",
                            "I have read and agree to the Terms of Service"
                        }
                    }

                    if !error_message.read().is_empty() {
                        div {
                            class: "p-3 text-center bg-red-800 border border-red-600 rounded-md text-sm",
                            "{error_message}"
                        }
                    }

                    // Buttons
                    div {
                        class: "flex gap-3 pt-4",
                        button {
                            class: "flex-1 py-2 px-4 bg-input hover:bg-input rounded-md font-medium text-fg-muted hover:text-fg transition-colors",
                            onclick: decline_tos,
                            "Decline"
                        }
                        button {
                            class: if *tos_checkbox_checked.read() {
                                "flex-1 py-2 px-4 bg-btn-primary hover:bg-btn-primary-hover rounded-md font-bold transition-colors"
                            } else {
                                "flex-1 py-2 px-4 bg-gray-600 rounded-md font-bold cursor-not-allowed text-fg-muted"
                            },
                            disabled: !*tos_checkbox_checked.read(),
                            onclick: accept_tos,
                            "Accept & Continue"
                        }
                    }

                    // Footer note
                    p {
                        class: "text-xs text-fg-muted text-center",
                        "By clicking \"Accept & Continue\", you agree to be bound by our Terms of Service."
                    }
                }
            }
        }

        OnboardingStep::Declined => {
            rsx! {
                div {
                    class: "px-8 py-10 bg-section rounded-lg shadow-lg max-w-md flex flex-col gap-y-6 min-h-[300px] text-center",
                    // Icon
                    div {
                        class: "text-5xl mb-2",
                        "⚠️"
                    }

                    h1 { class: "text-2xl font-bold", "Terms Required" }

                    p {
                        class: "text-fg-muted",
                        "You must accept the Terms of Service to use Hobbes. Without acceptance, we cannot grant you access to the software."
                    }

                    div {
                        class: "flex gap-3 mt-auto pt-6",
                        button {
                            class: "flex-1 py-2 px-4 bg-input hover:bg-card rounded-md font-medium text-fg-muted hover:text-fg transition-colors",
                            onclick: quit_app,
                            "Quit"
                        }
                        button {
                            class: "flex-1 py-2 px-4 bg-btn-primary hover:bg-btn-primary-hover rounded-md font-bold transition-colors",
                            onclick: go_back_to_tos,
                            "Review Terms"
                        }
                    }
                }
            }
        }

        OnboardingStep::ApiKeySetup => {
            rsx! {
                div {
                    class: "px-8 py-10 bg-section rounded-lg shadow-lg max-w-md flex flex-col gap-y-6 min-h-[450px]",
                    // Header
                    div {
                        class: "text-center",
                        h1 { class: "text-2xl font-bold mb-2", "Set Up Your API Key" }
                        p { class: "mb-6 text-fg-muted", "Configure your Gemini API key to get started." }
                    }

                    // Form Content
                    div {
                        class: "flex-grow",
                        if !success_message.read().is_empty() {
                            div {
                                class: "mb-4 p-4 text-center bg-green-800 border border-green-600 rounded-md",
                                "{success_message}"
                            }
                        }

                        if !error_message.read().is_empty() {
                            div {
                                class: "mb-4 p-4 text-center bg-red-800 border border-red-600 rounded-md",
                                "{error_message}"
                            }
                        }

                        div { class: "mb-6",
                            label { class: "block mb-2 text-sm font-medium text-fg-muted", "Gemini API Key" }
                            input {
                                class: "w-full p-2 bg-input border border-primary-600 rounded-md focus:outline-none focus:ring-2 focus:ring-primary-500",
                                r#type: "password",
                                value: "{gemini_api_key}",
                                oninput: move |event| gemini_api_key.set(event.value())
                            }
                            p {
                                class: "mt-2 text-xs text-fg-muted",
                                "Get your API key from "
                                a {
                                    class: "text-primary-400 hover:underline",
                                    href: "https://aistudio.google.com/app/apikey",
                                    target: "_blank",
                                    "Google AI Studio"
                                }
                            }
                        }

                        // Keychain Storage Mode
                        div {
                            class: "mb-6 p-3 bg-app rounded-lg border border-subtle",
                            label { class: "block text-sm font-medium text-fg-muted mb-2", "API Key Storage" }

                            if sandboxed {
                                div {
                                    class: "flex gap-2",
                                    button {
                                        class: if *keychain_mode.read() == KeychainStorageMode::Biometric {
                                            "flex-1 px-3 py-2 rounded-md text-sm font-medium bg-btn-primary text-fg"
                                        } else {
                                            "flex-1 px-3 py-2 rounded-md text-sm font-medium bg-input text-fg-muted hover:text-fg"
                                        },
                                        onclick: move |_| keychain_mode.set(KeychainStorageMode::Biometric),
                                        "🔐 Biometric"
                                    }
                                    button {
                                        class: if *keychain_mode.read() == KeychainStorageMode::ICloudSync {
                                            "flex-1 px-3 py-2 rounded-md text-sm font-medium bg-btn-primary text-fg"
                                        } else {
                                            "flex-1 px-3 py-2 rounded-md text-sm font-medium bg-input text-fg-muted hover:text-fg"
                                        },
                                        onclick: move |_| keychain_mode.set(KeychainStorageMode::ICloudSync),
                                        "☁️ iCloud Sync"
                                    }
                                }
                                p {
                                    class: "text-xs text-fg-muted mt-2",
                                    if *keychain_mode.read() == KeychainStorageMode::Biometric {
                                        "Keys require Touch ID/passcode. Device-only, more secure."
                                    } else {
                                        "Keys sync across your devices via iCloud. No biometric lock."
                                    }
                                }
                            } else {
                                div {
                                    class: "flex gap-2",
                                    div {
                                        class: "flex-1 px-3 py-2 rounded-md text-sm font-medium bg-btn-primary text-fg text-center",
                                        "🔑 Local Keychain"
                                    }
                                }
                                p {
                                    class: "text-xs text-fg-muted mt-2",
                                    "API key stored securely in your local keychain."
                                }
                            }
                        }
                    }

                    // Footer
                    div {
                        class: "mt-auto pt-6",
                        button {
                            class: if *is_validating.read() {
                                "w-full py-2 px-4 bg-gray-500 rounded-md font-bold cursor-not-allowed"
                            } else {
                                "w-full py-2 px-4 bg-btn-primary hover:bg-btn-primary-hover rounded-md font-bold transition-colors"
                            },
                            disabled: *is_validating.read(),
                            onclick: save_settings,
                            if *is_validating.read() {
                                "Validating..."
                            } else {
                                "Save and Continue"
                            }
                        }
                    }
                }
            }
        }
    }
}
