use dioxus::prelude::*;
use crate::settings::{Settings, SettingsManager};
use crate::services::gemini_models::validate_gemini_api_key;

#[component]
pub fn Onboarding(needs_onboarding: Memo<bool>) -> Element {
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();

    let mut qdrant_uri = use_signal(|| settings.read().qdrant_url.clone().unwrap_or_else(|| "http://localhost:6333".to_string()));
    let mut gemini_api_key = use_signal(|| String::new());
    let mut error_message = use_signal(|| String::new());
    let mut success_message = use_signal(|| String::new());
    let mut is_validating = use_signal(|| false);

    let save_settings = move |_| {
        if gemini_api_key.read().is_empty() {
            error_message.set("Gemini API Key cannot be empty.".to_string());
            return;
        }

        // Clone values for async block
        let api_key = gemini_api_key.read().clone();
        let qdrant_url = qdrant_uri.read().clone();

        spawn(async move {
            is_validating.set(true);
            error_message.set("".to_string());

            // Validate API key before saving
            match validate_gemini_api_key(&api_key).await {
                Ok(()) => {
                    // Save API key to keychain with biometric protection if available
                    let save_result = tokio::task::spawn_blocking({
                        let api_key = api_key.clone();
                        move || {
                            crate::keychain_ffi::set_generic_password_with_biometric_protection("api_key", &api_key)
                                .or_else(|e| {
                                    if let crate::keychain_ffi::KeychainError::SecurityError(-34018) = e {
                                        crate::keychain_ffi::set_generic_password("api_key", &api_key)
                                    } else {
                                        Err(e)
                                    }
                                })
                        }
                    }).await;
                    
                    if let Err(e) = save_result.unwrap_or(Err(crate::keychain_ffi::KeychainError::SecurityError(-1))) {
                        error_message.set(format!("Failed to save API key: {}", e));
                        is_validating.set(false);
                        return;
                    }

                    // Update settings signal
                    let mut current_settings = settings.read().clone();
                    current_settings.qdrant_url = Some(qdrant_url);
                    current_settings.gemini_config.api_key = Some(api_key);
                    
                    // Save settings to file
                    if let Err(e) = settings_manager.read().save(&current_settings) {
                        error_message.set(format!("Failed to save settings: {}", e));
                        is_validating.set(false);
                        return;
                    }

                    // Update the global settings signal
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

    rsx! {
        div {
            class: "px-8 py-10 bg-dark-section rounded-lg shadow-lg max-w-md flex flex-col gap-y-6 min-h-[450px]",
            // Header
            div {
                class: "text-center",
                h1 { class: "text-2xl font-bold mb-2", "Welcome to Hobbes" }
                p { class: "mb-6 text-gray-400", "Please configure your settings to get started." }
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

                div { class: "mb-4",
                    label { class: "block mb-2 text-sm font-medium text-gray-300", "QDrant URI" }
                    input {
                        class: "w-full p-2 bg-dark-input border border-primary-600 rounded-md focus:outline-none focus:ring-2 focus:ring-primary-500",
                        value: "{qdrant_uri}",
                        oninput: move |event| qdrant_uri.set(event.value())
                    }
                }

                div { class: "mb-6",
                    label { class: "block mb-2 text-sm font-medium text-gray-300", "Gemini API Key" }
                    input {
                        class: "w-full p-2 bg-dark-input border border-primary-600 rounded-md focus:outline-none focus:ring-2 focus:ring-primary-500",
                        r#type: "password",
                        value: "{gemini_api_key}",
                        oninput: move |event| gemini_api_key.set(event.value())
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
                        "w-full py-2 px-4 bg-primary-500 hover:bg-primary-600 rounded-md font-bold transition-colors"
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