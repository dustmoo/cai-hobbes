# Hobbes v0.13.0 — The Heartbeat

## ✨ New Features

### The Heartbeat
- **Hobbes keeps watch between chats.** Turn it on from the Fleet or Settings → Planner. It watches your planner, the Fleet and your projects' CI, and wakes its own chat when something needs attention: a worker that's stuck or waiting on you, a failing build, an overdue to-do.
- **Works from your day, not just from changes.** It checks in at the start of your workday, about 45 minutes before each meeting, when the plan is over capacity, and near the end of the day. Each wake starts with the next few hours (calendar, plan, deadlines, agents) and does the single most useful thing: prep for a meeting, propose what to defer, or unblock a worker.
- **Diagnoses stuck work.** A wake about a stuck worker reads the end of its transcript, names the actual blocker, and fixes it within the rules or asks you one clear question.
- **Within a budget, and visible.** Two wakes an hour and $5 a day by default, both adjustable. Wakes with nothing to do are skipped. The Fleet card shows what each wake did ("Dispatched 'Clyde prep'") and what it cost, with the full trail behind Details and a Clear trail button. Its tools follow agent time's rules: outside effects wait for your approval.
- **Handles the small things itself.** Without spending a wake, it keeps agent to-dos in step with their workers (a finished run moves to review) and pauses a focus timer left running for hours, banking your estimate. The card lists what it handled, with **Keep real time** if the full time was right after all. The Heartbeat's tab gets its own metal heart.
- **Only real questions count as "needs you."** A Claude Code session that simply finished its turn is shown as idle, not waiting on you, across the Fleet badge, notifications, the planner and the Heartbeat. Permission prompts and other questions still flag, and an old question from a previous turn no longer lingers.

