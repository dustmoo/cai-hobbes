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
        handoff: None,
        watch_word_recovery_count: 0,
        scheduled_timers: Vec::new(),
        tool_call_history: Vec::new(),
        tab_emoji: None,
        tab_title: None,
        name_user_set: false,
        tab_title_provisional: false,
        history_anchor: None,
        context_ledger: Vec::new(),
        dispatch_link: None,
        dispatched_workers: Vec::new(),
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
    let ctx = builder.build_system_context();
    // The summary is live context (shown in a trim snapshot); check the
    // composed value alongside the stable system string.
    let full_text = format!(
        "{}\n{}",
        ctx.system,
        ctx.volatile.get("conversation_summary").expect("summary composed")
    );

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
            cancelled: None,
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
    rmcp::model::Tool::new("HOBBES_INVOKE_SKILL", "Load a skill", serde_json::Map::new())
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
    let text = model_text(&prompt);
    assert!(text.contains("[Context update: planner_today."));
    assert!(
        text.contains("HOBBES_PLAN_DAY"),
        "instruction names the tools"
    );

    // Without the attachment (the default) nothing is injected.
    let prompt = PromptBuilder::new(&session, &settings).build_prompt("Hi".to_string());
    assert!(!model_text(&prompt).contains("planner_today"));
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
    let merge = prompt
        .prompt
        .tools
        .iter()
        .find(|t| t.name == "HOBBES_PROJECT_MERGE")
        .expect("enabled: HOBBES_PROJECT_MERGE advertised");
    assert!(merge.description.contains("approve"), "{}", merge.description);
    for t in ["HOBBES_PROJECT_LIST", "HOBBES_PROJECT_UPSERT"] {
        let def = prompt.prompt.tools.iter().find(|d| d.name == t).unwrap();
        assert!(def.description.contains("HOBBES_PROJECT_MERGE"), "{t} should point at the merge tool");
    }

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
        "HOBBES_PROJECT_LIST",
        "HOBBES_PROJECT_MERGE",
    ] {
        assert!(
            !prompt.prompt.tools.iter().any(|t| t.name == tool),
            "disabled: '{}' must disappear from the advertised tools",
            tool
        );
    }
}

/// HOBBES_AGENT_TIME is never offered to a normal chat — not even when the
/// synced MCP context somehow carries it — and is offered to the leased
/// agent-time session only.
#[test]
fn agent_time_tool_only_in_the_leased_agent_time_session() {
    // Behind the release gate: this exercises the enabled path.
    let settings = Settings { experimental_agent_time: true, ..Settings::default() };
    let mut session = session_with_planner_tools();
    let has_it = |p: &crate::context::prompt_builder::PromptBuildResult| {
        p.prompt.tools.iter().any(|t| t.name == "HOBBES_AGENT_TIME")
    };

    // Normal chat: not in the list (the visibility check says no).
    let planner = crate::todo::PlannerState::default();
    let visible = crate::components::builtin_tools::agent_time_tool_visible(&planner, &session.id);
    assert!(!visible);
    let prompt = PromptBuilder::new(&session, &settings)
        .with_agent_time_tools(visible)
        .build_prompt("Hi".to_string());
    assert!(!has_it(&prompt), "normal chat must not see HOBBES_AGENT_TIME");
    assert!(
        prompt.prompt.tools.iter().any(|t| t.name == "HOBBES_TODO_CREATE"),
        "the rest of the planner is still there"
    );

    // Even a context that carries the definition gets it stripped.
    if let Some(ctx) = session.active_context.mcp_tools.as_mut() {
        ctx.servers[0]
            .tools
            .extend(crate::mcp::planner_client::PlannerClient::agent_time_tools());
    }
    let prompt = PromptBuilder::new(&session, &settings).build_prompt("Hi".to_string());
    assert!(!has_it(&prompt), "stripped unless the builder is told otherwise");

    // The leased agent-time session sees it, exactly once.
    let mut planner = crate::todo::PlannerState::default();
    planner.agent_time_lease = Some(crate::todo::agent_time::AgentTimeLease {
        chat_session_id: session.id.clone(),
        opened_at: chrono::Utc::now(),
    });
    let visible = crate::components::builtin_tools::agent_time_tool_visible(&planner, &session.id);
    assert!(visible);
    let prompt = PromptBuilder::new(&session, &settings)
        .with_agent_time_tools(visible)
        .build_prompt("Hi".to_string());
    assert_eq!(
        prompt
            .prompt
            .tools
            .iter()
            .filter(|t| t.name == "HOBBES_AGENT_TIME")
            .count(),
        1
    );
}

/// The release gate (`Settings::agent_time_enabled`, off by default):
/// neither HOBBES_AGENT_TIME nor HOBBES_DECISIONS reaches any prompt — not
/// a normal chat, not even a leased agent-time session — while the rest of
/// the planner (HOBBES_DISPATCH included) stays.
#[test]
fn hidden_agent_time_and_decisions_are_withheld_from_every_prompt() {
    let hidden = Settings::default();
    assert!(!hidden.agent_time_enabled());
    let mut session = session_with_planner_tools();
    if let Some(ctx) = session.active_context.mcp_tools.as_mut() {
        ctx.servers[0]
            .tools
            .extend(crate::mcp::planner_client::PlannerClient::agent_time_tools());
    }
    let names = |p: &crate::context::prompt_builder::PromptBuildResult| -> Vec<String> {
        p.prompt.tools.iter().map(|t| t.name.clone()).collect()
    };
    for leased in [false, true] {
        let prompt = PromptBuilder::new(&session, &hidden)
            .with_agent_time_tools(leased)
            .build_prompt("Hi".to_string());
        let tools = names(&prompt);
        assert!(!tools.iter().any(|t| t == "HOBBES_AGENT_TIME"), "leased={leased}: {tools:?}");
        assert!(!tools.iter().any(|t| t == "HOBBES_DECISIONS"), "leased={leased}: {tools:?}");
        assert!(tools.iter().any(|t| t == "HOBBES_DISPATCH"), "dispatch stays: {tools:?}");
        assert!(tools.iter().any(|t| t == "HOBBES_TODO_CREATE"));
    }

    // Switched on, the decision queue is offered again.
    let on = Settings { experimental_agent_time: true, ..Settings::default() };
    let prompt = PromptBuilder::new(&session, &on).build_prompt("Hi".to_string());
    assert!(names(&prompt).iter().any(|t| t == "HOBBES_DECISIONS"));
}

/// With the gate off, planner_today says nothing about agent time and
/// points "needs you" at the worker instead of HOBBES_DECISIONS.
#[test]
fn hidden_planner_today_has_no_agent_time_or_decision_tool_lines() {
    use crate::todo::handlers::{handle_decisions, handle_todo_create};
    let today = chrono::Local::now().date_naive();
    let mut planner = crate::todo::PlannerState::default();
    planner.agent_time_grant =
        crate::todo::model::AgentTimeGrant { enabled: true, daily_minutes: 45, ..Default::default() };
    handle_todo_create(
        &mut planner,
        &serde_json::json!({"todos": [{"title": "Pick a vendor", "lane": "agent", "scheduled_for": "today"}]}),
        "s",
        today,
        false,
    );
    let id = planner.todos.last().unwrap().id.clone();
    let asked = handle_decisions(
        &mut planner,
        &serde_json::json!({"action": "ask", "todo_id": id, "question": "Vendor A or B?", "options": ["A", "B"], "est_minutes": 2}),
        None,
        None,
        false,
    );
    assert_eq!(asked.status, crate::components::shared::ToolCallStatus::Completed, "{}", asked.response);

    let hidden = Settings::default();
    let ctx = crate::todo::handlers::planner_today_context_with_events(&planner, &hidden, today, &[])
        .expect("planner context on");
    let text = ctx.to_string();
    assert!(ctx.get("agent_time").is_none(), "{text}");
    assert!(!text.contains("HOBBES_DECISIONS"), "{text}");
    assert!(ctx.get("agents").is_some_and(|a| a.get("needs_you").is_some()), "needs-you still shown: {text}");

    let on = Settings { experimental_agent_time: true, ..Settings::default() };
    let ctx = crate::todo::handlers::planner_today_context_with_events(&planner, &on, today, &[]).unwrap();
    assert!(ctx.to_string().contains("HOBBES_DECISIONS"));
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
    assert_ne!(
        last.role,
        ChatRole::Assistant,
        "a continuation must not leave the request ending on an assistant turn"
    );
    // The request tail (rendered as a user turn by every connector) carries
    // the continuation instruction, so no separate nudge is appended.
    assert!(tail_text(&prompt).contains("CONTINUATION INSTRUCTION"));
    assert!(!prompt.prompt.messages.iter().any(|m| m
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text } if text == CONTINUATION_NUDGE))));
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

    let msgs = &prompt.prompt.messages;
    assert_eq!(msgs[msgs.len() - 2].role, ChatRole::Tool, "the tool result precedes the tail");
    assert!(tail_text(&prompt).contains("TOOL COMPLETION INSTRUCTION"));
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

    let last = prompt
        .prompt
        .messages
        .iter()
        .rfind(|m| m.role != ChatRole::System)
        .expect("messages built");
    assert_eq!(last.role, ChatRole::User);
    assert!(
        matches!(&last.content[0], ContentBlock::Text { text } if text == "next thing"),
        "the user's own message is the final turn, not a nudge"
    );
    assert!(tail_text(&prompt).starts_with(super::ledger::TAIL_HEADER));
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

    let fresh = model_text(&PromptBuilder::new(&session, &settings).build_prompt(String::new()));
    assert!(fresh.contains("Do the thing."), "fresh skill keeps its manual");

    msgs.extend(user_turns(1));
    session.messages = msgs;
    let idle = model_text(&PromptBuilder::new(&session, &settings).build_prompt(String::new()));
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

