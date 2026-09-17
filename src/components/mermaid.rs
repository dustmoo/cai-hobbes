use crate::components::chat::CodeBlock;
use dioxus::prelude::*;

/// The mermaid.js bundle, pinned by `build.rs` (copied from
/// `node_modules/mermaid/dist/mermaid.min.js`, or a small error-reporting
/// stub if that's unavailable — see `sync_mermaid_bundle()` in build.rs).
/// It's still compiled into the binary (so it works fully offline with no
/// CDN dependency), but unlike the old `<script>` tag in the webview head,
/// it is only sent into the page and executed the first time a diagram
/// actually needs to render — see `ensure_mermaid_loaded()` below.
const MERMAID_LIB_JS: &str = include_str!("../../assets/mermaid.min.js");

/// Process-wide "has the mermaid library been injected into this window
/// yet" gate. `tokio::sync::OnceCell::get_or_try_init` dedupes concurrent
/// callers (e.g. several diagrams rendering at once while a message
/// streams): the first caller runs the injection, every other caller just
/// awaits that same in-flight future instead of injecting a second time. On
/// failure the cell stays uninitialized so a later diagram can retry.
static MERMAID_LOADED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// Ensure `window.mermaid` is loaded and initialized in the current webview,
/// loading it at most once. Safe to call before every render.
async fn ensure_mermaid_loaded() -> Result<(), String> {
    MERMAID_LOADED
        .get_or_try_init(|| async {
            #[derive(serde::Deserialize)]
            struct LoadResult {
                ok: bool,
                error: Option<String>,
            }

            // `typeof window.mermaid !== 'undefined'` guards against
            // injecting twice even if this ever races ahead of the Rust-side
            // OnceCell (e.g. a future reload of the window).
            let script = format!(
                r#"
                (function() {{
                    try {{
                        if (typeof window.mermaid === 'undefined') {{
                            {lib}
                            window.mermaid.initialize({{
                                startOnLoad: false,
                                securityLevel: 'strict',
                                theme: 'dark',
                                themeVariables: {{
                                    background: '#1A1A1A',
                                    primaryColor: '#2D2D2D',
                                    primaryTextColor: '#FFFFFF',
                                    primaryBorderColor: '#4B5563',
                                    lineColor: '#9CA3AF',
                                    secondaryColor: '#242424',
                                    tertiaryColor: '#1A1A1A',
                                    textColor: '#FFFFFF',
                                    nodeTextColor: '#FFFFFF',
                                    mainBkg: '#2D2D2D',
                                    edgeLabelBackground: '#1A1A1A'
                                }}
                            }});
                        }}
                        dioxus.send({{ ok: true }});
                    }} catch (e) {{
                        dioxus.send({{ ok: false, error: String(e && e.message ? e.message : e) }});
                    }}
                }})();
                "#,
                lib = MERMAID_LIB_JS,
            );

            let mut eval = document::eval(&script);
            match eval.recv::<LoadResult>().await {
                Ok(LoadResult { ok: true, .. }) => Ok(()),
                Ok(LoadResult { ok: false, error }) => {
                    Err(error.unwrap_or_else(|| "mermaid library failed to load".to_string()))
                }
                Err(e) => Err(format!("mermaid library failed to load: {e}")),
            }
        })
        .await
        .map(|_| ())
}

