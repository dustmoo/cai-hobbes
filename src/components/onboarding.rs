use crate::components::markdown_renderer::MarkdownRenderer;
use crate::services::claude_validation::validate_claude_api_key;
use crate::services::gemini_models::validate_gemini_api_key;
use crate::services::openai_compat_validation::validate_openai_compat_endpoint;
use crate::settings::{
    is_sandboxed, KeychainStorageMode, LlmProvider, Settings, SettingsManager, CURRENT_TOS_VERSION,
};
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

/// Terms of Service content loaded from external markdown file.
/// Edit assets/legal/terms_of_service.md to update without code changes.
/// Remember to bump CURRENT_TOS_VERSION in settings.rs when content changes.
pub const TOS_CONTENT: &str = include_str!("../../assets/legal/terms_of_service.md");

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
    let mut keychain_mode = use_signal(KeychainStorageMode::default);
    let mut error_message = use_signal(String::new);
    let mut success_message = use_signal(String::new);
    let mut is_validating = use_signal(|| false);

    // Provider selection
    let mut selected_provider = use_signal(|| LlmProvider::Gemini);

    // Per-provider input signals
    let mut gemini_api_key = use_signal(String::new);
    let mut claude_api_key = use_signal(String::new);
    let mut oai_endpoint = use_signal(String::new);
    let mut oai_model = use_signal(String::new);
    let mut oai_api_key = use_signal(String::new);

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
        let provider = *selected_provider.read();

        // Validate required fields per provider
        match &provider {
            LlmProvider::Gemini => {
                if gemini_api_key.read().is_empty() {
                    error_message.set("Gemini API Key cannot be empty.".to_string());
                    return;
                }
            }
            LlmProvider::Claude => {
                if claude_api_key.read().is_empty() {
                    error_message.set("Claude API Key cannot be empty.".to_string());
                    return;
                }
            }
            LlmProvider::OpenAiCompat => {
                if oai_endpoint.read().is_empty() {
                    error_message.set("Endpoint URL cannot be empty.".to_string());
                    return;
                }
                if oai_model.read().is_empty() {
                    error_message.set("Model name cannot be empty.".to_string());
                    return;
                }
            }
        }

        let api_key_for_keychain = match &provider {
            LlmProvider::Gemini => gemini_api_key.read().clone(),
            LlmProvider::Claude => claude_api_key.read().clone(),
            LlmProvider::OpenAiCompat => oai_api_key.read().clone(), // may be empty (optional)
        };
        let keychain_key = provider.keychain_key().to_string();
        let use_biometric =
            is_sandboxed() && *keychain_mode.read() == KeychainStorageMode::Biometric;
        let selected_mode = if is_sandboxed() {
            keychain_mode.read().clone()
        } else {
            KeychainStorageMode::LocalKeychain
        };

        let endpoint_val = oai_endpoint.read().clone();
        let model_val = oai_model.read().clone();
        let oai_key_val = oai_api_key.read().clone();

        spawn(async move {
            is_validating.set(true);
            error_message.set("".to_string());

            // Provider-specific validation
            match &provider {
                LlmProvider::Gemini => {
                    if let Err(e) = validate_gemini_api_key(&api_key_for_keychain).await {
                        error_message.set(format!("Invalid API key: {}", e));
                        is_validating.set(false);
                        return;
                    }
                }
                LlmProvider::Claude => {
                    if let Err(e) = validate_claude_api_key(&api_key_for_keychain).await {
                        error_message.set(format!("Validation failed: {}", e));
                        is_validating.set(false);
                        return;
                    }
                }
                LlmProvider::OpenAiCompat => {
                    let oai_key_opt = if oai_key_val.is_empty() {
                        None
                    } else {
                        Some(oai_key_val.as_str())
                    };
                    if let Err(e) =
                        validate_openai_compat_endpoint(&endpoint_val, oai_key_opt).await
                    {
                        error_message.set(format!("Validation failed: {}", e));
                        is_validating.set(false);
                        return;
                    }
                }
            }

            // Save API key to keychain (skip for OpenAI-compat if key is empty)
            if !api_key_for_keychain.is_empty() {
                let save_result = tokio::task::spawn_blocking({
                    let api_key = api_key_for_keychain.clone();
                    let kc_key = keychain_key.clone();
                    move || {
                        if use_biometric {
                            crate::secret_manager::set_generic_password_with_biometric_protection(
                                &kc_key, &api_key,
                            )
                            .or_else(|e| {
                                if let crate::secret_manager::KeychainError::SecurityError(-34018) =
                                    e
                                {
                                    crate::secret_manager::set_generic_password(&kc_key, &api_key)
                                } else {
                                    Err(e)
                                }
                            })
                        } else {
                            crate::secret_manager::set_generic_password(&kc_key, &api_key)
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
            }

            let mut current_settings = settings.read().clone();
            current_settings.active_llm = provider;
            current_settings.keychain_storage_mode = selected_mode;

            // Apply provider-specific config
            match &provider {
                LlmProvider::Gemini => {
                    current_settings.gemini_config.api_key = Some(api_key_for_keychain);
                }
                LlmProvider::Claude => {
                    current_settings.claude_config.api_key = Some(api_key_for_keychain);
                    if current_settings.claude_config.model.is_empty() {
                        current_settings.claude_config.model =
                            "claude-sonnet-4-20250514".to_string();
                    }
                }
                LlmProvider::OpenAiCompat => {
                    current_settings.openai_compat_config.endpoint = endpoint_val;
                    current_settings.openai_compat_config.model = model_val;
                    if !api_key_for_keychain.is_empty() {
                        current_settings.openai_compat_config.api_key = Some(api_key_for_keychain);
                    }
                }
            }

            if let Err(e) = settings_manager.read().save(&current_settings) {
                error_message.set(format!("Failed to save settings: {}", e));
                is_validating.set(false);
                return;
            }

            settings.set(current_settings);
            success_message.set("Configuration saved!".to_string());
            is_validating.set(false);
        });
    };

    let step = *current_step.read();
    match step {
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
                        class: "flex-grow bg-app border border-subtle rounded-lg p-4 overflow-y-auto max-h-[300px] text-sm prose prose-sm dark:prose-invert max-w-none",
                        MarkdownRenderer {
                            content: TOS_CONTENT.to_string(),
                            comments: None,
                            pending_highlight: None,
                        }
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
            let current_provider = *selected_provider.read();

            // Dynamic text based on provider
            let (title, subtitle) = match &current_provider {
                LlmProvider::Gemini => (
                    "Set Up Your API Key",
                    "Configure your Gemini API key to get started.",
                ),
                LlmProvider::Claude => (
                    "Set Up Your API Key",
                    "Configure your Claude API key to get started.",
                ),
                LlmProvider::OpenAiCompat => (
                    "Connect Your Server",
                    "Connect to an OpenAI-compatible API server.",
                ),
            };

            rsx! {
                div {
                    class: "px-8 py-10 bg-section rounded-lg shadow-lg max-w-md flex flex-col gap-y-5 min-h-[500px]",
                    // Header
                    div {
                        class: "text-center",
                        h1 { class: "text-2xl font-bold mb-2", "{title}" }
                        p { class: "mb-4 text-fg-muted", "{subtitle}" }
                    }

                    // Provider Selector
                    div {
                        class: "flex gap-2 mb-2",
                        for variant in LlmProvider::all_variants() {
                            {
                                let v = *variant;
                                let is_selected = current_provider == v;
                                rsx! {
                                    button {
                                        key: "{v:?}",
                                        class: if is_selected {
                                            "flex-1 py-2 px-3 rounded-md text-sm font-medium bg-btn-primary text-fg transition-colors"
                                        } else {
                                            "flex-1 py-2 px-3 rounded-md text-sm font-medium bg-input text-fg-muted hover:text-fg transition-colors"
                                        },
                                        onclick: move |_| {
                                            error_message.set(String::new());
                                            success_message.set(String::new());
                                            selected_provider.set(v);
                                        },
                                        "{variant.display_name()}"
                                    }
                                }
                            }
                        }
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

                        // Gemini fields
                        if current_provider == LlmProvider::Gemini {
                            div { class: "mb-4",
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
                        }

                        // Claude fields
                        if current_provider == LlmProvider::Claude {
                            div { class: "mb-4",
                                label { class: "block mb-2 text-sm font-medium text-fg-muted", "Claude API Key" }
                                input {
                                    class: "w-full p-2 bg-input border border-primary-600 rounded-md focus:outline-none focus:ring-2 focus:ring-primary-500",
                                    r#type: "password",
                                    value: "{claude_api_key}",
                                    oninput: move |event| claude_api_key.set(event.value())
                                }
                                p {
                                    class: "mt-2 text-xs text-fg-muted",
                                    "Get your API key from "
                                    a {
                                        class: "text-primary-400 hover:underline",
                                        href: "https://console.anthropic.com/settings/keys",
                                        target: "_blank",
                                        "Anthropic Console"
                                    }
                                }
                            }
                        }

                        // OpenAI-compatible fields
                        if current_provider == LlmProvider::OpenAiCompat {
                            div { class: "mb-4",
                                label { class: "block mb-2 text-sm font-medium text-fg-muted", "Endpoint URL" }
                                input {
                                    class: "w-full p-2 bg-input border border-primary-600 rounded-md focus:outline-none focus:ring-2 focus:ring-primary-500",
                                    r#type: "text",
                                    placeholder: "http://localhost:11434",
                                    value: "{oai_endpoint}",
                                    oninput: move |event| oai_endpoint.set(event.value())
                                }
                                p {
                                    class: "mt-1 text-xs text-fg-muted",
                                    "Ollama, vLLM, LM Studio, or any OpenAI-format API"
                                }
                            }
                            div { class: "mb-4",
                                label { class: "block mb-2 text-sm font-medium text-fg-muted", "Model Name" }
                                input {
                                    class: "w-full p-2 bg-input border border-primary-600 rounded-md focus:outline-none focus:ring-2 focus:ring-primary-500",
                                    r#type: "text",
                                    placeholder: "llama3.2",
                                    value: "{oai_model}",
                                    oninput: move |event| oai_model.set(event.value())
                                }
                            }
                            div { class: "mb-4",
                                label { class: "block mb-2 text-sm font-medium text-fg-muted", "API Key (optional)" }
                                input {
                                    class: "w-full p-2 bg-input border border-primary-600 rounded-md focus:outline-none focus:ring-2 focus:ring-primary-500",
                                    r#type: "password",
                                    placeholder: "Leave blank for local servers",
                                    value: "{oai_api_key}",
                                    oninput: move |event| oai_api_key.set(event.value())
                                }
                            }
                        }

                        // Keychain Storage Mode (show for Gemini and Claude which require keys)
                        if current_provider != LlmProvider::OpenAiCompat {
                            div {
                                class: "mb-4 p-3 bg-app rounded-lg border border-subtle",
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
                    }

                    // Footer
                    div {
                        class: "mt-auto pt-4",
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
