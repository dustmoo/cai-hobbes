// src/components/focus_context.rs
// Signal-based focus coordination for keyboard event handling.
// When a modal is open, it claims focus ownership via this signal,
// preventing other components from handling keyboard events.

/// Represents which component currently "owns" keyboard focus.
/// Components check this signal before handling keyboard events.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub enum FocusContext {
    /// Default state - chat input handles keyboard events
    #[default]
    ChatInput,
    /// New Chat with Memory modal is open
    NewChatMemoryModal,
    /// Comment modal is open
    CommentModal,
    /// Confirm delete modal is open
    ConfirmDeleteModal,
    /// Confirm save modal is open
    ConfirmSaveModal,
    /// Conflict modal is open
    ConflictModal,
    /// Settings panel is focused
    SettingsPanel,
    /// Skill editor overlay is open
    SkillEditorModal,
}
