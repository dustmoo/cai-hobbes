# Short-Term Memory Refactor Plan

This document outlines the architectural changes required to refactor the short-term memory summarization process from being trigger-on-send to trigger-on-inactivity.

## 1. Current Architecture

- The `ConversationProcessor` is called directly within the `send_message` flow.
- This blocks the user's message from being sent until the summary generation is complete.
- The summarization LLM is called on every single message sent by the user.

## 2. Proposed Architecture

The new architecture will decouple summarization from the chat submission flow.

- **Trigger:** Summarization will be triggered when the user is idle for 5 seconds after any new message is added to the conversation (either from the user or the assistant).
- **Condition:** The summarizer will only run if the number of messages has changed since the last summary was generated.
- **Process:** The process will run as a non-blocking background task.
- **State:** The `PromptBuilder` will always have access to the latest successfully generated summary from the `SessionState`.
- **Configuration:** The 5-second delay will be hardcoded initially and made configurable in a future task.

### Component Changes

#### A. `SummarizationScheduler` (New Coroutine)

- A new `use_coroutine` will be created in `main.rs` and provided via context.
- It will manage an internal 5-second timer.
- It will maintain state to track the number of messages that were present during the last summary (`last_summarized_message_count`).
- It will expose a channel `activity_tx` to receive signals that reset the timer.
- When the timer expires, it will:
    1. Get the current `SessionState` and `Settings` from context.
    2. Compare the current message count with `last_summarized_message_count`.
    3. If the count is different, spawn a `tokio::task` to call the `ConversationProcessor`.
    4. Upon successful summarization, update both the `ConversationSummary` in `SessionState` and the `last_summarized_message_count` in its own state.
    3. Await the result from the processor.
    4. Write the new `ConversationSummary` back to the `SessionState` signal.

#### B. `ConversationProcessor` (`src/processing/conversation_processor.rs`)

- The `process_and_respond` method will be removed.
- The `generate_summary` method will be made `pub`.
- `generate_summary` will be refactored to accept `Session` and `Settings` by value or clone and return `Option<ConversationSummary>`. It will no longer mutate the session state directly.

#### C. `ChatInput` Component (`src/components/chat_input.rs`)

- It will get the `SummarizationScheduler`'s channel from the context.
- The `oninput` and `onkeydown` handlers for the `textarea` will send a message to the `activity_tx` channel to reset the inactivity timer.
- A similar trigger will be needed from the main `ChatWindow` or `StreamManager` to signal when an assistant's message has finished streaming, ensuring agent activity also resets the timer.

#### D. `SessionState` (`src/session.rs`)

- No direct changes are needed. The `SummarizationScheduler` will update the `active_context.conversation_summary` field within the existing `Session` struct. Because `SessionState` is a `Signal`, any part of the application reading it (like `PromptBuilder`) will automatically get the latest value.

## 3. Flow Diagram

```mermaid
sequenceDiagram
    participant User
    participant User
    participant ChatInput
    participant ChatWindow
    participant SummarizationScheduler
    participant ConversationProcessor
    participant SummaryLLM
    participant SessionState

    User->>ChatInput: Types in textarea
    ChatInput->>SummarizationScheduler: send(activity_signal)
    note right of SummarizationScheduler: Timer is reset to 5s
    loop Inactivity Timeout
        User->>ChatInput: Stops typing
        Note over SummarizationScheduler: 5-second timer expires
        SummarizationScheduler->>SessionState: Read current session & settings
        SummarizationScheduler->>SummarizationScheduler: if msg_count > last_summarized_count
        SummarizationScheduler->>ConversationProcessor: spawn_task(generate_summary)
        ConversationProcessor->>SummaryLLM: Summarize history
        SummaryLLM-->>ConversationProcessor: Return summary
        ConversationProcessor-->>SummarizationScheduler: Return summary
        SummarizationScheduler->>SessionState: Write new summary & update last_summarized_count
        end
    end

    ChatWindow->>StreamManager: Assistant finishes response
    StreamManager->>ChatWindow: Final message saved
    ChatWindow->>SummarizationScheduler: send(activity_signal)
    note right of SummarizationScheduler: Timer is reset to 5s

    User->>ChatInput: Clicks 'Send'
    ChatInput->>PromptBuilder: build_prompt()
    PromptBuilder->>SessionState: Read latest summary from active_context
    Note over PromptBuilder: Prompt now includes up-to-date summary