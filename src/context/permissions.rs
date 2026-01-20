use crate::settings::Settings;
use dioxus::prelude::Signal;
use dioxus_signals::{Readable, Writable};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub enum ToolCategory {
    // ReadOnly, // e.g., read_file, list_files
    // Write,    // e.g., write_to_file, apply_diff
    // Execute,  // e.g., execute_command
    Mcp, // General MCP tools
}

#[derive(Debug, PartialEq)]
pub enum PermissionStatus {
    Allowed,
    RequiresPrompt,
    #[allow(dead_code)]
    Denied(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PermissionSettings {
    pub auto_approval_enabled: bool,
    pub granular_permissions: HashMap<ToolCategory, bool>,
    #[serde(default)]
    pub mcp_server_permissions: HashMap<String, bool>,
    pub max_ai_turns: u32,
}

impl Default for PermissionSettings {
    fn default() -> Self {
        Self {
            auto_approval_enabled: false,
            granular_permissions: HashMap::new(),
            mcp_server_permissions: HashMap::new(),
            max_ai_turns: 10,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PermissionManager {
    settings: Signal<Settings>,
    turn_count: Signal<u32>,
}

impl PermissionManager {
    pub fn new(settings: Signal<Settings>) -> Self {
        Self {
            settings,
            turn_count: Signal::new(0),
        }
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

    pub fn increment_turn_count(&mut self) {
        *self.turn_count.write() += 1;
    }

    pub fn reset_turn_count(&mut self) {
        *self.turn_count.write() = 0;
    }

    pub fn is_turn_limit_reached(&self) -> bool {
        let settings = self.settings.read();
        *self.turn_count.read() >= settings.permission_settings.max_ai_turns
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
            let mut permission_manager = consume_context::<Signal<PermissionManager>>();

            use_effect(move || {
                let mut pm = permission_manager.write();

                assert!(!pm.is_turn_limit_reached());

                for _ in 0..10 {
                    pm.increment_turn_count();
                }

                assert!(pm.is_turn_limit_reached());

                pm.reset_turn_count();

                assert!(!pm.is_turn_limit_reached());
            });

            rsx! { div {} }
        });

        dom.rebuild_in_place();
        dom.wait_for_suspense().await;
    }
}
