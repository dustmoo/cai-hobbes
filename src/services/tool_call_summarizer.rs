use crate::session::SessionState;
use crate::settings::Settings;

const MAX_TOOL_SNAPSHOTS: usize = 20;

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
        session_id: &str,
    ) {
        let history = std::mem::take(&mut session_state.tool_call_history);
        if let Some(session) = session_state.sessions.get_mut(session_id) {
            for (counter, record) in history.into_iter().enumerate() {
                let summary = format!(
                    "Tool call '{}' on server '{}' finished with status '{}'.",
                    record.call.tool_name, record.call.server_name, record.result.status
                );
                let timestamp = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) + (counter as i64);
                let snapshot = serde_json::json!({
                    "tool_name": record.call.tool_name,
                    "arguments": record.call.arguments,
                    "result_summary": summary,
                    "created_at": timestamp,
                });
                session.active_context.extra.insert(
                    format!("tool_snapshot_{}", record.call.execution_id),
                    snapshot,
                );
            }

            // Evict oldest snapshots if over the cap by sorting by created_at timestamp
            let mut snapshot_entries: Vec<(String, i64)> = session.active_context.extra
                .iter()
                .filter(|(k, _)| k.starts_with("tool_snapshot_"))
                .map(|(k, v)| {
                    let ts = v.get("created_at")
                        .and_then(|t| t.as_i64())
                        .unwrap_or(0);
                    (k.clone(), ts)
                })
                .collect();

            if snapshot_entries.len() > MAX_TOOL_SNAPSHOTS {
                snapshot_entries.sort_by_key(|&(_, ts)| ts);
                let excess = snapshot_entries.len() - MAX_TOOL_SNAPSHOTS;
                for (key, _) in snapshot_entries.into_iter().take(excess) {
                    session.active_context.extra.remove(&key);
                    tracing::debug!("Evicted old tool snapshot: {}", key);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionState;
    use crate::components::shared::{ToolCallRecord, ToolCall, ToolResult, ToolCallStatus};
    use crate::settings::Settings;

    #[tokio::test]
    async fn test_tool_call_snapshot_capping() {
        let mut session_state = SessionState::default();
        let settings = Settings::default();
        let session_id = session_state.create_session_raw(None);

        // Generate 30 tool call records
        for i in 0..30 {
            let record = ToolCallRecord {
                call: ToolCall {
                    execution_id: format!("exec_{}", i),
                    server_name: "test-server".to_string(),
                    tool_name: format!("tool_{}", i),
                    arguments: "{}".to_string(),
                    status: ToolCallStatus::Completed,
                    response: "ok".to_string(),
                    thought_signature: None,
                    thought_summary: None,
                    cached_image_path: None,
                    result_summary: None,
                },
                result: ToolResult {
                    status: ToolCallStatus::Completed,
                    response: "ok".to_string(),
                },
                profile_color: None,
            };
            session_state.tool_call_history.push(record);
        }

        let summarizer = ToolCallSummarizer::new();
        summarizer.summarize_and_cleanup(&mut session_state, &settings, &session_id).await;

        let session_ref = session_state.sessions.get(&session_id).unwrap();
        let snapshot_keys: Vec<String> = session_ref.active_context.extra
            .keys()
            .filter(|k| k.starts_with("tool_snapshot_"))
            .cloned()
            .collect();

        // Should cap exactly at MAX_TOOL_SNAPSHOTS (20)
        assert_eq!(snapshot_keys.len(), 20);

        // First 10 should be evicted: "tool_snapshot_exec_0" through "tool_snapshot_exec_9" should NOT exist.
        for i in 0..10 {
            assert!(!session_ref.active_context.extra.contains_key(&format!("tool_snapshot_exec_{}", i)));
        }

        // Latest 20 should survive: "tool_snapshot_exec_10" through "tool_snapshot_exec_29" should exist.
        for i in 10..30 {
            assert!(session_ref.active_context.extra.contains_key(&format!("tool_snapshot_exec_{}", i)));
        }
    }
}
