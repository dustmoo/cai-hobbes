use super::*;
use crate::session::ActiveContext;
use crate::session::Session;
use crate::settings::Settings;
use crate::str_utils::floor_char_boundary;
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
        project_id: None,
        project_tag_user_set: false,
        loaded_skills: std::collections::HashMap::new(),
        scratchpad: String::new(),
        current_ai_turn_count: 0,
        watch_word_recovery_count: 0,
        scheduled_timers: Vec::new(),
        tool_call_history: Vec::new(),
        tab_emoji: None,
        tab_title: None,
        name_user_set: false,
    }
}

#[test]
fn test_composio_context_injected_when_profile_is_fully_configured() {
    use crate::settings::ComposioProfile;

    let session = create_test_session();

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

    let builder = PromptBuilder::new(&session, &settings);
    let prompt = builder.build_prompt("Hello".to_string());

    // Extract the system instruction text
    let full_text = prompt
        .prompt
        .system
        .expect("Should have system instruction");

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

    let settings = Settings {
        composio_profiles: vec![ComposioProfile {
            name: "Incomplete Profile".to_string(),
            user_id: Some("test-user-id-123".to_string()),
            api_key: None, // Missing API key
            ..Default::default()
        }],
        ..Default::default()
    };

    let builder = PromptBuilder::new(&session, &settings);
    let prompt = builder.build_prompt("Hello".to_string());

    let full_text = prompt
        .prompt
        .system
        .expect("Should have system instruction");

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

    let settings = Settings {
        composio_profiles: vec![ComposioProfile {
            name: "Incomplete Profile".to_string(),
            user_id: None, // Missing User ID
            api_key: Some("sk-test-api-key".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };

    let builder = PromptBuilder::new(&session, &settings);
    let prompt = builder.build_prompt("Hello".to_string());

    let full_text = prompt
        .prompt
        .system
        .expect("Should have system instruction");

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
        tab_title: None,
        tab_emoji: None,
    };

    let builder = PromptBuilder::new(&session, &settings);
    let prompt = builder.build_prompt("Hello".to_string());

    let full_text = prompt
        .prompt
        .system
        .expect("Should have system instruction");

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
        assert!(!page.is_empty(), "Page {} should not be empty", i);
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
            signature_provider: None,
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
            signature_provider: None,
            cached_image_path: None,
            result_summary,
            completed_at: None,
            profile_id: None,
            connector_name: None,
            model: None,
            round_id: None,
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
                    out.push_str(
                        &content
                            .as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| content.to_string()),
                    );
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
    let builder = PromptBuilder::new(&session, &settings);
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

    let builder = PromptBuilder::new(&session, &settings);
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

    let builder = PromptBuilder::new(&session, &settings);
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

    // 1. Idle session → skills advertised, loader tool present
    let session = session_with_core_tools();
    let prompt = PromptBuilder::new(&session, &settings).build_prompt("Hi".to_string());
    let system = prompt.prompt.system.clone().unwrap();
    assert!(
        system.contains("available_skills"),
        "idle: skills advertised"
    );
    assert!(
        prompt
            .prompt
            .tools
            .iter()
            .any(|t| t.name == "HOBBES_INVOKE_SKILL"),
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
    let prompt = PromptBuilder::new(&session, &settings).build_prompt(String::new());
    let system = prompt.prompt.system.clone().unwrap();
    assert!(
        !system.contains("available_skills"),
        "running: no other skills advertised"
    );
    assert!(
        !prompt
            .prompt
            .tools
            .iter()
            .any(|t| t.name == "HOBBES_INVOKE_SKILL"),
        "running: loader tool withheld"
    );

    // 3. Model just loaded a skill (last message is the loader tool call) → gated
    let mut session = session_with_core_tools();
    session.loaded_skills.insert(
        "news".to_string(),
        loaded_skill_payload("news", "unrelated"),
    );
    session.messages = vec![
        user_text_msg("check the news"),
        tool_call_msg("HOBBES_INVOKE_SKILL", "manual...".to_string(), None),
    ];
    let prompt = PromptBuilder::new(&session, &settings).build_prompt(String::new());
    assert!(
        !prompt
            .prompt
            .system
            .clone()
            .unwrap()
            .contains("available_skills"),
        "just-loaded: no other skills advertised"
    );
    assert!(
        !prompt
            .prompt
            .tools
            .iter()
            .any(|t| t.name == "HOBBES_INVOKE_SKILL"),
        "just-loaded: loader tool withheld"
    );

    // 4. No skills on disk → loader tool withheld even when idle
    set_available_skills_for_test(vec![]);
    let session = session_with_core_tools();
    let prompt = PromptBuilder::new(&session, &settings).build_prompt("Hi".to_string());
    assert!(
        !prompt
            .prompt
            .system
            .clone()
            .unwrap()
            .contains("available_skills"),
        "no skills: nothing advertised"
    );
    assert!(
        !prompt
            .prompt
            .tools
            .iter()
            .any(|t| t.name == "HOBBES_INVOKE_SKILL"),
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
    let settings = Settings::default();

    let planner = crate::todo::PlannerState::default();
    let today = chrono::Local::now().date_naive();
    let block = crate::todo::handlers::planner_today_context(&planner, &settings, today);
    assert!(block.is_some(), "enabled by default");

    let prompt = PromptBuilder::new(&session, &settings)
        .with_planner_today(block)
        .build_prompt("Hi".to_string());
    let system = prompt.prompt.system.unwrap();
    assert!(system.contains("planner_today"));
    assert!(
        system.contains("HOBBES_PLAN_DAY"),
        "instruction names the tools"
    );

    // Without the attachment (the default) nothing is injected.
    let prompt = PromptBuilder::new(&session, &settings).build_prompt("Hi".to_string());
    assert!(!prompt.prompt.system.unwrap().contains("planner_today"));
}

#[test]
fn planner_tools_are_withheld_when_the_planner_is_disabled() {
    let session = session_with_planner_tools();

    let enabled = Settings::default();
    let prompt = PromptBuilder::new(&session, &enabled).build_prompt("Hi".to_string());
    assert!(
        prompt
            .prompt
            .tools
            .iter()
            .any(|t| t.name == "HOBBES_TODO_CREATE"),
        "enabled: planner tools advertised"
    );

    let disabled = Settings {
        planner_enabled: false,
        ..Default::default()
    };
    let prompt = PromptBuilder::new(&session, &disabled).build_prompt("Hi".to_string());
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

// =========================================================================
// Thought-signature provenance filtering (mid-chat provider switch)
// =========================================================================

use super::message_history::signature_for_provider;
use crate::llm::types::{ChatRole, ContentBlock};

/// Assistant text message carrying a thinking summary + signature.
fn assistant_thinking_msg(
    text: &str,
    signature: Option<&str>,
    minted_by: Option<LlmProvider>,
) -> Message {
    Message {
        id: uuid::Uuid::new_v4(),
        author: "Hobbes".to_string(),
        content: MessageContent::Text {
            content: text.to_string(),
            thought_signature: signature.map(|s| s.to_string()),
            thought_summary: Some("thinking...".to_string()),
            signature_provider: minted_by,
        },
        attachments: vec![],
        comments: vec![],
        created_at: Utc::now(),
        usage: None,
    }
}

/// Tool-call message carrying a signature with explicit provenance.
fn signed_tool_call_msg(
    tool_name: &str,
    signature: Option<&str>,
    minted_by: Option<LlmProvider>,
) -> Message {
    let mut msg = tool_call_msg(tool_name, "{\"ok\":true}".to_string(), None);
    if let MessageContent::ToolCall(tc) = &mut msg.content {
        tc.thought_signature = signature.map(|s| s.to_string());
        tc.thought_summary = Some("thinking...".to_string());
        tc.signature_provider = minted_by;
    }
    msg
}

/// All `ContentBlock::ToolCall` signatures in prompt order.
fn tool_call_signatures(
    result: &crate::context::prompt_builder::PromptBuildResult,
) -> Vec<Option<String>> {
    result
        .prompt
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolCall { signature, .. } => Some(signature.clone()),
            _ => None,
        })
        .collect()
}

/// Signature on the `Thinking` block of the last assistant message that has one.
fn latest_thinking_signature(
    result: &crate::context::prompt_builder::PromptBuildResult,
) -> Option<Option<String>> {
    result
        .prompt
        .messages
        .iter()
        .rev()
        .find(|m| {
            m.role == ChatRole::Assistant
                && m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Thinking { .. }))
        })
        .and_then(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::Thinking { signature, .. } => Some(signature.clone()),
                _ => None,
            })
        })
}

