# Session State Controller Implementation Plan

**Date:** January 27, 2026  
**Status:** IN PROGRESS - Phase 1 & 2 Complete (StreamManager migrated)  
**Priority:** HIGH - Panic Regression Fix

---

## Executive Summary

The current implementation has **47 direct `session_state.write()` calls** scattered across 9 files, with 17 occurring inside async tasks in `stream_manager.rs`. This violates **Pattern 8 (Atomic State Update)** and **Pattern 23 (Async Signal Integrity)** from our Dioxus patterns, causing `AlreadyBorrowedMut` panics when render-path `.read()` calls collide with async `.write()` calls.

The fix requires implementing a **centralized SessionStateController** that serializes all mutations through a channel, ensuring:
1. No overlapping borrows
2. All writes are atomic
3. Render-path reads never conflict with async writes

---

## Root Cause Analysis

### The Collision Pattern
```
stream_manager.rs spawns async task
    ↓
Task calls session_state.write() inside streaming loop (lines 131-144, etc.)
    ↓ CONCURRENT
MessageList component calls session_state.read() for render (line 118)
ChatInput component calls session_state.read() for render (line 916)
    ↓
PANIC: AlreadyBorrowedMut
```

### Problematic Files (Writes)
| File | Write Count | Issue |
|------|-------------|-------|
| `stream_manager.rs` | 17 | Inside spawned async tasks |
| `chat.rs` | 14 | Event handlers, some effects |
| `session_manager.rs` | 5 | Click/blur handlers |
| `settings_panel.rs` | 4 | Import/export |
| `skill_call_display.rs` | 2 | Approve handlers |
| `tool_call_display.rs` | 2 | Approve handlers |
| `chat_input.rs` | 2 | Create session |
| `main.rs` | 1 | Delete session |

### Suppressions That Hid the Problem
```rust
// main.rs:2
#![allow(clippy::await_holding_invalid_type)]

// stream_manager.rs:1
#![allow(clippy::await_holding_invalid_type)]
```

---

## Architecture: SessionStateController

### Design Principles
1. **Single Writer**: All mutations go through one coroutine
2. **Channel-Based**: `mpsc::unbounded_channel` for fire-and-forget updates
3. **Snapshot Reads**: Render paths use `.peek()` or local clones
4. **Atomic Batching**: Related updates are batched into single commits
5. **Async-Safe**: No `.write()` guards held across `.await` boundaries

### New File: `src/state/session_controller.rs`

