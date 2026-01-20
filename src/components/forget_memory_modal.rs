use crate::components::focus_context::FocusContext;
use crate::components::llm::{Content, GeminiRequest, Part};
use crate::session::ActiveContext;
use crate::settings::Settings;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fi_icons;
use dioxus_free_icons::Icon;

#[component]
pub fn ForgetMemoryModal(
    is_visible: Signal<bool>,
    current_context: ActiveContext,
    on_apply: EventHandler<(ActiveContext, String)>,
    on_cancel: EventHandler<()>,
) -> Element {
    let mut instruction = use_signal(|| String::new());
    let mut is_generating = use_signal(|| false);
    let mut error_message = use_signal(|| Option::<String>::None);

    let mut focus_context = use_context::<Signal<FocusContext>>();
    let settings = use_context::<Signal<Settings>>(); // Used for gemini config

    // Reset state when visibility changes
    use_effect(move || {
        if *is_visible.read() {
            focus_context.set(FocusContext::NewChatMemoryModal); // Reuse this context for modal focus
            instruction.set(String::new());
            is_generating.set(false);
            error_message.set(None);
        } else {
            focus_context.set(FocusContext::ChatInput);
        }
    });

    let mut handle_generate = move || {
        if instruction.read().trim().is_empty() {
            error_message.set(Some(
                "Please describe what to forget or focus on.".to_string(),
            ));
            return;
        }
        is_generating.set(true);
        error_message.set(None);

        let user_instruction = instruction.read().clone();
        let current_context_val = current_context.clone();
        let context_json = serde_json::to_string_pretty(&current_context_val).unwrap_or_default();
        let settings_read = settings.read();

        // Clone the full config from state to preserve all fields (API key, etc.)
        // This ensures strictly correct state usage ("We SHOULD be using what is returned from STATE")
        let mut transient_config = settings_read.gemini_config.clone();
        // Override model to use the summary model for this administrative task
        transient_config.chat_model = settings_read.gemini_config.summary_model.clone();
        // Ensure thinking is disabled for this utility task
        transient_config.thinking_enabled = false;

        drop(settings_read);

        spawn(async move {
            // We use the configured summary model via the transient config
            // The connector will now use the properly loaded settings state

            let connector = crate::components::llm::GeminiConnector::new(transient_config);

            let prompt = format!(
                r#"You are an intelligent memory optimization assistant.
Your task is to update the current 'ActiveContext' (JSON) based on the user's instructions to 'forget' or 'focus'.

User Instruction:
"{}"

Current Context:
{}

Return a JSON object with exactly two keys:
- "optimized_context": The full updated ActiveContext JSON object.
- "summary": A concise, structured markdown summary (max 3 lines) of what was removed, condensed, or added.

Ensure the "optimized_context" maintains the correct schema for ActiveContext."#,
                user_instruction, context_json
            );

            let request_body = GeminiRequest {
                contents: vec![Content {
                    role: "user".to_string(),
                    parts: vec![Part::Text {
                        text: prompt,
                        thought: None,
                    }],
                }],
                tools: None,
                system_instruction: None,
                tool_config: None,
                generation_config: Some(crate::components::llm::GenerationConfig {
                    thinking_config: None, // No thinking needed for this utility task
                }),
            };

            match connector.generate_content(request_body).await {
                Ok(response) => {
                    if let Some(candidate) = response.candidates.first() {
                        if let Some(part) = candidate.content.parts.first() {
                            let text = part.text.clone();

                            // Extract JSON from response using shared helper
                            let json_str =
                                crate::components::shared::extract_json_from_response(&text);

                            match serde_json::from_str::<serde_json::Value>(json_str) {
                                Ok(val) => {
                                    if let (Some(ctx_val), Some(summary_val)) =
                                        (val.get("optimized_context"), val.get("summary"))
                                    {
                                        match serde_json::from_value::<ActiveContext>(
                                            ctx_val.clone(),
                                        ) {
                                            Ok(new_ctx) => {
                                                let summary = summary_val
                                                    .as_str()
                                                    .unwrap_or("Memory optimized.")
                                                    .to_string();
                                                // Auto-apply immediately on success
                                                on_apply.call((new_ctx, summary));
                                            }
                                            Err(e) => error_message.set(Some(format!(
                                                "Failed to parse optimized context: {}",
                                                e
                                            ))),
                                        }
                                    } else {
                                        error_message.set(Some("LLM response missing 'optimized_context' or 'summary' keys.".to_string()));
                                    }
                                }
                                Err(e) => error_message
                                    .set(Some(format!("Failed to parse LLM JSON: {}", e))),
                            }
                        }
                    } else {
                        error_message.set(Some("No response candidates from LLM.".to_string()));
                    }
                }
                Err(e) => error_message.set(Some(format!("API Error: {}", e))),
            }
            is_generating.set(false);
        });
    };

    if !*is_visible.read() {
        return rsx! {};
    }

    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm",
            onclick: move |_| {
                 // Close on backdrop click if not confirming? Better to keep modal strict.
                 // on_cancel.call(());
            },
            onkeydown: move |evt: KeyboardEvent| {
                if evt.key() == Key::Escape {
                    on_cancel.call(());
                }
            },
            div {
                class: "bg-dark-card border border-primary-700 rounded-lg shadow-xl w-[600px] flex flex-col overflow-hidden animate-in fade-in zoom-in duration-200",

                // Header
                div {
                    class: "p-4 border-b border-primary-700 flex justify-between items-center bg-dark-section",
                    div {
                        h2 { class: "text-lg font-semibold text-white flex items-center gap-2",
                            Icon { width: 20, height: 20, icon: fi_icons::FiZap, class: "text-yellow-400" }
                            "Optimize Memory"
                        }
                        p { class: "text-xs text-gray-400 mt-1", "Instruct Hobbes to forget or focus on specific topics." }
                    }
                    button {
                        class: "text-gray-400 hover:text-white transition-colors",
                        onclick: move |_| on_cancel.call(()),
                        Icon { width: 24, height: 24, icon: fi_icons::FiX }
                    }
                }

                // Body
                div {
                    class: "p-6 bg-dark-bg flex flex-col gap-4",

                    div {
                        label { class: "block text-sm font-medium text-gray-300 mb-2", "Instructions" }
                        textarea {
                            class: "w-full h-24 bg-dark-input border border-gray-600 rounded p-3 text-sm text-white focus:border-primary-500 focus:outline-none resize-none",
                            placeholder: "e.g., 'Forget the previous discussion about deployment and focus on the new UI design.'",
                            value: "{instruction}",
                            oninput: move |e| instruction.set(e.value()),
                        }
                    }

                    if *is_generating.read() {
                        div { class: "flex items-center justify-center p-8 text-primary-400 gap-2",
                            div { class: "animate-spin rounded-full h-5 w-5 border-b-2 border-primary-400" }
                            "Optimizing Memory..."
                        }
                    }

                    if let Some(err) = error_message.read().as_ref() {
                         div { class: "text-red-400 text-sm bg-red-900/20 border border-red-900/50 p-2 rounded", "{err}" }
                    }
                }

                // Footer
                div {
                    class: "p-4 border-t border-primary-700 bg-dark-section flex justify-end gap-3",

                    button {
                        class: "px-4 py-2 text-gray-300 hover:text-white transition-colors",
                        onclick: move |_| on_cancel.call(()),
                        "Cancel"
                    }
                    button {
                        class: "px-6 py-2 bg-primary-600 hover:bg-primary-500 text-white rounded font-medium shadow-lg transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
                        disabled: "{is_generating}",
                        onclick: move |_| handle_generate(),
                        "Optimize"
                    }
                }
            }
        }
    }
}