fn session_pinned_to(provider: LlmProvider, messages: Vec<Message>) -> Session {
    let mut session = create_test_session();
    session.llm_provider = Some(provider);
    session.messages = messages;
    session
}

#[test]
fn test_claude_tool_call_signature_dropped_when_session_pinned_to_gemini() {
    let session = session_pinned_to(
        LlmProvider::Gemini,
        vec![
            user_text_msg("do the thing"),
            signed_tool_call_msg("alpha", Some("claude-sig"), Some(LlmProvider::Claude)),
            signed_tool_call_msg("beta", Some("gemini-sig"), Some(LlmProvider::Gemini)),
        ],
    );
    let settings = Settings::default();
    let result = PromptBuilder::new(&session, &settings).build_prompt(String::new());

    let sigs = tool_call_signatures(&result);
    assert_eq!(sigs.len(), 2, "expected two tool calls in the prompt");
    assert_eq!(
        sigs[0], None,
        "Claude-minted signature must be dropped for a Gemini-pinned session"
    );
    assert_eq!(
        sigs[1].as_deref(),
        Some("gemini-sig"),
        "the later Gemini-minted signature must survive (backfill must not resurrect the Claude one)"
    );
}

#[test]
fn test_gemini_thinking_signature_dropped_when_session_pinned_to_claude() {
    let session = session_pinned_to(
        LlmProvider::Claude,
        vec![
            user_text_msg("hello"),
            assistant_thinking_msg("hi there", Some("gemini-sig"), Some(LlmProvider::Gemini)),
        ],
    );
    let settings = Settings::default();
    let result = PromptBuilder::new(&session, &settings).build_prompt(String::new());

    assert_eq!(
        latest_thinking_signature(&result),
        Some(None),
        "a Gemini-minted signature on the newest assistant turn must be stripped for Claude"
    );
}

