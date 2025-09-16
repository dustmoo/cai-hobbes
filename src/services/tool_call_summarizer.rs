use crate::session::SessionState;
use crate::settings::Settings;

pub struct ToolCallSummarizer {
    // We may need access to settings or other services in the future.
}

impl ToolCallSummarizer {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn summarize_and_cleanup(
        &self,
        session_state: &mut SessionState,
        _settings: &Settings,
    ) {
        let history = std::mem::take(&mut session_state.tool_call_history);
        if let Some(session) = session_state.get_active_session_mut() {
            for record in history {
                let summary = format!(
                    "Tool call '{}' on server '{}' finished with status '{}'.",
                    record.call.tool_name, record.call.server_name, record.result.status
                );
                let snapshot = serde_json::json!({
                    "tool_name": record.call.tool_name,
                    "arguments": record.call.arguments,
                    "result_summary": summary,
                    "full_result_ref": format!("qdrant_vector_id:{}", record.call.execution_id)
                });
                session.active_context.extra.insert(
                    format!("tool_snapshot_{}", record.call.execution_id),
                    snapshot,
                );
            }
        }
    }
}