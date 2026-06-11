# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Development (hot-reload UI)
dx serve --platform desktop

# Run all tests
cargo test --release

# Run a single test by name
cargo test --release <test_name>

# Lint
cargo clippy --release

# Production build (macOS .app bundle)
dx build --release
# App output: target/dx/Hobbes/release/macos/Hobbes.app

# Full macOS release (build + sign + notarize)
./scripts/build_release.sh

# Full Windows release
./scripts/build_windows.ps1 -Release
```

The build system runs Tailwind CSS at compile time via `build.rs` — `npm ci` must be run once before building, and `node_modules/` must be present.

### CI Workflows

- **CI** (`.github/workflows/ci.yml`) — triggers on push to `main` and `feat/**`: security audit, cargo check (Linux), full build + tests (macOS), unsigned Windows binary. Azure signing is gated to `main` only.
- **Clippy** (`.github/workflows/clippy.yml`) — runs on push to `main`.
- **Release** (`.github/workflows/release.yml`) — triggers on `v*` tags; builds signed macOS DMG and signed Windows installer.

To release: merge to `main`, push a `vX.Y.Z` tag.

---

## Architecture

Hobbes is a **local-first, multi-tab AI assistant** built with Rust + Dioxus 0.6 (desktop). All chat history and settings are stored locally; LLM providers are called directly with user-supplied API keys.

### Core Loop

```
User input → ChatInput (chat_input.rs)
           → ChatCommand Signal → main.rs command handler
           → StreamManager (stream_manager.rs) sends prompt
           → LlmConnector trait (llm/) streams response
           → McpManager (mcp/manager.rs) executes tool calls
           → StreamManager writes results back to SessionState
           → Dioxus Signals re-render ChatWindow / MessageList
```

The `StreamManager` is the central orchestrator. It owns the full turn lifecycle: buffering the LLM stream, executing tool calls via `McpManager`, managing the continuation loop, triggering `ToolCallSummarizer` at turn end, and firing proactive summarization.

### State Model

**`SessionState`** (`src/session.rs`) is the single source of truth for all chat state. It is held in a Dioxus `Signal<SessionState>` and persisted to `sessions.json` on every write.

Key types within a session:
- `Session` — one per tab; contains `messages: Vec<Message>`, `active_context: ActiveContext`, `conversation_summary`, `scratchpad`, and per-session provider/model overrides (`llm_provider`, `chat_model`).
- `ActiveContext` — live in-memory context injected into every prompt: MCP tools, snapshots, skills, scratchpad.
- `ConversationSummary` — background-generated rolling summary used to seed new prompts after history scrolling.

Persistence uses a **serialize-then-move** pattern (P-009): never clone `SessionState` for async saves — serialize to bytes on the calling thread, move only bytes to the background I/O task. Use `SessionState::save_async(&state, ...)` or `SessionState::save_signal(&session_state, ...)`.

### LLM Layer (`src/llm/`)

`LlmConnector` is the trait all providers implement. Three connectors exist:
- `GeminiConnector` — primary provider; supports extended thinking, context caching (`gemini_cache.rs`), and the Baton Pattern for thought signatures.
- `ClaudeConnector` — Anthropic Messages API; supports extended thinking and transient-failure retry.
- `OpenAiCompatConnector` — generic OpenAI-compatible endpoint.

**Per-session provider/model overrides**: `Session.llm_provider` and `Session.chat_model` override the global `Settings.active_llm`. Use `settings.provider_for_session(session)` and `settings.chat_model_for_session(session)` to resolve the effective pair. `build_connector_for(settings, provider, model)` in `llm/mod.rs` constructs a connector for a non-global provider. Gemini connectors use a process-wide `OnceLock` cache store via `GeminiConnector::new_shared()` (tests use `new()` for isolation).

**Settings API**: provider-aware methods always take an explicit `LlmProvider` argument — `resolve_context_window_for(provider, model)`, `effective_context_tuning_for(provider)`, `model_slots_for(provider)`, `set_chat_model_for(provider, model)`, `is_provider_configured(provider)`.

### Prompt Construction (`src/context/prompt_builder/`)

`PromptBuilder` assembles the full prompt from session state in four phases:
1. `build_system_context()` — persona, MCP tools, skills, scratchpad, conversation summary.
2. `linearise_messages()` — Pass 1: walk message history applying token budget, tool result compression, and context window limits.
3. `apply_pass2_budget()` — Pass 2: paginate oversized tool results into `HOBBES_PAGE_RESULT` continuations.
4. `strip_historical_thinking()` — remove thinking blocks from all but the most recent assistant message.

The prompt builder is **session-aware**: it resolves provider/context window from `session.llm_provider` override, not the global settings.

**Critical (P-001)**: Never call `get_mcp_context().await` inside the prompt build or send-message cycle. MCP context is reactively synced into `session.active_context` via `use_effect` in `ChatWindow` — trust that sync.

### MCP Layer (`src/mcp/`)

`McpManager` manages all tool servers as an `Arc<McpManager>`. Lock ordering is **mandatory** (P-010): always acquire `servers` before `dynamic_local_tools` before `dynamic_composio_tools`. Violations cause ABBA deadlocks.

Two virtual built-in servers are registered for AI introspection:
- `hobbes-core` (via `CoreClient`) — `HOBBES_UPDATE_SCRATCHPAD`, `HOBBES_PAGE_RESULT`. Dispatched in `stream_manager.rs` before MCP dispatch (requires `SessionState`).
- `hobbes-meta` — `MCP_LOAD_SERVER_TOOLS`, `MCP_UNLOAD_SERVER_TOOLS`.

Composio integration uses explicit per-user OAuth (P-002 through P-007) — see `SYSTEM_PATTERNS.md` for the full 6-point reconnect lifecycle.

### Multi-Tab Streaming State

Streaming and summarization state is **per-session**, not global:
- `StreamManagerContext.streaming_sessions: Signal<HashSet<String>>` — keyed by session ID.
- `StreamManagerContext.summarizing_sessions: Signal<HashSet<String>>` — guards against overlapping summarization tasks per tab.
- `Session.current_ai_turn_count` and `watch_word_recovery_count` — per-session turn tracking.

### Command Dispatch

`ChatCommand` (defined in `src/components/chat_input.rs`) is the single UI→logic bus. Commands are set on a `Signal<Option<ChatCommand>>` in `ChatInput` and handled in `main.rs`'s command handler. Key commands: `SwitchModel(usize)`, `SwitchProvider(usize)`, `ToggleSettings`, `ToggleProviderSelector`, `TriggerAiAnalysis`.

`SwitchModel` and `SwitchProvider` pin the **current session** (`session.llm_provider` + `session.chat_model`) rather than changing global settings. The global setting is also updated as the new default for future sessions.

### Memory & Persistence Patterns

- **Sessions**: `~/.config/com.hobbes.app/sessions.json` (macOS: `~/Library/Application Support/...`)
- **Settings**: same config dir, `settings.json`
- **Credentials**: macOS Keychain via `SecretManager` (biometric-protected). Windows via `keyring` crate. Never call keychain APIs directly from components — use `secret_manager::save_secret_to_keychain()` (P-011).
- **Logs**: daily rolling log at `~/Library/Application Support/com.hobbes.app/hobbes.log`

### Clippy Configuration

Three `allow` lints are set globally in `main.rs`:
- `await_holding_invalid_type` — Dioxus `Signal` types held across `.await`
- `collapsible_if` / `collapsible_else_if` / `collapsible_match` — readability preference

### Cross-Platform Notes

`secret_manager.rs` (macOS Keychain + Touch ID) and `secret_manager_generic.rs` (keyring) expose the identical public API and are swapped at compile time via `#[cfg(target_os = "macos")]`. Biometric methods are no-ops on non-macOS. Never import `keychain_ffi` directly from components.

---

## Key Reference Documents

- `ARCHITECTURE.md` — full component-level documentation and sequence diagrams
- `SYSTEM_PATTERNS.md` — **critical mandates** (P-001 through P-011) and anti-pattern registry; read before touching MCP, credentials, or state persistence
- `COMPOSIO_ENDPOINTS.md` — Composio API surface
- `GUIDE_TO_APPLE_SIGNING.md` — macOS signing and notarization workflow
