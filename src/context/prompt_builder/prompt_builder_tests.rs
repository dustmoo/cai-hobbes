use super::*;
use crate::session::ActiveContext;
use crate::session::Session;
use crate::settings::Settings;
use chrono::Utc;

// Obsolete tests removed. Schema sanitization is now handled in `src/gemini/convert.rs`.

// =========================================================================
// Composio Context Injection Tests
// =========================================================================

fn create_test_session() -> Session {
    Session {
        id: "test_session".to_string(),
        name: "Test Session".to_string(),
        messages: vec![],
        active_context: ActiveContext::default(),
        last_updated: Utc::now(),
        accumulated_cost: 0.0,
        accumulated_tokens: 0,
        accumulated_turns: 0,
        memory_optimization_summary: None,
        composio_profile: None,
    }
}

fn create_test_session_state() -> crate::session::SessionState {
    crate::session::SessionState::default()
}

#[test]
fn test_composio_context_injected_when_profile_is_fully_configured() {
    use crate::settings::ComposioProfile;

    let session = create_test_session();
    let session_state = create_test_session_state();

    let settings = Settings {
        composio_profiles: vec![ComposioProfile {
            id: "test-profile-id-001".to_string(),
            name: "Test Profile".to_string(),
            user_id: Some("test-user-id-123".to_string()),
            api_key: Some("sk-test-api-key".to_string()),
            ..Default::default()
        }],
        active_composio_profile: Some("test-profile-id-001".to_string()),
        ..Default::default()
    };

    let builder = PromptBuilder::new(&session, &settings, &session_state);
    let prompt = builder.build_prompt("Hello".to_string(), None);

    // Extract the system instruction text
    let system_instruction = prompt
        .system_instruction
        .expect("Should have system instruction");
    let full_text: String = system_instruction
        .parts
        .iter()
        .filter_map(|p| {
            if let Part::Text { text, .. } = p {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("");

    // Assert: Composio context should be present
    assert!(
        full_text.contains("composio_context"),
        "System prompt should contain composio_context"
    );
    assert!(
        full_text.contains("Test Profile"),
        "System prompt should contain active profile name"
    );
    assert!(
        full_text.contains("external tools via Composio"),
        "System prompt should explain Composio"
    );
}

#[test]
fn test_composio_context_not_injected_when_profile_missing_api_key() {
    use crate::settings::ComposioProfile;

    let session = create_test_session();
    let session_state = create_test_session_state();

    let settings = Settings {
        composio_profiles: vec![ComposioProfile {
            name: "Incomplete Profile".to_string(),
            user_id: Some("test-user-id-123".to_string()),
            api_key: None, // Missing API key
            ..Default::default()
        }],
        ..Default::default()
    };

    let builder = PromptBuilder::new(&session, &settings, &session_state);
    let prompt = builder.build_prompt("Hello".to_string(), None);

    let system_instruction = prompt
        .system_instruction
        .expect("Should have system instruction");
    let full_text: String = system_instruction
        .parts
        .iter()
        .filter_map(|p| {
            if let Part::Text { text, .. } = p {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("");

    // Assert: Composio context should NOT be present
    assert!(
        !full_text.contains("composio_context"),
        "System prompt should NOT contain composio_context when API key is missing"
    );
}

#[test]
fn test_composio_context_not_injected_when_profile_missing_user_id() {
    use crate::settings::ComposioProfile;

    let session = create_test_session();
    let session_state = create_test_session_state();

    let settings = Settings {
        composio_profiles: vec![ComposioProfile {
            name: "Incomplete Profile".to_string(),
            user_id: None, // Missing User ID
            api_key: Some("sk-test-api-key".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };

    let builder = PromptBuilder::new(&session, &settings, &session_state);
    let prompt = builder.build_prompt("Hello".to_string(), None);

    let system_instruction = prompt
        .system_instruction
        .expect("Should have system instruction");
    let full_text: String = system_instruction
        .parts
        .iter()
        .filter_map(|p| {
            if let Part::Text { text, .. } = p {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("");

    // Assert: Composio context should NOT be present
    assert!(
        !full_text.contains("composio_context"),
        "System prompt should NOT contain composio_context when User ID is missing"
    );
}
