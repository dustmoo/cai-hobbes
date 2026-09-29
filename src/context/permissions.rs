use crate::settings::Settings;
use dioxus::prelude::Signal;
use dioxus_signals::Readable;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub enum ToolCategory {
    Mcp, // All MCP tool calls use this single category
}

#[derive(Debug, PartialEq)]
pub enum PermissionStatus {
    Allowed,
    RequiresPrompt,
    #[allow(dead_code)]
    // API contract: matched in manager.rs but denials currently use RequiresPrompt → UI flow
    Denied(String),
}

/// A user-authored allow-rule, created from the permission bubble at the
/// moment of approval. Allow-only in v1: rules can only skip a prompt the
/// user would otherwise see, never widen anything else. Hit statistics live
/// in the `trust_decisions` table, not here — the gate never writes settings.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TrustRule {
    pub id: String,
    /// Matched against the RAW `ToolCall.server_name` (what the bubble saw),
    /// not the composio-collapsed permission name.
    pub server: String,
    /// None = any tool on the server.
    #[serde(default)]
    pub tool: Option<String>,
    /// Terminal-command prefix (token-boundary matched, metachar-guarded —
    /// see `context::trust::command_matches_prefix`).
    #[serde(default)]
    pub command_prefix: Option<String>,
    /// None = any project.
    #[serde(default)]
    pub project_id: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PermissionSettings {
    pub auto_approval_enabled: bool,
    pub granular_permissions: HashMap<ToolCategory, bool>,
    #[serde(default)]
    pub mcp_server_permissions: HashMap<String, bool>,
    #[serde(default)]
    pub skill_permissions: HashMap<String, bool>,
    #[serde(default)]
    pub trust_rules: Vec<TrustRule>,
    pub max_ai_turns: u32,
}

