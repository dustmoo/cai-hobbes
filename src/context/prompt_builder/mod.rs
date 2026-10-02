mod context_compression;
mod ledger;
mod message_history;
mod system_context;
pub(crate) mod types;
mod tool_result_budget;

#[cfg(test)]
mod prompt_builder_tests;

// Re-exports for external consumers
pub use types::PromptBuildResult;

use crate::llm::types::{ChatMessage, ChatRole, ContentBlock, LlmPrompt as NeutralLlmPrompt};
use crate::session::Session;
use crate::settings::Settings;


/// Stands in for the user's silence when a continuation would otherwise leave
/// the request ending on an assistant turn. See `ensure_ends_with_user_turn`.
const CONTINUATION_NUDGE: &str = "[Continue. The user has not replied yet.]";

/// Builds a structured `LlmPrompt` object for the LLM.
pub struct PromptBuilder<'a> {
    pub(crate) session: &'a Session,
    pub(crate) settings: &'a Settings,
    /// Prebuilt `planner_today` system-context block
    /// (`todo::handlers::planner_today_context`). Computed at the call site so
    /// the builder stays free of planner state; `None` (the default) when the
    /// planner or its context injection is disabled.
    pub(crate) planner_today: Option<serde_json::Value>,
    /// Clock for a new request tail; `None` (the default) reads the local
    /// clock. Tests pin it so a build is a pure function of its inputs.
    pub(crate) now: Option<chrono::DateTime<chrono::Local>>,
}

impl<'a> PromptBuilder<'a> {
    pub fn new(session: &'a Session, settings: &'a Settings) -> Self {
        Self {
            session,
            settings,
            planner_today: None,
            now: None,
        }
    }

    /// Attach the `planner_today` block built by
    /// `todo::handlers::planner_today_context`.
    pub fn with_planner_today(mut self, planner_today: Option<serde_json::Value>) -> Self {
        self.planner_today = planner_today;
        self
    }

    /// Pin the clock a new request tail records (tests).
    #[cfg(test)]
    pub fn with_now(mut self, now: chrono::DateTime<chrono::Local>) -> Self {
        self.now = Some(now);
        self
    }

    /// Effective LLM provider for this prompt: session override → global settings.
    pub(crate) fn effective_provider(&self) -> crate::settings::LlmProvider {
        self.settings.provider_for_session(self.session)
    }

    /// The connector instance this prompt resolves to (session pin → legacy
    /// kind match → global active connector).
    pub(crate) fn effective_connector(&self) -> Option<&crate::settings::ProviderInstance> {
        self.settings.connector_for_session(self.session)
    }

    /// Per-connector context tuning resolved against the session's connector.
    pub(crate) fn effective_tuning(&self) -> crate::settings::ResolvedContextTuning {
        match self.effective_connector() {
            Some(instance) => self.settings.effective_context_tuning_for_connector(instance),
            None => self
                .settings
                .effective_context_tuning_for(self.effective_provider()),
        }
    }

    /// Context window resolved against the session's connector + model.
    pub(crate) fn effective_context_window(&self) -> Option<usize> {
        let model = self.settings.chat_model_for_session(self.session);
        match self.effective_connector() {
            Some(instance) => self
                .settings
                .resolve_context_window_for_connector(instance, &model),
            None => self
                .settings
                .resolve_context_window_for(self.effective_provider(), &model),
        }
    }

