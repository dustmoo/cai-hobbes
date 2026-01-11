use dioxus::{html::HasFileData, prelude::*};
use dioxus_free_icons::{icons::fi_icons, Icon};
use rfd::FileDialog;
use std::time::SystemTime;
use base64::{engine::general_purpose, Engine as _};
use image::{imageops, ImageFormat, DynamicImage};
use std::io::Cursor;

#[cfg(debug_assertions)]
use crate::context::prompt_builder::PromptBuilder;
use crate::settings::{Settings, SettingsManager};
use hobbes_core::models::Attachment;

use crate::processing::summarization_scheduler::{SchedulerSignal};
use crate::components::focus_context::FocusContext;


#[derive(Clone, Debug, PartialEq)]
pub enum ChatCommand {
    ToggleProfile,
    OpenAttachments,
    ToggleSettings,
    ToggleHistory,
    ToggleMcp,
    NewChat,
    NewChatWithMemory,
    ScrollToBottom,
    FocusChat,
    SubmitModal,
    CloseModal,
    SaveModal,
}

#[component]
pub fn ChatInput(
    is_sending: Signal<bool>,
    has_new_comments: Signal<bool>,
    has_pending_approvals: Signal<bool>,
    on_send: EventHandler<(String, Vec<Attachment>)>,
    on_cancel: EventHandler<()>,
    on_interaction: EventHandler<()>,
    on_toggle_sessions: EventHandler<()>,
    on_toggle_settings: EventHandler<()>,
    on_toggle_mcp_manager: EventHandler<()>,
    on_new_chat_with_memory: EventHandler<()>,
) -> Element {
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    let _settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let _mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let _mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let mut draft = use_context::<Signal<String>>();
    let mut is_dragging = use_signal(|| false);
    let mut attachments = use_signal(Vec::<Attachment>::new);
    let mut is_processing_attachments = use_signal(|| false);
    let mut show_profile_selector = use_signal(|| false);
    let mut show_new_chat_menu = use_signal(|| false);
    let scheduler = use_context::<Coroutine<SchedulerSignal>>();
    let focus_context = use_context::<Signal<FocusContext>>();
    
    // Listen for global chat commands (from menu hotkeys)
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    use_effect(move || {
        if let Some(cmd) = chat_command.read().clone() {
            tracing::debug!("ChatInput received ChatCommand: {:?}", cmd);
            match cmd {
                ChatCommand::ToggleProfile => {
                    show_profile_selector.set(!show_profile_selector());
                }
                ChatCommand::ToggleSettings => {
                    on_toggle_settings.call(());
                }
                ChatCommand::ToggleHistory => {
                    on_toggle_sessions.call(());
                }
                ChatCommand::ToggleMcp => {
                     on_toggle_mcp_manager.call(());
                }
                ChatCommand::NewChat => {
                    tracing::info!("ChatCommand::NewChat triggered");
                    session_state.write().create_session();
                }
                ChatCommand::NewChatWithMemory => {
                    tracing::info!("ChatCommand::NewChatWithMemory triggered");
                    on_new_chat_with_memory.call(());
                }
                ChatCommand::OpenAttachments => {
                    // Trigger attachment dialog
                    let mut attachments = attachments;
                    let mut is_processing_attachments = is_processing_attachments;
                    spawn(async move {
                        is_processing_attachments.set(true);
                        if let Some(files) = FileDialog::new()
                            .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
                            .pick_files()
                        {
                            for file_path in files {
                                if let Ok(file_data) = tokio::fs::read(&file_path).await {
                                    let file_name = file_path
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .to_string();
                                    let extension = file_path
                                        .extension()
                                        .and_then(std::ffi::OsStr::to_str)
                                        .unwrap_or("");
                                    if let Some(mime_type) = get_mime_type(extension) {
                                        if let Some(attachment) = process_image_data(
                                            file_name,
                                            mime_type.to_string(),
                                            file_data,
                                        )
                                        .await
                                        {
                                            attachments.write().push(attachment);
                                        }
                                    } else {
                                        tracing::warn!(
                                            "Unsupported file type selected: {:?}",
                                            file_path
                                        );
                                    }
                                }
                            }
                        }
                        is_processing_attachments.set(false);
                    });
                }
                ChatCommand::ScrollToBottom => {
                    // Handled by MessageList
                }
                ChatCommand::FocusChat => {
                     let _ = document::eval(r#"
                        const el = document.getElementById('chat-textarea');
                        if (el) { el.focus(); }
                    "#);
                }
                ChatCommand::SubmitModal | ChatCommand::CloseModal | ChatCommand::SaveModal => {
                    // Handled by active modal
                }
            }
            // Reset command to avoid re-triggering
            spawn(async move {
                chat_command.set(None);
            });
        }
    });

    let mut send_message = move || {
        if *is_sending.read() || *is_processing_attachments.read() {
            tracing::warn!("'send_message' blocked: already sending or processing attachments.");
            return;
        }
        let user_message = draft.read().clone();
        if user_message.is_empty() && attachments.read().is_empty() && !*has_new_comments.read() && !*has_pending_approvals.read() {
            return;
        }
        on_send.call((user_message, attachments.read().clone()));
        draft.set("".to_string());
        attachments.set(Vec::new());
        let _ = document::eval(r#"
            const el = document.getElementById('chat-textarea');
            if (el) { el.style.height = 'auto'; }
        "#);
    };

    rsx! {
        if !attachments.read().is_empty() {
            div {
                class: "flex items-center space-x-2 p-2 bg-dark-section rounded-t-lg",
                for (index, attachment) in attachments.read().iter().enumerate() {
                    div {
                        class: "flex items-center space-x-2 bg-dark-card p-1 rounded",
                        span {
                            class: "text-sm text-gray-300",
                            "{attachment.file_name}"
                        }
                        button {
                            class: "p-1 rounded-full text-gray-400 hover:bg-dark-input hover:text-white",
                            onclick: move |_| {
                                attachments.write().remove(index);
                            },
                            Icon {
                                width: 16,
                                height: 16,
                                icon: fi_icons::FiX
                            }
                        }
                    }
                }
            }
        }
        div {
            class: if *is_dragging.read() {
                "bg-dark-bg p-4 border-t-2 border-dashed border-primary-500"
            } else {
                "bg-dark-bg p-4 border-t border-primary-700"
            },
            onmousedown: |e| e.stop_propagation(),
            ondragover: move |event| {
                event.prevent_default();
                is_dragging.set(true);
            },
            ondragleave: move |_| {
                is_dragging.set(false);
            },
            ondrop: move |event| {
                event.prevent_default();
                is_dragging.set(false);
                if let Some(file_engine) = event.files() {
                    is_processing_attachments.set(true);
                    spawn(async move {
                        let files = file_engine.files();
                        for file_name in &files {
                            let extension = std::path::Path::new(file_name)
                                .extension()
                                .and_then(std::ffi::OsStr::to_str)
                                .unwrap_or("");
                            if let Some(mime_type) = get_mime_type(extension) {
                                if let Some(file_data) = file_engine.read_file(file_name).await {
                                    if let Some(attachment) = process_image_data(
                                        file_name.clone(),
                                        mime_type.to_string(),
                                        file_data,
                                    )
                                    .await
                                    {
                                        attachments.write().push(attachment);
                                    }
                                }
                            } else {
                                tracing::warn!("Unsupported file type dropped: {}", file_name);
                            }
                        }
                        is_processing_attachments.set(false);
                    });
                }
            },
            div {
                class: "flex items-center space-x-3",
                button {
                    class: "p-2 rounded-full text-gray-400 hover:bg-dark-card hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                    onclick: move |_| on_toggle_settings.call(()),
                    Icon {
                        width: 20,
                        height: 20,
                        icon: fi_icons::FiSettings
                    }
                }
                button {
                    class: "p-2 rounded-full text-gray-400 hover:bg-dark-card hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                    onclick: move |_| on_toggle_sessions.call(()),
                    Icon {
                        width: 20,
                        height: 20,
                        icon: fi_icons::FiClock
                    }
                }
                button {
                    class: "p-2 rounded-full text-gray-400 hover:bg-dark-card hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                    onclick: move |_| on_toggle_mcp_manager.call(()),
                    Icon {
                        width: 20,
                        height: 20,
                        icon: fi_icons::FiPackage
                    }
                }
                // Composio Profile Selector
                {
                    let settings_read = _settings.read();
                    let profiles = &settings_read.composio_profiles;
                    
                    if profiles.len() > 1 {
                        let active_profile = settings_read.get_active_profile().cloned().unwrap_or_default();
                        let active_initial = active_profile.name.chars().next().unwrap_or('?').to_uppercase();
                        
                        rsx! {
                            div {
                                class: "relative",
                                button {
                                    class: format!("w-8 h-8 rounded-full {} border border-primary-700 flex items-center justify-center text-xs font-bold text-white hover:brightness-110 hover:border-primary-500 transition-all focus:outline-none focus:ring-2 focus:ring-primary-600 shadow-md", active_profile.color),
                                    onclick: move |_| show_profile_selector.set(!show_profile_selector()),
                                    "{active_initial}"
                                }
                                
                                if show_profile_selector() {
                                    div {
                                        class: "absolute bottom-10 left-0 w-56 bg-dark-card border border-primary-700 rounded-lg shadow-xl z-50 overflow-hidden py-1",
                                        for (index, profile) in profiles.iter().enumerate() {
                                            if profile.name != active_profile.name {
                                                button {
                                                    class: "w-full text-left px-4 py-2 text-sm text-gray-300 hover:bg-primary-900/50 hover:text-white transition-colors flex items-center justify-between",
                                                    onclick: {
                                                        let new_profile_name = profile.name.clone();
                                                        let mut settings = _settings;
                                                        let settings_manager = settings_manager;
                                                        let scheduler = scheduler.clone();
                                                        move |_| {
                                                            settings.write().active_composio_profile = Some(new_profile_name.clone());
                                                            // Auto-save changes
                                                            if let Err(e) = settings_manager.read().save(&settings.read()) {
                                                                tracing::error!("Failed to save profile change: {}", e);
                                                            }
                                                            // Trigger summary refresh
                                                            scheduler.send(SchedulerSignal::ForceRefresh);
                                                            show_profile_selector.set(false);
                                                        }
                                                    },
                                                    div {
                                                        class: "flex items-center space-x-2",
                                                        span {
                                                            class: format!("w-6 h-6 rounded-full {} border border-gray-700 flex items-center justify-center text-[10px] text-white font-bold shadow-sm", profile.color),
                                                            "{profile.name.chars().next().unwrap_or('?').to_uppercase()}"
                                                        }
                                                        span { class: "truncate", "{profile.name}" }
                                                    }
                                                    // Hotkey hint (1-indexed)
                                                    if index < 9 {
                                                        span {
                                                            class: "text-xs text-gray-500 font-mono",
                                                            "⌘{index + 1}"
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                // Click outside handler to close dropdown
                                if show_profile_selector() {
                                    div {
                                        class: "fixed inset-0 z-40",
                                        onclick: move |_| show_profile_selector.set(false)
                                    }
                                }
                            }
                        }
                    } else {
                        rsx!({})
                    }
                }
                button {
                    class: "p-2 rounded-full text-gray-400 hover:bg-dark-card hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                    onclick: move |_| {
                        let mut attachments = attachments;
                        spawn(async move {
                            is_processing_attachments.set(true);
                            if let Some(files) = FileDialog::new()
                                .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
                                .pick_files()
                            {
                                for file_path in files {
                                    if let Ok(file_data) = tokio::fs::read(&file_path).await {
                                        let file_name = file_path
                                            .file_name()
                                            .unwrap_or_default()
                                            .to_string_lossy()
                                            .to_string();
                                        let extension = file_path
                                            .extension()
                                            .and_then(std::ffi::OsStr::to_str)
                                            .unwrap_or("");
                                        if let Some(mime_type) = get_mime_type(extension) {
                                            if let Some(attachment) = process_image_data(
                                                file_name,
                                                mime_type.to_string(),
                                                file_data,
                                            )
                                            .await
                                            {
                                                attachments.write().push(attachment);
                                            }
                                        } else {
                                            tracing::warn!(
                                                "Unsupported file type selected: {:?}",
                                                file_path
                                            );
                                        }
                                    }
                                }
                            }
                            is_processing_attachments.set(false);
                        });
                    },
                    Icon {
                        width: 20,
                        height: 20,
                        icon: fi_icons::FiPaperclip
                    }
                }
                textarea {
                    id: "chat-textarea",
                    class: "flex-1 py-2 px-4 rounded-xl bg-dark-input border border-primary-700 text-dark-text placeholder-gray-500 focus:outline-none focus:ring-2 focus:ring-primary-500 resize-none overflow-y-auto",
                    style: "max-height: 50vh;",
                    rows: "1",
                    placeholder: "Type your message...",
                    value: "{draft}",
                    oninput: move |event| {
                        scheduler.send(SchedulerSignal::Activity);
                        
                        // Auto-resize logic
                        let _ = document::eval(r#"
                            const el = document.getElementById('chat-textarea');
                            if (el) {
                                el.style.height = 'auto';
                                el.style.height = (el.scrollHeight) + 'px';
                            }
                        "#);
                        
                        draft.set(event.value());
                    },
                    onkeydown: move |event| {
                        tracing::debug!("ChatInput::onkeydown - Key: {:?}, Modifiers: {:?}, Context: {:?}", event.key(), event.data.modifiers(), *focus_context.read());
                        // If a modal owns focus, don't handle keyboard events
                        if *focus_context.read() != FocusContext::ChatInput {
                            tracing::debug!("ChatInput ignoring event - Focus owns: {:?}", *focus_context.read());
                            return;
                        }
                        
                        scheduler.send(SchedulerSignal::Activity);
                        let modifiers = event.data.modifiers();

                        // Handle Enter key submission (must be BEFORE modifier guard to catch Cmd+Enter)
                        if event.key() == Key::Enter {
                            let is_explicit_submit = modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::CONTROL);
                            let is_shift = modifiers.contains(Modifiers::SHIFT);

                            // Submit if:
                            // 1. Enter is pressed WITHOUT Shift (standard chat behavior)
                            // 2. Cmd+Enter or Ctrl+Enter is pressed (explicit force submit)
                            if (!is_shift) || is_explicit_submit {
                                event.prevent_default();
                                on_interaction.call(());
                                send_message();
                                return;
                            }
                        }

                        // Block other modifier-based shortcuts (let OS/browser handle them)
                        if modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::CONTROL) || modifiers.contains(Modifiers::ALT) {
                            return;
                        }

                        if event.key() == Key::Tab {
                            event.prevent_default();
                            let script = if modifiers.contains(Modifiers::SHIFT) {
                                r#"
                                const el = document.getElementById('chat-textarea');
                                if (el) {
                                    const start = el.selectionStart;
                                    const value = el.value;
                                    let line_start = value.lastIndexOf('\n', start - 1) + 1;
                                    if (value.substring(line_start, line_start + 1) === '\t') {
                                        el.value = value.substring(0, line_start) + value.substring(line_start + 1);
                                        el.selectionStart = el.selectionEnd = Math.max(start - 1, line_start);
                                    }
                                }
                                "#
                            } else {
                                r#"
                                const el = document.getElementById('chat-textarea');
                                if (el) {
                                    const start = el.selectionStart;
                                    const end = el.selectionEnd;
                                    el.value = el.value.substring(0, start) + '\t' + el.value.substring(end);
                                    el.selectionStart = el.selectionEnd = start + 1;
                                }
                                "#
                            };
                            let _ = document::eval(script);
                            let _ = document::eval(r#"
                                const el = document.getElementById('chat-textarea');
                                if (el) {
                                    var event = new Event('input', { bubbles: true, cancelable: true });
                                    el.dispatchEvent(event);
                                }
                            "#);
                            return;
                        }
                    },
                    onpaste: move |_| {
                        // All clipboard operations must happen on the main thread.
                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                            if let Ok(image) = clipboard.get_image() {
                                // Only spawn a task if there's an image to process.
                                spawn(async move {
                                    is_processing_attachments.set(true);
                                    let image_data = arboard::ImageData {
                                        width: image.width,
                                        height: image.height,
                                        bytes: image.bytes,
                                    };
                                    if let Some(attachment) = process_clipboard_image(image_data).await {
                                        attachments.write().push(attachment);
                                    }
                                    is_processing_attachments.set(false);
                                });
                            }
                        }
                    },
                },
                div {
                    class: "flex items-center space-x-3",
                    {
                        cfg_if::cfg_if! {
                            if #[cfg(debug_assertions)] {
                                rsx! {
                                    button {
                                        class: "p-2 rounded-full text-gray-400 hover:bg-dark-card hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                                        onclick: move |_| {
                                            let session_state = session_state.clone();
                                            let settings = _settings.clone();
                                            let mcp_manager = _mcp_manager.clone();
                                            spawn(async move {
                                                let mcp_context = {
                                                    let mcp_manager_reader = mcp_manager.read();
                                                    mcp_manager_reader.get_mcp_context().await
                                                };

                                                let context_string = {
                                                    let state = session_state.read();
                                                    if let Some(session) = state.get_active_session().cloned() {
                                                        let mut session_for_debug = session;

                                                        if !mcp_context.servers.is_empty() {
                                                            session_for_debug.active_context.mcp_tools = Some(mcp_context);
                                                        }

                                                        let settings_reader = settings.read();
                                                        let builder = PromptBuilder::new(&session_for_debug, &settings_reader, &state);
                                                        let prompt_data = builder.build_prompt("[DEBUG USER MESSAGE]".to_string(), None);
                                                        format!("{:#?}", prompt_data)
                                                    } else {
                                                        "[No active session]".to_string()
                                                    }
                                                };
                                                let timestamp = SystemTime::now()
                                                    .duration_since(SystemTime::UNIX_EPOCH)
                                                    .unwrap()
                                                    .as_secs();
                                                let debug_dir = std::env::temp_dir().join("hobbes_debug_logs");
                                                if let Ok(_) = std::fs::create_dir_all(&debug_dir) {
                                                    let file_path = debug_dir.join(format!("prompt_{}.log", timestamp));
                                                    if let Err(e) = std::fs::write(&file_path, &context_string) {
                                                        tracing::error!("Failed to write debug prompt to file: {}", e);
                                                    } else {
                                                        tracing::debug!("Debug prompt written to {:?}", file_path);
                                                    }
                                                }
                                            });
                                        },
                                        Icon {
                                            width: 20,
                                            height: 20,
                                            icon: fi_icons::FiCpu
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // New Chat Menu (Popup)
                    div {
                        class: "relative",
                        button {
                            class: "p-2 rounded-full text-gray-400 hover:bg-dark-card hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                            title: "New Chat",
                            onclick: move |_| show_new_chat_menu.set(!show_new_chat_menu()),
                            Icon {
                                width: 20,
                                height: 20,
                                icon: fi_icons::FiPlus
                            }
                        }
                        
                        if show_new_chat_menu() {
                            div {
                                class: "absolute bottom-10 right-0 w-64 bg-dark-card border border-primary-700 rounded-lg shadow-xl z-50 overflow-hidden py-1",
                                // New Chat (No Memory)
                                button {
                                    class: "w-full text-left px-4 py-2 text-sm text-gray-300 hover:bg-primary-900/50 hover:text-white transition-colors flex items-center justify-between",
                                    onclick: move |_| {
                                        session_state.write().create_session();
                                        show_new_chat_menu.set(false);
                                    },
                                    div {
                                        class: "flex items-center space-x-2",
                                        Icon {
                                            width: 16,
                                            height: 16,
                                            icon: fi_icons::FiPlus,
                                            class: "text-gray-400"
                                        }
                                        span { "New Chat" }
                                    }
                                    span {
                                        class: "text-xs text-gray-500 font-mono",
                                        "{format_hotkey(&_settings.read().hotkeys.toggle_new_chat)}"
                                    }
                                }
                                // New Chat with Memory
                                button {
                                    class: "w-full text-left px-4 py-2 text-sm text-gray-300 hover:bg-primary-900/50 hover:text-white transition-colors flex items-center justify-between",
                                    onclick: move |_| {
                                        on_new_chat_with_memory.call(());
                                        show_new_chat_menu.set(false);
                                    },
                                    div {
                                        class: "flex items-center space-x-2",
                                        Icon {
                                            width: 16,
                                            height: 16,
                                            icon: fi_icons::FiCpu,
                                            class: "text-primary-400"
                                        }
                                        span { "New Chat with Memory" }
                                    }
                                    span {
                                        class: "text-xs text-gray-500 font-mono",
                                        "{format_hotkey(&_settings.read().hotkeys.toggle_new_chat_with_memory)}"
                                    }
                                }
                            }
                        }
                        // Click outside handler to close dropdown
                        if show_new_chat_menu() {
                            div {
                                class: "fixed inset-0 z-40",
                                onclick: move |_| show_new_chat_menu.set(false)
                            }
                        }
                    }
                    if !*is_sending.read() {
                        button {
                            class: "px-5 py-2 bg-primary-500 rounded-full text-white font-semibold hover:bg-primary-600 focus:outline-none focus:ring-2 focus:ring-primary-500 focus:ring-opacity-50 transition-colors disabled:bg-gray-500",
                            disabled: *is_processing_attachments.read(),
                            onclick: move |_| {
                                on_interaction.call(());
                                send_message();
                            },
                            if *has_new_comments.read() || *has_pending_approvals.read() { "Submit" } else { "Send" }
                        }
                    } else {
                        button {
                            class: "px-4 py-2 bg-red-600 rounded-full text-white font-semibold hover:bg-red-700 focus:outline-none focus:ring-2 focus:ring-red-500 focus:ring-opacity-50 transition-colors flex items-center space-x-2",
                            onclick: move |_| on_cancel.call(()),
                            Icon {
                                width: 20,
                                height: 20,
                                icon: fi_icons::FiSquare
                            },
                            span { "Stop" }
                        }
                    }
                }
            }    
        }
    }
}

async fn process_image_data(
    file_name: String,
    mime_type: String,
    file_data: Vec<u8>,
) -> Option<Attachment> {
    tokio::task::spawn_blocking(move || {
        image::load_from_memory(&file_data).ok().and_then(|img| {
            let resized_img = if img.height() > 200 {
                img.resize(u32::MAX, 200, imageops::FilterType::Lanczos3)
            } else {
                img
            };
            let format = match mime_type.as_str() {
                "image/png" => ImageFormat::Png,
                "image/jpeg" => ImageFormat::Jpeg,
                "image/gif" => ImageFormat::Gif,
                "image/webp" => ImageFormat::WebP,
                _ => ImageFormat::Png,
            };
            let mut buffer = Cursor::new(Vec::new());
            resized_img.write_to(&mut buffer, format).ok().map(|_| {
                let data = general_purpose::STANDARD.encode(buffer.into_inner());
                Attachment {
                    file_name,
                    mime_type,
                    data,
                }
            })
        })
    })
    .await
    .ok()
    .flatten()
}

async fn process_clipboard_image(image_data: arboard::ImageData<'_>) -> Option<Attachment> {
    let width = image_data.width as u32;
    let height = image_data.height as u32;
    let bytes = image_data.bytes.into_owned();

    tokio::task::spawn_blocking(move || {
        image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, bytes).and_then(
            |img_buf| {
                let resized_img: DynamicImage = if img_buf.height() > 200 {
                    let dynamic_image: DynamicImage = img_buf.into();
                    dynamic_image.resize(u32::MAX, 200, imageops::FilterType::Lanczos3)
                } else {
                    img_buf.into()
                };

                let mut buffer = Cursor::new(Vec::new());
                resized_img
                    .write_to(&mut buffer, ImageFormat::Png)
                    .ok()
                    .map(|_| {
                        let data = general_purpose::STANDARD.encode(buffer.into_inner());
                        let timestamp = SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let file_name = format!("pasted-image-{}.png", timestamp);
                        Attachment {
                            file_name,
                            mime_type: "image/png".to_string(),
                            data,
                        }
                    })
            },
        )
    })
    .await
    .ok()
    .flatten()
}

fn get_mime_type(extension: &str) -> Option<&'static str> {
    match extension.to_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Formats a hotkey string for display (e.g., "CmdOrCtrl+Shift+N" -> "⌘⇧N")
fn format_hotkey(hotkey: &str) -> String {
    hotkey
        .replace("CmdOrCtrl", "⌘")
        .replace("Ctrl", "⌃")
        .replace("Alt", "⌥")
        .replace("Shift", "⇧")
        .replace("+", "")
}