```rust
//! Centralized controller for SessionState mutations.
//! 
//! All components should use this controller to update session state,
//! rather than calling session_state.write() directly. This prevents
//! borrow conflicts between async tasks and render-path reads.

use crate::components::chat::Message;
use crate::components::shared::{MessageContent, ToolCallRecord, ToolCallStatus, UsageData};
use crate::session::SessionState;
use dioxus::prelude::*;
use std::sync::Arc;
use tokio::sync::mpsc::{self, UnboundedSender};
use uuid::Uuid;

/// Commands that can be sent to the SessionStateController
#[derive(Clone, Debug)]
pub enum SessionCommand {
    // === Message Operations ===
    /// Update a message's content
    UpdateMessageContent {
        message_id: Uuid,
        content: MessageContent,
    },
    /// Update a message's usage data
    UpdateMessageUsage {
        message_id: Uuid,
        usage: UsageData,
    },
    /// Add accumulated usage to the active session
    AddSessionUsage {
        cost: f64,
        tokens: i32,
        turns: i32,
    },
    /// Remove a message from the active session
    RemoveMessage {
        message_id: Uuid,
    },
    /// Push a new message to the active session
    PushMessage {
        message: Message,
    },
    
    // === Tool Call Operations ===
    /// Update a tool call's status and response
    UpdateToolCallResult {
        message_id: Uuid,
        status: ToolCallStatus,
        response: String,
    },
    /// Upgrade a message to a PermissionRequest
    UpgradeToPermissionRequest {
        message_id: Uuid,
        payload: crate::components::shared::PermissionRequestPayload,
    },
    /// Add tool call records to history
    ExtendToolCallHistory {
        records: Vec<ToolCallRecord>,
    },
    /// Clear tool call history
    ClearToolCallHistory,
    
    // === Session Operations ===
    /// Create a new session and set it as active
    CreateSession {
        /// Optional callback to receive the new session ID
        response_tx: Option<tokio::sync::oneshot::Sender<String>>,
    },
    /// Delete a session
    DeleteSession {
        session_id: String,
    },
    /// Set the active session
    SetActiveSession {
        session_id: String,
    },
    /// Update the session name
    UpdateSessionName {
        session_id: String,
        new_name: String,
    },
    /// Touch the active session (update last_updated timestamp)
    TouchActiveSession,
    /// Insert/update a session (for import)
    UpsertSession {
        session_id: String,
        session: crate::session::Session,
    },
    
    // === Persistence ===
    /// Save state to disk (fire-and-forget)
    Save,
    /// Save state to disk asynchronously on a background thread
    SaveAsync,
    
    // === Window ===
    /// Update window size
    UpdateWindowSize {
        width: f64,
        height: f64,
    },
}

/// Controller handle that can be cloned and passed to components
#[derive(Clone, Copy)]
pub struct SessionStateController {
    tx: Signal<UnboundedSender<SessionCommand>>,
    /// Direct read access for snapshot operations (NOT subscriptions!)
    session_state: Signal<SessionState>,
}

impl SessionStateController {
    /// Send a command to the controller
    pub fn send(&self, cmd: SessionCommand) {
        if let Err(e) = self.tx.read().send(cmd) {
            tracing::error!("Failed to send session command: {}", e);
        }
    }
    
    /// Send multiple commands atomically
    pub fn send_batch(&self, cmds: impl IntoIterator<Item = SessionCommand>) {
        let tx = self.tx.read();
        for cmd in cmds {
            if let Err(e) = tx.send(cmd) {
                tracing::error!("Failed to send session command in batch: {}", e);
                break;
            }
        }
    }
    
    /// Get a snapshot of the current state for reading
    /// 
    /// Use this for render paths. Does NOT subscribe to changes.
    /// For reactive rendering, use the session_state signal directly.
    pub fn snapshot(&self) -> SessionState {
        self.session_state.peek().clone()
    }
    
    /// Get the underlying signal for reactive subscriptions
    /// 
    /// Use this in RSX for reactive rendering.
    pub fn state_signal(&self) -> Signal<SessionState> {
        self.session_state
    }
    
    // === Convenience Methods ===
    
    pub fn update_message_content(&self, message_id: Uuid, content: MessageContent) {
        self.send(SessionCommand::UpdateMessageContent { message_id, content });
    }
    
    pub fn update_message_usage(&self, message_id: Uuid, usage: UsageData) {
        self.send(SessionCommand::UpdateMessageUsage { message_id, usage });
    }
    
    pub fn push_message(&self, message: Message) {
        self.send(SessionCommand::PushMessage { message });
    }
    
    pub fn remove_message(&self, message_id: Uuid) {
        self.send(SessionCommand::RemoveMessage { message_id });
    }
    
    pub fn update_tool_call_result(&self, message_id: Uuid, status: ToolCallStatus, response: String) {
        self.send(SessionCommand::UpdateToolCallResult { message_id, status, response });
    }
    
    pub fn create_session(&self) {
        self.send(SessionCommand::CreateSession { response_tx: None });
    }
    
    /// Create a session and wait for the new ID
    pub async fn create_session_with_id(&self) -> Option<String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.send(SessionCommand::CreateSession { response_tx: Some(tx) });
        rx.await.ok()
    }
    
    pub fn delete_session(&self, session_id: String) {
        self.send(SessionCommand::DeleteSession { session_id });
    }
    
    pub fn set_active_session(&self, session_id: String) {
        self.send(SessionCommand::SetActiveSession { session_id });
    }
    
    pub fn save(&self) {
        self.send(SessionCommand::Save);
    }
    
    pub fn save_async(&self) {
        self.send(SessionCommand::SaveAsync);
    }
    
    pub fn touch_active_session(&self) {
        self.send(SessionCommand::TouchActiveSession);
    }
}

/// Initialize the SessionStateController.
/// 
/// Call this once in `main.rs` BEFORE providing session_state as context.
/// Returns the controller and the session_state signal.
pub fn use_session_controller() -> SessionStateController {
    // Get the session_state signal from context
    let mut session_state = use_context::<Signal<SessionState>>();
    
    // Create a channel for commands
    let (tx, mut rx) = mpsc::unbounded_channel::<SessionCommand>();
    let tx_signal = use_context_provider(|| Signal::new(tx));
    
    // Spawn the command processor coroutine
    use_coroutine(move |_: UnboundedReceiver<()>| async move {
        tracing::info!("SessionStateController: Command processor started");
        
        while let Some(cmd) = rx.recv().await {
            // Process each command with a short-lived write guard
            match cmd {
                SessionCommand::UpdateMessageContent { message_id, content } => {
                    let mut state = session_state.write();
                    if let Some(msg) = state.get_message_mut(&message_id) {
                        msg.content = content;
                    }
                }
                
                SessionCommand::UpdateMessageUsage { message_id, usage } => {
                    let mut state = session_state.write();
                    if let Some(msg) = state.get_message_mut(&message_id) {
                        msg.usage = Some(usage);
                    }
                }
                
                SessionCommand::AddSessionUsage { cost, tokens, turns } => {
                    let mut state = session_state.write();
                    if let Some(session) = state.get_active_session_mut() {
                        session.accumulated_cost += cost;
                        session.accumulated_tokens += tokens;
                        session.accumulated_turns += turns;
                    }
                }
                
                SessionCommand::RemoveMessage { message_id } => {
                    session_state.write().remove_message(&message_id);
                }
                
                SessionCommand::PushMessage { message } => {
                    let mut state = session_state.write();
                    if let Some(session) = state.get_active_session_mut() {
                        session.messages.push(message);
                    }
                }
                
                SessionCommand::UpdateToolCallResult { message_id, status, response } => {
                    let mut state = session_state.write();
                    if let Some(msg) = state.get_message_mut(&message_id) {
                        if let MessageContent::ToolCall(tc) = &mut msg.content {
                            tc.status = status;
                            tc.response = response;
                        }
                    }
                }
                
                SessionCommand::UpgradeToPermissionRequest { message_id, payload } => {
                    let mut state = session_state.write();
                    if let Some(msg) = state.get_message_mut(&message_id) {
                        msg.content = MessageContent::PermissionRequest(payload);
                    }
                }
                
                SessionCommand::ExtendToolCallHistory { records } => {
                    session_state.write().tool_call_history.extend(records);
                }
                
                SessionCommand::ClearToolCallHistory => {
                    session_state.write().tool_call_history.clear();
                }
                
                SessionCommand::CreateSession { response_tx } => {
                    let new_id = session_state.write().create_session();
                    if let Some(tx) = response_tx {
                        let _ = tx.send(new_id);
                    }
                }
                
                SessionCommand::DeleteSession { session_id } => {
                    session_state.write().delete_session(&session_id);
                }
                
                SessionCommand::SetActiveSession { session_id } => {
                    session_state.write().set_active_session(session_id);
                }
                
                SessionCommand::UpdateSessionName { session_id, new_name } => {
                    session_state.write().update_session_name(&session_id, new_name);
                }
                
                SessionCommand::TouchActiveSession => {
                    session_state.write().touch_active_session();
                }
                
                SessionCommand::UpsertSession { session_id, session } => {
                    session_state.write().sessions.insert(session_id, session);
                }
                
                SessionCommand::Save => {
                    let state = session_state.read().clone();
                    if let Err(e) = state.save() {
                        tracing::error!("Failed to save session state: {}", e);
                    }
                }
                
                SessionCommand::SaveAsync => {
                    let state = session_state.read().clone();
                    SessionState::save_async(state);
                }
                
                SessionCommand::UpdateWindowSize { width, height } => {
                    session_state.write().update_window_size(width, height);
                }
            }
        }
        
        tracing::warn!("SessionStateController: Command processor exited");
    });
    
    SessionStateController {
        tx: tx_signal,
        session_state,
    }
}
```

