# Feature Plan: Chatbot File Support (Revised)

This document outlines the architectural changes and implementation plan required to add file support to Hobbes, allowing users to send images and other documents to multimodal LLMs like Gemini. This revision incorporates user feedback for drag-and-drop functionality and future extensibility.

## User Story

As a user, I want to be able to attach files (starting with images) to my chat messages by clicking a button or by dragging and dropping them onto the chat input, so that multimodal models can see and analyze them.

## Architectural Impact

Adding extensible file support will touch several key areas of the application, transforming the input pipeline from text-only to multimodal.

1.  **UI (`ChatInput`)**: The component must be enhanced to act as a dropzone for files, manage a list of pending attachments, and display previews for them.
2.  **State Management (`SessionState`)**: The core `Message` struct must be updated to support a list of generic file attachments, not just a single image.
3.  **Prompt Construction (`PromptBuilder`)**: This service needs a significant update to construct a valid multimodal request payload for the Gemini API, iterating over multiple attachments and including their specific data and MIME types.
4.  **Message Rendering (`MessageList`)**: The chat history view must be updated to display sent files within the message bubbles.

## Implementation Plan

The implementation will be broken down into the following phases and tasks:

### Phase 1: Extensible Data Model & UI Foundation

This phase focuses on creating a flexible data structure and the necessary UI handlers for file input.

-   **Task 1.1: Create an Extensible `Attachment` Data Structure**
    -   **File:** `packages/hobbes_core/src/models.rs`
    -   **Action:** Create a new `Attachment` struct containing `file_name: String`, `mime_type: String`, and `data: String` (for the base64 data URI). Modify the `Message` struct to include `attachments: Vec<Attachment>`. This design is not limited to images and can support other file types in the future.

-   **Task 1.2: Implement Drag-and-Drop Functionality**
    -   **File:** `src/components/chat_input.rs`
    -   **Action:** Implement `ondragover`, `ondragleave`, and `ondrop` event handlers for the main chat input container. Provide visual feedback (e.g., a highlighted border) when a file is being dragged over the component. The `ondrop` handler will read the file(s), convert them to base64 data URIs, and add them to the component's state.

-   **Task 1.3: Implement File Picker Button**
    -   **File:** `src/components/chat_input.rs`
    -   **Action:** Add a file picker button (e.g., a paperclip icon) that opens a native file dialog. This dialog should allow multiple file selections.

-   **Task 1.4: Implement Attachment Preview UI**
    -   **File:** `src/components/chat_input.rs`
    -   **Action:** Create a preview area above the text input that displays thumbnails for all staged attachments. Each thumbnail should have a button to remove it from the list before sending.

### Phase 2: Multimodal Prompt Construction

This phase focuses on adapting the backend logic to correctly format the LLM request with multiple, varied attachments.

-   **Task 2.1: Refactor `PromptBuilder` for Multiple Attachments**
    -   **File:** `src/context/prompt_builder.rs`
    -   **Action:** Modify the `PromptBuilder` service to iterate over the `attachments` vector in each message.

-   **Task 2.2: Implement Generic Gemini API Payload Formatting**
    -   **File:** `src/context/prompt_builder.rs`
    -   **Action:** For each attachment, implement the serialization logic to create a `Part` with `inlineData` containing the base64 string and the correct `mime_type` from the `Attachment` struct. This will correctly format the request for the Gemini API.

### Phase 3: Rendering & Display

This phase ensures that sent attachments are correctly displayed in the chat history.

-   **Task 3.1: Render Attachments in `MessageList`**
    -   **File:** `src/components/message_list.rs`
    -   **Action:** Update the message rendering logic to iterate over the `attachments` vector. For image MIME types, render an `<img>` tag. For other file types, render a placeholder icon and the file name.

-   **Task 3.2: Style Attachment Previews**
    -   **File:** `assets/tailwind.css` (or similar)
    -   **Action:** Add CSS rules to ensure that attachments within chat bubbles are appropriately styled and sized to maintain a clean and readable UI.

### Phase 4: UAT & Documentation

This final phase is for verification and updating project memory.

-   **Task 4.1: End-to-End User Acceptance Testing (UAT)**
    -   **Action:** Perform a full test of the feature:
        1.  Select multiple images using the file picker.
        2.  Drag and drop an image onto the input area.
        3.  Verify thumbnails appear correctly and can be removed.
        4.  Send the message with attachments.
        5.  Verify the attachments appear correctly in the chat history.
        6.  Verify the LLM receives the images and can respond contextually.

-   **Task 4.2: Update `ARCHITECTURE.md`**
    -   **Action:** Add a new section to the architecture document describing the extensible multimodal input flow and the components involved.

-   **Task 4.3: Update ConPort Progress**
    -   **Action:** Log all completed tasks for this feature into the ConPort progress log.