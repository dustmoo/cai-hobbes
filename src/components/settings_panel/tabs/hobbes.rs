//! Settings → Hobbes: the Hobbes tab (home profile, reading other tabs),
//! Voice, and the Heartbeat, gathered in one place. Every control keeps its
//! field and save behaviour from where it used to live (General / Planner).

use super::super::SettingsCtx;
use crate::components::hotkey_recorder::HotkeyRecorder;
use crate::settings::{
    realtime_capable, HeartbeatDaysOff, VoiceProfile, VOICE_END_OF_TURN_MAX_MS, VOICE_END_OF_TURN_MIN_MS,
    VOICE_NEEDS_REALTIME,
};
use crate::voice::audio::{MicPermission, OutputKind, VoiceEcho};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

/// Gemini Live's prebuilt voices with Google's one-word style for each
/// (Vertex AI "Configure language and voice", 30 voices).
const GEMINI_VOICES: &[(&str, &str)] = &[
    ("Zephyr", "Bright"),
    ("Puck", "Upbeat"),
    ("Charon", "Informative"),
    ("Kore", "Firm"),
    ("Fenrir", "Excitable"),
    ("Leda", "Youthful"),
    ("Orus", "Firm"),
    ("Aoede", "Breezy"),
    ("Callirrhoe", "Easy-going"),
    ("Autonoe", "Bright"),
    ("Enceladus", "Breathy"),
    ("Iapetus", "Clear"),
    ("Umbriel", "Easy-going"),
    ("Algieba", "Smooth"),
    ("Despina", "Smooth"),
    ("Erinome", "Clear"),
    ("Algenib", "Gravelly"),
    ("Rasalgethi", "Informative"),
    ("Laomedeia", "Upbeat"),
    ("Achernar", "Soft"),
    ("Alnilam", "Firm"),
    ("Schedar", "Even"),
    ("Gacrux", "Mature"),
    ("Pulcherrima", "Forward"),
    ("Achird", "Friendly"),
    ("Zubenelgenubi", "Casual"),
    ("Vindemiatrix", "Gentle"),
    ("Sadachbia", "Lively"),
    ("Sadaltager", "Knowledgeable"),
    ("Sulafat", "Warm"),
];

/// The voices whose name or style contains `filter` (case-insensitive),
/// in list order; an empty filter keeps them all.
fn filter_voices(filter: &str) -> Vec<(&'static str, &'static str)> {
    let f = filter.trim().to_lowercase();
    GEMINI_VOICES
        .iter()
        .copied()
        .filter(|(name, style)| f.is_empty() || name.to_lowercase().contains(&f) || style.to_lowercase().contains(&f))
        .collect()
}

/// A voice's style, when it is one of Gemini's.
fn voice_style(name: &str) -> Option<&'static str> {
    GEMINI_VOICES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, s)| *s)
}

const CARD: &str = "border border-subtle rounded-lg mb-4";
const CARD_HEAD: &str = "p-4 bg-section rounded-t-lg";
const CARD_TITLE: &str = "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium";
const GROUP: &str = "pt-4 mt-4 border-t border-subtle";
const GROUP_TITLE: &str = "text-[11px] uppercase tracking-[0.08em] text-fg-muted font-medium mb-3";
const PILL: &str = "px-2 py-0.5 rounded border border-subtle text-xs text-fg-muted";
const BTN: &str = "px-3 py-1.5 rounded-md text-xs bg-input text-fg-muted hover:text-fg border border-subtle";
const NUM: &str = "w-24 rounded border border-subtle bg-input px-3 py-2 text-sm text-fg";

#[component]
pub(in super::super) fn HobbesTab(ctx: SettingsCtx) -> Element {
    rsx! {
        HobbesTabCard { ctx }
        VoiceCard { ctx }
        HeartbeatCard { ctx }
    }
}