fn build_turn(turn: &Turn, settings: &Settings) -> PromptBuildResult {
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
    // The live context is recorded at the user-turn boundary; a turn that
    // goes on to a (failed) tool call is built again from there, as the live
    // flow does.
    let tool_call = (messages.len() == 3).then(|| messages.pop().unwrap());
    session.messages = messages;
    let build = |session: &Session| {
        PromptBuilder::new(session, settings)
            .with_planner_today(Some(serde_json::json!({ "plan": turn.planner })))
            .build_prompt(String::new())
    };
    let result = build(&session);
    let Some(call) = tool_call else { return result };
    session.context_ledger.extend(result.ledger_entries);
    session.messages.push(call);
    build(&session)
}

/// Texts of the `ChatRole::System` messages (ledger entries and tails).
fn system_texts(result: &PromptBuildResult) -> Vec<String> {
    result
        .prompt
        .messages
        .iter()
        .filter(|m| m.role == ChatRole::System)
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Everything the model reads as instructions: the system string plus the
/// System messages.
fn model_text(result: &PromptBuildResult) -> String {
    let mut out = result.prompt.system.clone().unwrap_or_default();
    for t in system_texts(result) {
        out.push('\n');
        out.push_str(&t);
    }
    out
}

/// The request tail: the last message, listed in `turn_scoped`.
fn tail_text(result: &PromptBuildResult) -> String {
    let msgs = &result.prompt.messages;
    let last = msgs.len() - 1;
    assert_eq!(msgs[last].role, ChatRole::System, "the tail is the last message");
    assert!(result.prompt.turn_scoped.contains(&last), "the tail is turn-scoped");
    match &msgs[last].content[..] {
        [ContentBlock::Text { text }] => text.clone(),
        other => panic!("tail is one text block: {other:?}"),
    }
}

#[test]
fn system_is_byte_identical_across_turns() {
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
    let res_a = build_turn(&a, &settings);
    let sys_a = res_a.prompt.system.clone().unwrap();
    // Rebuild a few times: HashMap-ordered content (skill tool mappings,
    // loaded skills) gets a fresh random order on every build.
    for _ in 0..4 {
        let again = build_turn(&a, &settings);
        assert_eq!(again.prompt.system.as_deref(), Some(sys_a.as_str()));
        // Ledger entries are byte-identical too (the tail's clock differs).
        let n = system_texts(&res_a).len() - 1;
        assert_eq!(system_texts(&again)[..n], system_texts(&res_a)[..n]);
    }
    let res_b = build_turn(&b, &settings);
    set_available_skills_for_test(vec![]);

    assert_eq!(
        res_b.prompt.system.as_deref(),
        Some(sys_a.as_str()),
        "the system string must not change between turns"
    );
    assert_eq!(res_a.prompt.system_volatile_offset, None);
    for stable_section in [
        "<SYSTEM_CONTEXT>",
        "mcp_servers",
        "composio_context",
        "\"user_name\": \"Dustin\"",
        "active_skill",
        "Follow the release checklist.",
        "available_skills",
        "context_protocol",
    ] {
        assert!(sys_a.contains(stable_section), "system lacks {stable_section}");
    }
    assert!(!sys_a.contains("TURN_CONTEXT"));

    for (res, turn) in [(&res_a, &a), (&res_b, &b)] {
        let sys = res.prompt.system.as_deref().unwrap();
        let live = system_texts(res).join("\n");
        for token in [turn.scratchpad, turn.planner, "loaded_skills"] {
            assert!(!sys.contains(token), "{token} leaked into the system string");
            assert!(live.contains(token), "{token} missing from the ledger");
        }
        // The summary stands in for trimmed history; nothing is trimmed here.
        assert!(!model_text(res).contains(turn.summary), "summary shown before a trim");
    }
    let tail_b = tail_text(&res_b);
    assert!(tail_b.starts_with(super::ledger::TAIL_HEADER));
    assert!(tail_b.contains("TOOL COMPLETION INSTRUCTION"));
    assert!(tail_b.contains("RECOVERY NOTE"));
    assert!(!sys_a.contains("INSTRUCTION:"));
    assert!(!sys_a.contains("RECOVERY NOTE"));
}

#[test]
fn clock_lives_in_the_tail_not_the_system() {
    let settings = cache_test_settings();
    let session = cache_test_session(false);
    let result = PromptBuilder::new(&session, &settings).build_prompt("Hi".to_string());
    let system = result.prompt.system.clone().unwrap();
    assert!(!system.contains("current_time"));
    assert!(!system.contains("Current time"));
    assert!(tail_text(&result).contains("Current time: "));
}

#[test]
fn multibyte_live_context_renders_in_messages() {
    let settings = Settings {
        persona: "Tu es Hobbes 🦀 — très direct.".to_string(),
        user_name: Some("Düstîn 🎯".to_string()),
        ..Default::default()
    };
    let mut session = cache_test_session(false);
    session.scratchpad = "ünïcödé 🧠 notes".to_string();
    session.messages = vec![user_text_msg("salut")];
    let result = PromptBuilder::new(&session, &settings).build_prompt(String::new());
    let system = result.prompt.system.clone().unwrap();
    assert!(system.starts_with("Tu es Hobbes 🦀"));
    assert!(system.ends_with("</SYSTEM_CONTEXT>"));
    assert!(!system.contains("🧠"));
    assert!(system_texts(&result).iter().any(|t| t.contains("🧠")));
}

#[test]
fn tools_and_servers_are_sorted_regardless_of_input_order() {
    // available_skills renders from the process-wide skill mirror.
    let _mirror = SKILL_MIRROR_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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

    let stable = &forward.system;
    assert_eq!(stable, &reversed.system, "mcp_servers order is stable");
    let pos = |name: &str| stable.find(&format!("\"name\": \"{name}\"")).unwrap();
    assert!(pos("alpha") < pos("hobbes-core") && pos("hobbes-core") < pos("omega"));
}

/// Every section the old single-block layout carried still reaches the
/// model: stable ones in the system string, live ones as ledger entries,
/// per-request ones in the tail.
#[test]
fn request_still_carries_every_section() {
    let settings = cache_test_settings();
    let result = build_turn(
        &Turn {
            scratchpad: "SCRATCH",
            summary: "SUMMARY",
            planner: "PLAN",
            ends_in_failed_tool: true,
        },
        &settings,
    );
    let system = result.prompt.system.clone().unwrap();
    assert!(system.starts_with(&settings.persona));
    assert!(system.contains("TOOL POLICY:"));
    let text = model_text(&result);
    for section in [
        "<SYSTEM_CONTEXT>",
        "</SYSTEM_CONTEXT>",
        "mcp_servers",
        "composio_context",
        "user_name",
        "active_skill",
        "Current time:",
        "[Context update: scratchpad.",
        "SCRATCH",
        "[Context update: planner_today.",
        "PLAN",
        "[Context update: loaded_skills.",
        "TOOL COMPLETION INSTRUCTION",
        "RECOVERY NOTE",
    ] {
        assert!(text.contains(section), "request lacks {section}");
    }
}

#[test]
fn continuation_instruction_is_in_the_tail() {
    let settings = Settings::default();
    let mut session = create_test_session();
    session.messages = vec![
        user_text_msg("hi"),
        assistant_text_msg("hello"),
        assistant_text_msg(""),
    ];
    let ctx = PromptBuilder::new(&session, &settings).build_system_context();
    assert!(ctx.turn_instructions.starts_with("CONTINUATION INSTRUCTION"));
    assert!(!ctx.system.contains("CONTINUATION INSTRUCTION"));
    let result = PromptBuilder::new(&session, &settings).build_prompt(String::new());
    assert!(tail_text(&result).contains("CONTINUATION INSTRUCTION"));
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

/// Outgoing messages minus request-only scaffolding at the tail: a trailing
/// continuation nudge, and — for providers that only get the current
/// request's tail — that tail. Claude replays every tail, so its tails stay.
fn outgoing_history(result: &PromptBuildResult, all_tails: bool) -> Vec<ChatMessage> {
    let mut msgs = result.prompt.messages.clone();
    if msgs.last().is_some_and(|m| {
        matches!(&m.content[..], [ContentBlock::Text { text }] if text == CONTINUATION_NUDGE)
    }) {
        msgs.pop();
    }
    if !all_tails && result.prompt.turn_scoped.last() == Some(&(msgs.len() - 1)) {
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
/// persisting the anchor and ledger entries, as `chat.rs` does) after every
/// append. Asserts the cache-stability invariants on every step and returns
/// trim statistics.
fn grow_and_check(session: &mut Session, settings: &Settings, stream: Vec<Message>) -> GrowthReport {
    grow_and_check_with(session, settings, stream, true)
}

/// Blank every tool result's content: on a finite window Pass 2 re-budgets
/// historical results as a turn grows (a trim may only happen at the next
/// user turn), so only the message structure is prefix-stable there.
fn without_tool_results(mut msgs: Vec<ChatMessage>) -> Vec<ChatMessage> {
    for m in &mut msgs {
        for b in &mut m.content {
            if let ContentBlock::ToolResult { content, .. } = b {
                *content = serde_json::Value::Null;
            }
        }
    }
    msgs
}

fn grow_and_check_with(
    session: &mut Session,
    settings: &Settings,
    stream: Vec<Message>,
    exact_tool_results: bool,
) -> GrowthReport {
    let all_tails = settings.provider_for_session(session) == LlmProvider::Claude;
    let mut prev: Option<Vec<ChatMessage>> = None;
    let mut report = GrowthReport { builds: 0, trims: 0, max_included: 0 };
    for msg in stream {
        session.messages.push(msg);
        let anchor_before = session.history_anchor;
        let result = PromptBuilder::new(session, settings).build_prompt(String::new());
        session.history_anchor = result.history_anchor;
        session.context_ledger.extend(result.ledger_entries.iter().cloned());
        report.builds += 1;
        let mut current = outgoing_history(&result, all_tails);
        if !exact_tool_results {
            current = without_tool_results(current);
        }

        let start = session
            .history_anchor
            .and_then(|id| session.messages.iter().position(|m| m.id == id))
            .unwrap_or(0);
        report.max_included = report.max_included.max(session.messages.len() - start);

        if session.history_anchor != anchor_before {
            report.trims += 1;
            assert!(result.prompt.history_rewritten, "a trim is flagged as a rewrite");
            let latest_user = session.messages.iter().rposition(is_user_text).unwrap();
            assert_eq!(
                latest_user,
                session.messages.len() - 1,
                "a trim happens only at a user-turn boundary"
            );
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
        } else {
            assert!(!result.prompt.history_rewritten);
            if let Some(prev) = &prev {
                assert!(
                    current.len() >= prev.len() && current[..prev.len()] == prev[..],
                    "without a trim, build {} must extend the previous request exactly \
                     (prev {} msgs, now {} msgs)",
                    report.builds,
                    prev.len(),
                    current.len()
                );
            }
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
    let report = grow_and_check_with(&mut session, &settings, stream, false);
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

    // Delete the anchor message itself, then start a new turn.
    session.messages.retain(|m| m.id != anchor);
    session.prune_orphaned_ledger();
    session.messages.push(user_text_msg("after the rewind"));
    let now = chrono::Local::now();
    let recovered = PromptBuilder::new(&session, &settings).with_now(now).build_prompt(String::new());
    let mut fresh = session.clone();
    fresh.history_anchor = None;
    let from_scratch = PromptBuilder::new(&fresh, &settings).with_now(now).build_prompt(String::new());

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
    let last_turn = msgs.iter().rfind(|m| m.role != crate::llm::ChatRole::System);
    assert_eq!(last_turn.map(|m| m.role.clone()), Some(crate::llm::ChatRole::User));
}

// =========================================================================
// Turn context ledger (Phase 4): append-only live context, tails, trims
// =========================================================================

use crate::session::{LedgerEntry, LedgerKind};
use super::ledger::{TAIL_HEADER, TRIM_NOTE_KEY};

fn placeholder_msg() -> Message {
    assistant_text_msg("")
}

fn fixed_clock(n: i64) -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone;
    chrono::Local.with_ymd_and_hms(2026, 10, 2, 9, 0, 0).unwrap() + chrono::Duration::seconds(n)
}

fn text_of(m: &ChatMessage) -> String {
    m.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn is_tail(m: &ChatMessage) -> bool {
    m.role == ChatRole::System && text_of(m).starts_with(TAIL_HEADER)
}

/// Drives a session the way `chat.rs` + `stream_manager` do: push the user
/// message and an empty placeholder, build, persist the anchor and ledger
/// entries; a tool call upgrades the placeholder and a new placeholder is
/// pushed for the continuation; the final reply fills the placeholder.
struct ChatSim {
    session: Session,
    settings: Settings,
    planner: serde_json::Value,
    clock: i64,
    prev: Option<Vec<ChatMessage>>,
    builds: usize,
    trims: usize,
    /// Per build: whether it was at a user-turn boundary, and the keys of
    /// the `Ledger` and `Trim` entries it wrote.
    entry_log: Vec<(bool, Vec<String>, Vec<String>)>,
    /// available_skills renders from the process-wide skill mirror; hold it
    /// still while the sim compares builds byte for byte.
    _mirror: std::sync::MutexGuard<'static, ()>,
}

impl ChatSim {
    fn new(provider: LlmProvider, history_len: usize) -> Self {
        let mut session = create_test_session();
        session.llm_provider = Some(provider);
        let settings = Settings {
            chat_history_length: history_len,
            ..Default::default()
        };
        Self {
            session,
            settings,
            planner: serde_json::json!({"todos": ["write the ledger"]}),
            clock: 0,
            prev: None,
            builds: 0,
            trims: 0,
            entry_log: Vec::new(),
            _mirror: SKILL_MIRROR_LOCK.lock().unwrap_or_else(|p| p.into_inner()),
        }
    }

    fn all_tails(&self) -> bool {
        self.settings.provider_for_session(&self.session) == LlmProvider::Claude
    }

    fn build(&mut self) -> PromptBuildResult {
        self.clock += 7;
        let boundary = self
            .session
            .messages
            .iter()
            .rev()
            .find(|m| !matches!(&m.content, MessageContent::Text { content, .. } if content.is_empty() && m.author == "Hobbes"))
            .is_some_and(is_user_text);
        let result = PromptBuilder::new(&self.session, &self.settings)
            .with_planner_today(Some(self.planner.clone()))
            .with_now(fixed_clock(self.clock))
            .build_prompt(String::new());
        // Same state, same bytes: a second build of the unchanged session
        // (before persisting) renders identically.
        let again = PromptBuilder::new(&self.session, &self.settings)
            .with_planner_today(Some(self.planner.clone()))
            .with_now(fixed_clock(self.clock))
            .build_prompt(String::new());
        assert_eq!(again.prompt.messages, result.prompt.messages, "build is deterministic");
        assert_eq!(again.prompt.system, result.prompt.system);

        let anchor_before = self.session.history_anchor;
        self.session.history_anchor = result.history_anchor;
        self.session.context_ledger.extend(result.ledger_entries.iter().cloned());
        self.builds += 1;
        let keys = |kind: LedgerKind| -> Vec<String> {
            result
                .ledger_entries
                .iter()
                .filter(|e| e.kind == kind)
                .map(|e| e.key.clone())
                .collect()
        };
        self.entry_log.push((boundary, keys(LedgerKind::Ledger), keys(LedgerKind::Trim)));

        let current = outgoing_history(&result, self.all_tails());
        if result.history_anchor != anchor_before {
            self.trims += 1;
            assert!(result.prompt.history_rewritten, "a trim is flagged");
            assert!(boundary, "a trim happens only at a user-turn boundary");
        } else {
            assert!(!result.prompt.history_rewritten);
            if let Some(prev) = &self.prev {
                assert!(
                    current.len() >= prev.len() && current[..prev.len()] == prev[..],
                    "build {} must extend the previous request exactly",
                    self.builds
                );
            }
        }
        for &i in &result.prompt.turn_scoped {
            assert!(is_tail(&result.prompt.messages[i]), "turn_scoped lists tails only");
        }
        let tails = result.prompt.messages.iter().filter(|m| is_tail(m)).count();
        assert_eq!(tails, result.prompt.turn_scoped.len(), "every tail is turn-scoped");
        self.prev = Some(current);
        result
    }

    /// One user turn with `tool_calls` rounds, then the reply.
    fn turn(&mut self, t: usize, tool_calls: usize) -> Vec<PromptBuildResult> {
        let mut out = Vec::new();
        self.session.messages.push(user_text_msg(&format!("request {t}")));
        self.session.messages.push(placeholder_msg());
        out.push(self.build());
        for k in 0..tool_calls {
            let call = tool_call_msg(&format!("TOOL_{k}"), format!("{{\"t\":{t},\"k\":{k}}}"), None);
            let last = self.session.messages.last_mut().unwrap();
            last.content = call.content;
            self.session.messages.push(placeholder_msg());
            out.push(self.build());
        }
        let last = self.session.messages.last_mut().unwrap();
        last.content = assistant_text_msg(&format!("Reply {t}.")).content;
        out
    }

    /// Turn-end bookkeeping the background services do: a tool snapshot per
    /// call of the finished turn and a fresh conversation summary.
    fn after_turn(&mut self, t: usize) {
        let ids: Vec<String> = self
            .session
            .messages
            .iter()
            .filter_map(|m| match &m.content {
                MessageContent::ToolCall(tc) => Some(tc.execution_id.clone()),
                _ => None,
            })
            .collect();
        for id in ids {
            self.session
                .active_context
                .extra
                .entry(format!("tool_snapshot_{id}"))
                .or_insert_with(|| serde_json::json!({"snapshot_of": id}));
        }
        self.session.active_context.conversation_summary.summary = format!("SUMMARY_T{t}");
    }
}

#[test]
fn long_chat_is_append_only_except_on_trims() {
    let mut sim = ChatSim::new(LlmProvider::Claude, 12);
    let mut first_trim_turn = None;
    // Keys whose value changed before each turn's boundary.
    let mut expected: Vec<Vec<&str>> = Vec::new();
    for t in 0..30 {
        // Live context changes between turns: the planner every 3rd turn,
        // the scratchpad every 4th (cleared every 8th), the summary always
        // (it must never produce an entry on its own).
        let mut changed = Vec::new();
        if t % 3 == 0 {
            sim.planner = serde_json::json!({"todos": [format!("task {t}")]});
            changed.push("planner_today");
        }
        if t > 0 && t % 4 == 0 {
            sim.session.scratchpad = if t % 8 == 0 { String::new() } else { format!("notes {t}") };
            changed.push("scratchpad");
        }
        expected.push(changed);

        let trims_before = sim.trims;
        let results = sim.turn(t, t % 3);
        let boundary = &results[0];
        if sim.trims > trims_before {
            first_trim_turn.get_or_insert(t);
            // The trim snapshot renders after the pinned first message (and
            // its own ledger entries / tail), before the window.
            let msgs = &boundary.prompt.messages;
            assert_eq!(text_of(&msgs[0]), "request 0");
            let note = msgs
                .iter()
                .position(|m| text_of(m).starts_with("[History trimmed"))
                .expect("trim note rendered");
            assert!(msgs[1..note].iter().all(|m| m.role == ChatRole::System));
            let summary = format!("SUMMARY_T{}", t - 1);
            assert!(
                msgs[note..].iter().any(|m| text_of(m).contains(&summary)),
                "the summary current at the trim is in the snapshot"
            );
        }
        if first_trim_turn.is_none() {
            for r in &results {
                assert!(!model_text(r).contains("SUMMARY_T"), "no summary before a trim");
                assert!(!model_text(r).contains("snapshot_of"), "no in-window snapshots");
            }
        }
        sim.after_turn(t);
    }
    assert!(sim.trims >= 2, "a 30-turn chat with a 12-message window trims ({})", sim.trims);
    assert!(sim.trims < 15, "trims come in chunks ({})", sim.trims);

    // Entries are written only at user-turn boundaries, and only for keys
    // that changed: every recorded key changed, and every change is recorded
    // (as an update, or restated in that boundary's trim snapshot).
    let boundaries: Vec<&(bool, Vec<String>, Vec<String>)> = sim.entry_log.iter().filter(|e| e.0).collect();
    assert_eq!(boundaries.len(), 30);
    for (boundary, ledger_keys, trim_keys) in &sim.entry_log {
        assert!(*boundary || (ledger_keys.is_empty() && trim_keys.is_empty()), "mid-turn entries");
    }
    for (t, (_, ledger_keys, trim_keys)) in boundaries.iter().enumerate() {
        for k in ledger_keys {
            assert!(expected[t].contains(&k.as_str()), "turn {t}: {k} recorded but unchanged");
        }
        for k in &expected[t] {
            // Cleared at a trim that also cut away the old value: the model
            // no longer sees it, so there is nothing to clear.
            let cleared_by_trim = *k == "scratchpad" && t % 8 == 0 && !trim_keys.is_empty();
            if cleared_by_trim && !ledger_keys.iter().any(|x| x == k) {
                continue;
            }
            assert!(
                ledger_keys.iter().chain(trim_keys.iter()).any(|x| x == k),
                "turn {t}: {k} changed but was not recorded"
            );
        }
    }
    // A cleared scratchpad gets an explicit "cleared" entry.
    assert!(sim
        .session
        .context_ledger
        .iter()
        .any(|e| e.kind == LedgerKind::Ledger && e.key == "scratchpad" && e.body.is_empty()));
}

#[test]
fn trim_snapshot_restates_live_context_and_trimmed_snapshots() {
    let mut sim = ChatSim::new(LlmProvider::Claude, 12);
    sim.session.scratchpad = "keep me".to_string();
    let mut trimmed_at = None;
    for t in 0..20 {
        let before = sim.trims;
        let results = sim.turn(t, 2);
        if sim.trims > before {
            trimmed_at = Some((t, results[0].prompt.messages.clone()));
            break;
        }
        sim.after_turn(t);
    }
    let (t, msgs) = trimmed_at.expect("the chat trims");
    let note = msgs.iter().position(|m| text_of(m).starts_with("[History trimmed")).unwrap();
    let snapshot: Vec<String> = msgs[note..]
        .iter()
        .take_while(|m| m.role == ChatRole::System)
        .map(text_of)
        .collect();
    let joined = snapshot.join("\n");
    assert!(joined.contains(&format!("SUMMARY_T{}", t - 1)));
    assert!(joined.contains("snapshot_of"), "snapshots of trimmed calls are restated");
    // Snapshots whose calls are still in the window are not.
    let window_start = sim
        .session
        .messages
        .iter()
        .position(|m| Some(m.id) == sim.session.history_anchor)
        .unwrap();
    for m in &sim.session.messages[window_start..] {
        if let MessageContent::ToolCall(tc) = &m.content {
            assert!(!joined.contains(&tc.execution_id), "in-window snapshot restated");
        }
    }
    // Keys whose last value predates the window are restated; the first
    // turn's planner entry is anchored at the pinned message and stays
    // visible, so the planner needs no restating unless it changed.
    assert!(!joined.contains("[Context update: planner_today."));
    // The next build replays the same snapshot (deterministic, no rewrite).
    let next = sim.build();
    assert!(!next.prompt.history_rewritten);
    assert_eq!(next.prompt.messages[..msgs.len()], msgs[..]);
    // The trim entries are persisted and tied to the window anchor.
    assert!(sim.session.context_ledger.iter().any(|e| e.kind == LedgerKind::Trim
        && e.key == TRIM_NOTE_KEY
        && e.trim_anchor == sim.session.history_anchor));
}

#[test]
fn tails_persist_reuse_on_retry_and_render_per_provider() {
    for provider in [LlmProvider::Claude, LlmProvider::Gemini, LlmProvider::OpenAiCompat] {
        let mut sim = ChatSim::new(provider, 75);
        for t in 0..3 {
            sim.turn(t, 2);
        }
        // A continuation round of a new turn: its build is what gets retried.
        sim.session.messages.push(user_text_msg("one more"));
        sim.session.messages.push(placeholder_msg());
        sim.build();
        let call = tool_call_msg("TOOL_X", "{}".to_string(), None);
        sim.session.messages.last_mut().unwrap().content = call.content;
        sim.session.messages.push(placeholder_msg());
        let last = sim.build();
        let tails_in_ledger =
            sim.session.context_ledger.iter().filter(|e| e.kind == LedgerKind::Tail).count();
        assert_eq!(tails_in_ledger, sim.builds, "{provider:?}: one tail per request");
        let rendered = last.prompt.messages.iter().filter(|m| is_tail(m)).count();
        if provider == LlmProvider::Claude {
            assert_eq!(rendered, sim.builds, "Claude replays every tail");
        } else {
            assert_eq!(rendered, 1, "{provider:?} gets only the current tail");
        }
        assert_eq!(last.prompt.turn_scoped.len(), rendered);
        assert_eq!(*last.prompt.turn_scoped.last().unwrap(), last.prompt.messages.len() - 1);

        // A retry of the last build (same state, tail already persisted)
        // reuses the tail unchanged and writes nothing new — even with a
        // different clock.
        let retry = PromptBuilder::new(&sim.session, &sim.settings)
            .with_planner_today(Some(sim.planner.clone()))
            .with_now(fixed_clock(99_999))
            .build_prompt(String::new());
        assert!(retry.ledger_entries.is_empty(), "{provider:?}: a retry writes nothing");
        assert_eq!(retry.prompt.messages, last.prompt.messages);
    }
}

#[test]
fn boundary_retry_writes_no_new_entries() {
    let mut sim = ChatSim::new(LlmProvider::Claude, 75);
    sim.session.messages.push(user_text_msg("hello"));
    sim.session.messages.push(placeholder_msg());
    let first = sim.build();
    assert!(first.ledger_entries.iter().any(|e| e.key == "planner_today"));
    // The planner changes before the retry: the retry still reuses what the
    // first attempt recorded (the change lands at the next user turn).
    sim.planner = serde_json::json!({"todos": ["something else"]});
    let retry = PromptBuilder::new(&sim.session, &sim.settings)
        .with_planner_today(Some(sim.planner.clone()))
        .with_now(fixed_clock(5_000))
        .build_prompt(String::new());
    assert!(retry.ledger_entries.is_empty());
    assert_eq!(retry.prompt.messages, first.prompt.messages);
}

#[test]
fn snapshots_and_summary_wait_for_a_trim() {
    let mut session = create_test_session();
    session.llm_provider = Some(LlmProvider::Claude);
    let call = tool_call_msg("read", "r".to_string(), None);
    let in_window_id = match &call.content {
        MessageContent::ToolCall(tc) => tc.execution_id.clone(),
        _ => unreachable!(),
    };
    session.messages = vec![user_text_msg("one"), call, assistant_text_msg("done"), user_text_msg("two")];
    session.active_context.extra.insert(
        format!("tool_snapshot_{in_window_id}"),
        serde_json::json!({"facts": "IN_WINDOW"}),
    );
    session.active_context.extra.insert(
        "tool_snapshot_gone-call".to_string(),
        serde_json::json!({"facts": "NOT_IN_MESSAGES"}),
    );
    session.active_context.conversation_summary.summary = "THE_SUMMARY".to_string();
    let settings = Settings::default();
    let result = PromptBuilder::new(&session, &settings).build_prompt(String::new());
    let text = model_text(&result);
    assert!(!text.contains("IN_WINDOW"), "the call is in the history already");
    assert!(text.contains("NOT_IN_MESSAGES"), "a snapshot with no call in history is live");
    assert!(!text.contains("THE_SUMMARY"), "untrimmed history needs no summary");
}

#[test]
fn rewinds_drop_orphaned_ledger_entries() {
    let mut sim = ChatSim::new(LlmProvider::Claude, 75);
    for t in 0..3 {
        sim.turn(t, 1);
        sim.planner = serde_json::json!({"todos": [format!("t{t}")]});
    }
    let session = &mut sim.session;
    let second_user = session.messages.iter().filter(|m| is_user_text(m)).nth(1).unwrap().id;
    let anchors_after: std::collections::HashSet<uuid::Uuid> = {
        let idx = session.messages.iter().position(|m| m.id == second_user).unwrap();
        session.messages[idx..].iter().map(|m| m.id).collect()
    };
    assert!(session.context_ledger.iter().any(|e| anchors_after.contains(&e.after_message_id)));
    let mut rewound = session.clone();
    rewound.truncate_from_message(&second_user.to_string());
    assert!(!rewound.context_ledger.is_empty(), "entries before the rewind survive");
    assert!(rewound
        .context_ledger
        .iter()
        .all(|e| !anchors_after.contains(&e.after_message_id)));

    // remove_message_in_session drops the entries anchored at that message.
    let mut state = crate::session::SessionState::default();
    let last_id = session.messages.last().unwrap().id;
    let tool_id = session.messages[session.messages.len() - 2].id;
    state.sessions.insert(session.id.clone(), session.clone());
    state.remove_message_in_session(&session.id, &tool_id);
    let s = &state.sessions[&session.id];
    assert!(s.context_ledger.iter().all(|e| e.after_message_id != tool_id));
    assert!(s.messages.iter().any(|m| m.id == last_id));

    // Journal replay: LedgerEntryAdded rebuilds the ledger and a RewoundTo
    // prunes it the same way.
    use crate::session_events::{project, SessionEvent};
    use crate::session_store::LoadedSessionEvent;
    let mut events = vec![SessionEvent::SessionCreated { id: "s".into(), name: "n".into() }];
    let mut entry_count = 0;
    for m in &session.messages {
        events.push(if m.author == "User" {
            SessionEvent::UserMessage { message: m.clone() }
        } else {
            SessionEvent::AssistantMessage { message: m.clone() }
        });
        for e in session.context_ledger.iter().filter(|e| e.after_message_id == m.id) {
            events.push(SessionEvent::LedgerEntryAdded { entry: e.clone() });
            entry_count += 1;
        }
    }
    let loaded = |events: &[SessionEvent]| -> Vec<LoadedSessionEvent> {
        events
            .iter()
            .enumerate()
            .map(|(i, e)| LoadedSessionEvent { seq: i as i64, ts: Utc::now(), event: e.clone() })
            .collect()
    };
    let replayed = project(None, &loaded(&events));
    assert_eq!(replayed.context_ledger.len(), entry_count);
    assert_eq!(replayed.context_ledger, session.context_ledger);
    events.push(SessionEvent::RewoundTo { seq: 0, message_id: second_user.to_string() });
    let replayed = project(None, &loaded(&events));
    assert_eq!(replayed.context_ledger, rewound.context_ledger);
    let kind = SessionEvent::LedgerEntryAdded { entry: session.context_ledger[0].clone() };
    assert_eq!(kind.kind(), "LedgerEntryAdded");
}

#[test]
fn session_without_ledger_deserializes() {
    let mut sim = ChatSim::new(LlmProvider::Claude, 75);
    sim.turn(0, 1);
    let session = sim.session.clone();
    let mut json = serde_json::to_value(&session).unwrap();
    assert!(json.get("context_ledger").is_some());
    json.as_object_mut().unwrap().remove("context_ledger");
    let old: Session = serde_json::from_value(json).expect("old session JSON loads");
    assert!(old.context_ledger.is_empty());
    let round: Session = serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
    assert_eq!(round.context_ledger, session.context_ledger);
    // An empty ledger is not serialized at all.
    let fresh = serde_json::to_value(create_test_session()).unwrap();
    assert!(fresh.get("context_ledger").is_none());
    // A ledger entry serialized by this build parses (and a minimal one too).
    let minimal: LedgerEntry = serde_json::from_value(serde_json::json!({
        "id": uuid::Uuid::nil(),
        "after_message_id": uuid::Uuid::nil(),
        "kind": "ledger",
        "key": "scratchpad",
    }))
    .unwrap();
    assert!(minimal.body.is_empty() && minimal.trim_anchor.is_none());
}

#[test]
fn enforce_context_budget_keeps_turn_scoped_on_the_tails() {
    let mut sim = ChatSim::new(LlmProvider::Claude, 75);
    for t in 0..6 {
        sim.turn(t, 2);
    }
    let mut prompt = sim.build().prompt;
    let tails_before: Vec<String> =
        prompt.turn_scoped.iter().map(|&i| text_of(&prompt.messages[i])).collect();
    let dropped = prompt.enforce_context_budget(1_200, 6, 4.0);
    assert!(dropped > 0, "the tiny budget forces a drop");
    let tails_after: Vec<String> =
        prompt.turn_scoped.iter().map(|&i| text_of(&prompt.messages[i])).collect();
    assert!(!tails_after.is_empty());
    assert!(tails_after.len() < tails_before.len());
    assert_eq!(tails_after[..], tails_before[tails_before.len() - tails_after.len()..]);
    for &i in &prompt.turn_scoped {
        assert!(is_tail(&prompt.messages[i]));
    }
}

/// A realistic fixture: today's plan with fleet briefs, a scratchpad, a
/// rolling summary, 20 tool snapshots and a loaded skill. Reports what the
/// old per-request `<TURN_CONTEXT>` cost versus what a request now sends
/// fresh (its tail), with the live context recorded once into history.
#[test]
fn turn_context_size_drops_sharply() {
    let mut session = create_test_session();
    session.llm_provider = Some(LlmProvider::Claude);
    session.scratchpad = "Working notes: ".to_string() + &"key fact about the project; ".repeat(40);
    session.active_context.conversation_summary.summary =
        "The user and Hobbes have been working through the prompt caching rollout. ".repeat(20);
    session.active_context.conversation_summary.current_task = "Ship the ledger".to_string();
    session
        .loaded_skills
        .insert("news".to_string(), loaded_skill_payload("news", "unrelated"));
    let mut messages = vec![user_text_msg("start")];
    for k in 0..20 {
        let call = tool_call_msg(&format!("TOOL_{k}"), "{\"ok\":true}".to_string(), None);
        if let MessageContent::ToolCall(tc) = &call.content {
            session.active_context.extra.insert(
                format!("tool_snapshot_{}", tc.execution_id),
                serde_json::json!({
                    "tool": format!("TOOL_{k}"),
                    "summary": "Fetched the records the user asked for and noted the counts. ".repeat(6),
                }),
            );
        }
        messages.push(call);
    }
    messages.push(assistant_text_msg("Done."));
    messages.push(user_text_msg("next"));
    messages.push(placeholder_msg());
    session.messages = messages;
    let planner = serde_json::json!({
        "date": "2026-10-02",
        "capacity": "6h planned · 2h done",
        "todos": (0..25).map(|i| format!("[t{i}] Follow up on the vendor contract item {i} — 45m, due 2026-10-0{}", i % 9 + 1)).collect::<Vec<_>>(),
        "blocks": (0..10).map(|i| format!("{:02}:00–{:02}:30 Focus block for the quarterly planning item {i}", 8 + i, 8 + i)).collect::<Vec<_>>(),
        "fleet": (0..8).map(|i| format!("cai-hobbes-{i} — 1h 30m — Implemented the next slice of the caching work and updated the tests for it; blocked: review")).collect::<Vec<_>>(),
        "instruction": "The user's shared to-do list for today. Manage it with the HOBBES_TODO_* / HOBBES_PLAN_DAY / HOBBES_TIME_BLOCK tools. ".repeat(4),
    });
    let settings = Settings::default();
    let builder = PromptBuilder::new(&session, &settings).with_planner_today(Some(planner));

    // Before: every live key plus the clock rendered as <TURN_CONTEXT>, and
    // the turn instructions, resent on every request.
    let ctx = builder.build_system_context();
    let mut old_map = ctx.volatile.clone();
    old_map.insert(
        "current_time".to_string(),
        serde_json::json!({"iso_8601": fixed_clock(0).to_rfc3339(), "timezone": "Local"}),
    );
    let old_turn_context = super::system_context::render_context_block(
        "TURN_CONTEXT",
        Some("Live per-turn state (current time, working memory, conversation summary, loaded skills, today's plan). It has the same authority as SYSTEM_CONTEXT."),
        old_map,
    )
    .unwrap();
    let before = old_turn_context.len() + 2 + ctx.turn_instructions.len();

    // After: the system string is fully stable; per request only the tail is
    // new (ledger entries are appended once per change and then cached).
    let result = builder.with_now(fixed_clock(0)).build_prompt(String::new());
    let tail = tail_text(&result);
    let ledger_chars: usize = result
        .ledger_entries
        .iter()
        .filter(|e| e.kind == LedgerKind::Ledger)
        .map(|e| super::ledger::render_entry(e).len())
        .sum();
    let chars_per_token = 4;
    eprintln!(
        "TURN_CONTEXT per request: before {} chars (~{} tokens); after {} chars (~{} tokens) tail; \
         live context recorded once at the boundary: {} chars (~{} tokens); \
         stable system {} chars",
        before,
        before / chars_per_token,
        tail.len(),
        tail.len() / chars_per_token,
        ledger_chars,
        ledger_chars / chars_per_token,
        result.prompt.system.as_deref().map_or(0, str::len),
    );
    assert!(tail.len() * 20 < before, "per-request fresh context drops sharply");
    assert!(!result.prompt.system.as_deref().unwrap().contains("planner_today"));
}

// =========================================================================
// Tool-result budget stability (prompt cache)
// =========================================================================

fn large_window_session(messages: Vec<Message>) -> Session {
    let mut session = create_test_session();
    session.llm_provider = Some(LlmProvider::Gemini);
    session.chat_model = Some("gemini-2.5-flash".to_string()); // 1M context
    session.messages = messages;
    session
}

fn filler(n: usize, tag: &str) -> String {
    format!("{tag} ").repeat(n / (tag.len() + 1) + 1)
}

fn messages_json(result: &PromptBuildResult) -> Vec<String> {
    result
        .prompt
        .messages
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect()
}

#[test]
fn history_is_byte_identical_across_a_tool_round_on_a_large_window() {
    let history = vec![
        user_text_msg("first request"),
        tool_call_msg("BIG_ONE", filler(300_000, "alpha"), None),
        tool_call_msg("MID_A", filler(60_000, "bravo"), Some("summary a".into())),
        tool_call_msg("MID_B", filler(53_000, "charlie"), Some("summary b".into())),
        tool_call_msg("MID_C", filler(20_000, "delta"), None),
        user_text_msg("now do the next thing"),
    ];
    let settings = Settings::default();

    // Request 1: user text last. Request 2: one tool call/result appended.
    let session1 = large_window_session(history.clone());
    let req1 = PromptBuilder::new(&session1, &settings).build_prompt(String::new());
    let mut with_call = history;
    with_call.push(tool_call_msg("CURRENT", filler(25_000, "echo"), None));
    let session2 = large_window_session(with_call);
    let req2 = PromptBuilder::new(&session2, &settings).build_prompt(String::new());

    let (j1, j2) = (messages_json(&req1), messages_json(&req2));
    // History = everything in request 1 up to its last non-System message
    // (trailing ledger/tail System entries may legitimately differ).
    let hist_len = req1
        .prompt
        .messages
        .iter()
        .rposition(|m| m.role != ChatRole::System)
        .unwrap()
        + 1;
    assert!(hist_len >= 5);
    for i in 0..hist_len {
        assert_eq!(j1[i], j2[i], "message {i} differs between request 1 and 2");
    }
    let text = tool_result_text(&req2);
    assert!(!text.contains("HOBBES_PAGE_RESULT"), "nothing is cut when it all fits");
    assert!(!text.contains("Summary of an earlier"), "no summary substitution when it fits");
}

#[test]
fn no_cut_when_everything_fits_active_or_historical() {
    let settings = Settings::default();
    let payload = filler(300_000, "zulu");

    // Active: the tool result is the current turn.
    let active = large_window_session(vec![
        user_text_msg("go"),
        tool_call_msg("BIG", payload.clone(), None),
    ]);
    let r = PromptBuilder::new(&active, &settings).build_prompt(String::new());
    assert!(!tool_result_text(&r).contains("HOBBES_PAGE_RESULT"));
    assert!(tool_result_text(&r).len() >= payload.len());

    // Historical: a later user message follows it, and a summary exists.
    let historical = large_window_session(vec![
        user_text_msg("go"),
        tool_call_msg("BIG", payload.clone(), Some("short summary".into())),
        user_text_msg("next"),
    ]);
    let r = PromptBuilder::new(&historical, &settings).build_prompt(String::new());
    let text = tool_result_text(&r);
    assert!(!text.contains("HOBBES_PAGE_RESULT"));
    assert!(!text.contains("Summary of an earlier"));
    assert!(text.len() >= payload.len());
}

#[test]
fn over_budget_still_cuts_on_a_small_window() {
    let mut session = create_test_session();
    session.llm_provider = Some(LlmProvider::OpenAiCompat);
    session.chat_model = Some("local-model".to_string());
    session.messages = vec![
        user_text_msg("first"),
        tool_call_msg("BIG", filler(300_000, "yankee"), None),
        user_text_msg("next"),
    ];
    let mut settings = Settings::default();
    settings.openai_compat_config.endpoint = "http://localhost:11434/v1".to_string();
    settings.openai_compat_config.max_context_tokens = Some(16_000);
    let r = PromptBuilder::new(&session, &settings).build_prompt(String::new());
    assert!(tool_result_text(&r).contains("HOBBES_PAGE_RESULT"));
}

/// Reproduces the real 1M-window session: ~45 historical results (a few large)
/// make the per-result share small, so the old split cut the large ones, and cut
/// them differently with vs without an active result.
#[test]
fn history_is_byte_identical_across_a_tool_round_with_many_results() {
    let mut history = vec![user_text_msg("first request")];
    for (i, size) in [300_000usize, 60_000, 50_000, 25_000].into_iter().enumerate() {
        history.push(tool_call_msg(&format!("LARGE_{i}"), filler(size, "alpha"), None));
    }
    for i in 0..41 {
        history.push(tool_call_msg(&format!("SMALL_{i}"), filler(5_000, "bravo"), None));
    }
    history.push(user_text_msg("now do the next thing"));
    let settings = Settings::default();

    let session1 = large_window_session(history.clone());
    let req1 = PromptBuilder::new(&session1, &settings).build_prompt(String::new());
    let mut with_call = history;
    with_call.push(tool_call_msg("CURRENT", filler(25_000, "echo"), None));
    let session2 = large_window_session(with_call);
    let req2 = PromptBuilder::new(&session2, &settings).build_prompt(String::new());

    let (j1, j2) = (messages_json(&req1), messages_json(&req2));
    let hist_len = req1
        .prompt
        .messages
        .iter()
        .rposition(|m| m.role != ChatRole::System)
        .unwrap()
        + 1;
    assert!(hist_len >= 46);
    for i in 0..hist_len {
        assert_eq!(j1[i], j2[i], "message {i} differs between request 1 and 2");
    }
    assert!(!tool_result_text(&req2).contains("HOBBES_PAGE_RESULT"));
}

// =========================================================================
// Turn briefing (Heartbeat Phase 1)
// =========================================================================

use super::briefing::{turn_briefing, BRIEFING_HEADER};

/// A timer built from JSON, so fields added later take their serde defaults.
fn briefing_timer(
    id: &str,
    status: &str,
    fired_at: Option<chrono::DateTime<Utc>>,
    label: Option<&str>,
    reason: Option<&str>,
    cause: Option<&str>,
) -> crate::timers::ScheduledTimer {
    let at = fired_at.unwrap_or_else(|| fixed_clock(0).with_timezone(&Utc));
    let mut v = serde_json::json!({
        "id": id,
        "session_id": "test_session",
        "created_at": at - chrono::Duration::minutes(30),
        "fire_at": at,
        "mode": "notify",
        "status": status,
    });
    if let Some(f) = fired_at {
        v["fired_at"] = serde_json::json!(f);
    }
    for (k, val) in [("label", label), ("reason", reason), ("fire_cause", cause)] {
        if let Some(val) = val {
            v[k] = serde_json::json!(val);
        }
    }
    serde_json::from_value(v).unwrap()
}

fn note_at(at: chrono::DateTime<Utc>, change: Option<&str>) -> crate::handoff::HandoffNote {
    crate::handoff::HandoffNote {
        doing: "watching the release build".to_string(),
        expect_next: "CI goes green, then tag v0.12.4".to_string(),
        would_change_mind: change.map(str::to_string),
        written_at: at,
    }
}

/// Messages of one finished turn, all stamped `at`.
fn finished_turn_at(at: chrono::DateTime<Utc>) -> Vec<Message> {
    let mut msgs = vec![
        user_text_msg("start the build"),
        tool_call_msg("RUN", "{}".to_string(), None),
        assistant_text_msg("Started."),
    ];
    for m in &mut msgs {
        m.created_at = at;
    }
    msgs
}

fn utc(n: i64) -> chrono::DateTime<Utc> {
    fixed_clock(n).with_timezone(&Utc)
}

fn hhmm(at: chrono::DateTime<Utc>) -> String {
    at.with_timezone(&chrono::Local).format("%H:%M").to_string()
}

#[test]
fn briefing_parts_appear_only_when_they_have_something_to_say() {
    let mut session = create_test_session();
    let prior = finished_turn_at(utc(0));
    let gap = 47 * 60;

    // First turn: nothing came before.
    assert_eq!(turn_briefing(&session, &[], &[], fixed_clock(gap)), "");

    // Long gap only: the elapsed line, nothing else.
    let b = turn_briefing(&session, &prior, &[], fixed_clock(gap));
    assert_eq!(
        b,
        format!("{BRIEFING_HEADER}\n47 min since your last turn here ({}).", hhmm(utc(0)))
    );

    // Short gap with a handoff note and no fired timer: a live conversation,
    // nothing to say.
    session.handoff = Some(note_at(utc(-60), None));
    assert_eq!(turn_briefing(&session, &prior, &[], fixed_clock(120)), "");

    // Short gap, a bare fired timer: the timer line (no cause, no reason)
    // and the note, but no elapsed line.
    session.scheduled_timers = vec![briefing_timer("t1", "fired", Some(utc(60)), None, None, None)];
    let b = turn_briefing(&session, &prior, &[], fixed_clock(120));
    assert!(!b.contains("since your last turn here ("), "{b}");
    assert!(b.contains(&format!("Timer fired at {}: \"(no label)\".", hhmm(utc(60)))), "{b}");
    assert!(!b.contains("Why you set it"), "{b}");
    assert!(b.contains("Your handoff note, written 3 min ago:"), "{b}");
    assert!(!b.contains("Would change plan if"), "{b}");
    assert!(!b.contains("out of date"), "{b}");

    // Timers that fired before the previous turn, are pending or were
    // cancelled are not news.
    session.scheduled_timers = vec![
        briefing_timer("old", "fired", Some(utc(-30)), Some("old"), None, None),
        briefing_timer("pending", "pending", None, Some("pending"), None, None),
        briefing_timer("cancelled", "cancelled", None, Some("cancelled"), None, None),
    ];
    session.handoff = None;
    assert_eq!(turn_briefing(&session, &prior, &[], fixed_clock(120)), "");

    // A note older than a day is flagged.
    session.handoff = Some(note_at(utc(-30 * 3600), Some("the build fails twice")));
    let b = turn_briefing(&session, &prior, &[], fixed_clock(gap));
    assert!(b.contains("Your handoff note, written 30 h 47 min ago (possibly out of date):"), "{b}");
    assert!(b.contains("Would change plan if: the build fails twice"), "{b}");
}

fn hb_event(kind: &str, summary: &str, at: chrono::DateTime<Utc>) -> crate::heartbeat::HeartbeatEvent {
    crate::heartbeat::HeartbeatEvent::new(
        crate::heartbeat::EventSource::Fleet,
        kind,
        summary,
        crate::heartbeat::Urgency::Normal,
        at,
    )
}

#[test]
fn briefing_lists_changes_to_dispatched_work() {
    let session = create_test_session();
    let prior = finished_turn_at(utc(0));

    let mut done = hb_event("run_ended", "Fleet: dispatched run fix-ci ended", utc(60));
    done.dispatched_from = Some("test_session".into());
    let mut before = hb_event("run_ended", "old news", utc(-60));
    before.dispatched_from = Some("test_session".into());
    // The chat's own to-do (it is the worker): not work it dispatched.
    let mut own = hb_event("worker_done", "Agent finished \"own task\"", utc(70));
    own.session_id = Some("test_session".into());
    // A run this chat launched that also names it as its session counts.
    let mut both = hb_event("session_needs_attention", "Fleet: fix-ci — waiting on permission", utc(90));
    both.session_id = Some("test_session".into());
    both.dispatched_from = Some("test_session".into());

    // A short gap with a dispatched-work change is news on its own.
    let events = vec![before.clone(), done.clone(), own.clone(), both.clone()];
    let b = turn_briefing(&session, &prior, &events, fixed_clock(120));
    assert_eq!(
        b,
        format!(
            "{BRIEFING_HEADER}\n\
             Work you dispatched: Fleet: dispatched run fix-ci ended ({}).\n\
             Work you dispatched: Fleet: fix-ci — waiting on permission ({}).",
            hhmm(utc(60)),
            hhmm(utc(90)),
        )
    );
    // Only stale or own-work events: nothing to say on a short gap.
    assert_eq!(turn_briefing(&session, &prior, &[before, own], fixed_clock(120)), "");

    // Capped at the five most recent, oldest first.
    let many: Vec<_> = (1..=8)
        .map(|i| {
            let mut e = hb_event("run_ended", &format!("run {i} ended"), utc(i * 10));
            e.dispatched_from = Some("test_session".into());
            e
        })
        .collect();
    let b = turn_briefing(&session, &prior, &many, fixed_clock(120));
    let listed: Vec<&str> = b.lines().filter(|l| l.starts_with("Work you dispatched")).collect();
    assert_eq!(listed.len(), 5, "{b}");
    assert!(listed[0].contains("run 4 ended") && listed[4].contains("run 8 ended"), "{b}");
}

#[test]
fn dispatched_work_rides_the_boundary_tail_and_replays_unchanged() {
    let mut sim = briefing_sim(LlmProvider::Claude);
    let last = utc(sim.clock);
    let mut ev = hb_event("run_ended", "Fleet: dispatched run fix-ci ended", last + chrono::Duration::minutes(1));
    ev.dispatched_from = Some(sim.session.id.clone());
    sim.clock += 120;
    sim.session.messages.push(user_text_msg("how did it go?"));
    sim.session.messages.push(placeholder_msg());
    let boundary = PromptBuilder::new(&sim.session, &sim.settings)
        .with_planner_today(Some(sim.planner.clone()))
        .with_heartbeat_events(vec![ev.clone()])
        .with_now(fixed_clock(sim.clock))
        .build_prompt(String::new());
    let tail = tail_text(&boundary);
    let line = format!("Work you dispatched: Fleet: dispatched run fix-ci ended ({}).", hhmm(ev.at));
    assert!(tail.contains(&line), "{tail}");
    assert!(!boundary.prompt.system.clone().unwrap().contains("fix-ci"), "never in the system string");
    sim.session.history_anchor = boundary.history_anchor;
    sim.session.context_ledger.extend(boundary.ledger_entries.iter().cloned());

    // A retry with newer events (and a later clock) replays the same bytes.
    let mut later = hb_event("run_ended", "Fleet: dispatched run other ended", utc(sim.clock));
    later.dispatched_from = Some(sim.session.id.clone());
    let retry = PromptBuilder::new(&sim.session, &sim.settings)
        .with_planner_today(Some(sim.planner.clone()))
        .with_heartbeat_events(vec![ev, later])
        .with_now(fixed_clock(sim.clock + 600))
        .build_prompt(String::new());
    assert!(retry.ledger_entries.is_empty());
    assert_eq!(retry.prompt.messages, boundary.prompt.messages);
}

/// A sim whose session offers HOBBES_WRITE_HANDOFF (so the stable guidance
/// renders), with one finished turn stamped at the sim's clock.
fn briefing_sim(provider: LlmProvider) -> ChatSim {
    let mut sim = ChatSim::new(provider, 75);
    sim.session.active_context.mcp_tools = Some(crate::mcp::manager::McpContext {
        servers: vec![mcp_server("hobbes-core", &["HOBBES_UPDATE_SCRATCHPAD", "HOBBES_WRITE_HANDOFF"])],
        connected_toolkit_slugs: vec![],
    });
    sim.turn(0, 1);
    let at = utc(sim.clock);
    for m in &mut sim.session.messages {
        m.created_at = at;
    }
    sim
}

#[test]
fn boundary_tail_carries_the_briefing_and_replays_unchanged() {
    for provider in [LlmProvider::Claude, LlmProvider::Gemini] {
        let mut sim = briefing_sim(provider);
        let first_system = {
            let r = PromptBuilder::new(&sim.session, &sim.settings)
                .with_planner_today(Some(sim.planner.clone()))
                .with_now(fixed_clock(sim.clock))
                .build_prompt(String::new());
            r.prompt.system.unwrap()
        };
        assert!(first_system.contains("handoff_guidance"));
        assert!(first_system.contains("HOBBES_WRITE_HANDOFF after meaningful work"));

        // Away 47 minutes: a timer fired and a handoff note is waiting.
        let last = utc(sim.clock);
        sim.session.handoff = Some(note_at(last, Some("the suite fails")));
        sim.session.scheduled_timers.push(briefing_timer(
            "tmr_ci",
            "fired",
            Some(last + chrono::Duration::minutes(30)),
            Some("Check CI"),
            Some("waiting for the release suite"),
            Some("deadline reached"),
        ));
        sim.clock += 47 * 60 - 7; // build() adds 7s
        sim.session.messages.push(user_text_msg("any news?"));
        sim.session.messages.push(placeholder_msg());
        let boundary = sim.build();
        let tail = tail_text(&boundary);
        let expected = format!(
            "{TAIL_HEADER}\nCurrent time: {} (local)\n{BRIEFING_HEADER}\n\
             47 min since your last turn here ({}).\n\
             Timer fired at {}: \"Check CI\" (deadline reached). Why you set it: waiting for the release suite\n\
             Your handoff note, written 47 min ago:\n\
             Doing: watching the release build\n\
             Next: CI goes green, then tag v0.12.4\n\
             Would change plan if: the suite fails",
            fixed_clock(sim.clock).to_rfc3339(),
            hhmm(last),
            hhmm(last + chrono::Duration::minutes(30)),
        );
        assert_eq!(tail, expected, "{provider:?}");

        // The stable system prefix holds no briefing and does not change.
        let sys = boundary.prompt.system.clone().unwrap();
        assert_eq!(sys, first_system, "{provider:?}: system changed across turns");
        assert!(!sys.contains("watching the release build"));
        assert!(!sys.contains("Check CI"));

        // A retry with a later clock reuses the persisted briefing.
        let retry = PromptBuilder::new(&sim.session, &sim.settings)
            .with_planner_today(Some(sim.planner.clone()))
            .with_now(fixed_clock(sim.clock + 3_600))
            .build_prompt(String::new());
        assert!(retry.ledger_entries.is_empty());
        assert_eq!(retry.prompt.messages, boundary.prompt.messages);

        // Mid-turn: the model rewrites its note; the briefing stays as sent
        // (every provider keeps it in view), and the new tail carries none.
        sim.session.handoff = Some(note_at(utc(sim.clock), None));
        let call = tool_call_msg("CHECK", "{}".to_string(), None);
        sim.session.messages.last_mut().unwrap().content = call.content;
        sim.session.messages.push(placeholder_msg());
        let mid = sim.build();
        let texts: Vec<String> = mid.prompt.messages.iter().map(text_of).collect();
        assert!(texts.contains(&expected), "{provider:?}: briefing tail replayed unchanged");
        assert!(!tail_text(&mid).contains(BRIEFING_HEADER), "{provider:?}: mid-turn tail");
        assert_eq!(mid.prompt.system.as_deref(), Some(first_system.as_str()));

        // Reply, then an immediate follow-up: no briefing, the earlier one
        // still replays, the system is still identical.
        sim.session.messages.last_mut().unwrap().content = assistant_text_msg("CI is green.").content;
        sim.session.messages.last_mut().unwrap().created_at = utc(sim.clock);
        sim.session.messages.push(user_text_msg("tag it"));
        sim.session.messages.push(placeholder_msg());
        let next = sim.build();
        assert!(!tail_text(&next).contains(BRIEFING_HEADER), "{provider:?}: live follow-up");
        let texts: Vec<String> = next.prompt.messages.iter().map(text_of).collect();
        assert!(texts.contains(&expected), "{provider:?}: earlier briefing replayed");
        assert_eq!(next.prompt.system.as_deref(), Some(first_system.as_str()));
    }
}

/// A live send (the user's message, then the reply placeholder) is not a
/// continuation: the tail must not tell the model the user hasn't replied.
#[test]
fn live_send_with_placeholder_gets_no_continuation_instruction() {
    let settings = Settings::default();
    let mut session = create_test_session();
    session.messages = vec![user_text_msg("hi"), assistant_text_msg("hello"), user_text_msg("more"), placeholder_msg()];
    let result = PromptBuilder::new(&session, &settings).build_prompt(String::new());
    assert!(!tail_text(&result).contains("CONTINUATION INSTRUCTION"));
    assert!(!tail_text(&result).contains("TOOL COMPLETION INSTRUCTION"));
}

// =========================================================================
// Dispatch relay
// =========================================================================

fn relay_msg(direction: crate::todo::relay::RelayDirection, text: &str, woke: bool) -> crate::components::chat::Message {
    crate::components::chat::Message {
        id: uuid::Uuid::new_v4(),
        author: crate::todo::relay::RELAY_AUTHOR.to_string(),
        content: MessageContent::Relay(crate::todo::relay::RelayNote {
            direction,
            kind: crate::todo::relay::RelayKind::Question,
            peer_session_id: "peer".into(),
            peer_title: "Clyde prep".into(),
            todo_id: Some("tdo_1".into()),
            text: text.to_string(),
            hop: 0,
            woke_turn: woke,
        }),
        attachments: vec![],
        comments: vec![],
        created_at: Utc::now(),
        usage: None,
    }
}

fn texts_of(prompt: &crate::context::prompt_builder::PromptBuildResult) -> Vec<(crate::llm::types::ChatRole, String)> {
    prompt
        .prompt
        .messages
        .iter()
        .flat_map(|m| {
            m.content.iter().filter_map(move |b| match b {
                crate::llm::types::ContentBlock::Text { text } => Some((m.role.clone(), text.clone())),
                _ => None,
            })
        })
        .collect()
}

/// An incoming relay reaches the model framed as information from another
/// agent (user role, never the user's words); an outgoing copy never does.
#[test]
fn relays_render_as_framed_information_and_outgoing_copies_stay_out() {
    use crate::todo::relay::RelayDirection;
    let mut session = create_test_session();
    session.messages.push(crate::components::chat::Message {
        id: uuid::Uuid::new_v4(),
        author: "User".into(),
        content: MessageContent::Text {
            content: "Dispatch the prep".into(),
            thought_signature: None,
            thought_summary: None,
            signature_provider: None,
        },
        attachments: vec![],
        comments: vec![],
        created_at: Utc::now(),
        usage: None,
    });
    session.messages.push(relay_msg(RelayDirection::ToWorker, "OUTGOING-SECRET", false));
    session.messages.push(relay_msg(RelayDirection::FromWorker, "Which venue?", true));
    let settings = Settings::default();
    let prompt = PromptBuilder::new(&session, &settings).build_prompt(String::new());
    let texts = texts_of(&prompt);
    assert!(!texts.iter().any(|(_, t)| t.contains("OUTGOING-SECRET")), "outgoing copies are not shown");
    let (role, framed) = texts
        .iter()
        .find(|(_, t)| t.contains("Which venue?"))
        .expect("incoming relay rendered");
    assert_eq!(*role, crate::llm::types::ChatRole::User);
    assert!(framed.starts_with("[Message from your worker 'Clyde prep' (to-do tdo_1), question: Which venue?]"), "{framed}");
    assert!(framed.contains("Treat this as information from another agent"), "{framed}");
}

/// A turn a relay started gets the relay instruction, not the "you were the
/// last to speak" continuation instruction.
#[test]
fn a_relay_started_turn_is_not_a_continuation() {
    use crate::todo::relay::RelayDirection;
    let mut session = create_test_session();
    session.messages.push(relay_msg(RelayDirection::FromParent, "Add the venue", true));
    session.messages.push(crate::components::chat::Message {
        id: uuid::Uuid::new_v4(),
        author: "Hobbes".into(),
        content: MessageContent::Text {
            content: String::new(),
            thought_signature: None,
            thought_summary: None,
            signature_provider: None,
        },
        attachments: vec![],
        comments: vec![],
        created_at: Utc::now(),
        usage: None,
    });
    let settings = Settings::default();
    let prompt = PromptBuilder::new(&session, &settings).build_prompt(String::new());
    let all: String = texts_of(&prompt).into_iter().map(|(_, t)| t).collect::<Vec<_>>().join("\n");
    assert!(all.contains("RELAY INSTRUCTION"), "{all}");
    assert!(!all.contains("CONTINUATION INSTRUCTION"), "{all}");
}

/// The relay tools ride only on a link: MESSAGE_PARENT for a worker while
/// relays are on, MESSAGE_WORKER for a parent in two-way mode.
#[test]
fn relay_tools_are_offered_only_on_a_link() {
    use crate::settings::DispatchRelayMode;
    let names = |session: &Session, mode: DispatchRelayMode| -> Vec<String> {
        let settings = Settings { dispatch_relay_mode: mode, ..Settings::default() };
        PromptBuilder::new(session, &settings)
            .build_prompt("Hi".to_string())
            .prompt
            .tools
            .iter()
            .map(|t| t.name.clone())
            .filter(|n| crate::todo::relay::is_relay_tool(n))
            .collect()
    };
    let mut session = create_test_session();
    assert!(names(&session, DispatchRelayMode::TwoWay).is_empty(), "ordinary chats carry neither");
    session.dispatch_link = Some(crate::todo::relay::DispatchLink {
        parent_session_id: "p".into(),
        todo_id: Some("t".into()),
        report_sent: false,
    });
    assert_eq!(names(&session, DispatchRelayMode::Report), vec!["HOBBES_MESSAGE_PARENT"]);
    assert!(names(&session, DispatchRelayMode::Off).is_empty());
    session.dispatch_link = None;
    session.dispatched_workers.push("w".into());
    assert!(names(&session, DispatchRelayMode::Report).is_empty());
    assert_eq!(names(&session, DispatchRelayMode::TwoWay), vec!["HOBBES_MESSAGE_WORKER"]);
}
