// src/components/focus_context.rs
// Signal-based focus coordination for keyboard event handling.
// When a modal is open, it claims focus ownership via this signal,
// preventing other components from handling keyboard events.

/// Represents which component currently "owns" keyboard focus.
/// Components check this signal before handling keyboard events.
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub enum FocusContext {
    /// Default state - chat input handles keyboard events
    #[default]
    ChatInput,
    /// New Chat with Memory modal is open
    NewChatMemoryModal,
    
    // The following variants are currently unused but reserved for future
    // centralized focus management. Some modals currently use local focus
    // handling (e.g., ConfirmSaveModal) or are not yet fully integrated (CommentModal).
    // We allow dead code here to keep the centralized registry intact for implementation.
    #[allow(dead_code)]
    CommentModal,
    #[allow(dead_code)]
    ConfirmDeleteModal,
    #[allow(dead_code)]
    ConfirmSaveModal,
    #[allow(dead_code)]
    ConflictModal,
    #[allow(dead_code)]
    SettingsPanel,
}