    /// Builds the structured `LlmPrompt` with system instructions, tools, and conversation history.
    pub fn build_prompt(
        &self,
        user_message: String,
    ) -> PromptBuildResult {
        // Phase 1: Build system context (persona, tools, skills, scratchpad, etc.)
        let ctx = self.build_system_context();

        // Phase 2: Linearise message history (Pass 1), with the turn context
        // ledger spliced in and the request tail at the end.
        let mut linearised = self.linearise_messages(&ctx, user_message);

        // 3b. A request asking for a new completion must not end on an
        // assistant turn. Appending here (at the tail) keeps every
        // `tool_result_positions` and `turn_scoped` index from Pass 1 valid.
        // The request tail (a System message carrying the CONTINUATION /
        // TOOL COMPLETION instruction) is always last, so this only fires
        // when no tail rendered.
        Self::ensure_ends_with_user_turn(&mut linearised.messages);

        // Phase 3: Apply Pass 2 budget & pagination
        self.apply_pass2_budget(
            &mut linearised.messages,
            &linearised.tool_result_positions,
            &mut linearised.pages_to_store,
            &ctx.tools,
            &ctx.system,
            ctx.provider_context,
            &ctx.tuning,
        );

        // Log page queue availability
        if !linearised.pages_to_store.is_empty() {
            tracing::info!(
                "HOBBES_PAGE_RESULT available via hobbes-core — {} paginated result(s) queued",
                linearised.pages_to_store.len()
            );
        }

        // Phase 4: Strip historical thinking blocks for finite context windows
        if ctx.provider_context.is_some() {
            Self::strip_historical_thinking(&mut linearised.messages);
        }

        PromptBuildResult {
            prompt: NeutralLlmPrompt {
                // Stable for the whole session: the live context and the
                // per-request tail are messages now.
                system: Some(ctx.system),
                system_volatile_offset: None,
                turn_scoped: linearised.turn_scoped,
                history_rewritten: linearised.history_rewritten,
                messages: linearised.messages,
                tools: ctx.tools,
            },
            pages_to_store: linearised.pages_to_store,
            history_anchor: linearised.history_anchor,
            ledger_entries: linearised.ledger_entries,
        }
    }

    /// Guarantee the outgoing message list never ends on an assistant turn.
    ///
    /// Claude refuses such a request outright on models without prefill
    /// support — "This model does not support assistant message prefill. The
    /// conversation must end with a user message" — and every other provider
    /// needs a user turn to answer as well.
    ///
    /// This used to hold by accident. A turn's last stored message was its
    /// tool call, and a tool call always linearises into an assistant
    /// `tool_use` *plus* its `tool_result`, which rides on a user turn. Once a
    /// finished reply started being ordered after the tool calls it follows
    /// (so the transcript reads thinking → commands → answer), the assistant's
    /// own prose became the last message of the turn and the invariant broke:
    /// a continuation — which carries no new user text — then built a request
    /// ending on that prose.
    ///
    /// A continuation has nothing from the conversation left to append, so the
    /// nudge below stands in for the user's silence. It exists only in the
    /// outgoing request: it is never stored on the session, journaled, or
    /// shown in the transcript. The system prompt already carries the fuller
    /// instruction for this case (see `build_system_context`'s
    /// TOOL COMPLETION / CONTINUATION instructions), so this stays terse.
    fn ensure_ends_with_user_turn(messages: &mut Vec<ChatMessage>) {
        if messages
            .last()
            .is_some_and(|m| m.role == ChatRole::Assistant)
        {
            messages.push(ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::Text {
                    text: CONTINUATION_NUDGE.to_string(),
                }],
            });
        }
    }

    /// Strip `ContentBlock::Thinking` blocks from all assistant messages except
    /// the most recent one. This dramatically reduces context usage for models
    /// with finite context windows — thinking content from historical turns is
    /// the single largest contributor to context exhaustion on small models.
    ///
    /// Walks messages in reverse: the most recent `ChatRole::Assistant` message
    /// keeps its thinking blocks, and so does every assistant message in the
    /// current tool loop (after the last `ChatRole::User` message) — Claude
    /// Opus 5.5 and Fable 5.1 reject a tool loop whose thinking blocks were
    /// partially dropped. Assistant messages from earlier turns are stripped.
    fn strip_historical_thinking(messages: &mut [ChatMessage]) {
        let mut found_latest = false;
        let mut in_current_turn = true;
        for msg in messages.iter_mut().rev() {
            if msg.role == ChatRole::User {
                in_current_turn = false;
            }
            if msg.role == ChatRole::Assistant {
                if found_latest && !in_current_turn {
                    // Strip thinking from older assistant messages
                    let before = msg.content.len();
                    msg.content.retain(|block| !matches!(block, ContentBlock::Thinking { .. }));
                    if msg.content.len() < before {
                        tracing::debug!(
                            "Stripped {} thinking block(s) from historical assistant message",
                            before - msg.content.len()
                        );
                    }
                } else {
                    found_latest = true;
                    // Keep thinking on the most recent assistant message
                }
            }
        }
    }
}