#[test]
fn test_same_provider_signatures_pass_through_unchanged() {
    let session = session_pinned_to(
        LlmProvider::Gemini,
        vec![
            user_text_msg("hello"),
            assistant_thinking_msg("planning", Some("gemini-sig"), Some(LlmProvider::Gemini)),
            // No signature of its own — must inherit the Gemini baton.
            signed_tool_call_msg("alpha", None, None),
        ],
    );
    let settings = Settings::default();
    let result = PromptBuilder::new(&session, &settings).build_prompt(String::new());

    assert_eq!(
        tool_call_signatures(&result),
        vec![Some("gemini-sig".to_string())],
        "same-provider signature must be backfilled onto the later tool call (Baton Pattern)"
    );
}

#[test]
fn test_legacy_unstamped_signature_kept_in_latest_turn() {
    let session = session_pinned_to(
        LlmProvider::Gemini,
        vec![
            user_text_msg("hello"),
            signed_tool_call_msg("alpha", Some("legacy-sig"), None),
        ],
    );
    let settings = Settings::default();
    let result = PromptBuilder::new(&session, &settings).build_prompt(String::new());

    assert_eq!(
        tool_call_signatures(&result),
        vec![Some("legacy-sig".to_string())],
        "an unstamped signature on the most recent turn is kept (no switch can have happened mid-turn)"
    );
}

#[test]
fn test_legacy_unstamped_signature_dropped_in_historical_turn() {
    let session = session_pinned_to(
        LlmProvider::Gemini,
        vec![
            user_text_msg("hello"),
            signed_tool_call_msg("alpha", Some("legacy-sig"), None),
            user_text_msg("now do something else"),
            signed_tool_call_msg("beta", None, None),
        ],
    );
    let settings = Settings::default();
    let result = PromptBuilder::new(&session, &settings).build_prompt(String::new());

    assert_eq!(
        tool_call_signatures(&result),
        vec![None, None],
        "unstamped signatures on historical turns are dropped, and must not be backfilled forward"
    );
}

#[test]
fn test_signature_for_provider_matching_stamp_is_kept() {
    assert_eq!(
        signature_for_provider(
            Some("sig"),
            Some(LlmProvider::Gemini),
            LlmProvider::Gemini,
            false
        ),
        Some("sig".to_string())
    );
}

#[test]
fn test_signature_for_provider_foreign_stamp_is_dropped() {
    assert_eq!(
        signature_for_provider(
            Some("sig"),
            Some(LlmProvider::Claude),
            LlmProvider::Gemini,
            true
        ),
        None,
        "a foreign stamp is dropped even inside the latest turn"
    );
}

#[test]
fn test_signature_for_provider_legacy_depends_on_turn() {
    assert_eq!(
        signature_for_provider(Some("sig"), None, LlmProvider::Gemini, true),
        Some("sig".to_string())
    );
    assert_eq!(
        signature_for_provider(Some("sig"), None, LlmProvider::Gemini, false),
        None
    );
}

#[test]
fn test_signature_for_provider_none_signature_stays_none() {
    assert_eq!(
        signature_for_provider(None, Some(LlmProvider::Gemini), LlmProvider::Gemini, true),
        None
    );
}

