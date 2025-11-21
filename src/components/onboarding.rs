use dioxus::prelude::*;
use crate::settings::{Settings, SettingsManager};
use crate::secure_storage;

#[derive(Props, Clone, PartialEq)]
pub struct OnboardingProps {
    pub needs_onboarding: Signal<bool>,
}

#[component]
pub fn Onboarding(mut props: OnboardingProps) -> Element {
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();

    let mut qdrant_uri = use_signal(|| settings.read().qdrant_url.clone().unwrap_or_else(|| "http://localhost:6333".to_string()));
    let mut gemini_api_key = use_signal(|| String::new());
    let mut error_message = use_signal(|| String::new());
    let mut success_message = use_signal(|| String::new());

    let mut save_settings = move || {
        if gemini_api_key.read().is_empty() {
            error_message.set("Gemini API Key cannot be empty.".to_string());
            return;
        }

        // Save API key to secure storage
        if let Err(e) = secure_storage::save_secret("api_key", &gemini_api_key.read()) {
            error_message.set(format!("Failed to save API key: {}", e));
            return;
        }

        // Update settings signal
        let mut current_settings = settings.read().clone();
        current_settings.qdrant_url = Some(qdrant_uri.read().clone());
        current_settings.gemini_config.api_key = Some(gemini_api_key.read().clone());
        
        // Save settings to file
        if let Err(e) = settings_manager.read().save(&current_settings) {
            error_message.set(format!("Failed to save settings: {}", e));
            return;
        }

        // Update the global settings signal
        settings.set(current_settings);

        success_message.set("Configuration saved!".to_string());
        error_message.set("".to_string());

        // This will cause the main app to re-render and show the chat window
        props.needs_onboarding.set(false);
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
                    class: "w-full py-2 px-4 bg-primary-500 hover:bg-primary-600 rounded-md font-bold transition-colors",
                    onclick: move |_| save_settings(),
                    "Save and Continue"
                }
            }
        }
    }
}