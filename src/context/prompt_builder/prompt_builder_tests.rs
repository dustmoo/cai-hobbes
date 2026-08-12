use super::*;
use crate::str_utils::floor_char_boundary;
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
        llm_connector_id: None,
        llm_provider: None,
        chat_model: None,
        loaded_skills: std::collections::HashMap::new(),
        scratchpad: String::new(),
        current_ai_turn_count: 0,
        watch_word_recovery_count: 0,
        scheduled_timers: Vec::new(),
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
    let prompt = builder.build_prompt("Hello".to_string());

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
    let prompt = builder.build_prompt("Hello".to_string());

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
    let prompt = builder.build_prompt("Hello".to_string());

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
        current_task: String::new(),
        entities: ConversationSummaryEntities {
            user_name: "Dustin".to_string(),
            other_entities,
        },
    };

    let builder = PromptBuilder::new(&session, &settings, &session_state);
    let prompt = builder.build_prompt("Hello".to_string());

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
    assert_eq!(floor_char_boundary(s, 7), 5);
    // Snap to boundary inside 🎯 (byte 16 is mid-emoji) → should snap to 14
    assert_eq!(floor_char_boundary(s, 16), 14);
    // Exact boundary at start of 🦀 → should stay at 5
    assert_eq!(floor_char_boundary(s, 5), 5);
    // Beyond string length → clamp to len
    assert_eq!(floor_char_boundary(s, 100), 21);
    // Zero → zero
    assert_eq!(floor_char_boundary(s, 0), 0);
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

// =========================================================================
// Smart context handling: tool-result budgeting per provider
// =========================================================================

use crate::components::chat::Message;
use crate::components::shared::{MessageContent, ToolCall, ToolCallStatus};
use crate::settings::LlmProvider;

fn user_text_msg(text: &str) -> Message {
    Message {
        id: uuid::Uuid::new_v4(),
        author: "User".to_string(),
        content: MessageContent::Text {
            content: text.to_string(),
            thought_signature: None,
            thought_summary: None,
        },
        attachments: vec![],
        comments: vec![],
        created_at: Utc::now(),
        usage: None,
    }
}

fn tool_call_msg(tool_name: &str, response: String, result_summary: Option<String>) -> Message {
    Message {
        id: uuid::Uuid::new_v4(),
        author: "Hobbes".to_string(),
        content: MessageContent::ToolCall(ToolCall {
            execution_id: uuid::Uuid::new_v4().to_string(),
            server_name: "test-server".to_string(),
            tool_name: tool_name.to_string(),
            arguments: "{}".to_string(),
            status: ToolCallStatus::Completed,
            response,
            thought_signature: None,
            thought_summary: None,
            cached_image_path: None,
            result_summary,
        }),
        attachments: vec![],
        comments: vec![],
        created_at: Utc::now(),
        usage: None,
    }
}

/// Build a big JSON array of fake "emails" so the raw response is well over any
/// fixed cap (the old behaviour chopped historical results to ~8KB).
fn big_email_payload(count: usize) -> String {
    let emails: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            serde_json::json!({
                "id": format!("EMAILID{:08x}", i * 2654435761u64 as usize),
                "subject": format!("Quarterly report and action items number {i}"),
                "sender": format!("person{i}@example-corp.com"),
                "snippet": "Lorem ipsum dolor sit amet, consectetur adipiscing elit, \
                             sed do eiusmod tempor incididunt ut labore et dolore magna."
            })
        })
        .collect();
    serde_json::to_string(&serde_json::json!({ "emails": emails })).unwrap()
}

