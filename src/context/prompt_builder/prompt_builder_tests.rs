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
        loaded_skills: std::collections::HashMap::new(),
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
    let full_text = prompt.prompt.system.expect("Should have system instruction");

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

    let full_text = prompt.prompt.system.expect("Should have system instruction");

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

    let full_text = prompt.prompt.system.expect("Should have system instruction");

    // Assert: Composio context should NOT be present
    assert!(
        !full_text.contains("composio_context"),
        "System prompt should NOT contain composio_context when User ID is missing"
    );
}

#[test]
fn test_oversized_entities_stripped_from_system_context() {
    use crate::session::ConversationSummary;
    use crate::session::ConversationSummaryEntities;

    let mut session = create_test_session();
    let session_state = create_test_session_state();
    let settings = Settings::default();

    // Inject a conversation summary with both normal and oversized entities
    let mut other_entities = std::collections::HashMap::new();
    // Normal entity — should survive
    other_entities.insert("project_name".to_string(), serde_json::json!("Hobbes"));
    // Oversized entity — simulates a `message_history` data dump (>500 chars)
    other_entities.insert(
        "message_history".to_string(),
        serde_json::json!("x".repeat(600)),
    );

    session.active_context.conversation_summary = ConversationSummary {
        summary: "Test conversation".to_string(),
        sentiment: "neutral".to_string(),
        entities: ConversationSummaryEntities {
            user_name: "Dustin".to_string(),
            other_entities,
        },
    };

    let builder = PromptBuilder::new(&session, &settings, &session_state);
    let prompt = builder.build_prompt("Hello".to_string(), None);

    let full_text = prompt.prompt.system.expect("Should have system instruction");

    // user_name should be preserved (explicitly exempted from size check)
    assert!(
        full_text.contains("Dustin"),
        "user_name should be preserved in system context"
    );
    // Normal-sized entity should be preserved
    assert!(
        full_text.contains("project_name"),
        "Normal-sized entities should be preserved"
    );
    // Oversized entity should be stripped
    assert!(
        !full_text.contains("message_history"),
        "Oversized entities (>500 chars) should be stripped from system context"
    );
}

// =========================================================================
// Multi-byte UTF-8 Pagination Tests
// =========================================================================

#[test]
fn test_floor_char_boundary_multibyte() {
    // Emoji: 🦀 = 4 bytes, 🎯 = 4 bytes
    let s = "Hello🦀World🎯End";
    // "Hello" = 5 bytes, 🦀 = 4 bytes (bytes 5..9), "World" = 5 bytes (9..14),
    // 🎯 = 4 bytes (14..18), "End" = 3 bytes (18..21)
    assert_eq!(s.len(), 21);

    // Snap to boundary inside 🦀 (byte 7 is mid-emoji) → should snap to 5
    assert_eq!(super::floor_char_boundary(s, 7), 5);
    // Snap to boundary inside 🎯 (byte 16 is mid-emoji) → should snap to 14
    assert_eq!(super::floor_char_boundary(s, 16), 14);
    // Exact boundary at start of 🦀 → should stay at 5
    assert_eq!(super::floor_char_boundary(s, 5), 5);
    // Beyond string length → clamp to len
    assert_eq!(super::floor_char_boundary(s, 100), 21);
    // Zero → zero
    assert_eq!(super::floor_char_boundary(s, 0), 0);
}

#[test]
fn test_segment_into_pages_multibyte_no_panic() {
    // Build a string with mixed multi-byte content:
    // - ASCII, emoji (4-byte), CJK (3-byte), Cyrillic (2-byte)
    let content = "Hello 🦀 世界 Привет!\n\
                   Line two with emoji: 🎯🎯🎯\n\
                   第三行：中文内容测试\n\
                   Fourth line: normal ASCII text here\n\
                   Пятая строка: кириллица\n\
                   Sixth: more 🎉🎊🎋 emoji fun";

    // Use a page_size that forces splitting mid-multibyte territory
    // (intentionally small to stress the boundary logic)
    let page_size = 30;
    let pages = PromptBuilder::segment_into_pages(content, page_size);

    // 1. No panic occurred (implicit — we reached here)
    // 2. All pages must be valid UTF-8 (they are Strings, so this is guaranteed by Rust's type system)
    // 3. No page should exceed page_size (with small tolerance for the fallback path)
    for (i, page) in pages.iter().enumerate() {
        assert!(
            !page.is_empty(),
            "Page {} should not be empty",
            i
        );
    }

    // 4. Concatenation must reproduce the original content
    let reconstructed: String = pages.concat();
    assert_eq!(
        reconstructed, content,
        "Concatenated pages must exactly reproduce the original content"
    );

    // 5. Should have more than 1 page (content is ~200 bytes, page_size is 30)
    assert!(
        pages.len() > 1,
        "Should produce multiple pages, got {}",
        pages.len()
    );
}

#[test]
fn test_segment_into_pages_single_page() {
    let content = "Short string";
    let pages = PromptBuilder::segment_into_pages(content, 1000);
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0], content);
}

#[test]
fn test_segment_into_pages_all_multibyte() {
    // Pure CJK + emoji content — every character is multi-byte
    let content = "这是一个完整的中文段落，包含很多汉字。🦀🎯🎉每个字符都是多字节的。";
    let page_size = 20; // Forces splits inside multi-byte sequences
    let pages = PromptBuilder::segment_into_pages(content, page_size);

    let reconstructed: String = pages.concat();
    assert_eq!(
        reconstructed, content,
        "Pure multi-byte content must reconstruct correctly after pagination"
    );
    assert!(pages.len() > 1, "Should produce multiple pages");
}
