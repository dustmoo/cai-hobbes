# Migration Plan: `gemini-rust` to `atlaspathwaysai`

This document outlines the steps to migrate the project from the `gemini-rust` crate to the `atlaspathwaysai` crate for handling LLM interactions and tool-calling.

## Rationale

The `gemini-rust` crate does not expose the necessary schema types, which blocks the implementation of robust tool-call schema sanitization. The `atlaspathwaysai` crate provides a `Tool` trait with a `definition` method that allows for the creation of proper JSON schemas for tools, resolving this limitation.

## Plan

```mermaid
graph TD
    A[Start] --> B{1. Update Dependencies};
    B --> C{2. Clean Up Obsolete Code};
    C --> D{3. Refactor `prompt_builder.rs`};
    D --> E{4. Update LLM Client};
    E --> F{5. Test Implementation};
    F --> G[Finish];

    subgraph "Step 1: Dependencies"
        B --> B1[Remove `gemini-rust` from Cargo.toml];
        B1 --> B2[Add `atlaspathwaysai` to Cargo.toml];
    end

    subgraph "Step 2: Cleanup"
        C --> C1[Delete `src/context/gemini_schema.rs`];
    end

    subgraph "Step 3: Core Logic"
        D --> D1[Implement `atlas::tool::Tool` trait for MCP tools];
        D1 --> D2[Use `Tool::definition()` to generate JSON schema];
    end

    subgraph "Step 4: LLM Client"
        E --> E1[Replace `gemini-rust` client with `atlas` client];
        E1 --> E2[Update API calls in `session.rs` and `llm.rs`];
    end

    subgraph "Step 5: Testing"
        F --> F1[Compile the project];
        F1 --> F2[Manually test tool-calling functionality];
    end
```

## Detailed Steps:

1.  **Update Dependencies:** In `Cargo.toml`, remove the `gemini-rust` crate and add `atlaspathwaysai`.
2.  **Clean Up Obsolete Code:** Delete the `src/context/gemini_schema.rs` file, as it was a workaround for the previous library's limitations.
3.  **Refactor `prompt_builder.rs`:** Refactor the tool-handling logic in `src/context/prompt_builder.rs` to use the `atlas::tool::Tool` trait. This will allow us to define our tool schemas correctly using the `definition` method, which returns a `ToolDefinition` struct containing the JSON schema.
4.  **Update LLM Client:** Replace the `gemini-rust` client with the `atlas` client in `src/session.rs` and any other relevant files, and update the API calls accordingly.
5.  **Test Implementation:** After the code changes are complete, compile the project and perform a manual test to ensure that tool calls are working correctly.