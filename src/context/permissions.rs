use crate::settings::Settings;
use dioxus::prelude::Signal;
use dioxus_signals::Readable;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub enum ToolCategory {
    // ReadOnly, // e.g., read_file, list_files
    // Write,    // e.g., write_to_file, apply_diff
    // Execute,  // e.g., execute_command
    Mcp,      // General MCP tools
}

#[derive(Debug, PartialEq)]
pub enum PermissionStatus {
    Allowed,
    RequiresPrompt,
    Denied(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PermissionSettings {
    pub auto_approval_enabled: bool,
    pub granular_permissions: HashMap<ToolCategory, bool>,
    pub max_requests: u32,
    pub max_cost: f64,
}

impl Default for PermissionSettings {
    fn default() -> Self {
        Self {
            auto_approval_enabled: false,
            granular_permissions: HashMap::new(),
            max_requests: 10,
            max_cost: 0.50,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PermissionManager {
    settings: Signal<Settings>,
    request_count: Signal<u32>,
    current_cost: Signal<f64>,
}

impl PermissionManager {
    pub fn new(settings: Signal<Settings>) -> Self {
        Self {
            settings,
            request_count: Signal::new(0),
            current_cost: Signal::new(0.0),
        }
    }

    pub fn check_permission(&self, category: &ToolCategory) -> PermissionStatus {
        let settings = self.settings.read();
        if *self.request_count.read() >= settings.permission_settings.max_requests {
            return PermissionStatus::Denied("Request limit reached".to_string());
        }

        if *self.current_cost.read() >= settings.permission_settings.max_cost {
            return PermissionStatus::Denied("Cost limit reached".to_string());
        }

        if settings.permission_settings.auto_approval_enabled {
            // If auto-approval is on, check the granular permission for the specific category
            if settings
                .permission_settings
                .granular_permissions
                .get(category)
                .copied()
                .unwrap_or(false)
            {
                PermissionStatus::Allowed
            } else {
                PermissionStatus::Denied(format!("Auto-approval is on, but permission is denied for category: {:?}", category))
            }
        } else {
            // If auto-approval is off, always prompt
            PermissionStatus::RequiresPrompt
        }
    }

}