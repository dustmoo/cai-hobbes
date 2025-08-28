use crate::session::Session;
use serde_json;

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
        if snapshot.is_empty() {
            return "".to_string();
        }

        let context_json = serde_json::to_string_pretty(&snapshot).unwrap_or_default();
        format!("<SYSTEM_CONTEXT>\n{}\n</SYSTEM_CONTEXT>\n", context_json)
    }
}