### Chat box keys
- **The Claude Code keys you know.** **Esc** stops a reply and puts your prompt back in the box (along with anything you'd queued); press it twice on an idle chat to clear the box. **↑/↓** bring back what you've sent, **⌃R** searches it, and **⌃C** clears the box without losing the text.
- **All remappable.** A new Chat box section in Settings → Hotkeys lets you re-record, reset or turn off each one, effective on save. Shortcut fields can be set from the keyboard and are labelled for screen readers. Control now means just the Control key, so ⌘C keeps copying.

### Continuity
- **Handoff notes.** The AI can leave itself a note in a chat: what it's doing, what it expects next, what would change its plan.
- **Turn briefings.** Coming back to a chat after a gap, the AI is told how long it's been, which timers fired and why, its own handoff note, and what changed in work it dispatched.
- **Timers with a purpose.** A timer can carry a reason and fire as soon as a dispatched worker finishes instead of waiting for its deadline. A tab waiting on its own timer shows a hollow dot and stays "working" in the planner.

### The Fleet (Pro)
- **Redesigned around one question: what needs you.** One summary line, then a single list grouped by Needs you, Working and Idle, one line per session with details a click away. Wide windows show three columns. Day in review moves to the bottom.
- **Click to reach a session's terminal.** Clicking a Claude Code session focuses its exact iTerm2 or Terminal.app tab (macOS asks once for permission). Other terminals are brought forward; ended sessions can be resumed in a new tab or their resume command copied.
- **Answer Claude in Claude.** Claude Code's permission prompts and questions are answered in the terminal, where all their choices are, via "Answer in iTerm2". Headless runs that Hobbes dispatched keep Approve / Deny in the app.
- **Hobbes tabs show their current name and emoji,** and idle sessions waiting on you keep their "blocked" line.

### Delegation
- **Workers report back.** A chat dispatched from another chat now reports its result to that chat when it finishes, in a lighter relay bubble you can click to jump between them. Choose Off, Report back or Two-way in Settings → Planner; two-way lets the parent send its worker corrections.
- **Pick the right accounts.** Dispatch can run a chat under a different Composio profile (say, a client's accounts) when the work belongs there. Turn it off in Settings → Planner to always use the dispatching tab's profile.
- **A clearer agent lane.** Compact cards with one action each, colours that match the Fleet, and a new **To review** section for runs that finished with something left for you, instead of leaving them "stuck" forever.

### Planner & tabs
- **Today's time on a task.** When a to-do has time from earlier days, the focus bar and menu-bar timer show today's time beside the total.
- **More room on the timeline.** A taller timeline, a five-hour default view, and calendar events in their own lane so their titles are readable. "Not on the timeline" shows six to-dos with Show more.
- **Tidy every tab.** The tab tidy sheet now lists every closable tab, oldest first, not only the ones it has a suggestion for.

## 🐛 Fixes
- **Every message was told "the user has not replied."** Since v0.12.2 a normal send also carried a continuation instruction meant for turns the AI starts itself. Replies should feel more responsive.
- **Gemini keeps working after a tool call.** Gemini 4 Argon read the post-tool instruction as an order to stop; it now continues when the task needs more steps.
- **Large tool sets on Gemini.** Over Gemini's 128-tool limit, Hobbes now keeps its own tools and the ones you're using, drops bulk Composio tools first, and tells the AI what was left out.
- **No more raw JSON as progress** on agent-lane cards, and long dispatch briefs no longer become giant to-do titles.
- **Tab picks the suggestion.** Tab accepts the highlighted skill or toolkit suggestion instead of indenting; Shift+Tab always outdents or moves back.

## 🛡️ Security
- Updated `ammonia` (4.2.4), `rustls-webpki` (0.103.16), `webpki-roots` (1.0.11) and `zip` (9.0.5) for recent RustSec advisories.

# Hobbes v0.12.3 — Gemini 4 Argon

## ✨ New Features
- **Gemini 4 Argon.** Google's newest model is now supported, with a 1M-token context and low / medium / high thinking levels. Hobbes tracks its introductory pricing ($2 / $10 per million input / output tokens through November 30, then $4 / $20) and its lower 5% rate for cached input, so turn costs stay accurate. Argon needs a paid Gemini API key; add `gemini-4-argon` to a model slot to try it.

## 🐛 Fixes
- **Chat dates use your local time.** History and renamed tabs showed chats from the evening under the next day's date. They now use your local day.
- **No wasted cache requests on Argon.** Argon only supports Google's automatic caching, so Hobbes no longer tries to create an explicit context cache for it on every turn.

## 🛡️ Security
- Updated `openssl` (0.10.81), `rand` (0.8.8 / 0.9.5) and `rustls` (0.23.46) for recent RustSec advisories.

# Hobbes v0.12.2 — The Planner, the Fleet, and Side Tabs

## ✨ New Features

### The Planner
- **A built-in planner, shared with the AI.** Open it from the chat bar or its own tab. Inbox, Today, Upcoming, Anytime, Someday and Logbook lists, with quick-add tokens (`~30m`, `@2pm`, `+project`, `!chore`, priorities) and a live preview chip. Press ⌘K anywhere for quick capture.
- **Today, with a timeline.** Drag to-dos onto the day, then drag blocks to move or resize them. A capacity bar shows planned against done. Unfinished work rolls over to the next day, and you can end the day when you're done. The timeline fits your workday and zooms to any visible range.
- **Your calendars on the timeline.** Mirror Google Calendar (through Composio) or any ICS/webcal feed as read-only blocks. Firm meetings count against the day's capacity, and the AI plans around them.
- **Your time & goals.** Protect blocks for what your time is *for* (reading, getting outside, gaming) and set weekly targets. The AI plans work around protected time and never over it.
- **Projects & areas.** File to-dos into projects grouped by area. Hobbes matches names and aliases, so the AI reuses an existing project instead of creating a duplicate. **Manage projects** handles renames, aliases, status and merging duplicates.
- **Focus mode.** Start a to-do to track time against its estimate, with a menu-bar timer. Activating a to-do can also open a fresh chat seeded with it.
- **Two lanes.** Hand a to-do to an agent and it moves into the agent lane beside your own work, so you can see what's working, what's stuck and what needs you.
- **Weekly review (experimental).** A look back at your week: your-time goals, agent leverage, outcomes reached and chore minutes, with feedback in a style you choose. It's marked Experimental while it takes shape.
- **The AI uses the planner too.** New `HOBBES_TODO_*`, `HOBBES_PLAN_DAY`, `HOBBES_TIME_BLOCK`, `HOBBES_PROJECT_*`, `HOBBES_TIME_GOALS` and `HOBBES_CALENDAR_LIST` tools. Today's plan is included in every chat, so the AI knows your day without asking. Turn the planner off in Settings → Planner.

### Fleet & delegation (Pro)
- **Watch your coding agents.** The Fleet tab lists every Claude Code session on your machine: what it's doing, transcript briefs, a day rollup, and the "exocortex" measure of how much agent work runs while you're engaged.
- **Dispatch work.** `HOBBES_DISPATCH` hands a to-do to a new Hobbes chat or to a headless Claude Code run in the project's folder, on a model you allow. When a background agent finishes, the chat that sent it picks up the result and carries on.
- **Trust rules.** Approve a tool call once and choose to always allow that command or tool in a project. Every approval decision is logged.
- **Built-in terminal (opt-in).** A persistent per-chat shell the AI can use, always behind tool approval. Off by default; turn it on in Settings → Terminal.

### Tabs & layout
- **Side tabs are the new default.** Open tabs now sit in a resizable left sidebar that can collapse to an emoji rail. It's sortable, can be grouped by project, and has keys for jumping between groups. You can switch back to the top strip in Settings → Behavior.
- **Tidy tabs.** Hobbes suggests which tabs to close or file into projects, with an optional nudge once too many are open.
- **Tabs that say what they're about.** Each tab shows an emoji, a date and a topic, and the title settles after a few messages. Narrow layouts give way gracefully, with hairline dividers between columns you can resize.
- **One drawer layout for Settings, History and MCP.** Fleet and Terminal get their own Settings tabs.

### Chat
- **Prompt caching for Claude.** A stable system prefix, per-turn context sent once, and optional keep-alive pings for idle tabs. Long chats get much cheaper and faster. You can tune it under the prompt-caching toggle.
- **Activity stream.** Each turn shows its tool calls and thinking as one readable activity row, with real step timing.
- **Mermaid diagrams and LaTeX** render in replies. Messages show timestamps and day dividers.
- **Skills:** three-state permissions (Auto / Ask / Off), a configurable idle unload, and `HOBBES_UNLOAD_SKILL`.
- **Models:** Claude Opus 5.5, Sonnet 5.5 and Haiku 5.5, plus current lineups and pricing for Claude, Gemini and OpenAI.
- **A new tiger mascot** in the chat bar, the menu bar and the empty chat.

## 🐛 Fixes
- **Image attachments stay legible.** Attachments and tool screenshots are capped at 1568px on the long edge instead of being shrunk further.
- **Mid-chat model switches work.** Switching providers no longer fails on another model's thought signatures.
- **Prompt construction:** history trims in whole chunks, the user turn is never sent twice, a request never ends on an assistant turn, and tool results that already fit aren't cut.
- **Terminal:** clean output, no more hangs on stuck prompts, and timed-out output no longer leaks into later commands.
- **Tabs:** titles and emoji are set reliably, crowded tab strips don't bleed into each other, and approvals resume in the tab that asked for them.
- **Composio:** no more duplicate tool declarations in dispatched tabs, and background refreshes stay within one profile.

## 🛡️ Reliability & Security
- **Logs without secrets.** Logging no longer records secrets or message content at the default level. The Pro build writes daily log files you can share when reporting a bug.
- **Planner data is local**, in its own SQLite store next to your sessions, and it survives restarts.
- **Dependency audit findings resolved.**

---

# Hobbes v0.11.4 — On-Demand MCP Loading Fix

## 🐛 Fixes

- **On-demand local MCP servers are loadable again.** The built-in `hobbes-meta` server (which provides `MCP_LOAD_SERVER_TOOLS`) could itself be switched to on-demand mode in MCP settings — locking the loader behind the loader, so servers marked [ON-DEMAND] (Playwright, iTerm, etc.) could never be loaded and the AI hit "Tool Not Available" errors. Built-in servers now always stay fully loaded: the mode dropdown no longer appears for them, and any previously saved on-demand/disabled state for them is ignored at startup.
- **Gemini recognizes bare built-in tool names.** If the model calls a built-in tool by its bare name (e.g. `MCP_LOAD_SERVER_TOOLS` instead of the prefixed `hobbes-meta_MCP_LOAD_SERVER_TOOLS`), Gemini sessions now route it to the right server instead of failing — matching the existing behavior for OpenAI-compatible models.

---

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
