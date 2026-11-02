//! Settings → Voice card (Pro-gated, mirrors the Heartbeat card) and the
//! per-connector "Use for" roles editor.

use super::super::SettingsCtx;
use crate::components::hotkey_recorder::HotkeyRecorder;
use crate::settings::{
    default_realtime_model, realtime_capable, VoiceProfile, VOICE_NEEDS_REALTIME,
};
use crate::voice::audio::MicPermission;
use dioxus::prelude::*;

const GEMINI_VOICES: &[&str] = &[
    "Kore", "Puck", "Charon", "Fenrir", "Aoede", "Leda", "Orus", "Zephyr",
];

/// "Use for: Chat / Voice" checkboxes plus the Voice sub-section for the
/// selected connector. Edits go straight into the draft instance.
#[component]
pub(in super::super) fn ConnectorRoles(ctx: SettingsCtx) -> Element {
    let mut local_settings = ctx.local_settings;
    let selected = ctx.selected_connector_id;
    let mut live_models = use_signal(Vec::<String>::new);
    let mut live_models_loading = use_signal(|| false);
    let mut live_models_error = use_signal(|| Option::<String>::None);

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
    let vp = inst.voice.clone().unwrap_or_default();
    let default_model = default_realtime_model(inst.provider()).unwrap_or("");
    let api_key = inst.config.api_key().cloned().unwrap_or_default();
    let (chat_id, voice_id, model_id, name_id, lang_id) =
        (id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
    let (sel_id, load_id) = (id.clone(), id.clone());

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
            }
            if inst.roles.voice && !capable {
                p { class: "text-xs text-yellow-400 mt-2", "{VOICE_NEEDS_REALTIME}" }
            }
            if inst.roles.voice && capable {
                div {
                    class: "mt-3 pl-3 border-l-2 border-subtle",
                    h4 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium mb-2", "Voice" }
                    div {
                        class: "mb-3",
                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Realtime model" }
                        div {
                            class: "flex gap-2",
                            input {
                                class: "flex-1 px-3 py-2 bg-input border border-subtle rounded-md text-sm",
                                placeholder: "{default_model}",
                                value: "{vp.model}",
                                list: "voice-live-models",
                                oninput: move |e| {
                                    if let Some(c) = local_settings.write().connector_by_id_mut(&model_id) {
                                        c.voice.get_or_insert_with(VoiceProfile::default).model = e.value();
                                    }
                                }
                            }
                            datalist {
                                id: "voice-live-models",
                                for m in live_models.read().iter() {
                                    option { value: "{m}" }
                                }
                            }
                            button {
                                class: "px-3 py-2 rounded-md text-xs bg-input text-fg-muted hover:text-fg border border-subtle",
                                disabled: *live_models_loading.read() || api_key.trim().is_empty(),
                                title: if api_key.trim().is_empty() { "Enter this connector's API key first" } else { "" },
                                onclick: move |_| {
                                    let key = local_settings
                                        .read()
                                        .connector_by_id(&load_id)
                                        .and_then(|c| c.config.api_key().cloned())
                                        .unwrap_or_default();
                                    live_models_loading.set(true);
                                    live_models_error.set(None);
                                    spawn(async move {
                                        match crate::voice::gemini_live::list_live_models(&key).await {
                                            Ok(list) => live_models.set(list),
                                            Err(e) => live_models_error.set(Some(e)),
                                        }
                                        live_models_loading.set(false);
                                    });
                                },
                                if *live_models_loading.read() { "Loading..." } else { "Load models" }
                            }
                        }
                        if let Some(err) = live_models_error.read().as_ref() {
                            p { class: "text-xs text-red-400 mt-1", "{err}" }
                        }
                    }
                    div {
                        class: "mb-3",
                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Voice" }
                        div {
                            class: "flex gap-2",
                            input {
                                class: "flex-1 px-3 py-2 bg-input border border-subtle rounded-md text-sm",
                                placeholder: "Kore",
                                value: "{vp.voice_name.clone().unwrap_or_default()}",
                                oninput: move |e| {
                                    let v = e.value();
                                    if let Some(c) = local_settings.write().connector_by_id_mut(&name_id) {
                                        c.voice.get_or_insert_with(VoiceProfile::default).voice_name =
                                            if v.trim().is_empty() { None } else { Some(v) };
                                    }
                                }
                            }
                            select {
                                class: "px-3 py-2 bg-input border border-subtle rounded-md text-sm text-fg",
                                onchange: move |e| {
                                    let v = e.value();
                                    if v.is_empty() { return; }
                                    if let Some(c) = local_settings.write().connector_by_id_mut(&sel_id) {
                                        c.voice.get_or_insert_with(VoiceProfile::default).voice_name = Some(v);
                                    }
                                },
                                option { value: "", "Common voices" }
                                for v in GEMINI_VOICES.iter().copied() {
                                    option { value: "{v}", "{v}" }
                                }
                            }
                        }
                    }
                    div {
                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Language" }
                        input {
                            class: "block w-48 px-3 py-2 bg-input border border-subtle rounded-md text-sm",
                            placeholder: "auto",
                            value: "{vp.language.clone().unwrap_or_default()}",
                            oninput: move |e| {
                                let v = e.value();
                                if let Some(c) = local_settings.write().connector_by_id_mut(&lang_id) {
                                    c.voice.get_or_insert_with(VoiceProfile::default).language =
                                        if v.trim().is_empty() { None } else { Some(v) };
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The global Voice card. Renders nothing when Voice isn't available (non-Pro).
#[component]
pub(in super::super) fn VoiceCard(ctx: SettingsCtx) -> Element {
    let mut local_settings = ctx.local_settings;
    let mut mic = use_signal(crate::voice::audio::mic_permission);
    let mut devices = use_signal(crate::voice::audio::list_input_devices);
    if !crate::voice::voice_available() {
        return rsx! {};
    }
    let s = local_settings.read().clone();
    let voice_profiles: Vec<(String, String)> = s
        .voice_connectors()
        .map(|c| (c.id.clone(), c.name.clone()))
        .collect();
    let chosen = s.voice.connector_id.clone().unwrap_or_default();
    let chosen_inst = s.voice_connector().cloned();
    let needs_realtime = match &chosen_inst {
        Some(c) => !realtime_capable(c),
        None => true,
    };
    let any_capable = s.voice_connectors().any(realtime_capable);
    let idle_mins_secs = s.voice.idle_timeout_secs;

    rsx! {
        div {
            class: "border border-subtle rounded-lg mb-4",
            div {
                class: "p-4 bg-section rounded-t-lg",
                h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium", "Voice" }
            }
            div {
                class: "p-4",
                div {
                    class: "flex items-center justify-between mb-4",
                    div {
                        label { class: "text-sm font-medium text-fg-muted", "Enable Voice" }
                        p { class: "text-xs text-fg-muted", "Tap the hotkey to start or end a conversation. Hold it to talk, release to send." }
                    }
                    input {
                        r#type: "checkbox",
                        class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                        checked: "{s.voice.enabled}",
                        onchange: move |e| { local_settings.write().voice.enabled = e.value() == "true"; }
                    }
                }
                p { class: "text-xs text-fg-muted mb-4", "While a conversation is live, your microphone audio streams to the selected provider." }
                div {
                    class: "mb-4",
                    label { class: "block text-sm font-medium text-fg-muted mb-1", "Voice profile" }
                    select {
                        class: "block w-64 px-3 py-2 bg-input border border-subtle rounded-md text-sm text-fg",
                        onchange: move |e| {
                            let v = e.value();
                            local_settings.write().voice.connector_id = if v.is_empty() { None } else { Some(v) };
                        },
                        option { value: "", selected: chosen.is_empty(), "None" }
                        for (id, name) in voice_profiles.iter() {
                            option { value: "{id}", selected: *id == chosen, "{name}" }
                        }
                    }
                    if voice_profiles.is_empty() || !any_capable {
                        p { class: "text-xs text-fg-muted mt-1", "Tick Voice on a Gemini profile under Connectors." }
                    }
                    if needs_realtime {
                        p { class: "text-xs text-yellow-400 mt-1", "{VOICE_NEEDS_REALTIME}" }
                    }
                }
                div {
                    class: "grid grid-cols-2 items-center gap-4 mb-4",
                    label { class: "text-sm text-fg-muted", "Hotkey" }
                    HotkeyRecorder {
                        value: s.voice.hotkey.clone(),
                        onchange: move |v: String| local_settings.write().voice.hotkey = v,
                    }
                }
                div {
                    class: "mb-4",
                    label { class: "block text-sm font-medium text-fg-muted mb-1", "Hold threshold (ms)" }
                    p { class: "text-xs text-fg-muted mb-1", "A press shorter than this is a tap; longer is push-to-talk." }
                    input {
                        r#type: "number",
                        min: "100",
                        max: "2000",
                        step: "50",
                        class: "w-32 rounded border border-subtle bg-input px-3 py-2 text-sm text-fg",
                        value: "{s.voice.hold_threshold_ms}",
                        oninput: move |e| {
                            if let Ok(v) = e.value().parse::<u64>() {
                                local_settings.write().voice.hold_threshold_ms = v.clamp(100, 2000);
                            }
                        }
                    }
                }
                div {
                    class: "mb-4",
                    label { class: "block text-sm font-medium text-fg-muted mb-1", "Idle timeout (seconds)" }
                    p { class: "text-xs text-fg-muted mb-1", "A conversation ends after this long with nobody speaking." }
                    input {
                        r#type: "number",
                        min: "10",
                        max: "3600",
                        class: "w-32 rounded border border-subtle bg-input px-3 py-2 text-sm text-fg",
                        value: "{idle_mins_secs}",
                        oninput: move |e| {
                            if let Ok(v) = e.value().parse::<u64>() {
                                local_settings.write().voice.idle_timeout_secs = v.clamp(10, 3600);
                            }
                        }
                    }
                }
                div {
                    class: "flex items-center justify-between mb-4",
                    div {
                        label { class: "text-sm font-medium text-fg-muted", "Speak urgent Heartbeat events" }
                        p { class: "text-xs text-fg-muted", "While a conversation is live, urgent events are spoken aloud." }
                    }
                    input {
                        r#type: "checkbox",
                        class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                        checked: "{s.voice.speak_heartbeat}",
                        onchange: move |e| { local_settings.write().voice.speak_heartbeat = e.value() == "true"; }
                    }
                }
                div {
                    class: "mb-4",
                    label { class: "block text-sm font-medium text-fg-muted mb-1", "Microphone input" }
                    p { class: "text-xs text-fg-muted mb-1", "Which microphone voice listens on. A device that's unplugged falls back to the system default." }
                    div {
                        class: "flex items-center gap-2",
                        select {
                            class: "block w-64 px-3 py-2 bg-input border border-subtle rounded-md text-sm text-fg",
                            onchange: move |e| {
                                let v = e.value();
                                local_settings.write().voice.input_device_uid = if v.is_empty() { None } else { Some(v) };
                            },
                            {
                                let default_name = devices.read().iter().find(|d| d.is_default).map(|d| d.name.clone());
                                let label = match default_name {
                                    Some(n) => format!("System default ({n})"),
                                    None => "System default".to_string(),
                                };
                                rsx! { option { value: "", selected: s.voice.input_device_uid.is_none(), "{label}" } }
                            }
                            for d in devices.read().iter() {
                                option {
                                    value: "{d.uid}",
                                    selected: s.voice.input_device_uid.as_deref() == Some(d.uid.as_str()),
                                    "{d.name}"
                                }
                            }
                            if let Some(uid) = s.voice.input_device_uid.clone().filter(|u| !devices.read().iter().any(|d| &d.uid == u)) {
                                option { value: "{uid}", selected: true, "Unavailable device (using default)" }
                            }
                        }
                        button {
                            class: "px-3 py-1.5 rounded-md text-xs bg-input text-fg-muted hover:text-fg border border-subtle",
                            onclick: move |_| devices.set(crate::voice::audio::list_input_devices()),
                            "Refresh"
                        }
                    }
                }
                div {
                    class: "flex items-center justify-between",
                    div {
                        label { class: "text-sm font-medium text-fg-muted", "Microphone" }
                        p {
                            class: "text-xs text-fg-muted",
                            match *mic.read() {
                                MicPermission::Granted => "Granted",
                                MicPermission::Undetermined => "Not yet asked",
                                MicPermission::Denied => "Denied. Allow Hobbes under System Settings, Privacy & Security, Microphone.",
                            }
                        }
                    }
                    match *mic.read() {
                        MicPermission::Granted => rsx! {},
                        MicPermission::Undetermined => rsx! {
                            button {
                                class: "px-3 py-1.5 rounded-md text-xs bg-input text-fg-muted hover:text-fg border border-subtle",
                                onclick: move |_| {
                                    spawn(async move {
                                        let r = crate::voice::audio::request_mic_permission().await;
                                        mic.set(r);
                                    });
                                },
                                "Allow microphone"
                            }
                        },
                        MicPermission::Denied => rsx! {
                            button {
                                class: "px-3 py-1.5 rounded-md text-xs bg-input text-fg-muted hover:text-fg border border-subtle",
                                onclick: move |_| {
                                    let _ = open::that("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone");
                                },
                                "Open System Settings"
                            }
                        },
                    }
                }
            }
        }
    }
}