---

## Implementation Phases

### Phase 1: Foundation (Create Controller Infrastructure)

**Files to Create:**
- `src/state/mod.rs` - Module declaration
- `src/state/session_controller.rs` - Controller implementation

**Files to Modify:**
- `src/main.rs` - Initialize controller after session_state, provide as context
- `src/lib.rs` or `mod.rs` - Add `mod state;`

**Changes in `main.rs`:**
```rust
// Line 152-162: After session_state is provided
let mut session_state = use_context_provider(|| {
    // ... existing code ...
    Signal::new(state)
});

// NEW: Initialize the session state controller
let session_controller = state::session_controller::use_session_controller();
use_context_provider(|| session_controller);
```

### Phase 2: Migrate StreamManager (High Priority - 17 writes)

This is the primary source of conflicts. All 17 `.write()` calls need migration.

**Current Pattern (PROBLEM):**
```rust
// stream_manager.rs:131-143
let mut state = self.session_state.write();
if let Some(msg) = state.get_message_mut(&message_id) {
    // ... modify msg ...
}
```

**New Pattern (SOLUTION):**
```rust
// Use controller instead
let controller = self.session_controller;
controller.update_message_content(message_id, MessageContent::Text {
    content: final_text_for_this_turn.clone(),
    thought_signature: thought_signature_for_this_turn.clone(),
    thought_summary: thought_summary_for_this_turn.clone(),
});
```

**StreamManager Changes:**
1. Add `session_controller: SessionStateController` to `StreamManagerContext`
2. Replace all 17 `session_state.write()` calls with controller methods
3. Remove `#![allow(clippy::await_holding_invalid_type)]` header
4. Keep `session_state` field for read-only snapshots

### Phase 3: Migrate Chat Component (Medium Priority - 14 writes)

**Files:**
- `src/components/chat.rs`

**Pattern Changes:**
- Event handlers: Use controller directly
- Effects: Use controller, remove inline `.write()` calls
- Most are in `onclick` handlers so migration is straightforward