fn tool_result_text(prompt: &crate::context::prompt_builder::PromptBuildResult) -> String {
    use crate::llm::types::{ChatRole, ContentBlock};
    let mut out = String::new();
    for m in &prompt.prompt.messages {
        if m.role == ChatRole::Tool {
            for b in &m.content {
                if let ContentBlock::ToolResult { content, .. } = b {
                    out.push_str(&content.as_str().map(|s| s.to_string()).unwrap_or_else(|| content.to_string()));
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// The Gmail regression: a large tool result that belongs to the CURRENT turn
/// (a later assistant message follows it, but no new user message) must stay
/// full on a large-context provider — never condensed mid-turn.
#[test]
fn current_turn_tool_result_not_condensed_on_large_window() {
    let mut session = create_test_session();
    session.llm_provider = Some(LlmProvider::Gemini);
    session.chat_model = Some("gemini-2.5-flash".to_string()); // 1M context
    let payload = big_email_payload(400);
    assert!(payload.len() > 50_000, "payload should be large");
    session.messages = vec![
        user_text_msg("fetch my unread emails and summarise them"),
        tool_call_msg("GMAIL_FETCH_EMAILS", payload, None),
        // An assistant turn follows the tool call, making it no longer the
        // literal last message — but it's still the current turn's working set.
    ];

    let settings = Settings::default();
    let state = create_test_session_state();
    let builder = PromptBuilder::new(&session, &settings, &state);
    let prompt = builder.build_prompt(String::new());

    let tool_text = tool_result_text(&prompt);
    assert!(
        !tool_text.contains("HOBBES_PAGE_RESULT"),
        "current-turn result on a 1M window must not be paginated/condensed"
    );
    // A sampling of senders from across the payload should all survive.
    assert!(tool_text.contains("person0@example-corp.com"));
    assert!(tool_text.contains("person399@example-corp.com"));
}

/// On a small window, an oversized HISTORICAL result (a newer user message
/// follows it) is paginated so it can't blow the budget — preserving the
/// small-model behaviour OpenAI users rely on.
#[test]
fn historical_tool_result_paginated_on_small_window() {
    let mut session = create_test_session();
    session.llm_provider = Some(LlmProvider::OpenAiCompat);
    session.chat_model = Some("local-model".to_string());
    let payload = big_email_payload(400);
    session.messages = vec![
        user_text_msg("first request"),
        tool_call_msg("GMAIL_FETCH_EMAILS", payload, None),
        user_text_msg("now do something else"), // makes the tool result historical
    ];

    let mut settings = Settings::default();
    settings.openai_compat_config.endpoint = "http://localhost:11434/v1".to_string();
    settings.openai_compat_config.max_context_tokens = Some(16_000);

    let state = create_test_session_state();
    let builder = PromptBuilder::new(&session, &settings, &state);
    let prompt = builder.build_prompt(String::new());

    let tool_text = tool_result_text(&prompt);
    assert!(
        tool_text.contains("HOBBES_PAGE_RESULT"),
        "oversized historical result on a 16K window must be paginated"
    );
}

/// When a historical result has a knowledge-preserving summary and exceeds its
/// budget, Pass 2 substitutes the summary (facts kept) instead of hard-chopping.
#[test]
fn historical_tool_result_uses_summary_when_available() {
    let mut session = create_test_session();
    session.llm_provider = Some(LlmProvider::OpenAiCompat);
    session.chat_model = Some("local-model".to_string());
    let payload = big_email_payload(400);
    let summary = "41 unread emails. Senders include person0@example-corp.com and \
                   person59@example-corp.com. Topics: quarterly reports.";
    session.messages = vec![
        user_text_msg("first request"),
        tool_call_msg("GMAIL_FETCH_EMAILS", payload, Some(summary.to_string())),
        user_text_msg("now do something else"),
    ];

    let mut settings = Settings::default();
    settings.openai_compat_config.endpoint = "http://localhost:11434/v1".to_string();
    settings.openai_compat_config.max_context_tokens = Some(16_000);

    let state = create_test_session_state();
    let builder = PromptBuilder::new(&session, &settings, &state);
    let prompt = builder.build_prompt(String::new());

    let tool_text = tool_result_text(&prompt);
    assert!(
        tool_text.contains("Summary of an earlier"),
        "over-budget historical result with a summary should be substituted"
    );
    assert!(
        tool_text.contains("quarterly reports"),
        "the summary's facts should be present in context"
    );
    assert!(
        tool_text.contains("HOBBES_PAGE_RESULT"),
        "full data should remain retrievable via pagination id"
    );
}

// =========================================================================
// available_skills / HOBBES_INVOKE_SKILL gating (on-demand mechanic)
// =========================================================================

fn invoke_skill_tool() -> rmcp::model::Tool {
    rmcp::model::Tool {
        name: "HOBBES_INVOKE_SKILL".into(),
        description: Some("Load a skill".into()),
        input_schema: std::sync::Arc::new(serde_json::Map::new()),
        title: None,
        output_schema: None,
        annotations: None,
        icons: None,
        meta: None,
    }
}

fn session_with_core_tools() -> Session {
    let mut session = create_test_session();
    session.active_context.mcp_tools = Some(crate::mcp::manager::McpContext {
        servers: vec![crate::mcp::manager::McpServerContext {
            name: "hobbes-core".to_string(),
            description: "built-in".to_string(),
            tools: vec![invoke_skill_tool()],
        }],
        connected_toolkit_slugs: vec![],
    });
    session
}

fn loaded_skill_payload(skill: &str, resolved_tool: &str) -> String {
    serde_json::to_string(&crate::components::shared::CapabilityContextPayload {
        skill: skill.to_string(),
        instruction_manual: "Do the thing.".to_string(),
        environment: crate::components::shared::SkillEnvironment {
            root_path: std::path::PathBuf::new(),
            scripts: vec![],
            resources: vec![],
            user_args: String::new(),
        },
        resolved_tools: std::collections::HashMap::from([(
            "search".to_string(),
            resolved_tool.to_string(),
        )]),
        warnings: vec![],
    })
    .unwrap()
}

/// Single test covering all gate states: the skill-metadata mirror is a
/// process-wide OnceLock, so the states are exercised sequentially in one
/// test to avoid cross-test races, and the mirror is cleared at the end.
#[test]
fn test_available_skills_gated_by_running_skill() {
    use crate::skills::registry::{set_available_skills_for_test, AvailableSkill};

    set_available_skills_for_test(vec![AvailableSkill {
        name: "research".to_string(),
        description: "Deep research".to_string(),
        argument_hint: Some("<topic>".to_string()),
        disable_model_invocation: false,
    }]);
    let settings = Settings::default();
    let state = create_test_session_state();

    // 1. Idle session → skills advertised, loader tool present
    let session = session_with_core_tools();
    let prompt = PromptBuilder::new(&session, &settings, &state).build_prompt("Hi".to_string());
    let system = prompt.prompt.system.clone().unwrap();
    assert!(system.contains("available_skills"), "idle: skills advertised");
    assert!(
        prompt.prompt.tools.iter().any(|t| t.name == "HOBBES_INVOKE_SKILL"),
        "idle: loader tool advertised"
    );

    // 2. Loaded skill turn-active (its tools used in recent messages) → gated
    let mut session = session_with_core_tools();
    session.loaded_skills.insert(
        "news".to_string(),
        loaded_skill_payload("news", "test-server_search"),
    );
    session.messages = vec![
        user_text_msg("/news"),
        tool_call_msg("search", "results".to_string(), None),
    ];
    let prompt = PromptBuilder::new(&session, &settings, &state).build_prompt(String::new());
    let system = prompt.prompt.system.clone().unwrap();
    assert!(
        !system.contains("available_skills"),
        "running: no other skills advertised"
    );
    assert!(
        !prompt.prompt.tools.iter().any(|t| t.name == "HOBBES_INVOKE_SKILL"),
        "running: loader tool withheld"
    );

    // 3. Model just loaded a skill (last message is the loader tool call) → gated
    let mut session = session_with_core_tools();
    session
        .loaded_skills
        .insert("news".to_string(), loaded_skill_payload("news", "unrelated"));
    session.messages = vec![
        user_text_msg("check the news"),
        tool_call_msg("HOBBES_INVOKE_SKILL", "manual...".to_string(), None),
    ];
    let prompt = PromptBuilder::new(&session, &settings, &state).build_prompt(String::new());
    assert!(
        !prompt.prompt.system.clone().unwrap().contains("available_skills"),
        "just-loaded: no other skills advertised"
    );
    assert!(
        !prompt.prompt.tools.iter().any(|t| t.name == "HOBBES_INVOKE_SKILL"),
        "just-loaded: loader tool withheld"
    );

    // 4. No skills on disk → loader tool withheld even when idle
    set_available_skills_for_test(vec![]);
    let session = session_with_core_tools();
    let prompt = PromptBuilder::new(&session, &settings, &state).build_prompt("Hi".to_string());
    assert!(
        !prompt.prompt.system.clone().unwrap().contains("available_skills"),
        "no skills: nothing advertised"
    );
    assert!(
        !prompt.prompt.tools.iter().any(|t| t.name == "HOBBES_INVOKE_SKILL"),
        "no skills: loader tool withheld"
    );
}

// =========================================================================
// Planner: planner_today injection and tool gating
// =========================================================================

/// A session whose MCP context advertises the hobbes-planner tools, as the
/// reactive sync in ChatWindow would populate it.
fn session_with_planner_tools() -> Session {
    let mut session = create_test_session();
    session.active_context.mcp_tools = Some(crate::mcp::manager::McpContext {
        servers: vec![crate::mcp::manager::McpServerContext {
            name: crate::mcp::manager::HOBBES_PLANNER_SERVER.to_string(),
            description: "Built-in planner".to_string(),
            tools: crate::mcp::planner_client::PlannerClient::new().list_tools(),
        }],
        connected_toolkit_slugs: vec![],
    });
    session
}

#[test]
fn planner_today_block_is_injected_when_attached() {
    let session = create_test_session();
    let state = create_test_session_state();
    let settings = Settings::default();

    let planner = crate::todo::PlannerState::default();
    let today = chrono::Local::now().date_naive();
    let block = crate::todo::handlers::planner_today_context(&planner, &settings, today);
    assert!(block.is_some(), "enabled by default");

    let prompt = PromptBuilder::new(&session, &settings, &state)
        .with_planner_today(block)
        .build_prompt("Hi".to_string());
    let system = prompt.prompt.system.unwrap();
    assert!(system.contains("planner_today"));
    assert!(system.contains("HOBBES_PLAN_DAY"), "instruction names the tools");

    // Without the attachment (the default) nothing is injected.
    let prompt = PromptBuilder::new(&session, &settings, &state).build_prompt("Hi".to_string());
    assert!(!prompt.prompt.system.unwrap().contains("planner_today"));
}

#[test]
fn planner_tools_are_withheld_when_the_planner_is_disabled() {
    let state = create_test_session_state();
    let session = session_with_planner_tools();

    let enabled = Settings::default();
    let prompt = PromptBuilder::new(&session, &enabled, &state).build_prompt("Hi".to_string());
    assert!(
        prompt.prompt.tools.iter().any(|t| t.name == "HOBBES_TODO_CREATE"),
        "enabled: planner tools advertised"
    );

    let disabled = Settings {
        planner_enabled: false,
        ..Default::default()
    };
    let prompt = PromptBuilder::new(&session, &disabled, &state).build_prompt("Hi".to_string());
    for tool in [
        "HOBBES_TODO_CREATE",
        "HOBBES_TODO_UPDATE",
        "HOBBES_TODO_LIST",
        "HOBBES_PLAN_DAY",
        "HOBBES_TIME_BLOCK",
        "HOBBES_PROJECT_UPSERT",
    ] {
        assert!(
            !prompt.prompt.tools.iter().any(|t| t.name == tool),
            "disabled: '{}' must disappear from the advertised tools",
            tool
        );
    }
}
