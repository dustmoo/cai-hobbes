//! The per-connector "Use for" roles editor. The Voice settings
//! themselves live on the Hobbes tab (`hobbes.rs`).

use super::super::SettingsCtx;
use crate::settings::{realtime_capable, VOICE_NEEDS_REALTIME};
use dioxus::prelude::*;

/// "Use for: Chat / Voice" checkboxes for the selected connector. Edits go
/// straight into the draft instance.
#[component]
pub(in super::super) fn ConnectorRoles(ctx: SettingsCtx) -> Element {
    let mut local_settings = ctx.local_settings;
    let selected = ctx.selected_connector_id;
    let mut ui_state = ctx.ui_state;
    let ui_state_manager = ctx.ui_state_manager;

    let Some(id) = selected.read().clone() else {
        return rsx! {};
    };
    let Some(inst) = local_settings.read().connector_by_id(&id).cloned() else {
        return rsx! {};
    };
    let chat_locked = {
        let s = local_settings.read();
        let is_default = s.active_connector().map(|c| c.id.as_str()) == Some(id.as_str());
        let chat_count = s.chat_connectors().count();
        inst.roles.chat && (is_default || chat_count <= 1)
    };
    let capable = realtime_capable(&inst);
    let (chat_id, voice_id) = (id.clone(), id.clone());

    rsx! {
        div {
            class: "mb-4 p-3 bg-app rounded-lg border border-subtle",
            label { class: "block text-sm font-medium text-fg-muted mb-2", "Use for" }
            div {
                class: "flex items-center gap-4",
                label {
                    class: "flex items-center gap-2 text-sm text-fg-muted",
                    title: if chat_locked { "Keep Chat on while this is the default or the only chat connector" } else { "" },
                    input {
                        r#type: "checkbox",
                        class: "text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                        checked: "{inst.roles.chat}",
                        disabled: chat_locked,
                        onchange: move |e| {
                            let on = e.value() == "true";
                            if let Some(c) = local_settings.write().connector_by_id_mut(&chat_id) {
                                c.roles.chat = on;
                            }
                        }
                    }
                    "Chat"
                }
                label {
                    class: "flex items-center gap-2 text-sm text-fg-muted",
                    input {
                        r#type: "checkbox",
                        class: "text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                        checked: "{inst.roles.voice}",
                        onchange: move |e| {
                            let on = e.value() == "true";
                            if let Some(c) = local_settings.write().connector_by_id_mut(&voice_id) {
                                c.roles.voice = on;
                            }
                        }
                    }
                    "Voice"
                }
                if inst.roles.voice {
                    button {
                        class: "text-xs text-primary-300 hover:text-primary-200 hover:underline",
                        onclick: move |_| {
                            ui_state.write().active_settings_tab = crate::settings::SettingsTab::Hobbes;
                            let state = (*ui_state.read()).clone();
                            let manager = (*ui_state_manager.read()).clone();
                            spawn(async move { let _ = manager.save(&state); });
                        },
                        "Voice settings →"
                    }
                }
            }
            if inst.roles.voice && !capable {
                p { class: "text-xs text-yellow-400 mt-2", "{VOICE_NEEDS_REALTIME}" }
            }
        }
    }
}