/// A Hobbes-authored prose message, the shape a finished reply takes once the
/// turn-ordering pass moves it after the tool calls it follows.
fn assistant_text_msg(text: &str) -> Message {
    Message {
        id: uuid::Uuid::new_v4(),
        author: "Hobbes".to_string(),
        content: MessageContent::Text {
            content: text.to_string(),
            thought_signature: None,
            thought_summary: None,
            signature_provider: None,
        },
        attachments: vec![],
        comments: vec![],
        created_at: Utc::now(),
        usage: None,
    }
}

/// Regression: a continuation whose turn ends on the assistant's own prose —
/// the order a turn takes once its reply is moved past its tool calls — must
/// not build a request that ends on an assistant turn. Claude rejects that
/// with "This model does not support assistant message prefill".
#[test]
fn continuation_after_a_reply_still_ends_on_a_user_turn() {
    let mut session = create_test_session();
    session.messages = vec![
        user_text_msg("plan my day"),
        tool_call_msg("HOBBES_TERMINAL_EXEC", "ok".to_string(), None),
        assistant_text_msg("Let me look at how your skills are structured."),
    ];

    let settings = Settings::default();
    // Empty user message = the continuation path.
    let prompt = PromptBuilder::new(&session, &settings).build_prompt(String::new());

    let last = prompt.prompt.messages.last().expect("messages built");
    assert_eq!(
        last.role,
        ChatRole::User,
        "a continuation must not leave the request ending on an assistant turn"
    );
    assert!(
        matches!(&last.content[0], ContentBlock::Text { text } if text.contains("Continue")),
        "the trailing turn is the continuation nudge"
    );
}

/// The nudge is only for the broken shape: a turn that ends on its tool call
/// already supplies a user turn (the tool result), and must be left alone.
#[test]
fn continuation_after_a_tool_call_needs_no_nudge() {
    let mut session = create_test_session();
    session.messages = vec![
        user_text_msg("plan my day"),
        tool_call_msg("HOBBES_TERMINAL_EXEC", "ok".to_string(), None),
    ];

    let settings = Settings::default();
    let prompt = PromptBuilder::new(&session, &settings).build_prompt(String::new());

    let last = prompt.prompt.messages.last().expect("messages built");
    assert_eq!(last.role, ChatRole::Tool, "the tool result is the last turn");
    assert!(
        !prompt.prompt.messages.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("Continue.")))),
        "no nudge is appended when the turn already ends on a user-role turn"
    );
}

/// A live send carries its own user message, so nothing is appended.
#[test]
fn a_live_send_ends_on_the_users_own_message() {
    let mut session = create_test_session();
    session.messages = vec![assistant_text_msg("Anything else?")];

    let settings = Settings::default();
    let prompt = PromptBuilder::new(&session, &settings).build_prompt("next thing".to_string());

    let last = prompt.prompt.messages.last().expect("messages built");
    assert_eq!(last.role, ChatRole::User);
    assert!(
        matches!(&last.content[0], ContentBlock::Text { text } if text == "next thing"),
        "the user's own message is the final turn, not a nudge"
    );
}

/// Thinking survives on every assistant message of the current tool loop
/// (Opus 5.5 / Fable 5.1 reject a loop with dropped blocks), while earlier
/// turns are still stripped.
#[test]
fn strip_historical_thinking_keeps_the_current_tool_loop() {
    let thinking = |sig: &str| ContentBlock::Thinking {
        text: "t".into(),
        signature: Some(sig.into()),
    };
    let text = |t: &str| ContentBlock::Text { text: t.into() };
    let msg = |role, content| ChatMessage { role, content };
    let mut messages = vec![
        msg(ChatRole::User, vec![text("first question")]),
        msg(ChatRole::Assistant, vec![thinking("old"), text("old answer")]),
        msg(ChatRole::User, vec![text("second question")]),
        msg(ChatRole::Assistant, vec![thinking("loop-1"), text("calling a tool")]),
        msg(ChatRole::Tool, vec![text("result 1")]),
        msg(ChatRole::Assistant, vec![thinking("loop-2"), text("calling again")]),
        msg(ChatRole::Tool, vec![text("result 2")]),
    ];
    PromptBuilder::strip_historical_thinking(&mut messages);
    let has_thinking = |m: &ChatMessage| {
        m.content.iter().any(|b| matches!(b, ContentBlock::Thinking { .. }))
    };
    assert!(!has_thinking(&messages[1]), "earlier turn is stripped");
    assert!(has_thinking(&messages[3]), "first loop step keeps its thinking");
    assert!(has_thinking(&messages[5]), "latest loop step keeps its thinking");
}
