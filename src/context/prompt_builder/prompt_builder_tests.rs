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
        tab_title_provisional: false,
        history_anchor: None,
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

/// Serialises the tests that write the process-wide skill-metadata mirror
/// (`set_available_skills_for_test`) or need it to hold still across builds.
static SKILL_MIRROR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Single test covering all gate states: the skill-metadata mirror is a
/// process-wide OnceLock, so the states are exercised sequentially in one
/// test to avoid cross-test races, and the mirror is cleared at the end.
#[test]
fn test_available_skills_gated_by_running_skill() {
    use crate::skills::registry::{set_available_skills_for_test, AvailableSkill};
    let _mirror = SKILL_MIRROR_LOCK.lock().unwrap_or_else(|p| p.into_inner());

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

    // 3b. Advert filtering: explicit `false` is omitted, unlisted and `true`
    // stay advertised.
    set_available_skills_for_test(vec![
        AvailableSkill {
            name: "denied-skill".to_string(),
            description: "d".to_string(),
            argument_hint: None,
            disable_model_invocation: false,
        },
        AvailableSkill {
            name: "unlisted-skill".to_string(),
            description: "u".to_string(),
            argument_hint: None,
            disable_model_invocation: false,
        },
        AvailableSkill {
            name: "allowed-skill".to_string(),
            description: "a".to_string(),
            argument_hint: None,
            disable_model_invocation: false,
        },
    ]);
    let mut filtered_settings = Settings::default();
    filtered_settings
        .permission_settings
        .skill_permissions
        .insert("denied-skill".to_string(), false);
    filtered_settings
        .permission_settings
        .skill_permissions
        .insert("allowed-skill".to_string(), true);
    let session = session_with_core_tools();
    let prompt =
        PromptBuilder::new(&session, &filtered_settings).build_prompt("Hi".to_string());
    let system = prompt.prompt.system.clone().unwrap();
    assert!(!system.contains("denied-skill"), "explicit false is not advertised");
    assert!(system.contains("unlisted-skill"), "unlisted stays advertised");
    assert!(system.contains("allowed-skill"), "allowed stays advertised");

    // 3c. An idle-stubbed loaded skill is re-advertised so it can be reloaded.
    set_available_skills_for_test(vec![AvailableSkill {
        name: "news".to_string(),
        description: "News".to_string(),
        argument_hint: None,
        disable_model_invocation: false,
    }]);
    let mut session = session_with_core_tools();
    session.loaded_skills.insert(
        "news".to_string(),
        loaded_skill_payload("news", "unrelated"),
    );
    let mut msgs = vec![invoke_skill_msg("news")];
    msgs.extend((0..SKILL_IDLE_USER_TURNS).map(|i| user_text_msg(&format!("turn {i}"))));
    session.messages = msgs;
    let prompt = PromptBuilder::new(&session, &settings).build_prompt(String::new());
    assert!(
        prompt.prompt.system.clone().unwrap().contains("available_skills"),
        "idle-stubbed skill is advertised for reload"
    );
    assert!(
        prompt
            .prompt
            .tools
            .iter()
            .any(|t| t.name == "HOBBES_INVOKE_SKILL"),
        "loader tool present so the idle skill can be reloaded"
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

// =========================================================================
// Idle skill stub (lifetime control)
// =========================================================================

use super::system_context::{idle_loaded_skills, skill_is_advertised, SKILL_IDLE_USER_TURNS};

fn invoke_skill_msg(skill: &str) -> Message {
    let mut m = tool_call_msg("HOBBES_INVOKE_SKILL", "manual".to_string(), None);
    if let MessageContent::ToolCall(tc) = &mut m.content {
        tc.server_name = "hobbes-core".to_string();
        tc.arguments = serde_json::json!({ "name": skill }).to_string();
    }
    m
}

fn loaded(skill: &str, tool: &str) -> std::collections::HashMap<String, String> {
    std::collections::HashMap::from([(skill.to_string(), loaded_skill_payload(skill, tool))])
}

fn user_turns(n: usize) -> Vec<Message> {
    (0..n).map(|i| user_text_msg(&format!("turn {i}"))).collect()
}

#[test]
fn idle_decision_counts_user_turns_since_last_use() {
    let skills = loaded("news", "test-server_search");

    // Load, then N-1 user turns → still fresh; N → idle.
    let mut msgs = vec![invoke_skill_msg("news")];
    msgs.extend(user_turns(SKILL_IDLE_USER_TURNS - 1));
    assert!(idle_loaded_skills(&msgs, &skills, SKILL_IDLE_USER_TURNS).is_empty());
    msgs.extend(user_turns(1));
    assert!(idle_loaded_skills(&msgs, &skills, SKILL_IDLE_USER_TURNS).contains("news"));

    // A call to the skill's resolved tool resets the clock.
    msgs.push(tool_call_msg("search", "r".to_string(), None));
    assert!(idle_loaded_skills(&msgs, &skills, SKILL_IDLE_USER_TURNS).is_empty());
    msgs.extend(user_turns(SKILL_IDLE_USER_TURNS));
    assert!(idle_loaded_skills(&msgs, &skills, SKILL_IDLE_USER_TURNS).contains("news"));

    // A re-invoke by name resets it too.
    msgs.push(invoke_skill_msg("news"));
    assert!(idle_loaded_skills(&msgs, &skills, SKILL_IDLE_USER_TURNS).is_empty());
}

#[test]
fn idle_decision_ignores_other_skills_and_non_user_text() {
    let skills = loaded("news", "test-server_search");
    // Re-invoking a DIFFERENT skill is not use of "news"; assistant text and
    // tool calls between user turns are not user turns.
    let mut msgs = vec![invoke_skill_msg("news")];
    msgs.extend(user_turns(SKILL_IDLE_USER_TURNS - 1));
    msgs.push(invoke_skill_msg("other"));
    msgs.push(assistant_thinking_msg("hi", None, None));
    msgs.extend(user_turns(1));
    assert!(idle_loaded_skills(&msgs, &skills, SKILL_IDLE_USER_TURNS).contains("news"));
}

#[test]
fn skill_with_no_recorded_use_is_never_idle() {
    let skills = loaded("news", "unrelated");
    let msgs = user_turns(SKILL_IDLE_USER_TURNS * 2);
    assert!(idle_loaded_skills(&msgs, &skills, SKILL_IDLE_USER_TURNS).is_empty());
}

#[test]
fn zero_idle_turns_never_stubs() {
    let skills = loaded("news", "unrelated");
    let mut msgs = vec![invoke_skill_msg("news")];
    msgs.extend(user_turns(50));
    assert!(idle_loaded_skills(&msgs, &skills, SKILL_IDLE_USER_TURNS).contains("news"));
    assert!(idle_loaded_skills(&msgs, &skills, 0).is_empty());
}

#[test]
fn idle_skill_is_injected_as_stub_not_manual() {
    let settings = Settings::default();
    let mut session = session_with_core_tools();
    session
        .loaded_skills
        .insert("news".to_string(), loaded_skill_payload("news", "unrelated"));
    let mut msgs = vec![invoke_skill_msg("news")];
    msgs.extend(user_turns(SKILL_IDLE_USER_TURNS - 1));
    session.messages = msgs.clone();

    let fresh = PromptBuilder::new(&session, &settings)
        .build_prompt(String::new())
        .prompt
        .system
        .unwrap();
    assert!(fresh.contains("Do the thing."), "fresh skill keeps its manual");

    msgs.extend(user_turns(1));
    session.messages = msgs;
    let idle = PromptBuilder::new(&session, &settings)
        .build_prompt(String::new())
        .prompt
        .system
        .unwrap();
    assert!(!idle.contains("Do the thing."), "idle skill's manual is dropped");
    assert!(idle.contains("HOBBES_INVOKE_SKILL with this name to reload"));
    assert!(idle.contains("unrelated"), "stub keeps the tool list");
}

#[test]
fn advert_filter_table() {
    use crate::context::permissions::PermissionSettings;
    let mut perms = PermissionSettings::default();
    perms.skill_permissions.insert("no".into(), false);
    perms.skill_permissions.insert("yes".into(), true);
    // not loaded: only explicit false is hidden
    assert!(!skill_is_advertised("no", false, false, &perms));
    assert!(skill_is_advertised("yes", false, false, &perms));
    assert!(skill_is_advertised("unlisted", false, false, &perms));
    // loaded: hidden while fresh, advertised when idle-stubbed
    assert!(!skill_is_advertised("yes", true, false, &perms));
    assert!(skill_is_advertised("yes", true, true, &perms));
}

#[test]
fn unload_tool_withheld_when_nothing_loaded() {
    let mut session = session_with_core_tools();
    if let Some(ctx) = session.active_context.mcp_tools.as_mut() {
        let mut t = invoke_skill_tool();
        t.name = "HOBBES_UNLOAD_SKILL".into();
        ctx.servers[0].tools.push(t);
    }
    let settings = Settings::default();
    let has_unload = |s: &Session| {
        PromptBuilder::new(s, &settings)
            .build_prompt("Hi".to_string())
            .prompt
            .tools
            .iter()
            .any(|t| t.name == "HOBBES_UNLOAD_SKILL")
    };
    assert!(!has_unload(&session));
    session
        .loaded_skills
        .insert("news".to_string(), loaded_skill_payload("news", "x"));
    assert!(has_unload(&session));
}

// =========================================================================
// Prompt-cache layout: stable SYSTEM_CONTEXT prefix | volatile TURN_CONTEXT
// =========================================================================

fn named_tool(name: &str) -> rmcp::model::Tool {
    let mut t = invoke_skill_tool();
    t.name = name.to_string().into();
    t
}

fn mcp_server(name: &str, tools: &[&str]) -> crate::mcp::manager::McpServerContext {
    crate::mcp::manager::McpServerContext {
        name: name.to_string(),
        description: format!("{name} server"),
        tools: tools.iter().map(|t| named_tool(t)).collect(),
    }
}

/// A completed `/skill` run whose payload resolves several tools, so a
/// HashMap-ordered rendering of `resolved_tools` would show up as a diff.
fn completed_skill_call_msg(skill: &str) -> Message {
    let payload = crate::components::shared::CapabilityContextPayload {
        skill: skill.to_string(),
        instruction_manual: "Follow the release checklist.".to_string(),
        environment: crate::components::shared::SkillEnvironment {
            root_path: std::path::PathBuf::new(),
            scripts: vec![],
            resources: vec![],
            user_args: String::new(),
        },
        resolved_tools: (0..8)
            .map(|i| (format!("cap_{i}"), format!("skill-server_tool_{i}")))
            .collect(),
        warnings: vec![],
    };
    Message {
        id: uuid::Uuid::new_v4(),
        author: "User".to_string(),
        content: MessageContent::SkillCall(crate::components::shared::SkillCall {
            execution_id: "skill-exec".to_string(),
            skill_name: skill.to_string(),
            status: crate::components::shared::SkillCallStatus::Completed,
            response: serde_json::to_string(&payload).unwrap(),
            ..Default::default()
        }),
        attachments: vec![],
        comments: vec![],
        created_at: Utc::now(),
        usage: None,
    }
}

fn cache_test_settings() -> Settings {
    use crate::settings::ComposioProfile;
    Settings {
        user_name: Some("Dustin".to_string()),
        composio_profiles: vec![ComposioProfile {
            id: "profile-1".to_string(),
            name: "Work".to_string(),
            user_id: Some("u".to_string()),
            api_key: Some("k".to_string()),
            ..Default::default()
        }],
        active_composio_profile: Some("profile-1".to_string()),
        ..Default::default()
    }
}

/// A session with every stable section populated (MCP servers, active skill,
/// loaded skill, user name via settings, composio via settings).
fn cache_test_session(server_order_reversed: bool) -> Session {
    let mut session = create_test_session();
    let mut servers = vec![
        mcp_server("alpha", &["zeta", "beta"]),
        mcp_server("omega", &["read", "write"]),
        mcp_server("hobbes-core", &["HOBBES_UPDATE_SCRATCHPAD"]),
    ];
    if server_order_reversed {
        servers.reverse();
        for s in &mut servers {
            s.tools.reverse();
        }
    }
    session.active_context.mcp_tools = Some(crate::mcp::manager::McpContext {
        servers,
        connected_toolkit_slugs: vec![],
    });
    session
        .loaded_skills
        .insert("news".to_string(), loaded_skill_payload("news", "unrelated"));
    session
        .loaded_skills
        .insert("weather".to_string(), loaded_skill_payload("weather", "unrelated2"));
    session
}

struct Turn {
    scratchpad: &'static str,
    summary: &'static str,
    planner: &'static str,
    ends_in_failed_tool: bool,
}

fn build_turn(turn: &Turn, settings: &Settings) -> (String, usize) {
    let mut session = cache_test_session(false);
    session.scratchpad = turn.scratchpad.to_string();
    session.active_context.conversation_summary.summary = turn.summary.to_string();
    let mut messages = vec![completed_skill_call_msg("release"), user_text_msg("go")];
    if turn.ends_in_failed_tool {
        let mut call = tool_call_msg("read", "boom".to_string(), None);
        if let MessageContent::ToolCall(tc) = &mut call.content {
            tc.server_name = "omega".to_string();
            tc.status = ToolCallStatus::Error;
        }
        messages.push(call);
    }
    session.messages = messages;
    let ctx = PromptBuilder::new(&session, settings)
        .with_planner_today(Some(serde_json::json!({ "plan": turn.planner })))
        .build_system_context();
    let offset = ctx.volatile_offset.expect("there is always per-turn state");
    (ctx.system, offset)
}

fn assert_offset_valid(system: &str, offset: usize) {
    assert!(offset <= system.len(), "offset {offset} > len {}", system.len());
    assert!(system.is_char_boundary(offset), "offset {offset} not on a char boundary");
}

#[test]
fn stable_prefix_is_byte_identical_across_turns() {
    use crate::skills::registry::{set_available_skills_for_test, AvailableSkill};
    let _mirror = SKILL_MIRROR_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_available_skills_for_test(vec![AvailableSkill {
        name: "research".to_string(),
        description: "Deep research".to_string(),
        argument_hint: None,
        disable_model_invocation: false,
    }]);
    let settings = cache_test_settings();

    let a = Turn {
        scratchpad: "SCRATCH_ALPHA",
        summary: "SUMMARY_ONE",
        planner: "PLAN_ONE",
        ends_in_failed_tool: false,
    };
    let b = Turn {
        scratchpad: "SCRATCH_BETA",
        summary: "SUMMARY_TWO",
        planner: "PLAN_TWO",
        ends_in_failed_tool: true,
    };
    let (sys_a, off_a) = build_turn(&a, &settings);
    // Rebuild a few times: HashMap-ordered content (skill tool mappings,
    // loaded skills) gets a fresh random order on every build.
    for _ in 0..4 {
        let (again, off) = build_turn(&a, &settings);
        assert_eq!(&again[..off], &sys_a[..off_a]);
    }
    let (sys_b, off_b) = build_turn(&b, &settings);
    set_available_skills_for_test(vec![]);

    assert_offset_valid(&sys_a, off_a);
    assert_offset_valid(&sys_b, off_b);
    assert_eq!(
        &sys_a[..off_a],
        &sys_b[..off_b],
        "stable part must not change between turns"
    );

    let stable = &sys_a[..off_a];
    for stable_section in [
        "<SYSTEM_CONTEXT>",
        "mcp_servers",
        "composio_context",
        "\"user_name\": \"Dustin\"",
        "active_skill",
        "Follow the release checklist.",
        "available_skills",
    ] {
        assert!(stable.contains(stable_section), "stable part lacks {stable_section}");
    }

    for (sys, off, turn) in [(&sys_a, off_a, &a), (&sys_b, off_b, &b)] {
        let (stable, volatile) = sys.split_at(off);
        for token in [turn.scratchpad, turn.summary, turn.planner, "loaded_skills"] {
            assert!(!stable.contains(token), "{token} leaked into the stable part");
            assert!(volatile.contains(token), "{token} missing from the volatile part");
        }
        assert!(volatile.starts_with("\n\n<TURN_CONTEXT>\n"), "{volatile}");
    }
    let volatile_b = &sys_b[off_b..];
    assert!(volatile_b.contains("TOOL COMPLETION INSTRUCTION"));
    assert!(volatile_b.contains("RECOVERY NOTE"));
    assert!(!sys_b[..off_b].contains("INSTRUCTION:"));
    assert!(!sys_b[..off_b].contains("RECOVERY NOTE"));
    assert!(
        volatile_b.find("</TURN_CONTEXT>").unwrap()
            < volatile_b.find("TOOL COMPLETION INSTRUCTION").unwrap(),
        "turn instructions follow the TURN_CONTEXT block"
    );
}

#[test]
fn current_time_only_appears_after_the_offset() {
    let settings = cache_test_settings();
    let session = cache_test_session(false);
    let result = PromptBuilder::new(&session, &settings).build_prompt("Hi".to_string());
    let system = result.prompt.system.unwrap();
    let offset = result
        .prompt
        .system_volatile_offset
        .expect("offset is wired into LlmPrompt");
    assert_offset_valid(&system, offset);
    assert!(!system[..offset].contains("current_time"));
    assert!(!system[..offset].contains("iso_8601"));
    assert!(system[offset..].contains("current_time"));
}

#[test]
fn offset_lands_on_a_char_boundary_with_multibyte_content() {
    let settings = Settings {
        persona: "Tu es Hobbes 🦀 — très direct.".to_string(),
        user_name: Some("Düstîn 🎯".to_string()),
        ..Default::default()
    };
    let mut session = cache_test_session(false);
    session.scratchpad = "ünïcödé 🧠 notes".to_string();
    let ctx = PromptBuilder::new(&session, &settings).build_system_context();
    let offset = ctx.volatile_offset.unwrap();
    assert_offset_valid(&ctx.system, offset);
    assert!(ctx.system[..offset].starts_with("Tu es Hobbes 🦀"));
    assert!(ctx.system[..offset].ends_with("</SYSTEM_CONTEXT>"));
    assert!(ctx.system[offset..].contains("🧠"));
}

#[test]
fn tools_and_servers_are_sorted_regardless_of_input_order() {
    let settings = cache_test_settings();
    let build = |reversed: bool| {
        let session = cache_test_session(reversed);
        PromptBuilder::new(&session, &settings).build_system_context()
    };
    let forward = build(false);
    let reversed = build(true);

    let wire = |tools: &[crate::llm::types::ToolDefinition]| -> Vec<String> {
        tools
            .iter()
            .map(|t| crate::gemini::convert::get_prefixed_tool_name(&t.server_name, &t.name))
            .collect()
    };
    let forward_names = wire(&forward.tools);
    assert_eq!(forward_names, wire(&reversed.tools));
    let mut sorted = forward_names.clone();
    sorted.sort();
    assert_eq!(forward_names, sorted, "tools sorted by wire name");
    assert_eq!(forward.tools.len(), 5);

    let off_f = forward.volatile_offset.unwrap();
    let off_r = reversed.volatile_offset.unwrap();
    let stable = &forward.system[..off_f];
    assert_eq!(stable, &reversed.system[..off_r], "mcp_servers order is stable");
    let pos = |name: &str| stable.find(&format!("\"name\": \"{name}\"")).unwrap();
    assert!(pos("alpha") < pos("hobbes-core") && pos("hobbes-core") < pos("omega"));
}

/// Gemini / OpenAI-compatible connectors send the whole `system` string: every
/// section the old single-block layout carried must still be in it.
#[test]
fn full_system_still_carries_every_section() {
    let settings = cache_test_settings();
    let (system, offset) = build_turn(
        &Turn {
            scratchpad: "SCRATCH",
            summary: "SUMMARY",
            planner: "PLAN",
            ends_in_failed_tool: true,
        },
        &settings,
    );
    assert!(system.starts_with(&settings.persona));
    for section in [
        "TOOL POLICY:",
        "<SYSTEM_CONTEXT>",
        "</SYSTEM_CONTEXT>",
        "mcp_servers",
        "composio_context",
        "user_name",
        "active_skill",
        "<TURN_CONTEXT>",
        "</TURN_CONTEXT>",
        "same authority as SYSTEM_CONTEXT",
        "current_time",
        "scratchpad",
        "SCRATCH",
        "conversation_summary",
        "SUMMARY",
        "planner_today",
        "PLAN",
        "loaded_skills",
        "TOOL COMPLETION INSTRUCTION",
        "RECOVERY NOTE",
    ] {
        assert!(system.contains(section), "full system lacks {section}");
    }
    // The persona (and its TOOL POLICY) stays in the stable part.
    assert!(system[..offset].contains("TOOL POLICY:"));
}

#[test]
fn continuation_instruction_is_volatile() {
    let settings = Settings::default();
    let mut session = create_test_session();
    session.messages = vec![
        user_text_msg("hi"),
        assistant_text_msg("hello"),
        assistant_text_msg(""),
    ];
    let ctx = PromptBuilder::new(&session, &settings).build_system_context();
    let offset = ctx.volatile_offset.unwrap();
    assert!(ctx.system[offset..].contains("CONTINUATION INSTRUCTION"));
    assert!(!ctx.system[..offset].contains("CONTINUATION INSTRUCTION"));
}

/// The budget sees the combined size: a stable enrichment key is dropped when
/// the *volatile* content pushes the total over budget, and the free text
/// around the map counts.
#[test]
fn budget_applies_across_stable_and_volatile_parts() {
    let tuning = Settings::default().effective_context_tuning_for(LlmProvider::Gemini);
    let budget_chars = (1000.0 * tuning.system_prompt_budget_ratio * tuning.chars_per_token) as usize;
    let base = || {
        let mut map = serde_json::Map::new();
        map.insert("composio_context".to_string(), serde_json::json!({"info": "x"}));
        map.insert("current_time".to_string(), serde_json::json!({"iso_8601": "t"}));
        map
    };

    let mut small = base();
    PromptBuilder::compose_system_for_budget(&mut small, 0, Some(1000), &tuning);
    assert!(small.contains_key("composio_context"), "fits: nothing dropped");

    let mut big_volatile = base();
    big_volatile.insert(
        "scratchpad".to_string(),
        serde_json::json!({"content": "s".repeat(budget_chars)}),
    );
    PromptBuilder::compose_system_for_budget(&mut big_volatile, 0, Some(1000), &tuning);
    assert!(!big_volatile.contains_key("composio_context"));
    assert!(big_volatile.contains_key("scratchpad"), "scratchpad is never dropped");

    let mut big_text = base();
    PromptBuilder::compose_system_for_budget(&mut big_text, budget_chars, Some(1000), &tuning);
    assert!(!big_text.contains_key("composio_context"), "fixed text counts");
}

// =========================================================================
// Forward-only history window (prompt-cache stable trimming)
// =========================================================================

use super::message_history::resolve_history_window;

/// A realistic message stream: turn `t` is a user text, `2 + t % 4` tool
/// calls, then the assistant's reply. `reply_len` pads the reply so the char
/// budget can bind on small windows.
fn realistic_stream(turns: usize, reply_len: usize) -> Vec<Message> {
    let mut out = Vec::new();
    for t in 0..turns {
        out.push(user_text_msg(&format!("request number {t}")));
        for k in 0..(2 + t % 4) {
            out.push(tool_call_msg(
                &format!("TOOL_{k}"),
                format!("{{\"turn\":{t},\"step\":{k},\"ok\":true}}"),
                None,
            ));
        }
        let mut reply = format!("Reply to request {t}. ");
        while reply.len() < reply_len {
            reply.push_str("More detail about the result. ");
        }
        out.push(assistant_text_msg(&reply));
    }
    out
}

fn is_user_text(m: &Message) -> bool {
    m.author == "User" && matches!(m.content, MessageContent::Text { .. })
}

/// Outgoing messages minus a trailing continuation nudge — the nudge is
/// request-only scaffolding at the tail, never part of a cached prefix.
fn outgoing_history(result: &PromptBuildResult) -> Vec<ChatMessage> {
    let mut msgs = result.prompt.messages.clone();
    if msgs.last().is_some_and(|m| {
        matches!(&m.content[..], [ContentBlock::Text { text }] if text == CONTINUATION_NUDGE)
    }) {
        msgs.pop();
    }
    msgs
}

struct GrowthReport {
    builds: usize,
    trims: usize,
    max_included: usize,
}

/// Append `stream` to `session` one message at a time, building (and
/// persisting the anchor, as `chat.rs` does) after every append. Asserts the
/// cache-stability invariants on every step and returns trim statistics.
fn grow_and_check(session: &mut Session, settings: &Settings, stream: Vec<Message>) -> GrowthReport {
    let mut prev: Option<Vec<ChatMessage>> = None;
    let mut report = GrowthReport { builds: 0, trims: 0, max_included: 0 };
    for msg in stream {
        session.messages.push(msg);
        let anchor_before = session.history_anchor;
        let result = PromptBuilder::new(session, settings).build_prompt(String::new());
        session.history_anchor = result.history_anchor;
        report.builds += 1;
        let current = outgoing_history(&result);

        let start = session
            .history_anchor
            .and_then(|id| session.messages.iter().position(|m| m.id == id))
            .unwrap_or(0);
        report.max_included = report.max_included.max(session.messages.len() - start);

        if session.history_anchor != anchor_before {
            report.trims += 1;
            let latest_user = session.messages.iter().rposition(is_user_text).unwrap();
            assert!(
                is_user_text(&session.messages[start]),
                "a trim must cut at a user text message (turn start)"
            );
            assert!(start < latest_user, "a trim must never cut into the current turn");
            assert!(
                anchor_before
                    .and_then(|id| session.messages.iter().position(|m| m.id == id))
                    .is_none_or(|old| old < start),
                "the anchor only moves forward"
            );
        } else if let Some(prev) = &prev {
            assert!(
                current.len() >= prev.len() && current[..prev.len()] == prev[..],
                "without a trim, build {} must extend the previous request exactly \
                 (prev {} msgs, now {} msgs)",
                report.builds,
                prev.len(),
                current.len()
            );
        }
        prev = Some(current);
    }
    report
}

/// The Opus 5.5 regression: past the history limit the window used to slide
/// by one message per request, so the message after the pinned first user
/// message changed every time and the provider cache missed all history.
#[test]
fn history_window_is_prefix_stable_and_trims_in_chunks() {
    let mut session = create_test_session();
    let settings = Settings::default();
    let limit = settings.chat_history_length;
    // ~3x the limit, in realistic turns.
    let stream = realistic_stream(60, 200);
    let total = stream.len();
    assert!(total >= 3 * limit, "stream should reach ~3x the limit ({total})");

    let report = grow_and_check(&mut session, &settings, stream);
    assert_eq!(report.builds, total);
    assert!(report.trims >= 2, "a 3x-limit chat must trim more than once");
    // A trim drops to <= 2/3 of the limit, so the next one is >= ~limit/3
    // messages away: at most one per chunk, never one per message.
    let max_trims = total / (limit / 3) + 1;
    assert!(
        report.trims <= max_trims,
        "{} trims over {} builds — expected at most {max_trims}",
        report.trims,
        total
    );
    // The window never grows past the limit by more than one turn's worth.
    assert!(
        report.max_included <= limit + 7,
        "included history {} exceeded the limit {limit}",
        report.max_included
    );
}

/// Same invariants on a finite (16K) window where the char budget, not the
/// message count, decides when to trim — and the history still fits.
#[test]
fn small_window_trims_in_chunks_and_still_fits() {
    let mut session = create_test_session();
    session.llm_provider = Some(LlmProvider::OpenAiCompat);
    session.chat_model = Some("local-model".to_string());
    let mut settings = Settings::default();
    settings.openai_compat_config.endpoint = "http://localhost:11434/v1".to_string();
    settings.openai_compat_config.max_context_tokens = Some(16_000);

    let stream = realistic_stream(40, 4_000);
    let total = stream.len();
    let report = grow_and_check(&mut session, &settings, stream);
    assert!(report.trims >= 2, "the char budget should force trims on 16K");
    assert!(
        report.trims * 5 < total,
        "{} trims over {} builds is not chunked",
        report.trims,
        total
    );
    assert!(
        report.max_included < settings.chat_history_length,
        "the char budget, not the count, should bind here"
    );

    // The final request still fits the window.
    let builder = PromptBuilder::new(&session, &settings);
    let tuning = builder.effective_tuning();
    let result = builder.build_prompt(String::new());
    let message_chars: usize = result
        .prompt
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .map(|b| match b {
            ContentBlock::Text { text } | ContentBlock::Thinking { text, .. } => text.len(),
            ContentBlock::ToolCall { name, arguments, .. } => name.len() + arguments.to_string().len(),
            ContentBlock::ToolResult { content, .. } => content.to_string().len(),
            _ => 0,
        })
        .sum();
    let window_chars = (16_000.0 * tuning.chars_per_token) as usize;
    let system_chars = result.prompt.system.as_deref().map_or(0, str::len);
    assert!(
        system_chars + message_chars <= window_chars,
        "request ({system_chars} system + {message_chars} history chars) must fit {window_chars}"
    );
}

/// A session saved before the anchor existed deserializes with no anchor, and
/// a set anchor round-trips.
#[test]
fn session_without_history_anchor_deserializes() {
    let mut session = create_test_session();
    session.messages = realistic_stream(1, 10);
    let mut json = serde_json::to_value(&session).unwrap();
    assert!(json.get("history_anchor").is_none(), "None is not serialized");
    json.as_object_mut().unwrap().remove("history_anchor");
    let old: Session = serde_json::from_value(json).expect("old session JSON loads");
    assert_eq!(old.history_anchor, None);

    session.history_anchor = Some(session.messages[0].id);
    let round: Session =
        serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
    assert_eq!(round.history_anchor, session.history_anchor);
}

/// A deleted anchor (rewind, message removal) recovers by recomputing from
/// the start of the chat — the same result a session with no anchor gets.
#[test]
fn deleted_history_anchor_recovers() {
    let settings = Settings::default();
    let mut session = create_test_session();
    let _ = grow_and_check(&mut session, &settings, realistic_stream(40, 200));
    let anchor = session.history_anchor.expect("a long chat has an anchor");

    // Delete the anchor message itself.
    session.messages.retain(|m| m.id != anchor);
    let recovered = PromptBuilder::new(&session, &settings).build_prompt(String::new());
    let mut fresh = session.clone();
    fresh.history_anchor = None;
    let from_scratch = PromptBuilder::new(&fresh, &settings).build_prompt(String::new());

    assert_eq!(recovered.history_anchor, from_scratch.history_anchor);
    assert_eq!(recovered.prompt.messages, from_scratch.prompt.messages);
    let new_anchor = recovered.history_anchor.expect("still over the limit — re-anchored");
    let idx = session.messages.iter().position(|m| m.id == new_anchor).unwrap();
    assert!(is_user_text(&session.messages[idx]));
    let limit = settings.chat_history_length;
    assert!(session.messages.len() - idx <= limit * 2 / 3, "re-anchored at the low-water mark");
}

/// The window never cuts into the in-progress turn: a single turn longer than
/// the limit keeps its whole run (and its user message) rather than splitting.
#[test]
fn history_window_never_cuts_into_the_current_turn() {
    let mut messages = vec![user_text_msg("first"), assistant_text_msg("ok")];
    messages.push(user_text_msg("long task"));
    for k in 0..30 {
        messages.push(tool_call_msg(&format!("T{k}"), "{}".to_string(), None));
    }
    let window = resolve_history_window(&messages, None, Some(0), 10, None);
    assert!(
        window.start_index <= 2,
        "the current turn's user message (idx 2) must stay in the window"
    );
    // Re-resolving with the returned anchor is a fixed point.
    let again = resolve_history_window(&messages, window.anchor, Some(0), 10, None);
    assert_eq!(again, window);
}

/// Under the limit nothing moves: no anchor, the whole chat is sent.
#[test]
fn history_window_untouched_under_the_limit() {
    let messages = realistic_stream(3, 10);
    let window = resolve_history_window(&messages, None, Some(0), 75, None);
    assert_eq!(window.start_index, 0);
    assert_eq!(window.anchor, None);
}

/// A char budget that jitters between requests (the system prompt carries
/// per-turn content) must not flip the window back and forth: after a trim,
/// alternating budgets resolve the same window on every build.
#[test]
fn jittering_char_budget_does_not_flip_the_window() {
    let messages = realistic_stream(30, 1_000);
    let budget = 40_000usize;
    let first = resolve_history_window(&messages, None, Some(0), 1_000, Some(budget));
    assert!(first.anchor.is_some(), "the budget should force a trim");
    let mut anchor = first.anchor;
    for i in 0..20 {
        // +/-5% wobble around the budget.
        let b = if i % 2 == 0 { budget * 95 / 100 } else { budget * 105 / 100 };
        let w = resolve_history_window(&messages, anchor, Some(0), 1_000, Some(b));
        assert_eq!(w, first, "build {i}: window moved under a jittering budget");
        anchor = w.anchor;
    }
}

/// The live send pushes the user message (and an empty reply placeholder) onto
/// the session before building, so the builder must be called with an empty
/// `user_message`; the message must then appear exactly once, as the last turn.
#[test]
fn live_send_includes_the_user_message_once() {
    let mut reply = user_text_msg("first reply");
    reply.author = "Hobbes".to_string();
    let mut placeholder = user_text_msg("");
    placeholder.author = "Hobbes".to_string();
    let mut session = create_test_session();
    session.messages = vec![user_text_msg("first ask"), reply, user_text_msg("second ask"), placeholder];
    let settings = Settings::default();
    let result = PromptBuilder::new(&session, &settings).build_prompt(String::new());
    let msgs = &result.prompt.messages;
    let count = msgs
        .iter()
        .filter(|m| m.content.iter().any(|b| matches!(b, crate::llm::ContentBlock::Text { text } if text == "second ask")))
        .count();
    assert_eq!(count, 1);
    assert_eq!(msgs.last().map(|m| m.role.clone()), Some(crate::llm::ChatRole::User));
}
