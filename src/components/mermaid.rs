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

            // The bundle is a CLASSIC SCRIPT and only works as one. Its first
            // line declares `var __esbuild_esm_mermaid_nm` and its last reads
            // that name back off `globalThis` — which only lines up when the
            // `var` lands at script top level. `document::eval` runs its
            // script inside a function wrapper, where the same `var` is a
            // local, so the bundle throws on its own last line
            // ("undefined is not an object"). Injecting a <script> element
            // with `textContent` runs the source synchronously at top level,
            // exactly as the old <script> tag in the webview head did, and
            // keeps working if mermaid changes its prologue.
            //
            // The `typeof window.mermaid` guards make this idempotent even if
            // it ever races ahead of the Rust-side OnceCell (a window reload).
            let script = format!(
                r#"
                (function() {{
                    try {{
                        if (typeof window.mermaid === 'undefined') {{
                            const s = document.createElement('script');
                            s.textContent = {lib_literal};
                            document.head.appendChild(s);
                        }}
                        if (typeof window.mermaid === 'undefined') {{
                            throw new Error('bundle ran but did not define window.mermaid');
                        }}
                        if (!window.__hobbesMermaidInit) {{
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
                            window.__hobbesMermaidInit = true;
                        }}
                        dioxus.send({{ ok: true }});
                    }} catch (e) {{
                        dioxus.send({{ ok: false, error: String(e && e.message ? e.message : e) }});
                    }}
                }})();
                "#,
                // JSON-encoded so the source travels as a string literal, not
                // as code spliced into this wrapper's scope.
                lib_literal = serde_json::to_string(MERMAID_LIB_JS)
                    .unwrap_or_else(|_| "''".to_string()),
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
    // A *separate* id from `unique_id` for the wrapper div the visibility
    // observer below targets. `unique_id` is also passed to
    // `mermaid.render(id, code)` as the id mermaid assigns its own
    // internally-generated SVG/container — giving our permanent wrapper the
    // same id collided with that, silently breaking the render (mermaid
    // treating the persistent element as its own scratch node it manages).
    let observer_id = use_hook(|| format!("{unique_id}-anchor"));
    let mut view_mode = use_signal(|| ViewMode::Diagram);
    // Gates rendering on the block having scrolled near the viewport (see
    // the observer effect below). Without this, every diagram in the
    // currently-loaded message window renders the instant its tab mounts —
    // in DOM order, so a long history with several diagrams pops them in
    // one after another regardless of which one the user can actually see,
    // each swap reflowing everything below it. Most of that work is for
    // diagrams the user isn't looking at.
    let mut is_visible = use_signal(|| false);
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

    // Watch for the block scrolling near the viewport, once, at mount. This
    // effect reads no signal, so — per Dioxus 0.6 semantics (an effect only
    // re-runs when a Signal read *inside its closure* changes value) — it
    // runs exactly once per component instance, not on every re-render.
    // `document::eval`'s channel is a genuine multi-message stream (backed
    // by an unbounded receiver, not a single resolved Promise — confirmed
    // against dioxus-document-0.6.3's `Eval::recv`/`poll_recv`), so the
    // observer callback firing later, asynchronously, correctly delivers
    // its one message to this `recv()` even though the eval's own script
    // body returns immediately after registering the observer.
    use_effect({
        let id = observer_id.clone();
        move || {
            let id = id.clone();
            spawn(async move {
                let id_json = match serde_json::to_string(&id) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                // A generous rootMargin renders a diagram slightly before it
                // scrolls into view, so it's already there by the time the
                // user's eye reaches it rather than popping in mid-scroll.
                let script = format!(
                    r#"
                    const el = document.getElementById({id});
                    if (!el) {{
                        dioxus.send({{}});
                    }} else {{
                        const io = new IntersectionObserver((entries) => {{
                            if (entries.some((e) => e.isIntersecting)) {{
                                io.disconnect();
                                dioxus.send({{}});
                            }}
                        }}, {{ rootMargin: '600px 0px' }});
                        io.observe(el);
                    }}
                    "#,
                    id = id_json,
                );
                let mut eval = document::eval(&script);
                let _ = eval.recv::<serde_json::Value>().await;
                is_visible.set(true);
            });
        }
    });

    // Re-render whenever `code_signal` changes, or once the block becomes
    // visible (a historical diagram's code is set once at mount and never
    // changes again, so without also gating on `is_visible` here this
    // effect would fire — and render — once at mount regardless of scroll
    // position; reading `is_visible()` makes it re-run the moment the
    // observer above resolves).
    use_effect({
        let id = unique_id.clone();
        move || {
            let code = code_signal();
            let visible = is_visible();
            let id = id.clone();
            spawn(async move {
                if !visible {
                    return;
                }
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
            // The IntersectionObserver effect above targets this id from the
            // moment the component mounts. Deliberately `observer_id`, not
            // `unique_id` — the latter is mermaid's own render id (see its
            // doc comment); giving this permanent wrapper the same id
            // collided with mermaid's internal use of it.
            id: "{observer_id}",
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
                    // Plain opacity fade on first reveal: compositor-only, so
                    // it costs no extra layout, and reads as an intentional
                    // arrival rather than a jarring snap into place — the
                    // actual height jump from the CodeBlock placeholder is a
                    // one-time cost, but it no longer *looks* accidental.
                    class: "mermaid-svg-wrapper bg-section rounded-lg p-4 overflow-x-auto mermaid-reveal",
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
