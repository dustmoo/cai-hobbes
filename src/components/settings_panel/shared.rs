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
/// A change saved from outside the panel that touches only fields the app
/// writes on its own — `hobbes_read_tabs` (the Read other tabs toggle's
/// immediate save, the approval prompt's "always") and trust rules (made
/// at an approval) — patched into the panel's `draft` so its other unsaved
/// edits survive. `None` when `new` differs from `prev` in anything else:
/// the caller then replaces the draft as before.
pub(super) fn patch_draft(prev: &Settings, new: &Settings, draft: &Settings) -> Option<Settings> {
    let mut probe = comparable(new);
    probe.hobbes_read_tabs = prev.hobbes_read_tabs;
    probe.permission_settings.trust_rules = prev.permission_settings.trust_rules.clone();
    if probe != comparable(prev) {
        return None;
    }
    let mut out = draft.clone();
    out.hobbes_read_tabs = new.hobbes_read_tabs;
    out.permission_settings.trust_rules = new.permission_settings.trust_rules.clone();
    Some(out)
}

pub(super) fn comparable(s: &Settings) -> Settings {
    let mut c = s.clone();
    c.active_llm = Default::default();
    c.gemini_config = Default::default();
    c.openai_compat_config = Default::default();
    c.claude_config = Default::default();
    c
}

#[cfg(test)]
mod draft_patch_tests {
    use super::*;

    /// A change saved from outside the panel (Read other tabs, "always",
    /// a trust rule from an approval) is patched into the draft, keeping
    /// the panel's other unsaved edits; any other change still replaces it.
    #[test]
    fn an_outside_toggle_keeps_the_draft_s_other_edits() {
        let prev = Settings::default();
        let mut draft = prev.clone();
        draft.user_name = Some("unsaved edit".into());
        let mut saved = prev.clone();
        saved.hobbes_read_tabs = true;
        let patched = patch_draft(&prev, &saved, &draft).expect("patched, not replaced");
        assert!(patched.hobbes_read_tabs);
        assert_eq!(patched.user_name.as_deref(), Some("unsaved edit"));
        // A trust rule authored at an approval rides the same way.
        let mut ruled = prev.clone();
        ruled.permission_settings.trust_rules.push(crate::context::permissions::TrustRule {
            id: "r".into(),
            server: "s".into(),
            tool: None,
            command_prefix: None,
            project_id: None,
            created_at: chrono::Utc::now(),
        });
        let patched = patch_draft(&prev, &ruled, &draft).unwrap();
        assert_eq!(patched.permission_settings.trust_rules.len(), 1);
        assert_eq!(patched.user_name.as_deref(), Some("unsaved edit"));
        // Anything else: not patchable (the caller replaces the draft).
        let mut other = prev.clone();
        other.heartbeat_enabled = true;
        assert!(patch_draft(&prev, &other, &draft).is_none());
    }
}