### Phase 4: Migrate Remaining Components (Lower Priority - 14 writes)

**Files and Counts:**
- `session_manager.rs` (5 writes) - Session CRUD
- `settings_panel.rs` (4 writes) - Import/export
- `tool_call_display.rs` (2 writes) - Approval handlers
- `skill_call_display.rs` (2 writes) - Approval handlers  
- `chat_input.rs` (2 writes) - Create session
- `main.rs` (1 write) - Delete session

### Phase 5: Update Render Paths

**Files with Read Conflicts:**
- `message_list.rs` (lines 85, 118)
- `chat_input.rs` (line 916)
- `session_manager.rs` (line 51)

**Changes:**
```rust
// OLD (causes conflict with async writes)
let state = session_state.read();

// NEW (safe snapshot, no conflict)
let controller = use_context::<SessionStateController>();
let state = controller.snapshot();

// OR for reactive rendering:
let state_signal = controller.state_signal();
// ... use state_signal.read() in RSX only ...
```

### Phase 6: Remove Clippy Suppressions

**Files:**
- `src/main.rs` - Remove line 2: `#![allow(clippy::await_holding_invalid_type)]`
- `src/components/stream_manager.rs` - Remove line 1: `#![allow(clippy::await_holding_invalid_type)]`

**Verification:**
Run `cargo clippy` and ensure no `await_holding_invalid_type` warnings appear.

---

## Migration Checklist

### Phase 1: Foundation ✅ COMPLETE
- [x] Create `src/state/mod.rs`
- [x] Create `src/state/session_controller.rs`
- [x] Add `mod state;` to main
- [x] Initialize controller in `main.rs` after session_state
- [x] Provide controller as context
- [x] Verify builds: `cargo check`

### Phase 2: StreamManager Migration ✅ COMPLETE
- [x] Add `session_controller` to `StreamManagerContext`
- [x] Migrate line 131-144 (first text chunk update)
- [x] Migrate line 153-179 (error handling)
- [x] Migrate line 192-262 (tool call message creation)
- [x] Migrate line 380-400 (tool result update)
- [x] Migrate line 420-430 (usage update)
- [x] Migrate line 441-453 (final text update)
- [x] Migrate line 478-501 (tool history, permission requests)
- [x] Migrate line 529-541 (touch session, save) - Note: summarizer still uses write() - low risk
- [x] Migrate line 573 (remove message)
- [x] Remove `#![allow(clippy::await_holding_invalid_type)]`
- [x] Verify: `cargo check`, `cargo test` (83 tests pass)

### Phase 3: Chat Component Migration
- [ ] Add controller consumption
- [ ] Migrate all 14 `.write()` calls
- [ ] Verify: `cargo check`

### Phase 4: Other Components Migration
- [ ] `session_manager.rs` (5 writes)
- [ ] `settings_panel.rs` (4 writes)
- [ ] `tool_call_display.rs` (2 writes)
- [ ] `skill_call_display.rs` (2 writes)
- [ ] `chat_input.rs` (2 writes)
- [ ] `main.rs` (1 write)
- [ ] Verify: `cargo check`

### Phase 5: Render Path Updates
- [ ] Update `message_list.rs` to use snapshots
- [ ] Update `chat_input.rs:916` to use snapshots
- [ ] Update `session_manager.rs:51` to use snapshots
- [ ] Verify: `cargo check`

### Phase 6: Cleanup
- [x] Remove suppressions from `stream_manager.rs`
- [ ] Remove suppressions from `main.rs`
- [ ] Run `cargo clippy` with no warnings
- [x] Run `cargo test` (83 pass)
- [ ] Manual testing: Start stream, verify no panics

---

## Testing Strategy

### Unit Tests
1. Controller processes commands correctly
2. Commands are serialized (no race conditions)
3. Snapshot reads don't conflict with pending writes

### Integration Tests
1. Start a chat stream → panics should not occur
2. Switch sessions during stream → no panics
3. Delete session during stream → graceful handling

### Manual Testing
1. Rapid message sending
2. Opening settings panel while streaming
3. Session switching during active generation

---

## Rollback Plan

If issues are discovered:
1. Revert controller usage in components (keep controller code)
2. Re-add `session_state.write()` calls
3. Re-add Clippy suppressions temporarily
4. Investigate specific conflict patterns

---

## Knowledge Items to Update

After implementation, update:
- `Dioxus Development Patterns` → Add Pattern 44 (Centralized State Controller)
- `Session Memory and Context Management` → Document controller architecture

---

## Approval

- [ ] Architecture reviewed
- [ ] Risk assessment complete
- [ ] Testing strategy approved
- [ ] Ready for implementation

---

**Estimated Effort:** 4-6 hours  
**Risk Level:** Medium (significant refactor, but mechanically straightforward)  
**Dependencies:** None - self-contained change
