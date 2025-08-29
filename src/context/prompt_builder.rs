use crate::session::Session;
use serde_json::{self, json};

/// A simple builder to format dynamic context for the LLM prompt.
pub struct PromptBuilder<'a> {
    session: &'a Session,
}

impl<'a> PromptBuilder<'a> {
    pub fn new(session: &'a Session) -> Self {
        Self { session }
    }

    /// Builds a context string from the active session's context.
    pub fn build_context_string(&self) -> String {
        let snapshot = &self.session.active_context;
        let mut context_map = snapshot.clone();

        // Inject AI persona and instructions directly into the context
        context_map.insert(
            "ai_persona".to_string(),
            json!("You are Hobbes, a helpful AI assistant named after the comic, ready to act how the user needs."),
        );

        // Check for user_name within the nested conversation_summary.entities structure,
        // as this is where the summarizer places it.
        let user_name = context_map
            .get("conversation_summary")
            .and_then(|summary| summary.get("entities"))
            .and_then(|entities| entities.get("user_name"))
            .and_then(|name| name.as_str());

        if user_name.is_none() || user_name.unwrap_or("").trim().is_empty() {
            context_map.insert(
                "user_instruction".to_string(),
                json!("Your user's name is not in the current SYSTEM_CONTEXT. Please ask them what they would like to be called."),
            );
        } else {
            // If the user's name is present, ensure the instruction to ask for it is removed.
            context_map.remove("user_instruction");
        }

        if context_map.is_empty() {
            return "".to_string();
        }

        let context_json = serde_json::to_string_pretty(&context_map).unwrap_or_default();
        format!("<SYSTEM_CONTEXT>\n{}\n</SYSTEM_CONTEXT>\n", context_json)
    }
}