/// Home profile and whether the Hobbes tab may read other conversations.
#[component]
fn HobbesTabCard(ctx: SettingsCtx) -> Element {
    let mut local_settings = ctx.local_settings;
    let mut global_settings = ctx.settings;
    let settings_manager = ctx.settings_manager;
    let s = local_settings.read().clone();
    rsx! {
        div {
            class: CARD,
            div { class: CARD_HEAD, h3 { class: CARD_TITLE, "Hobbes tab" } }
            div {
                class: "p-4",
                div {
                    class: "mb-4",
                    label { class: "block text-sm font-medium text-fg-muted mb-1", "Hobbes home profile" }
                    p { class: "text-xs text-fg-muted mb-1",
                        "The Composio profile the Hobbes tab (the Heartbeat and voice) acts in by default. It can also use your other profiles; a write that lands outside home, or carries one profile's data into another's, asks you first."
                    }
                    select {
                        class: "block w-64 px-3 py-2 bg-input border border-subtle rounded-md text-sm text-fg",
                        onchange: move |e| {
                            let v = e.value();
                            local_settings.write().home_composio_profile = if v.is_empty() { None } else { Some(v) };
                        },
                        option { value: "", selected: s.home_composio_profile.is_none(), "Not set (each keeps its own)" }
                        for p in s.composio_profiles.iter() {
                            option { value: "{p.id}", selected: s.home_composio_profile.as_deref() == Some(p.id.as_str()), "{p.name}" }
                        }
                    }
                }
                // Saved the moment it is flipped, not with the panel's Save: the
                // gate reads the live setting at every call, so switching it off
                // revokes at once.
                div {
                    class: "flex items-center justify-between",
                    div {
                        label { class: "text-sm font-medium text-fg-muted", "Read other tabs" }
                        p { class: "text-xs text-fg-muted", "Lets the Hobbes tab read your other conversations. Off: it asks you each time." }
                    }
                    input {
                        r#type: "checkbox",
                        class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                        checked: "{s.hobbes_read_tabs}",
                        onchange: move |e| {
                            let on = e.value() == "true";
                            local_settings.write().hobbes_read_tabs = on;
                            // Only this field is saved; the panel's sync
                            // patches it into the draft and keeps the
                            // draft's other unsaved edits
                            // (`shared::patch_draft`).
                            global_settings.write().hobbes_read_tabs = on;
                            let snapshot = global_settings.peek().clone();
                            if let Err(e) = settings_manager.read().save(&snapshot) {
                                tracing::error!("settings save failed: {e}");
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The Voice card. Renders nothing when Voice isn't available (non-Pro).
#[component]
fn VoiceCard(ctx: SettingsCtx) -> Element {
    let mut local_settings = ctx.local_settings;
    let mut mic = use_signal(crate::voice::audio::mic_permission);
    let mut devices = use_signal(crate::voice::audio::list_input_devices);
    let mut output = use_signal(crate::voice::audio::default_output_kind);
    let mut check_level = use_signal(|| None::<f32>);
    let mut check_result = use_signal(|| None::<String>);
    let mut test_result = use_signal(|| None::<String>);
    let mut testing = use_signal(|| false);
    let mut voice_filter = use_signal(String::new);
    // The voice whose sample is being fetched or played, and the last
    // sample problem (shown under the list). `sample_playing` flips once
    // the clip is in hand: a spinner while connecting, a speaker while it plays.
    let mut sampling = use_signal(|| None::<&'static str>);
    let mut sample_playing = use_signal(|| false);
    let mut sample_note = use_signal(|| None::<String>);
    let mut live_models = use_signal(Vec::<String>::new);
    let mut live_models_loading = use_signal(|| false);
    let mut live_models_error = use_signal(|| Option::<String>::None);
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
    // The echo-unsafe case: speakers, and the settings would run without echo cancellation.
    let echo_unsafe = !crate::voice::audio::use_voice_processing(
        s.voice.echo,
        s.voice.input_device_uid.is_some(),
        *output.read(),
    ) && *output.read() == OutputKind::Speakers;

    // The voice profile's own settings (when it can do realtime).
    let model_conn = chosen_inst.clone().filter(realtime_capable);
    let model_section = model_conn.map(|inst| {
        let vp = inst.voice.clone().unwrap_or_default();
        let default_model = crate::settings::default_realtime_model(inst.provider()).unwrap_or("");
        let api_key = inst.config.api_key().cloned().unwrap_or_default();
        (inst.id.clone(), vp, default_model, api_key)
    });
    let pill_text = chosen_inst.as_ref().map(|c| {
        let vp = c.voice.clone().unwrap_or_default();
        let voice = vp.voice_name.filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "Kore".into());
        format!("{} · {}", c.provider().display_name(), voice)
    });

    rsx! {
        div {
            class: CARD,
            div {
                class: "{CARD_HEAD} flex items-center gap-2",
                h3 { class: CARD_TITLE, "Voice" }
                if *mic.read() == MicPermission::Granted {
                    span { class: "px-2 py-0.5 rounded border border-subtle text-xs text-green-400", "Mic allowed" }
                }
                if let Some(t) = pill_text {
                    span { class: PILL, "{t}" }
                }
                input {
                    r#type: "checkbox",
                    class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input ml-auto",
                    title: "Enable Voice",
                    "aria-label": "Enable Voice",
                    checked: "{s.voice.enabled}",
                    onchange: move |e| { local_settings.write().voice.enabled = e.value() == "true"; }
                }
            }
            div {
                class: "p-4",
                p { class: "text-xs text-fg-muted", "Tap the hotkey to start or end a conversation. Hold it to talk, release to send." }
                p { class: "text-xs text-fg-muted mt-1", "While a conversation is live, your microphone audio streams to the selected provider." }

                // ---- Model ----
                div {
                    class: GROUP,
                    h4 { class: GROUP_TITLE, "Model" }
                    div {
                        class: "grid grid-cols-1 md:grid-cols-2 gap-x-4 gap-y-3",
                        div {
                            label { class: "block text-sm font-medium text-fg-muted mb-1", "Voice profile" }
                            select {
                                class: "block w-full px-3 py-2 bg-input border border-subtle rounded-md text-sm text-fg",
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
                        if let Some((cid, vp, default_model, api_key)) = model_section {
                            {
                                let (model_id, load_id, sel_id, lang_id) =
                                    (cid.clone(), cid.clone(), cid.clone(), cid.clone());
                                rsx! {
                                    div {
                                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Realtime model" }
                                        div {
                                            class: "flex gap-2",
                                            input {
                                                class: "flex-1 min-w-0 px-3 py-2 bg-input border border-subtle rounded-md text-sm",
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
                                                class: "px-3 py-2 rounded-md text-xs bg-input text-fg-muted hover:text-fg border border-subtle shrink-0",
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
                                        class: "md:col-span-2",
                                        {
                                            let current = vp.voice_name.clone().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "Kore".into());
                                            let current_label = match voice_style(&current) {
                                                Some(style) => format!("{current} · {style}"),
                                                None => current.clone(),
                                            };
                                            let matches = filter_voices(&voice_filter.read());
                                            rsx! {
                                                div {
                                                    class: "flex items-center gap-2 mb-1",
                                                    label { class: "text-sm font-medium text-fg-muted", "Voice" }
                                                    span { class: PILL, "{current_label}" }
                                                }
                                                input {
                                                    class: "block w-full px-3 py-2 bg-input border border-subtle rounded-md text-sm",
                                                    placeholder: "Filter by name or style: warm, firm, bright…",
                                                    value: "{voice_filter}",
                                                    oninput: move |e| voice_filter.set(e.value()),
                                                }
                                                div {
                                                    class: "flex flex-wrap gap-1.5 mt-2",
                                                    for (name, style) in matches.iter().copied() {
                                                        {
                                                            let pick_id = sel_id.clone();
                                                            let on = name.eq_ignore_ascii_case(&current);
                                                            let busy = sampling.read().is_some() || crate::voice::controller::is_live();
                                                            let this_one = *sampling.read() == Some(name);
                                                            rsx! {
                                                                div {
                                                                    key: "{name}",
                                                                    class: if on {
                                                                        "flex items-center rounded-md text-xs border border-primary-500 bg-input"
                                                                    } else {
                                                                        "flex items-center rounded-md text-xs border border-subtle bg-input"
                                                                    },
                                                                    button {
                                                                        class: if on { "pl-2 pr-1 py-1 text-fg" } else { "pl-2 pr-1 py-1 text-fg-muted hover:text-fg" },
                                                                        title: "Use {name}",
                                                                        onclick: move |_| {
                                                                            if let Some(c) = local_settings.write().connector_by_id_mut(&pick_id) {
                                                                                c.voice.get_or_insert_with(VoiceProfile::default).voice_name = Some(name.to_string());
                                                                            }
                                                                        },
                                                                        "{name} "
                                                                        span { class: "text-fg-muted", "· {style}" }
                                                                    }
                                                                    button {
                                                                        class: "pl-1 pr-2 py-1 text-fg-muted hover:text-fg disabled:opacity-50",
                                                                        title: if this_one && *sample_playing.read() { "Playing…" } else if this_one { "Connecting…" } else { "Hear {name}" },
                                                                        "aria-label": "Hear {name}",
                                                                        disabled: busy,
                                                                        onclick: move |_| {
                                                                            let built = crate::voice::controller::realtime_for(&local_settings.read());
                                                                            sampling.set(Some(name));
                                                                            sample_playing.set(false);
                                                                            sample_note.set(None);
                                                                            spawn(async move {
                                                                                let r = match built {
                                                                                    Err(e) => Err(e),
                                                                                    Ok((provider, setup)) => {
                                                                                        match crate::voice::probe::voice_sample(provider.as_ref(), setup, name).await {
                                                                                            Err(e) => Err(format!("Couldn't get a sample of {name}: {e}")),
                                                                                            Ok((pcm, rate)) => {
                                                                                                sample_playing.set(true);
                                                                                                tokio::task::spawn_blocking(move || crate::voice::audio::play_clip(&pcm, rate))
                                                                                                .await
                                                                                                .map_err(|e| e.to_string())
                                                                                                .and_then(|r| r.map_err(|e| e.to_string()))
                                                                                                .map_err(|e| format!("Couldn't play the sample: {e}"))
                                                                                            }
                                                                                        }
                                                                                    }
                                                                                };
                                                                                if let Err(e) = r {
                                                                                    sample_note.set(Some(e));
                                                                                }
                                                                                sampling.set(None);
                                                                                sample_playing.set(false);
                                                                            });
                                                                        },
                                                                        if this_one && !*sample_playing.read() {
                                                                            span { class: "inline-block animate-spin h-2.5 w-2.5 border-2 border-primary-400 border-t-transparent rounded-full" }
                                                                        } else if this_one {
                                                                            Icon { width: 10, height: 10, icon: fi_icons::FiVolume2 }
                                                                        } else {
                                                                            Icon { width: 10, height: 10, icon: fi_icons::FiPlay }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                    if matches.is_empty() {
                                                        span { class: "text-xs text-fg-muted", "No voice matches." }
                                                    }
                                                }
                                                p { class: "text-xs text-fg-muted mt-1", "▶ plays a one-sentence sample through your speakers (one short call to the voice model; replays are free)." }
                                                if let Some(n) = sample_note.read().clone() {
                                                    p { class: "text-xs text-red-400 mt-1", "{n}" }
                                                }
                                            }
                                        }
                                    }
                                    div {
                                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Language" }
                                        input {
                                            class: "block w-full px-3 py-2 bg-input border border-subtle rounded-md text-sm",
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

                // ---- Talking ----
                div {
                    class: GROUP,
                    h4 { class: GROUP_TITLE, "Talking" }
                    div {
                        class: "grid grid-cols-1 md:grid-cols-2 gap-x-4 gap-y-3",
                        div {
                            label { class: "block text-sm font-medium text-fg-muted mb-1", "Hotkey" }
                            HotkeyRecorder {
                                value: s.voice.hotkey.clone(),
                                onchange: move |v: String| local_settings.write().voice.hotkey = v,
                            }
                        }
                        div {
                            label { class: "block text-sm font-medium text-fg-muted mb-1", "Tap vs. hold" }
                            div {
                                class: "flex items-center gap-2",
                                input {
                                    r#type: "number",
                                    min: "100",
                                    max: "2000",
                                    step: "50",
                                    class: NUM,
                                    value: "{s.voice.hold_threshold_ms}",
                                    oninput: move |e| {
                                        if let Ok(v) = e.value().parse::<u64>() {
                                            local_settings.write().voice.hold_threshold_ms = v.clamp(100, 2000);
                                        }
                                    }
                                }
                                span { class: "text-xs text-fg-muted", "ms" }
                            }
                            p { class: "text-xs text-fg-muted mt-1", "A press shorter than this is a tap; longer is push-to-talk." }
                        }
                        div {
                            label { class: "block text-sm font-medium text-fg-muted mb-1", "End of turn" }
                            div {
                                class: "flex items-center gap-2",
                                input {
                                    r#type: "number",
                                    min: "{VOICE_END_OF_TURN_MIN_MS}",
                                    max: "{VOICE_END_OF_TURN_MAX_MS}",
                                    step: "100",
                                    class: NUM,
                                    value: "{s.voice.end_of_turn_ms}",
                                    oninput: move |e| {
                                        if let Ok(v) = e.value().parse::<u64>() {
                                            // Clamped up to the minimum where it's used
                                            // (`end_of_turn`), so typing "1500" isn't
                                            // bumped to 300 at its first digit.
                                            local_settings.write().voice.end_of_turn_ms = v.min(VOICE_END_OF_TURN_MAX_MS);
                                        }
                                    }
                                }
                                span { class: "text-xs text-fg-muted", "ms of silence" }
                            }
                            p { class: "text-xs text-fg-muted mt-1", "Raise it if Hobbes answers while you pause." }
                        }
                        div {
                            label { class: "block text-sm font-medium text-fg-muted mb-1", "Hang up when idle" }
                            div {
                                class: "flex items-center gap-2",
                                input {
                                    r#type: "number",
                                    min: "10",
                                    max: "3600",
                                    class: NUM,
                                    value: "{s.voice.idle_timeout_secs}",
                                    oninput: move |e| {
                                        if let Ok(v) = e.value().parse::<u64>() {
                                            local_settings.write().voice.idle_timeout_secs = v.clamp(10, 3600);
                                        }
                                    }
                                }
                                span { class: "text-xs text-fg-muted", "s" }
                            }
                            p { class: "text-xs text-fg-muted mt-1", "A conversation ends after this long with nobody speaking." }
                        }
                    }
                }

                // ---- Audio ----
                div {
                    class: GROUP,
                    h4 { class: GROUP_TITLE, "Audio" }
                    div {
                        class: "mb-4",
                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Microphone" }
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
                                class: BTN,
                                onclick: move |_| {
                                    devices.set(crate::voice::audio::list_input_devices());
                                    output.set(crate::voice::audio::default_output_kind());
                                },
                                "Refresh"
                            }
                            match *mic.read() {
                                MicPermission::Granted => rsx! {},
                                MicPermission::Undetermined => rsx! {
                                    button {
                                        class: BTN,
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
                                        class: BTN,
                                        onclick: move |_| {
                                            let _ = open::that("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone");
                                        },
                                        "Open System Settings"
                                    }
                                },
                            }
                        }
                        if *mic.read() == MicPermission::Denied {
                            p { class: "text-xs text-yellow-400 mt-1", "Microphone denied. Allow Hobbes under System Settings, Privacy & Security, Microphone." }
                        }
                        if *mic.read() == MicPermission::Undetermined {
                            p { class: "text-xs text-fg-muted mt-1", "Microphone access not yet asked." }
                        }
                    }
                    div {
                        class: "mb-4",
                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Echo cancellation" }
                        p { class: "text-xs text-fg-muted mb-1",
                            "Stops Hobbes hearing itself on speakers. Apple's echo cancellation only works with the system default mic and switches Bluetooth headsets to call quality. Auto uses it only when no mic is chosen and you're on speakers; otherwise the chosen mic is used directly, and on speakers Hobbes only hears you over its own voice when you speak up clearly."
                        }
                        select {
                            class: "block w-64 px-3 py-2 bg-input border border-subtle rounded-md text-sm text-fg",
                            onchange: move |e| {
                                local_settings.write().voice.echo = match e.value().as_str() {
                                    "on" => VoiceEcho::On,
                                    "off" => VoiceEcho::Off,
                                    _ => VoiceEcho::Auto,
                                };
                            },
                            option { value: "auto", selected: s.voice.echo == VoiceEcho::Auto, "Auto (recommended)" }
                            option { value: "on", selected: s.voice.echo == VoiceEcho::On, "On (system default mic)" }
                            option { value: "off", selected: s.voice.echo == VoiceEcho::Off, "Off (use the chosen mic)" }
                        }
                        if echo_unsafe {
                            p { class: "text-xs text-yellow-400 mt-1",
                                "On speakers with this setup there's no echo cancellation: Hobbes may hear its own voice. Use headphones, the system default mic with echo cancellation on, or speak up clearly."
                            }
                        }
                    }
                    div {
                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Test the pipes" }
                        p { class: "text-xs text-fg-muted mb-2",
                            "Check mic listens for 3 seconds and shows the level (nothing is recorded or sent). Test voice model sends a short spoken question to the voice profile, without the mic, and shows what it heard and said back. Both use the settings above, saved or not."
                        }
                        div {
                            class: "flex items-center gap-2",
                    button {
                        class: "px-3 py-1.5 rounded-md text-xs bg-input text-fg-muted hover:text-fg border border-subtle disabled:opacity-50",
                        disabled: *testing.read() || crate::voice::controller::is_live(),
                        onclick: move |_| {
                            let (uid, echo) = {
                                let ls = local_settings.read();
                                (ls.voice.input_device_uid.clone(), ls.voice.echo)
                            };
                            testing.set(true);
                            check_result.set(Some("Listening… say something.".into()));
                            check_level.set(Some(0.0));
                            spawn(async move {
                                let r = crate::voice::probe::mic_check(uid, echo, std::time::Duration::from_secs(3), move |l| check_level.set(Some(l))).await;
                                check_level.set(None);
                                check_result.set(Some(match r {
                                    Ok(rep) => {
                                        let via = if rep.voice_processing { " · with Apple echo cancellation" } else { "" };
                                        format!("{}{via}", crate::voice::probe::verdict_text(rep.verdict(), &rep.input_name))
                                    }
                                    Err(e) => format!("Couldn't open the mic: {e}"),
                                }));
                                testing.set(false);
                            });
                        },
                        "Check mic"
                    }
                    button {
                        class: "px-3 py-1.5 rounded-md text-xs bg-input text-fg-muted hover:text-fg border border-subtle disabled:opacity-50",
                        disabled: *testing.read() || crate::voice::controller::is_live(),
                        onclick: move |_| {
                            let built = crate::voice::controller::realtime_for(&local_settings.read());
                            testing.set(true);
                            test_result.set(Some("Asking the voice model a test question…".into()));
                            spawn(async move {
                                let msg = match built {
                                    Err(e) => e,
                                    Ok((provider, mut setup)) => {
                                        setup.system_instruction = "This is an audio sound check. Answer in one short sentence.".into();
                                        setup.tools.clear();
                                        match tokio::task::spawn_blocking(|| crate::voice::probe::synth_speech(crate::voice::probe::TEST_QUESTION)).await {
                                            Ok(Ok(speech)) => match crate::voice::probe::speech_round_trip(provider.as_ref(), setup.clone(), speech).await {
                                                Ok(r) => {
                                                    let speech_line = format!(
                                                        "Working. Heard: “{}”. Replied: “{}” ({:.1} s of speech, first sound after {:.1} s).",
                                                        r.heard,
                                                        r.replied,
                                                        r.reply_secs,
                                                        r.first_audio_after.unwrap_or_default().as_secs_f32()
                                                    );
                                                    test_result.set(Some(format!("{speech_line} Now checking the brain handoff…")));
                                                    let handoff = match crate::voice::probe::brain_handoff_round_trip(provider.as_ref(), setup).await {
                                                        Ok(h) => {
                                                            let holding = [h.before_call.as_str(), h.holding.as_str()].join(" ").trim().to_string();
                                                            if holding.is_empty() {
                                                                format!("Brain handoff: the result came through (“{}”) but the voice stayed silent while waiting.", h.result)
                                                            } else {
                                                                format!("Brain handoff: said “{holding}” while waiting, then “{}”.", h.result)
                                                            }
                                                        }
                                                        Err(e) => format!("Brain handoff test failed: {e}"),
                                                    };
                                                    format!("{speech_line} {handoff}")
                                                }
                                                Err(e) => format!("The voice model test failed: {e}"),
                                            },
                                            Ok(Err(e)) => format!("Couldn't make the test speech: {e}"),
                                            Err(e) => format!("Couldn't make the test speech: {e}"),
                                        }
                                    }
                                };
                                test_result.set(Some(msg));
                                testing.set(false);
                            });
                        },
                        "Test voice model"
                    }
                    if let Some(l) = *check_level.read() {
                        div { class: "w-24 h-1.5 rounded bg-primary-800 overflow-hidden",
                            div { class: "h-full bg-primary-400", style: "width: {crate::components::voice_indicator::level_percent(l)}%" }
                        }
                    }
                        }
                        if let Some(r) = check_result.read().clone() {
                            p { class: "text-xs text-fg-muted mt-2", "{r}" }
                        }
                        if let Some(r) = test_result.read().clone() {
                            p { class: "text-xs text-fg-muted mt-1", "{r}" }
                        }
                    }
                }

                // ---- Speaks up ----
                div {
                    class: GROUP,
                    h4 { class: GROUP_TITLE, "Speaks up" }
                    div {
                        class: "flex items-center justify-between mb-4",
                        div {
                            label { class: "text-sm font-medium text-fg-muted", "Heartbeat news" }
                            p { class: "text-xs text-fg-muted", "When a Heartbeat wake finds something for you, Hobbes says \"Excuse me…\" and tells you (needs \"Results that come back later\" on)." }
                        }
                        input {
                            r#type: "checkbox",
                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                            checked: "{s.voice.speak_heartbeat}",
                            onchange: move |e| { local_settings.write().voice.speak_heartbeat = e.value() == "true"; }
                        }
                    }
                    div {
                        class: "flex items-center justify-between",
                        div {
                            label { class: "text-sm font-medium text-fg-muted", "Results that come back later" }
                            p { class: "text-xs text-fg-muted", "When dispatched work finishes or the brain answers after you've hung up, Hobbes says \"Excuse me…\" and tells you, with the mic off, instead of flagging the Hobbes tab." }
                        }
                        input {
                            r#type: "checkbox",
                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                            checked: "{s.voice.announce_results}",
                            onchange: move |e| { local_settings.write().voice.announce_results = e.value() == "true"; }
                        }
                    }
                }
            }
        }
    }
}

/// The Heartbeat card (Pro-gated via `heartbeat_available`).
#[component]
fn HeartbeatCard(ctx: SettingsCtx) -> Element {
    let mut local_settings = ctx.local_settings;
    if !crate::heartbeat::heartbeat_available() {
        return rsx! {};
    }
    let hb = local_settings.read().clone();
    rsx! {
        div {
            class: CARD,
            div { class: CARD_HEAD, h3 { class: CARD_TITLE, "Heartbeat" } }
            div {
                class: "p-4",
    div {
        class: "flex items-center justify-between mb-4",
        div {
            label { class: "text-sm font-medium text-fg-muted", "Enable Heartbeat" }
            p { class: "text-xs text-fg-muted", "Runs on its own between chats and spends from your connector." }
        }
        input {
            r#type: "checkbox",
            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
            checked: "{hb.heartbeat_enabled}",
            onchange: move |e| {
                local_settings.write().heartbeat_enabled = e.value() == "true";
            }
        }
    }
    div {
        class: "mb-4",
        label { class: "block text-sm font-medium text-fg-muted mb-1", "Schedule" }
        p { class: "text-xs text-fg-muted mb-2", "Morning and end-of-day briefings run on these days." }
        div {
            class: "flex items-center gap-2 mb-3",
            for (n, name, letter) in [(1u8, "Monday", "M"), (2, "Tuesday", "T"), (3, "Wednesday", "W"), (4, "Thursday", "T"), (5, "Friday", "F"), (6, "Saturday", "S"), (7, "Sunday", "S")] {
                {
                    let on = hb.heartbeat_workdays.contains(&n);
                    rsx! {
                        button {
                            key: "{n}",
                            r#type: "button",
                            title: "{name}",
                            "aria-label": "{name}",
                            "aria-pressed": "{on}",
                            class: if on {
                                "w-8 h-8 rounded-full border border-primary-600 bg-primary-600 text-sm font-medium text-white"
                            } else {
                                "w-8 h-8 rounded-full border border-subtle text-sm font-medium text-fg-muted hover:text-fg"
                            },
                            onclick: move |_| {
                                let mut s = local_settings.write();
                                if let Some(i) = s.heartbeat_workdays.iter().position(|d| *d == n) {
                                    s.heartbeat_workdays.remove(i);
                                } else {
                                    s.heartbeat_workdays.push(n);
                                    s.heartbeat_workdays.sort_unstable();
                                }
                            },
                            "{letter}"
                        }
                    }
                }
            }
        }
        div {
            class: "grid grid-cols-2 gap-4 mb-1 max-w-xs",
            div {
                label { class: "block text-xs text-fg-muted mb-1", "Workday starts" }
                input {
                    r#type: "time",
                    class: "w-full rounded border border-subtle bg-input px-3 py-2 text-sm text-fg",
                    value: "{hb.planner_workday_start}",
                    oninput: move |e| {
                        let v = e.value();
                        if !v.is_empty() {
                            local_settings.write().planner_workday_start = v;
                        }
                    }
                }
            }
            div {
                label { class: "block text-xs text-fg-muted mb-1", "Workday ends" }
                input {
                    r#type: "time",
                    class: "w-full rounded border border-subtle bg-input px-3 py-2 text-sm text-fg",
                    value: "{hb.planner_workday_end}",
                    oninput: move |e| {
                        let v = e.value();
                        if !v.is_empty() {
                            local_settings.write().planner_workday_end = v;
                        }
                    }
                }
            }
        }
        p { class: "text-xs text-fg-muted mb-3", "Shared with the Planner." }
        label { class: "block text-sm font-medium text-fg-muted mb-1", "Briefings" }
        div {
            class: "flex items-center gap-6 mb-3",
            label {
                class: "flex items-center gap-2 text-sm text-fg-muted",
                input {
                    r#type: "checkbox",
                    class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                    checked: "{hb.heartbeat_brief_start}",
                    onchange: move |e| { local_settings.write().heartbeat_brief_start = e.value() == "true"; }
                }
                "Start of day"
            }
            label {
                class: "flex items-center gap-2 text-sm text-fg-muted",
                input {
                    r#type: "checkbox",
                    class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                    checked: "{hb.heartbeat_brief_meetings}",
                    onchange: move |e| { local_settings.write().heartbeat_brief_meetings = e.value() == "true"; }
                }
                "Before meetings"
            }
            label {
                class: "flex items-center gap-2 text-sm text-fg-muted",
                input {
                    r#type: "checkbox",
                    class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                    checked: "{hb.heartbeat_brief_end}",
                    onchange: move |e| { local_settings.write().heartbeat_brief_end = e.value() == "true"; }
                }
                "End of day"
            }
        }
        label { class: "block text-sm font-medium text-fg-muted mb-1", "On days off" }
        p { class: "text-xs text-fg-muted mb-1", "You can always talk to Hobbes yourself." }
        select {
            class: "block w-64 px-3 py-2 bg-input border border-subtle rounded-md text-sm text-fg",
            onchange: move |e| {
                local_settings.write().heartbeat_days_off = match e.value().as_str() {
                    "meetings_only" => HeartbeatDaysOff::MeetingsOnly,
                    "off" => HeartbeatDaysOff::Off,
                    _ => HeartbeatDaysOff::MeetingsAndUrgent,
                };
            },
            option { value: "meetings_and_urgent", selected: hb.heartbeat_days_off == HeartbeatDaysOff::MeetingsAndUrgent, "Meetings and anything urgent" }
            option { value: "meetings_only", selected: hb.heartbeat_days_off == HeartbeatDaysOff::MeetingsOnly, "Meetings only" }
            option { value: "off", selected: hb.heartbeat_days_off == HeartbeatDaysOff::Off, "Stay quiet" }
        }
    }
    div {
        class: "mb-4",
        label { class: "block text-sm font-medium text-fg-muted mb-1", "Wakes per hour" }
        p { class: "text-xs text-fg-muted mb-1", "At most this many wakes in any 60 minutes (0 to 12). 0 never wakes." }
        input {
            r#type: "number",
            min: "0",
            max: "12",
            class: "w-32 rounded border border-subtle bg-input px-3 py-2 text-sm text-fg",
            value: "{hb.heartbeat_wakes_per_hour}",
            oninput: move |e| {
                if let Ok(v) = e.value().parse::<u32>() {
                    local_settings.write().heartbeat_wakes_per_hour = v.min(12);
                }
            }
        }
    }
    div {
        class: "mb-4",
        label { class: "block text-sm font-medium text-fg-muted mb-1", "Daily spend cap ($)" }
        p { class: "text-xs text-fg-muted mb-1", "Wakes pause for the day once their recorded cost reaches this." }
        input {
            r#type: "number",
            min: "0",
            step: "0.5",
            class: "w-32 rounded border border-subtle bg-input px-3 py-2 text-sm text-fg",
            value: "{hb.heartbeat_daily_usd}",
            oninput: move |e| {
                if let Ok(v) = e.value().parse::<f64>() {
                    if v.is_finite() && v >= 0.0 {
                        local_settings.write().heartbeat_daily_usd = v;
                    }
                }
            }
        }
    }
    div {
        class: "mb-4",
        label { class: "block text-sm font-medium text-fg-muted mb-1", "Sources" }
        p { class: "text-xs text-fg-muted mb-2", "What the Heartbeat watches. CI needs the gh CLI signed in." }
        div {
            class: "flex items-center gap-6",
            label {
                class: "flex items-center gap-2 text-sm text-fg-muted",
                input {
                    r#type: "checkbox",
                    class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                    checked: "{hb.heartbeat_source_planner}",
                    onchange: move |e| {
                        local_settings.write().heartbeat_source_planner = e.value() == "true";
                    }
                }
                "Planner"
            }
            label {
                class: "flex items-center gap-2 text-sm text-fg-muted",
                input {
                    r#type: "checkbox",
                    class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                    checked: "{hb.heartbeat_source_fleet}",
                    onchange: move |e| {
                        local_settings.write().heartbeat_source_fleet = e.value() == "true";
                    }
                }
                "Fleet"
            }
            label {
                class: "flex items-center gap-2 text-sm text-fg-muted",
                input {
                    r#type: "checkbox",
                    class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                    checked: "{hb.heartbeat_source_ci}",
                    onchange: move |e| {
                        local_settings.write().heartbeat_source_ci = e.value() == "true";
                    }
                }
                "CI"
            }
        }
    }
    if !hb.heartbeat_muted_kinds.is_empty() {
        div {
            class: "mb-1",
            label { class: "block text-sm font-medium text-fg-muted mb-1", "Muted kinds" }
            p { class: "text-xs text-fg-muted mb-2", "Events of these kinds never wake the Heartbeat. Click one to unmute." }
            div {
                class: "flex flex-wrap gap-2",
                for kind in hb.heartbeat_muted_kinds.iter().cloned() {
                    {
                        let k = kind.clone();
                        rsx! {
                            button {
                                key: "{kind}",
                                class: "px-2 py-0.5 rounded border border-subtle text-xs text-fg-muted hover:text-fg hover:bg-card",
                                title: "Unmute",
                                onclick: move |_| {
                                    local_settings.write().heartbeat_muted_kinds.retain(|x| x != &k);
                                },
                                "{kind} ×"
                            }
                        }
                    }
                }
            }
        }
    }
            }
        }
    }
}

#[cfg(test)]
mod voice_list_tests {
    use super::*;

    #[test]
    fn the_voice_filter_matches_name_or_style() {
        assert_eq!(filter_voices("").len(), 30);
        assert_eq!(filter_voices("warm"), vec![("Sulafat", "Warm")]);
        let firm: Vec<_> = filter_voices("FIRM").into_iter().map(|(n, _)| n).collect();
        assert_eq!(firm, vec!["Kore", "Orus", "Alnilam"]);
        assert_eq!(filter_voices("pu").first(), Some(&("Puck", "Upbeat")));
        assert!(filter_voices("nope").is_empty());
        assert_eq!(voice_style("kore"), Some("Firm"));
        assert_eq!(voice_style("Custom"), None);
    }
}