impl Default for PermissionSettings {
    fn default() -> Self {
        Self {
            auto_approval_enabled: false,
            granular_permissions: HashMap::new(),
            mcp_server_permissions: HashMap::new(),
            skill_permissions: HashMap::new(),
            trust_rules: Vec::new(),
            max_ai_turns: 25,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PermissionManager {
    settings: Signal<Settings>,
}

impl PermissionManager {
    pub fn new(settings: Signal<Settings>) -> Self {
        Self { settings }
    }

    pub fn check_mcp_permission(&self, server_name: &str) -> PermissionStatus {
        let settings = self.settings.read();

        // 1. Global Auto-Approval Check
        if !settings.permission_settings.auto_approval_enabled {
            return PermissionStatus::RequiresPrompt;
        }

        // 2. ToolCategory::Mcp Global Toggle
        if !settings
            .permission_settings
            .granular_permissions
            .get(&ToolCategory::Mcp)
            .copied()
            .unwrap_or(false)
        {
            // If global MCP tools are disabled, we return RequiresPrompt instead of Denied
            // to allow manual overrides for individual tool calls.
            return PermissionStatus::RequiresPrompt;
        }

        // 3. Per-Server Toggle
        if let Some(&allowed) = settings
            .permission_settings
            .mcp_server_permissions
            .get(server_name)
        {
            if allowed {
                PermissionStatus::Allowed
            } else {
                PermissionStatus::RequiresPrompt
            }
        } else {
            // Default to Allowed if no specific server setting exists (but global is on)
            PermissionStatus::Allowed
        }
    }

    pub fn is_turn_limit_reached_for(&self, turn_count: u32) -> bool {
        let settings = self.settings.read();
        turn_count >= settings.permission_settings.max_ai_turns
    }

    /// Check if skill execution is allowed based on permission settings.
    /// See [`PermissionSettings::skill_permission`] for the resolution table.
    pub fn check_skill_permission(
        &self,
        skill_name: &str,
        invoker: SkillInvoker,
    ) -> PermissionStatus {
        self.settings
            .read()
            .permission_settings
            .skill_permission(skill_name, invoker)
    }
}

/// Who is asking to load a skill. The user typing `/name` is itself the
/// approval; the model loading a skill on its own is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillInvoker {
    /// The user typed `/skill` in the chat input.
    User,
    /// The model called `HOBBES_INVOKE_SKILL`.
    Model,
}

impl PermissionSettings {
    /// The one skill-permission resolution function — the backend gates and
    /// the Settings → Permissions toggles all go through it, so the UI can
    /// never show a different state than the gate enforces.
    ///
    /// | `skill_permissions[name]` | model invoke      | user `/name`      |
    /// |---------------------------|-------------------|-------------------|
    /// | absent                    | RequiresPrompt    | Allowed           |
    /// | `true`                    | Allowed           | Allowed           |
    /// | `false`                   | RequiresPrompt    | RequiresPrompt    |
    ///
    /// `auto_approval_enabled` deliberately does NOT apply to skills. It is the
    /// blanket switch for MCP tool calls; letting it silently grant the model
    /// self-service skill loading would recreate the "UI shows OFF, behaviour is
    /// ON" mismatch. Skills are opted in one by one (Settings toggle, or the
    /// approval bubble's "Always allow").
    pub fn skill_permission(&self, skill_name: &str, invoker: SkillInvoker) -> PermissionStatus {
        match (self.skill_permissions.get(skill_name), invoker) {
            (Some(true), _) => PermissionStatus::Allowed,
            (Some(false), _) => PermissionStatus::RequiresPrompt,
            (None, SkillInvoker::User) => PermissionStatus::Allowed,
            (None, SkillInvoker::Model) => PermissionStatus::RequiresPrompt,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus::prelude::*;

    fn skill_settings(entry: Option<bool>) -> PermissionSettings {
        let mut s = PermissionSettings::default();
        if let Some(v) = entry {
            s.skill_permissions.insert("news".to_string(), v);
        }
        s
    }

    #[test]
    fn skill_permission_resolution_table() {
        use PermissionStatus::{Allowed, RequiresPrompt};
        use SkillInvoker::{Model, User};
        let cases = [
            (None, Model, RequiresPrompt),
            (None, User, Allowed),
            (Some(true), Model, Allowed),
            (Some(true), User, Allowed),
            (Some(false), Model, RequiresPrompt),
            (Some(false), User, RequiresPrompt),
        ];
        for (entry, invoker, expected) in cases {
            assert_eq!(
                skill_settings(entry).skill_permission("news", invoker),
                expected,
                "entry={entry:?} invoker={invoker:?}"
            );
        }
    }

    #[test]
    fn skill_permission_ignores_global_auto_approval() {
        let mut s = skill_settings(None);
        s.auto_approval_enabled = true;
        assert_eq!(
            s.skill_permission("news", SkillInvoker::Model),
            PermissionStatus::RequiresPrompt
        );
        let mut s = skill_settings(Some(false));
        s.auto_approval_enabled = true;
        assert_eq!(
            s.skill_permission("news", SkillInvoker::User),
            PermissionStatus::RequiresPrompt
        );
    }

    #[tokio::test]
    async fn test_turn_counting() {
        let mut dom = VirtualDom::new(|| {
            let settings = use_context_provider(|| Signal::new(Settings::default()));
            use_context_provider(|| Signal::new(PermissionManager::new(settings)));
            let pm = consume_context::<Signal<PermissionManager>>();

            use_effect(move || {
                let pm_read = pm.read();

                // Default limit is 25
                assert!(!pm_read.is_turn_limit_reached_for(0));
                assert!(!pm_read.is_turn_limit_reached_for(24));
                assert!(pm_read.is_turn_limit_reached_for(25));
                assert!(pm_read.is_turn_limit_reached_for(100));
            });

            rsx! { div {} }
        });

        dom.rebuild_in_place();
        dom.wait_for_suspense().await;
    }
}
