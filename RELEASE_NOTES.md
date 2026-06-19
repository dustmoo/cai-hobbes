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