/// Renders a fenced ```mermaid block as a live SVG diagram, with a toggle to
/// see the raw source (reusing `CodeBlock`).
///
/// `code` is a plain `String` prop (not a `Signal`) that grows chunk by
/// chunk while the model streams. This hits the same trap documented by the
/// ⚠️ warning above `CodeBlock` in chat.rs, but for `use_effect` rather than
/// `use_memo`: `use_effect` only reruns when a `Signal` *read inside its
/// closure* changes value; a plain `String` captured by `move` is inert, so
/// an effect that only closes over `code` would fire once at mount and
/// never again as the stream grows. To make the prop reactive we mirror it
/// into a `Signal` every render (`code_signal`, synced below via `peek` +
/// `set` so we only write — and thus only wake subscribers — when the value
/// actually changed) and have the effect read `code_signal()` instead of
/// `code` directly.
#[component]
pub fn MermaidDiagram(code: String) -> Element {
    #[derive(Clone, Copy, PartialEq)]
    enum ViewMode {
        Diagram,
        Source,
    }

    let unique_id = use_hook(|| {
        format!(
            "mermaid-{}",
            uuid::Uuid::new_v4().to_string().replace('-', "")
        )
    });
    let mut view_mode = use_signal(|| ViewMode::Diagram);
    // Last successfully rendered SVG (kept across failed re-renders so a
    // mid-stream invalid chunk never blanks a previously good diagram).
    let mut last_good_svg = use_signal(|| Option::<String>::None);
    // Set once we've rendered at least one good SVG; used to decide whether
    // to show the raw source (never-yet-valid) or the stale-good SVG
    // (previously valid, currently mid-edit) while source is invalid.
    let mut render_error = use_signal(|| Option::<String>::None);

    // Mirror the `code` prop into a Signal so the effect below can
    // subscribe to it (see doc comment). Only write when it actually
    // changed, to avoid waking subscribers on every no-op render.
    let mut code_signal = use_signal(|| code.clone());
    if *code_signal.peek() != code {
        code_signal.set(code.clone());
    }

    // Re-render whenever `code_signal` changes.
    use_effect({
        let id = unique_id.clone();
        move || {
            let code = code_signal();
            let id = id.clone();
            spawn(async move {
                let trimmed = code.trim();
                if trimmed.is_empty() {
                    return;
                }

                // Load the library on first use (see doc comment on
                // `ensure_mermaid_loaded`). While this is in flight the
                // component falls through to its existing "not yet
                // validated" branch below, which shows the raw source via
                // `CodeBlock` rather than an empty box or spinner.
                if let Err(e) = ensure_mermaid_loaded().await {
                    render_error.set(Some(format!("mermaid library unavailable: {e}")));
                    return;
                }

                let code_json = match serde_json::to_string(&code) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let id_json = match serde_json::to_string(&id) {
                    Ok(v) => v,
                    Err(_) => return,
                };

                let script = format!(
                    r#"
                    try {{
                        await window.mermaid.parse({code});
                        const {{ svg }} = await window.mermaid.render({id}, {code});
                        dioxus.send({{ ok: true, svg: svg }});
                    }} catch (e) {{
                        dioxus.send({{ ok: false, error: String(e && e.message ? e.message : e) }});
                    }}
                    "#,
                    code = code_json,
                    id = id_json,
                );

                let mut eval = document::eval(&script);

                #[derive(serde::Deserialize)]
                struct RenderResult {
                    ok: bool,
                    svg: Option<String>,
                    error: Option<String>,
                }

                match eval.recv::<RenderResult>().await {
                    Ok(RenderResult {
                        ok: true,
                        svg: Some(svg),
                        ..
                    }) => {
                        last_good_svg.set(Some(svg));
                        render_error.set(None);
                    }
                    Ok(RenderResult {
                        ok: false, error, ..
                    }) => {
                        render_error.set(Some(error.unwrap_or_else(|| "invalid diagram".to_string())));
                    }
                    _ => {
                        render_error.set(Some("mermaid render failed".to_string()));
                    }
                }
            });
        }
    });

    let has_svg = last_good_svg.read().is_some();
    let error = render_error.read().clone();

    let pill_class = |active: bool| -> &'static str {
        if active {
            "text-[11px] border border-faint rounded px-2 py-0.5 bg-btn-primary text-fg"
        } else {
            "text-[11px] border border-faint rounded px-2 py-0.5 text-fg-muted hover:text-fg"
        }
    };

    rsx! {
        div {
            class: "mermaid-block my-2",
            div {
                class: "flex items-center gap-1.5 mb-1.5",
                button {
                    class: pill_class(*view_mode.read() == ViewMode::Diagram),
                    onclick: move |_| view_mode.set(ViewMode::Diagram),
                    "Diagram"
                }
                button {
                    class: pill_class(*view_mode.read() == ViewMode::Source),
                    onclick: move |_| view_mode.set(ViewMode::Source),
                    "Source"
                }
            }
            if *view_mode.read() == ViewMode::Source {
                CodeBlock { lang: "mermaid".to_string(), code: code.clone() }
            } else if has_svg {
                div {
                    class: "mermaid-svg-wrapper bg-section rounded-lg p-4 overflow-x-auto",
                    dangerous_inner_html: "{last_good_svg.read().clone().unwrap_or_default()}"
                }
                if let Some(err) = &error {
                    p { class: "text-xs text-fg-muted mt-1", "Diagram may be out of date: {err}" }
                }
            } else if let Some(err) = &error {
                CodeBlock { lang: "mermaid".to_string(), code: code.clone() }
                p { class: "text-xs text-fg-muted mt-1", "Mermaid error: {err}" }
            } else {
                // Still streaming / not yet validated — show source, no JS
                // error and no empty box.
                CodeBlock { lang: "mermaid".to_string(), code: code.clone() }
            }
        }
    }
}
