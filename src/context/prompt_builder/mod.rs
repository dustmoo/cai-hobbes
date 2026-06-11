mod context_compression;
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


/// Builds a structured `LlmPrompt` object for the LLM.
pub struct PromptBuilder<'a> {
    pub(crate) session: &'a Session,
    pub(crate) settings: &'a Settings,
    pub(crate) session_state: &'a crate::session::SessionState,
}

impl<'a> PromptBuilder<'a> {
    pub fn new(
        session: &'a Session,
        settings: &'a Settings,
        session_state: &'a crate::session::SessionState,
    ) -> Self {
        Self {
            session,
            settings,
            session_state,
        }
    }

    /// Effective LLM provider for this prompt: session override → global settings.
    pub(crate) fn effective_provider(&self) -> crate::settings::LlmProvider {
        self.settings.provider_for_session(self.session)
    }

    /// Per-provider context tuning resolved against the session's provider.
    pub(crate) fn effective_tuning(&self) -> crate::settings::ResolvedContextTuning {
        self.settings
            .effective_context_tuning_for(self.effective_provider())
    }

    /// Context window resolved against the session's provider + model.
    pub(crate) fn effective_context_window(&self) -> Option<usize> {
        self.settings.resolve_context_window_for(
            self.effective_provider(),
            &self.settings.chat_model_for_session(self.session),
        )
    }

    /// Builds the structured `LlmPrompt` with system instructions, tools, and conversation history.
    pub fn build_prompt(
        &self,
        user_message: String,
    ) -> PromptBuildResult {
        // Phase 1: Build system context (persona, tools, skills, scratchpad, etc.)
        let ctx = self.build_system_context();

        // Phase 2: Linearise message history (Pass 1)
        let last_message = self.session.messages.last();
        let mut linearised = self.linearise_messages(
            &ctx.system,
            &ctx.tools,
            ctx.provider_context,
            &ctx.tuning,
            ctx.is_continuation_placeholder,
            last_message,
        );

        // 3. Add current user message
        if !user_message.is_empty() {
            linearised.messages.push(ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::Text { text: user_message }],
            });
        }

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
                system: Some(ctx.system),
                messages: linearised.messages,
                tools: ctx.tools,
            },
            pages_to_store: linearised.pages_to_store,
        }
    }

    /// Strip `ContentBlock::Thinking` blocks from all assistant messages except
    /// the most recent one. This dramatically reduces context usage for models
    /// with finite context windows — thinking content from historical turns is
    /// the single largest contributor to context exhaustion on small models.
    ///
    /// Walks messages in reverse: the first `ChatRole::Assistant` message encountered
    /// keeps its thinking blocks; all earlier assistant messages have them removed.
    fn strip_historical_thinking(messages: &mut [ChatMessage]) {
        let mut found_latest = false;
        for msg in messages.iter_mut().rev() {
            if msg.role == ChatRole::Assistant {
                if found_latest {
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
