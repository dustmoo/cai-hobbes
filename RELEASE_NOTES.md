# Hobbes v0.11.3 — Multiple Connectors, Skills & SQLite Sessions

## ✨ New Features

### Multiple LLM connectors
- **Add several connectors per provider.** You can now configure up to 10 named connector instances — e.g. two OpenAI-compatible endpoints (a local model and a cloud one), or separate Gemini keys for work and personal — each with its own API key, model slots, and tuning.
- **Pin a conversation to a connector.** The provider/model switcher pins the *current tab* to a connector instance; other tabs and new sessions keep using your global default. Per-connector API keys are stored individually in the keychain.
- **Per-instance feature flags.** Watch-word recovery, thinking, and tool-use settings now follow the connector a session actually resolves to, not the global settings.

### Skill management
- **In-app skill editor.** Create and edit skills (instructions, metadata, scripts) directly from Hobbes — no more hand-editing files.
- **`/command` invocation with autocomplete.** Type `/` at the start of a line to invoke a skill by name, with a searchable autocomplete popover. You can put context on the lines above the command; mentioning a skill mid-sentence stays plain text.
- **Live reload.** A file watcher picks up external changes to your skills folder immediately.

### SQLite session storage
- **Chat history moved from `sessions.json` to a database.** Sessions load lazily (open tabs and the active session hydrate at startup; everything else loads on demand), saves write only what changed, and nothing ever expires. Your existing history is imported automatically on first launch — the old JSON files are left in place as a backup.
- **Faster history search.** Search runs in the database across all sessions — including ones never opened this run — matching names, summaries, and message text.

### Composio
- **No-auth toolkits connect cleanly** (e.g. tools that need no account link).
- **Remove toolkits and recover wedged servers** from MCP → Status.
- **Profile isolation.** Clients and on-demand tools are scoped to the active profile, so tools no longer bleed across tabs using different profiles.

## 🛡️ Reliability & Security
- **Session-store hardening:** saves are atomic (a crash can't persist a half-written state), overlapping background saves can't revert newer data, deleted conversations stay deleted, and a corrupt legacy `sessions.json` is preserved for recovery instead of being treated as empty history.
- **Keychain fixes:** key-discovery indexes are always readable before authentication, and keys migrated from older versions honor your chosen storage mode — no more surprise Touch ID prompts at startup or being bounced back into onboarding despite a valid key.
- **Model pinning safety:** deleting a connector no longer sends its pinned model name to a different provider.
- **UI stability:** fixed a potential crash when autocompleting skills in text containing emoji/CJK, streaming stays live when returning to a tab mid-stream, and the session cost icon refreshes on stream lifecycle.
- **Dependency audit refresh:** dependencies bumped and the RUSTSEC ignore list pruned from 10 entries to 3, each with a documented justification.

---

# Hobbes v0.11.0 — Provider-Aware Tool Selection & Composio Tool Editor

## ✨ New Features

### Any provider can now curate large toolkits
- **Smart tool selection no longer depends on Gemini.** When you connect a Composio toolkit with more tools than the model limit (128), Hobbes asks an LLM to pick the most useful subset. That step was previously hardcoded to Gemini, so connecting a large toolkit failed outright if you didn't have a Gemini key. It now uses your **active provider** (Gemini, Claude, or any OpenAI-compatible endpoint), falling back to any other configured provider.
- **All providers implement tool selection** — Claude (Messages API), OpenAI Chat Completions, and OpenAI Responses join Gemini, each using its configured summary model.

### Edit a toolkit's tools directly from Hobbes
- **New tool editor in MCP → Status.** Composio removed the easy in-dashboard way to enable/disable individual tools for a connection, so Hobbes now has its own. Each connected toolkit gets an **edit button** that opens a searchable checklist of every tool the toolkit offers.
- **Search, bulk toggle, and live counts.** Filter a long tool list by name or description, check/uncheck everything currently shown, and watch an enabled-count badge that warns when you go above the optimal (50) or provider (128) limits.
- **Saves apply immediately.** Your selection is written as the connection's tool whitelist and the tool set is hot-reloaded — caches are busted (toolkit counts, on-demand discovery, and the loaded set) so the AI sees the new tools right away without a restart.

### OpenAI summary-model selector
- **Pick a dedicated summarization model for OpenAI-compatible endpoints.** Settings now discovers available models from your endpoint (with a refresh button) and lets you choose a separate, usually cheaper, model for conversation summaries — or fall back to your chat model.

### Session search across message text
- **Search your history by what was actually said.** Session search now matches against message content, not just session names and summaries.

## 🐛 Fixes
- **Chat scrolls to your message on send.** Sending a message now snaps the view to the bottom immediately, instead of only following along if you were already near the bottom.
- **Cleaner history trimming at turn boundaries.** When old messages are dropped to fit the context budget, the kept window now always starts on a complete user turn — tool-call/result pairs are never left orphaned, which could confuse the model.
- **Windows app icon.** The Windows build now embeds the Hobbes icon into the executable.

---

# Hobbes v0.10.0 — Smart Context, AI Timers & Message Queue

## ✨ New Features

### Smart context handling
- **Big tool results stay intact on large-context models.** Previously, fetching a lot of data (e.g. a full inbox of emails) could get silently condensed mid-turn even on Gemini/Claude with their huge context windows — which sometimes led the model to "fill in the gaps" with plausible-but-wrong details. Tool results in the **current turn are now kept in full**, so the model always sees the real data it's working with.
- **Per-turn budgeting is now provider-aware.** Hobbes sizes tool results against each model's *actual* context window instead of a one-size-fits-all cap. Small local models still get aggressive compression (so they don't overload); large models keep far more in context.
- **Knowledge-preserving history summaries.** When older tool results no longer fit the budget, Hobbes replaces them with a dense summary that preserves the concrete facts (IDs, names, numbers, dates) — instead of chopping the data behind a "fetch the rest" footer. The full data is still retrievable on demand.
- **Auto-recalibration on "prompt too large".** If a provider rejects a prompt as too big, Hobbes trims and retries in-turn, and remembers the real limit so later turns just work — no manual fiddling.
- **Correct OpenAI context windows.** OpenAI-compatible endpoints now resolve their window from the model name (gpt-4.1 → 1M, gpt-5.4 → 1M, gpt-5.5 → 512K, gpt-5 → 400K, gpt-4o → 128K, o-series → 200K), with runtime self-correction for anything unknown.

