// File watcher for the skills directories: auto-reloads the SkillRegistry
// signal when SKILL.md files are created, edited, or deleted on disk (in-app
// saves or external editors alike — reloads are idempotent).

use super::registry::{get_skills_directories, SkillRegistry};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::time::Duration;

/// Quiet period after the last filesystem event before reloading, so editor
/// save bursts (write + rename + metadata) trigger a single reload.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Watch all canonical skills directories and reload the registry signal on
/// changes. Runs until the app exits; call once from a spawned task.
pub async fn watch_skills_directories(signal: dioxus::prelude::Signal<SkillRegistry>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // Build the watcher on a blocking thread (it does filesystem I/O), then
    // keep it alive in this task — dropping it stops the watch.
    let watcher_result =
        tokio::task::spawn_blocking(move || -> Result<RecommendedWatcher, String> {
            let mut watcher = notify::recommended_watcher(
                move |res: Result<notify::Event, notify::Error>| match res {
                    Ok(event) => {
                        if matches!(
                            event.kind,
                            notify::EventKind::Create(_)
                                | notify::EventKind::Modify(_)
                                | notify::EventKind::Remove(_)
                        ) {
                            let _ = tx.send(());
                        }
                    }
                    Err(e) => tracing::warn!("Skills watcher error: {}", e),
                },
            )
            .map_err(|e| e.to_string())?;

            for dir in get_skills_directories() {
                // Create missing directories so the watch can bind (a watch on
                // a nonexistent path fails rather than activating later).
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    tracing::warn!("Could not create skills directory {:?}: {}", dir, e);
                    continue;
                }
                if let Err(e) = watcher.watch(&dir, RecursiveMode::Recursive) {
                    tracing::warn!("Could not watch skills directory {:?}: {}", dir, e);
                }
            }
            Ok(watcher)
        })
        .await;

    let _watcher = match watcher_result {
        Ok(Ok(w)) => w,
        Ok(Err(e)) => {
            tracing::error!("Failed to start skills file watcher: {}", e);
            return;
        }
        Err(e) => {
            tracing::error!("Skills watcher setup task failed: {}", e);
            return;
        }
    };
    tracing::info!("Skills file watcher active on {:?}", get_skills_directories());

    while rx.recv().await.is_some() {
        // Debounce: absorb the burst until DEBOUNCE of quiet
        while let Ok(Some(())) = tokio::time::timeout(DEBOUNCE, rx.recv()).await {}
        tracing::debug!("Skills directory changed on disk; reloading registry");
        SkillRegistry::reload_into_signal(signal).await;
    }
}
