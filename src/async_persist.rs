use dioxus::prelude::*;
use std::io;

/// Persist data to disk on a background thread without blocking the UI.
///
/// `save_fn` is a `FnOnce() -> io::Result<()>` that performs the actual I/O.
/// `context_label` is used for error logging (e.g., "session state", "settings").
/// `error_signal` optionally surfaces save failures to the Dioxus UI.
pub fn persist_async(
    save_fn: impl FnOnce() -> io::Result<()> + Send + 'static,
    context_label: &'static str,
    error_signal: Option<Signal<Option<String>>>,
) {
    let handle = tokio::spawn(async move {
        tokio::task::spawn_blocking(save_fn)
            .await
            .unwrap_or_else(|e| Err(io::Error::other(e)))
    });

    if let Some(mut sig) = error_signal {
        spawn(async move {
            if let Ok(Err(e)) = handle.await {
                tracing::error!("Failed to save {} async: {}", context_label, e);
                *sig.write() = Some(format!("Failed to save {}: {}", context_label, e));
            }
        });
    } else {
        tokio::spawn(async move {
            if let Ok(Err(e)) = handle.await {
                tracing::error!("Failed to save {} async: {}", context_label, e);
            }
        });
    }
}