### AI-settable timers & reminders
- The assistant can now set **timers and reminders** that notify you (or prompt a follow-up turn) when they fire, with a live pending-timer indicator above the chat bar and an auto-dismissing toast. Window-focus behavior is opt-in via a Behavior setting.

### Send-while-streaming message queue
- You can now **queue a message while a turn is still streaming** instead of waiting — it's sent automatically once the current turn completes.

## 🐛 Fixes
- **Windows scrollbars cleaned up.** The chat input and side panels no longer show chunky, always-visible scrollbars with arrow buttons on Windows/Linux — the input grows cleanly and then scrolls with a thin scrollbar, matching the macOS look. (macOS overlay scrollbars are untouched.)
- **Chat bar spacing.** Added breathing room below the chat bar and fixed padding that was being clipped by the app shell's overflow.

---

# Hobbes v0.9.62 — Pricing Accuracy

## 🐛 Fixes
- **OpenAI cost no longer mis-prices version families.** Model matching is now version-aware: `gpt-5.1` / `gpt-5.2` / `gpt-5.3` can no longer silently inherit bare `gpt-5`'s cheaper rate. Recognized families (gpt-5, gpt-5.4, gpt-5.5, gpt-4o, o-series) resolve correctly; an unrecognized model reports no cost rather than a wrong one.

## 🛠 Maintenance
- Verified Gemini and Claude pricing tables against current published rates (all correct as of 2026-06-17).
- Documented the model pricing tables as manually-maintained and drift-prone (SYSTEM_PATTERNS P-013), with per-table "last verified" dates.

---

# Hobbes v0.9.61 — OpenAI Cost Tracking & Windows Settings Reliability

## ✨ New Features

### OpenAI API cost tracking
- **Real per-turn USD cost** for OpenAI models — input/output token pricing for the gpt-5.5 / 5.4 families, gpt-5 / mini / nano / pro, gpt-4.1, gpt-4o, and the o-series.
- **Billed only for the real OpenAI API** (an API key present **and** an `api.openai.com` endpoint). Local / self-hosted / keyless endpoints (Ollama, vLLM, LM Studio, proxies) stay free — no fabricated costs. Unknown models report no cost rather than a wrong one.

