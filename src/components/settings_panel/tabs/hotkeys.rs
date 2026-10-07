use super::super::SettingsCtx;
use crate::components::hotkey_recorder::HotkeyRecorder;
use crate::settings::HotkeySettings;
use dioxus::prelude::*;
use dioxus_free_icons::{Icon, icons::fi_icons};

#[component]
pub(in super::super) fn HotkeysTab(ctx: SettingsCtx) -> Element {
    let settings = ctx.settings;
    let mut local_settings = ctx.local_settings;
    rsx! {
                        div {
                            class: "border border-subtle rounded-lg mb-4",
                            div {
                                class: "p-4 bg-section rounded-lg",
                                h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium mb-4", "Keyboard Shortcuts" }
                                p { class: "text-xs text-fg-muted mb-4",
                                    "Customize global shortcuts using the format: "
                                    code { class: "bg-black/30 px-1 rounded", "CmdOrCtrl+Shift+Key" }
                                ". Changes apply immediately after saving."
                                }
                                if local_settings.read().hotkeys != settings.read().hotkeys {
                                    div {
                                        class: "mb-4 p-3 bg-yellow-900/30 border border-yellow-700/50 rounded-md flex items-center gap-3",
                                        Icon {
                                            width: 18,
                                            height: 18,
                                            fill: "none",
                                            class: "text-yellow-500 min-w-[18px]",
                                            icon: fi_icons::FiAlertTriangle,
                                        }
                                        span { class: "text-xs text-yellow-200", "Restart required to apply changes." }
                                    }
                                }

                                div {
                                    class: "space-y-4",

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Submit Message" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.submit_chat.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.submit_chat = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Cancel Generation" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.cancel_generation.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.cancel_generation = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Focus Chat Input" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_focus_chat.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_focus_chat = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Toggle Settings" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_settings.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_settings = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Toggle History" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_history.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_history = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Toggle MCP Config" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_mcp.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_mcp = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Toggle Planner" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_planner.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_planner = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Toggle Fleet" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_fleet.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_fleet = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Quick Capture to Planner" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.quick_capture.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.quick_capture = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Next Tab" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.next_tab.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.next_tab = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Previous Tab" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.prev_tab.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.prev_tab = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Next Tab Group" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.next_tab_group.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.next_tab_group = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Previous Tab Group" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.prev_tab_group.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.prev_tab_group = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Open Profile Selector" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_profile.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_profile = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Open Provider Selector" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_provider.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_provider = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Add Attachments" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_attachments.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_attachments = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Global Toggle (Show/Hide)" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_tray.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_tray = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "New Chat (No Memory)" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_new_chat.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_new_chat = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "New Chat with Memory" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_new_chat_with_memory.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_new_chat_with_memory = v,
                                        }
                                    }

                                    div { class: "pt-2 pb-1", h4 { class: "text-xs uppercase tracking-[0.06em] text-fg-muted mt-2", "Session Tabs" } }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 1" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_1.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_1 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 2" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_2.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_2 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 3" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_3.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_3 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 4" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_4.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_4 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 5" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_5.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_5 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 6" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_6.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_6 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 7" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_7.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_7 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 8" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_8.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_8 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 9" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_9.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_9 = v,
                                        }
                                    }

                                    // Reset to Defaults button
                                    div {
                                        class: "pt-4 border-t border-gray-800",
                                        button {
                                            class: "px-4 py-2 bg-input rounded-md text-sm text-fg-muted hover:bg-input hover:text-fg transition-colors",
                                            onclick: move |_| {
                                                local_settings.write().hotkeys = HotkeySettings::default();
                                            },
                                            "Reset to Defaults"
                                        }
                                    }

                                    div {
                                        class: "pt-4 border-t border-gray-800",

                                        div {
                                            class: "flex justify-between items-center text-sm",
                                            span { class: "text-fg-muted", "Switch Profile (1-9)" }
                                            span { class: "font-mono text-fg-muted", "CmdOrCtrl + [1-9]" }
                                        }
                                        p { class: "text-[10px] text-fg-muted mt-1", "Profile switching hotkeys are currently fixed." }
                                    }
                                }
                            }
                        }
    }
}
