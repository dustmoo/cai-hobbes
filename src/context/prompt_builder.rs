use crate::session::Session;
use crate::settings::Settings;
use serde_json::{self};

/// A simple builder to format dynamic context for the LLM prompt.
pub struct PromptBuilder<'a> {
    session: &'a Session,
    settings: &'a Settings,
}

impl<'a> PromptBuilder<'a> {
    pub fn new(session: &'a Session, settings: &'a Settings) -> Self {
        Self { session, settings }
    }

    /// Builds a context string from the active session's context.
    pub fn build_context_string(&self) -> String {
        let mut active_context = self.session.active_context.clone();
        active_context.system_persona = Some(self.settings.persona.clone());

        // Check for user_name directly via the typed struct.
        let user_name = &active_context.conversation_summary.entities.user_name;

        if user_name.trim().is_empty() {
            active_context.user_instruction = Some("Your user's name is not in the current SYSTEM_CONTEXT. Please ask them what they would like to be called.".to_string());
        } else {
            // If the user's name is present, ensure the instruction to ask for it is removed.
            active_context.user_instruction = None;
        }

        let context_json = serde_json::to_string_pretty(&active_context).unwrap_or_default();
        format!("<SYSTEM_CONTEXT>\n{}\n</SYSTEM_CONTEXT>\n", context_json)
    }
}