## 🐛 Fixes

### Windows settings reliability
- **Settings now save reliably on Windows.** `settings.json` is written atomically (temp file → fsync → rename) instead of a non-atomic truncate-and-write, eliminating the corruption/race that made settings silently revert to defaults after the first save. Save failures now surface in the UI toast instead of failing silently.

---

# Hobbes v0.9.60 — OpenAI Responses API Support

## ✨ New Features

### OpenAI Responses API (gpt-5 / o-series)
- **Support for OpenAI's newest models** — gpt-5 and o-series reasoning models are served only by the Responses API (`/v1/responses`), which previously failed with a `404 — only supported in v1/responses` error. Hobbes now speaks both protocols.
- **Automatic routing** — Under the default `Auto` API style, OpenAI's gpt-5/o-series models on `api.openai.com` use the Responses API; everything else (older OpenAI models, local servers like vLLM/Ollama/LM Studio, OpenRouter) continues to use Chat Completions. No regression to existing setups.
- **Reasoning summaries** — With Thinking Mode enabled, reasoning summaries stream into the "Considering…" bubble.
- **Robust streaming** — Function calling, multi-turn tool history, vision input, and usage reporting are mapped to the Responses item/event model. A final-text fallback ensures models that reason silently before answering still surface their output.

## 🛠 Maintenance
- Added regression tests for Composio auth-error detection and toolkit-name conversion.
- Documented the Dioxus `use_memo`-over-props streaming footgun (SYSTEM_PATTERNS P-012).
- Cleared all CI clippy warnings.

---

# Hobbes v0.9.58 — Image Generation, Context Tuning & Hardening

## ✨ New Features

### Native Image Generation
- **Generate images directly in chat** — New `generate_image` tool powered by Gemini image models, available as a virtual MCP server (`hobbes-native-image`)
- **Image editing support** — Pass a previously generated image as a `reference_image` to riff on or modify it
- **Inline image rendering** — Generated images display inline in the chat with download controls; `file://` paths converted to data URIs for reliable WebView rendering
- **Image Generation Model selector** — Configurable in Settings → Model panel

### Per-Provider Context Tuning
- **Fine-grained context budget controls** — New `ContextTuningPreset` system lets you override chat history length, tool output limits, summary size, entity count, and budget ratios per provider (Gemini, OpenAI-compat, Claude)
- **Compact tool results** — Option to convert JSON tool results to compact markdown for smaller context models
- **Dynamic tool result budget & pagination** — Pass 2 budget allocation system automatically paginates oversized tool results with a `HOBBES_PAGE_RESULT` continuation tool

### Stream & Thinking Improvements
- **Thinking content visibility** — "Considering..." bubble now shows thinking content in real-time (thinking-only messages forwarded to UI)
- **OpenAI-compat auto-recovery** — Automatic retry on transient vLLM stream decode errors
- **Proactive summarization guard** — Prevents overlapping summarization tasks during rapid continuation turns

## 🔒 Security
- **File path validation module** (`security.rs`) — Centralized `validate_safe_file_path()` prevents arbitrary file exfiltration via image references; allowlist: config dir, data dir, temp dir
- **XSS hardening** — Sanitized all user-facing Markdown output via `ammonia`
- **Security test suite** — Tests for path traversal, SSH key access, `file://` prefix stripping, URL-encoded spaces

## 🔧 Fixes
- **Streaming regression resolved** — Removed incorrect `use_memo` caching in MarkdownRenderer; live updates restored during LLM streaming
- **Settings migration** — Robust migration logic prevents configuration loss when upgrading from older versions
- **API key management** — DRY-up of credential handling across providers

## 🏗️ Infrastructure
- **Codebase audit** — Comprehensive hardening pass (dead code removal, Clippy fixes, dependency cleanup)
- **Repo hygiene** — Untracked archive/, downloads/, and tmp_pro_icons/ from version control
- **Release workflow** — Added `RELEASE_NOTES.md` auto-populated into GitHub Releases

## 📦 Distribution
- macOS (Apple Silicon + Intel) — Direct Download DMG, signed & notarized
- Windows build temporarily unavailable (Azure certificate validation in progress)
