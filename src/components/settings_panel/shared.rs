use crate::settings::Settings;
use dioxus::prelude::*;

/// Shared class string for text/number inputs and selects across the
/// Settings drawer. Route every such control through this constant instead
/// of hand-rolled class strings so the drawer's input styling can't drift.
pub(crate) const INPUT: &str = "h-8 bg-app border border-faint rounded px-2.5 text-sm text-fg focus:outline-none focus:ring-2 focus:ring-primary-500 w-full";

/// A 34x18 on/off switch rendered from a plain checkbox, so existing
/// `bool` signals and `onchange` handlers keep working unchanged while the
/// visual is consistent across the whole panel.
#[component]
pub(crate) fn Toggle(checked: bool, onchange: EventHandler<bool>) -> Element {
    rsx! {
        label {
            class: "relative inline-flex items-center cursor-pointer shrink-0",
            input {
                r#type: "checkbox",
                class: "sr-only peer",
                checked,
                onchange: move |evt| onchange.call(evt.checked()),
            }
            span {
                class: if checked {
                    "w-[34px] h-[18px] rounded-full bg-primary-500 transition-colors relative after:content-[''] after:absolute after:top-[2px] after:left-[18px] after:w-[14px] after:h-[14px] after:rounded-full after:bg-white after:transition-all"
                } else {
                    "w-[34px] h-[18px] rounded-full bg-card border border-faint transition-colors relative after:content-[''] after:absolute after:top-[1px] after:left-[1px] after:w-[14px] after:h-[14px] after:rounded-full after:bg-fg-muted after:transition-all"
                },
            }
        }
    }
}

/// Copy a connector's config into the matching legacy singleton slot so the
/// per-provider editing blocks (bound to `gemini_config` / `openai_compat_config`
/// / `claude_config`) edit it in place. Also points `active_llm` at its kind so
/// the right block renders.
pub(super) fn load_instance_into_draft(draft: &mut Settings, connector_id: &str) {
    use crate::settings::ProviderInstanceConfig;
    if let Some(instance) = draft.connector_by_id(connector_id).cloned() {
        match instance.config {
            ProviderInstanceConfig::Gemini(c) => draft.gemini_config = c,
            ProviderInstanceConfig::OpenAiCompat(c) => draft.openai_compat_config = c,
            ProviderInstanceConfig::Claude(c) => draft.claude_config = c,
        }
    }
}

/// The provider kind of the connector currently selected for editing.
pub(super) fn selected_kind(
    draft: &Settings,
    selected: &Option<String>,
) -> Option<crate::settings::LlmProvider> {
    selected
        .as_deref()
        .and_then(|id| draft.connector_by_id(id))
        .map(|c| c.provider())
}

/// Flush edits from the legacy singleton slot back into the connector instance.
/// Returns true when the instance actually changed.
pub(super) fn flush_draft_into_instance(draft: &mut Settings, connector_id: &str) -> bool {
    use crate::settings::{LlmProvider, ProviderInstanceConfig};
    let kind = match draft.connector_by_id(connector_id) {
        Some(c) => c.provider(),
        None => return false,
    };
    let config = match kind {
        LlmProvider::Gemini => ProviderInstanceConfig::Gemini(draft.gemini_config.clone()),
        LlmProvider::OpenAiCompat => {
            ProviderInstanceConfig::OpenAiCompat(draft.openai_compat_config.clone())
        }
        LlmProvider::Claude => ProviderInstanceConfig::Claude(draft.claude_config.clone()),
    };
    if let Some(instance) = draft.connector_by_id_mut(connector_id) {
        if instance.config != config {
            instance.config = config;
            return true;
        }
    }
    false
}

/// Settings view used for change detection. The legacy singleton configs and
/// `active_llm` are editing mirrors of the selected connector (their edits are
/// flushed into `llm_connectors` reactively), so they're excluded to avoid
/// phantom "unsaved changes" when a connector is merely selected for editing.
pub(super) fn comparable(s: &Settings) -> Settings {
    let mut c = s.clone();
    c.active_llm = Default::default();
    c.gemini_config = Default::default();
    c.openai_compat_config = Default::default();
    c.claude_config = Default::default();
    c
}
