# Hobbes Architecture

This document outlines the software architecture for Hobbes, distinguishing between long-term and short-term memory systems.

## Core Principles

- **Local-First:** All user data, including chat history and context, is stored locally and securely on the user's machine.
- **Clear Memory Separation:** The system maintains a clear distinction between long-term strategic memory (managed by external MCPs like ConPort) and short-term, session-specific active context (managed internally).
- **Event-Driven Context:** Internal, short-term context is updated via an event bus, allowing for modular and scalable local context providers.

## System Components

The architecture is designed to integrate both external long-term memory and internal short-term memory seamlessly.

```mermaid
graph TD
    subgraph "External MCPs (Examples)"
        Ext1[GitHub MCP] -- "Provides PR/Issue data" --> H[PromptBuilder];
    end

    subgraph "Local Context Providers"
        A[ConPort MCP] -- "Provides strategic context" --> H;
        B[WorkspaceMcp] -->|Publishes ContextUpdateEvent| C(EventBus);
        D(Future: GitMcp, etc.) -->|Publishes ContextUpdateEvent| C;
    end

    subgraph "Internal Short-Term Memory & Core Logic"
        subgraph "Core Application"
            C -->|Subscribes to Events| E[main.rs Effect Hook];
            E -->|Updates Active Session| F[SessionState];
            
            G[ChatWindow] -->|Reads from Active Session| F;
            G -->|Triggers| J[ConversationProcessor];
            J -->|Updates Active Session| F;

            G -->|Builds Prompt| H;
            H -->|Gets Active Context| F;
            H -->|Formats Context| I["Chat LLM (e.g., Gemini Pro)"];
            G -- "Sends Message" --> I;

            J -- "Generates Summary" --> I2["Summary LLM (e.g., Gemini Flash)"];

            subgraph "Tool Call Feedback Loop"
                I -- "Responds with Tool Call" --> K[StreamManager];
                K -- "Updates Message State" --> F;
                G -- "Executes Tool" --> L[McpManager];
                L -- "Returns Result" --> G;
                G -- "Formats Result" --> H;
                H -- "Sends Result to LLM" --> I;
            end
        end
    end

    style A fill:#8d99ae,stroke:#333,stroke-width:2px
    style B fill:#2a9d8f,stroke:#333,stroke-width:2px
    style D fill:#e9c46a,stroke:#333,stroke-width:2px
    style J fill:#c77dff,stroke:#333,stroke-width:2px
    style F fill:#f4a261,stroke:#333,stroke-width:2px
    style I fill:#e76f51,stroke:#333,stroke-width:2px
    style I2 fill:#f77f00,stroke:#333,stroke-width:2px
    style L fill:#457b9d,stroke:#333,stroke-width:2px
```

### 1. Memory Systems

-   **Local Long-Term Memory (ConPort):** A local MCP that provides access to the project's strategic memory, including goals, architectural decisions, and user preferences. This is analogous to a project's knowledge base.
-   **Short-Term Memory (`SessionState`):** The core of the "live" context. This is managed internally and stored securely in `sessions.json`. Each `Session` object within the state contains its own `active_context` map, ensuring that short-term memory is scoped to a specific conversation.

### 2. Local Context Producers & Processors

-   **`WorkspaceMcp`**: A provider that emits data about the user's local workspace (active file, open tabs). This is a key source for short-term memory, pushed via the Event Bus.
-   **`ConversationProcessor`**: An internal service triggered *after* a message is sent. It reads the recent conversation history, uses a fast, dedicated **Summary LLM** (e.g., Gemini Flash) to extract entities and summaries, and writes this data directly to the active session's `active_context`. This is the primary mechanism for conversational memory, distinct from the main **Chat LLM**.
-   **(Future) `GitMcp`**: Will provide short-term context about the current Git branch and status.

### 3. Event Bus

-   A central `tokio::sync::broadcast` channel for internal communication from asynchronous producers like MCPs.
-   It decouples these producers from the core application, allowing them to publish `ContextUpdateEvent`s without needing direct access to the `SessionState`.

### 4. Session State (`session.rs`)

-   The single source of truth for all session-related data, including messages and the `active_context` map (`HashMap<String, serde_json::Value>`).
-   It is responsible for persisting this data to the filesystem, providing secure, local-first storage.

### 5. Prompt Builder

-   A utility that reads the `active_context` from the current `Session` within `SessionState`.
-   It formats this context map into a JSON string and wraps it in `<SYSTEM_CONTEXT>` tags for the LLM.

## Data Flow for Short-Term Context

The system uses two parallel flows to update short-term context:

**A) Event-Driven Context (e.g., Workspace changes):**
1.  A local producer (e.g., `WorkspaceMcp`) gathers context and publishes a `ContextUpdateEvent` to the `EventBus`.
2.  A `use_effect` hook in `main.rs` receives the event.
3.  The hook accesses the global `SessionState`, gets the currently active `Session`, and updates the `active_context` map.

**B) Conversational Context (The Race Condition Fix):**
1.  The user sends a message from the `ChatWindow`.
2.  The message is immediately added to the current `Session`'s message list.
3.  A single async task is spawned to handle the entire message processing flow sequentially, preventing race conditions:
    a. The `ConversationProcessor` is triggered. It reads the latest conversation history and uses the **Summary LLM** to generate an updated summary.
    b. The processor's summary is written back into the `active_context` map in the `SessionState`.
    c. **Only after the context is updated**, the `PromptBuilder` reads the fresh `active_context`.
    d. The builder formats the context and combines it with the user's message to create the final prompt, which is then sent to the LLM. This ensures the AI always has the most current context for every message.

This revised architecture correctly separates concerns and provides a robust mechanism for both ambient and conversational short-term memory, while clearly distinguishing between local and external context sources and the different LLM roles.

**C) Tool-Calling Context (The Inner Loop):**
1.  The `LLM Service`, having been provided a list of available tools by the `PromptBuilder`, responds with a structured request to call a tool.
2.  The `StreamManager` detects this request, updates the message in `SessionState` to a `ToolCall` type (triggering the UI to show a "using tool" state), and signals completion to the `chat_coroutine`.
3.  The `chat_coroutine` in `ChatWindow` receives the signal and executes the tool via the `McpManager`.
4.  Upon receiving the result, the coroutine uses the `PromptBuilder` to format the tool's output into a new `<SYSTEM_CONTEXT>` block.
5.  This new context is sent back to the `LLM Service` as a new message (which is hidden from the UI). This creates an inner loop, allowing the LLM to process the tool's result and continue its reasoning, potentially calling more tools if necessary.