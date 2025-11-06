use dioxus::{html::HasFileData, prelude::*};
use dioxus_free_icons::{icons::fi_icons, Icon};
use rfd::FileDialog;
use std::time::SystemTime;
use base64::{engine::general_purpose, Engine as _};
use image::{imageops, ImageFormat, DynamicImage};
use std::io::Cursor;

use crate::{context::prompt_builder::PromptBuilder, settings::Settings};
use hobbes_core::models::Attachment;

use crate::processing::summarization_scheduler::{SchedulerSignal};

#[component]
pub fn ChatInput(
    is_sending: Signal<bool>,
    on_send: EventHandler<(String, Vec<Attachment>)>,
    on_cancel: EventHandler<()>,
    on_interaction: EventHandler<()>,
    on_toggle_sessions: EventHandler<()>,
    on_toggle_settings: EventHandler<()>,
) -> Element {
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    let settings = use_context::<Signal<Settings>>();
    let mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let mut draft = use_context::<Signal<String>>();
    let mut is_dragging = use_signal(|| false);
    let mut attachments = use_signal(Vec::<Attachment>::new);
    let mut is_processing_attachments = use_signal(|| false);
    let scheduler = use_context::<Coroutine<SchedulerSignal>>();

    let mut send_message = move || {
        if *is_sending.read() || *is_processing_attachments.read() {
            tracing::warn!("'send_message' blocked: already sending or processing attachments.");
            return;
        }
        let user_message = draft.read().clone();
        if user_message.is_empty() && attachments.read().is_empty() {
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
                class: "flex items-center space-x-2 p-2 bg-gray-800 rounded-t-lg",
                for (index, attachment) in attachments.read().iter().enumerate() {
                    div {
                        class: "flex items-center space-x-2 bg-gray-700 p-1 rounded",
                        span {
                            class: "text-sm text-gray-300",
                            "{attachment.file_name}"
                        }
                        button {
                            class: "p-1 rounded-full text-gray-400 hover:bg-gray-600 hover:text-white",
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
                "bg-gray-900 p-4 border-t-2 border-dashed border-purple-500"
            } else {
                "bg-gray-900 p-4 border-t border-gray-700"
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
                    class: "p-2 rounded-full text-gray-400 hover:bg-gray-700 hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                    onclick: move |_| on_toggle_sessions.call(()),
                    Icon {
                        width: 20,
                        height: 20,
                        icon: fi_icons::FiMenu
                    }
                }
                button {
                    class: "p-2 rounded-full text-gray-400 hover:bg-gray-700 hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                    onclick: move |_| on_toggle_settings.call(()),
                    Icon {
                        width: 20,
                        height: 20,
                        icon: fi_icons::FiSettings
                    }
                }
                button {
                    class: "p-2 rounded-full text-gray-400 hover:bg-gray-700 hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
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
                    class: "flex-1 py-2 px-4 rounded-xl bg-gray-800 border border-gray-700 text-gray-100 placeholder-gray-500 focus:outline-none focus:ring-2 focus:ring-purple-500 resize-none overflow-y-auto",
                    style: "max-height: 50vh;",
                    rows: "1",
                    placeholder: if !mcp_context.read().servers.is_empty() { "Type your message..." } else { "Initializing..." },
                    disabled: mcp_context.read().servers.is_empty(),
                    value: "{draft}",
                    oninput: move |event| {
                        scheduler.send(SchedulerSignal::Activity);
                        let _ = document::eval(r#"
                            const el = document.getElementById('chat-textarea');
                            if (el) {
                                window.dioxusCursorPos = [el.selectionStart, el.selectionEnd];
                                el.style.height = 'auto';
                                el.style.height = (el.scrollHeight) + 'px';
                            }
                        "#);
                        draft.set(event.value());
                    },
                    onkeydown: move |event| {
                        scheduler.send(SchedulerSignal::Activity);
                        let modifiers = event.data.modifiers();
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

                        if event.key() == Key::Enter && !modifiers.contains(Modifiers::SHIFT) {
                            event.prevent_default();
                            on_interaction.call(());
                            send_message();
                        }
                    },
                    onpaste: move |_| {
                        spawn(async move {
                            is_processing_attachments.set(true);
                            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                if let Ok(image) = clipboard.get_image() {
                                    let image_data = arboard::ImageData {
                                        width: image.width,
                                        height: image.height,
                                        bytes: image.bytes,
                                    };
                                    if let Some(attachment) = process_clipboard_image(image_data).await {
                                        attachments.write().push(attachment);
                                    }
                                }
                            }
                            is_processing_attachments.set(false);
                        });
                    },
                },
                div {
                    class: "flex items-center space-x-3",
                    {
                        cfg_if::cfg_if! {
                            if #[cfg(debug_assertions)] {
                                rsx! {
                                    button {
                                        class: "p-2 rounded-full text-gray-400 hover:bg-gray-700 hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                                        onclick: move |_| {
                                            let session_state = session_state.clone();
                                            let settings = settings.clone();
                                            let mcp_manager = mcp_manager.clone();
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
                                                let file_name = format!("prompt_{}.log", timestamp);
                                                if let Err(e) = std::fs::write(&file_name, &context_string) {
                                                    tracing::error!("Failed to write debug prompt to file: {}", e);
                                                } else {
                                                    tracing::info!("Debug prompt written to {}", &file_name);
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
                    button {
                        class: "p-2 rounded-full text-gray-400 hover:bg-gray-700 hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                        onclick: move |_| {
                            session_state.write().create_session();
                        },
                        Icon {
                            width: 20,
                            height: 20,
                            icon: fi_icons::FiPlus
                        }
                    }
                    if !*is_sending.read() {
                        button {
                            class: "px-5 py-2 bg-purple-600 rounded-full text-white font-semibold hover:bg-purple-700 focus:outline-none focus:ring-2 focus:ring-purple-500 focus:ring-opacity-50 transition-colors disabled:bg-gray-500",
                            disabled: mcp_context.read().servers.is_empty() || *is_processing_attachments.read(),
                            onclick: move |_| {
                                on_interaction.call(());
                                send_message();
                            },
                            "Send"
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