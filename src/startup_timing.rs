//! Startup phase timings: what runs before the window shows its first frame
//! (store open, migrations, hydration, settings and planner loads), logged
//! as one INFO summary line once the first frame is up. Durations and
//! counts only, never user content.
//!
//! A phase that scales with the size of the chat history shows up here
//! first: the one-time `tab_title` backfill held the window for minutes on
//! a 1.5 GB store before it moved to a background thread.

use std::sync::Mutex;
use std::time::Instant;

static PHASES: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());
static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Mark process start (first thing in `main`).
pub fn begin() {
    let _ = START.get_or_init(Instant::now);
}

/// Run `f` and record how long it took under `label`.
pub fn time<T>(label: &'static str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let out = f();
    record(label, started);
    out
}

/// Record the time since `started` under `label`.
pub fn record(label: &'static str, started: Instant) {
    let ms = started.elapsed().as_millis() as u64;
    if let Ok(mut phases) = PHASES.lock() {
        phases.push((label, ms));
    }
}

/// The summary line, e.g. `store open 12 ms, hydrate 80 ms, total 140 ms`.
fn summary(phases: &[(&'static str, u64)], total_ms: Option<u64>) -> String {
    let mut parts: Vec<String> = phases.iter().map(|(label, ms)| format!("{label} {ms} ms")).collect();
    if let Some(total) = total_ms {
        parts.push(format!("first frame {total} ms"));
    }
    parts.join(", ")
}

/// Log the summary once (the first frame is up). Later calls do nothing.
pub fn report() {
    if REPORTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let total = START.get().map(|s| s.elapsed().as_millis() as u64);
    let phases = PHASES.lock().map(|p| p.clone()).unwrap_or_default();
    tracing::info!("startup: {}", summary(&phases, total));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_lists_phases_in_order_then_the_total() {
        let phases = [("store open", 12), ("hydrate", 80)];
        assert_eq!(summary(&phases, Some(140)), "store open 12 ms, hydrate 80 ms, first frame 140 ms");
        assert_eq!(summary(&[], None), "");
    }
}
