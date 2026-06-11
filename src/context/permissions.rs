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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PermissionSettings {
    pub auto_approval_enabled: bool,
    pub granular_permissions: HashMap<ToolCategory, bool>,
    #[serde(default)]
    pub mcp_server_permissions: HashMap<String, bool>,
    #[serde(default)]
    pub skill_permissions: HashMap<String, bool>,
    pub max_ai_turns: u32,
}

impl Default for PermissionSettings {
    fn default() -> Self {
        Self {
            auto_approval_enabled: false,
            granular_permissions: HashMap::new(),
            mcp_server_permissions: HashMap::new(),
            skill_permissions: HashMap::new(),
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
    pub fn check_skill_permission(&self, skill_name: &str) -> PermissionStatus {
        let settings = self.settings.read();

        // 1. Per-Skill Toggle (Specific Rule Priority)
        if let Some(&allowed) = settings
            .permission_settings
            .skill_permissions
            .get(skill_name)
        {
            if allowed {
                return PermissionStatus::Allowed;
            } else {
                return PermissionStatus::RequiresPrompt;
            }
        }

        // 2. Global Auto-Approval Check (Fallback)
        if settings.permission_settings.auto_approval_enabled {
            return PermissionStatus::Allowed;
        }

        // 3. Default: Auto-proceed for skills
        // Skills only inject context; actual tool calls have their own permission controls.
        PermissionStatus::Allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus::prelude::*;

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
