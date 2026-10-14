use dioxus_signals::{GlobalSignal, Signal};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub static WINDOW_VISIBLE: GlobalSignal<bool> = Signal::global(|| true);
pub static APP_QUIT: GlobalSignal<bool> = Signal::global(|| false);

/// Start the single global `TrayIconEvent` drain thread, exactly once.
///
/// There is one tray icon and one crossbeam channel; exactly one consumer
/// must exist (competing receivers would steal each other's clicks). Clicks
/// are routed by focus mode *at event time*: while a focus session is active
/// the icon is the timer — a left-click release bumps the focus-click
/// counter (drained by an effect in `main.rs` that surfaces the window and
/// planner, never hides them); with no focus active, the historical
/// any-click visibility toggle applies.
pub fn ensure_tray_listener() {
    static LISTENER: std::sync::Once = std::sync::Once::new();
    LISTENER.call_once(|| {
        let tray_channel = TrayIconEvent::receiver();
        std::thread::spawn(move || {
            tracing::info!("Tray listener thread started.");
            loop {
                let Ok(event) = tray_channel.recv() else {
                    // The static channel never closes; if it somehow does,
                    // there is nothing left to listen for.
                    return;
                };
                if let TrayIconEvent::Click {
                    button,
                    button_state,
                    ..
                } = event
                {
                    if crate::focus_tray::focus_mode_active() {
                        // One action per physical click: count only the
                        // left-button release. Never toggles visibility —
                        // focus mode must not hide the window.
                        if button == MouseButton::Left && button_state == MouseButtonState::Up {
                            tracing::info!("Tray clicked in focus mode.");
                            *crate::focus_tray::FOCUS_TRAY_CLICKS.write() += 1;
                        }
                    } else {
                        tracing::info!("Tray icon clicked, toggling visibility.");
                        let mut visible = WINDOW_VISIBLE.write();
                        *visible = !*visible;
                    }
                }
            }
        });
    });
}

/// Build the one tray icon, rasterised from the mascot grid at runtime.
///
/// The mark comes from `pixel_icons::tiger_rgba` — the same 16×16 grid the
/// chat-bar sprite is drawn from — so there is no PNG asset and no
/// sandboxed/Pro branding split for the tray: one stencil serves both.
///
/// macOS takes the *template* (ink opaque black, holes transparent) and
/// tints it for the menu bar's light/dark/highlighted states. tray-icon
/// 0.19.3 sizes the NSImage itself — `set_icon_with_as_template` calls
/// `setSize` with a fixed 18pt height and a width scaled to the image's
/// aspect ratio — so a square 36×36 raster lands as 18×18pt at @2x backing
/// resolution. Hence `cell_px = 2` over the 18-cell canvas.
///
/// Windows has no template concept, so it gets the colour raster at the same
/// 36×36.
pub fn init_tray() -> TrayIcon {
    let cell_px = 2;
    #[cfg(target_os = "macos")]
    let as_template = true;
    #[cfg(not(target_os = "macos"))]
    let as_template = false;

    let (rgba, width, height) = crate::components::pixel_icons::tiger_rgba(cell_px, !as_template);
    // Infallible in practice: the rasteriser always returns width*height*4
    // bytes for a non-zero canvas, which is the only thing `from_rgba`
    // validates. An empty icon is better than a panic if that ever changes.
    let icon = Icon::from_rgba(rgba, width, height).unwrap_or_else(|e| {
        tracing::error!("Failed to build tray icon from the mascot raster: {}", e);
        Icon::from_rgba(vec![0, 0, 0, 0], 1, 1).unwrap()
    });

    // Build a tray icon without a menu to avoid the muda class conflict
    // (dioxus-desktop's muda 0.11 and tray-icon's muda 0.15 both register the
    // ObjC class `MudaMenuItem`). The main application menu is handled
    // separately in menu.rs. Use the TrayIconEvent receiver for direct clicks.
    ensure_tray_listener();

    let builder = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("Hobbes");
    #[cfg(target_os = "macos")]
    let builder = builder.with_icon_as_template(as_template);
    builder.build().unwrap()
}
