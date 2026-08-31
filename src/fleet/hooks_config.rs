//! `~/.claude/settings.json` hook registration.
//!
//! Connect merges our five `type: "http"` hook entries into the user's
//! settings file; Disconnect removes exactly ours. The merge/unmerge cores
//! are pure functions over `serde_json::Value` (unit-tested on fixtures);
//! the file layer adds parse-refuse-on-malformed, a timestamped backup, and
//! atomic-ish write. Nothing here ever touches the real `~/.claude` in
//! tests — every file entry point takes an explicit path.
//!
//! Ours are recognizable structurally: an `http` hook whose URL is
//! `http://127.0.0.1:{port}/fleet/{token}/{event}` — the `/fleet/` path on a
//! loopback URL marks it as Hobbes-owned, so idempotent re-runs replace only
//! our entries and user hooks are never rewritten.
//!
//! Hook config shape (verified against the hooks reference):
//! `hooks.<Event>` is an array of matcher groups `{matcher?, hooks: [handler…]}`;
//! an `http` handler is `{type: "http", url, timeout?}` with `timeout` in
//! **seconds** (default 600 for http hooks). We register no `matcher`, so tool
//! events match every tool.

use serde_json::{json, Map, Value};
use std::path::Path;

/// (event, timeout seconds). PermissionRequest gets a generous 120s hold
/// window — the listener answers ~10s before this expires so a silent
/// Hobbes never delays the terminal prompt by more than the margin. The
/// others are quick notifications; SessionEnd's shared budget rises to the
/// per-hook timeout (docs), so 10s is both prompt and sufficient.
pub const FLEET_HOOK_EVENTS: &[(&str, u64)] = &[
    ("SessionStart", 10),
    ("SessionEnd", 10),
    ("Stop", 10),
    ("Notification", 10),
    ("PermissionRequest", 120),
];

/// Seconds the PermissionRequest hook is configured to wait.
pub const PERMISSION_HOOK_TIMEOUT_SECS: u64 = 120;
/// The listener resolves a held gate as passthrough this many seconds BEFORE
/// the hook timeout, so the terminal prompt reliably appears as fallback.
pub const GATE_TIMEOUT_MARGIN_SECS: u64 = 10;

/// The URL a given event's hook posts to.
pub fn hook_url(port: u16, token: &str, event: &str) -> String {
    format!("http://127.0.0.1:{port}/fleet/{token}/{event}")
}

/// Is this handler object one of ours?
fn is_our_handler(handler: &Value) -> bool {
    handler.get("type").and_then(Value::as_str) == Some("http")
        && handler
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(|u| u.starts_with("http://127.0.0.1:") && u.contains("/fleet/"))
}

/// Structural validation shared by merge and unmerge: the settings root must
/// be an object, `hooks` (when present) an object, each event value an array,
/// each group an object, and each group's `hooks` (when present) an array.
/// Anything else → refuse, so a broken file is never rewritten broken-er.
fn validate(settings: &Value) -> Result<(), String> {
    let root = settings
        .as_object()
        .ok_or("settings root is not a JSON object")?;
    let Some(hooks) = root.get("hooks") else {
        return Ok(());
    };
    let hooks = hooks
        .as_object()
        .ok_or("\"hooks\" is not a JSON object")?;
    for (event, groups) in hooks {
        let groups = groups
            .as_array()
            .ok_or_else(|| format!("hooks.{event} is not an array"))?;
        for group in groups {
            let group = group
                .as_object()
                .ok_or_else(|| format!("hooks.{event} contains a non-object entry"))?;
            if let Some(handlers) = group.get("hooks") {
                handlers
                    .as_array()
                    .ok_or_else(|| format!("hooks.{event} has a non-array \"hooks\" field"))?;
            }
        }
    }
    Ok(())
}

/// Strip our handlers from every event array in place. Groups left with an
/// empty `hooks` array are dropped; events left with no groups are dropped.
/// User-authored groups and handlers pass through byte-identical.
fn strip_ours(hooks: &mut Map<String, Value>) {
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        if let Some(groups) = hooks.get_mut(&event).and_then(Value::as_array_mut) {
            for group in groups.iter_mut() {
                if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                    handlers.retain(|h| !is_our_handler(h));
                }
            }
            groups.retain(|g| {
                g.get("hooks")
                    .and_then(Value::as_array)
                    .is_none_or(|h| !h.is_empty())
            });
            if groups.is_empty() {
                hooks.remove(&event);
            }
        }
    }
}

/// Pure merge: remove any previous Hobbes entries, then append ours for each
/// fleet event. Idempotent — re-running with a new port/token replaces the
/// old entries instead of stacking.
pub fn merge_fleet_hooks(settings: &mut Value, port: u16, token: &str) -> Result<(), String> {
    validate(settings)?;
    let root = settings.as_object_mut().expect("validated as object");
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("validated as object");
    strip_ours(hooks);

    for (event, timeout) in FLEET_HOOK_EVENTS {
        let handler = json!({
            "type": "http",
            "url": hook_url(port, token, event),
            "timeout": timeout,
        });
        let group = json!({ "hooks": [handler] });
        hooks
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("validated as array")
            .push(group);
    }
    Ok(())
}

/// Pure unmerge: remove exactly our entries. A `hooks` object left empty is
/// removed entirely (leaving no trace of the connect).
pub fn remove_fleet_hooks(settings: &mut Value) -> Result<(), String> {
    validate(settings)?;
    let root = settings.as_object_mut().expect("validated as object");
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        strip_ours(hooks);
        if hooks.is_empty() {
            root.remove("hooks");
        }
    }
    Ok(())
}

/// The port our hooks currently point at in this settings value, if any —
/// the "connected" status probe.
pub fn connected_port(settings: &Value) -> Option<u16> {
    let hooks = settings.get("hooks")?.as_object()?;
    for groups in hooks.values() {
        for group in groups.as_array()? {
            for handler in group.get("hooks")?.as_array()? {
                if is_our_handler(handler) {
                    let url = handler.get("url")?.as_str()?;
                    let rest = url.strip_prefix("http://127.0.0.1:")?;
                    let port_str = rest.split('/').next()?;
                    if let Ok(p) = port_str.parse() {
                        return Some(p);
                    }
                }
            }
        }
    }
    None
}

// ── File layer ──────────────────────────────────────────────────────────────

/// Where the user-level Claude Code settings live.
pub fn claude_settings_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

fn read_settings(path: &Path) -> Result<Value, String> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&raw)
        .map_err(|e| format!("{} is not valid JSON ({e}) — fix it or move it aside first", path.display()))
}

/// Timestamped sibling backup before any rewrite. Only when the file exists.
fn backup(path: &Path) -> Result<Option<std::path::PathBuf>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup_path = path.with_file_name(format!(
        "{}.hobbes-backup-{stamp}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("settings.json")
    ));
    std::fs::copy(path, &backup_path)
        .map_err(|e| format!("backup {}: {e}", backup_path.display()))?;
    Ok(Some(backup_path))
}

fn write_settings(path: &Path, settings: &Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let pretty =
        serde_json::to_string_pretty(settings).map_err(|e| format!("serialize settings: {e}"))?;
    std::fs::write(path, pretty).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Connect: parse-preserve-merge our hooks into the settings file at `path`.
/// Malformed JSON refuses before any write; an existing file is backed up
/// first.
pub fn connect_file(path: &Path, port: u16, token: &str) -> Result<(), String> {
    let mut settings = read_settings(path)?;
    merge_fleet_hooks(&mut settings, port, token)?;
    backup(path)?;
    write_settings(path, &settings)
}

/// Disconnect: remove exactly our entries from the settings file at `path`.
/// A missing file is already disconnected.
pub fn disconnect_file(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let mut settings = read_settings(path)?;
    remove_fleet_hooks(&mut settings)?;
    backup(path)?;
    write_settings(path, &settings)
}

/// The port the settings file's Hobbes hooks point at, if connected.
pub fn connected_port_file(path: &Path) -> Option<u16> {
    read_settings(path).ok().and_then(|v| connected_port(&v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged_empty() -> Value {
        let mut v = json!({});
        merge_fleet_hooks(&mut v, 43917, "tok123").unwrap();
        v
    }

    #[test]
    fn merge_into_empty_registers_all_five_events() {
        let v = merged_empty();
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 5);
        for (event, timeout) in FLEET_HOOK_EVENTS {
            let groups = hooks[*event].as_array().unwrap();
            assert_eq!(groups.len(), 1);
            let handler = &groups[0]["hooks"][0];
            assert_eq!(handler["type"], "http");
            assert_eq!(
                handler["url"],
                format!("http://127.0.0.1:43917/fleet/tok123/{event}")
            );
            assert_eq!(handler["timeout"], *timeout);
            assert!(groups[0].get("matcher").is_none(), "no matcher: match all tools");
        }
        // PermissionRequest carries the generous hold timeout.
        assert_eq!(
            v["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"],
            PERMISSION_HOOK_TIMEOUT_SECS
        );
    }

    #[test]
    fn merge_preserves_existing_user_hooks() {
        let mut v = json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "./check.sh" }] }
                ],
                "Stop": [
                    { "hooks": [{ "type": "command", "command": "say done" }] }
                ]
            }
        });
        merge_fleet_hooks(&mut v, 43917, "tok").unwrap();
        // Untouched user config survives byte-for-byte.
        assert_eq!(v["model"], "opus");
        assert_eq!(v["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "./check.sh");
        // Stop keeps the user group AND gains ours.
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "say done");
        assert!(is_our_handler(&stop[1]["hooks"][0]));
    }

    #[test]
    fn merge_is_idempotent_and_replaces_stale_entries() {
        let mut v = merged_empty();
        // Re-run with a different port/token: exactly one Hobbes entry per
        // event, pointing at the new URL.
        merge_fleet_hooks(&mut v, 50000, "newtok").unwrap();
        for (event, _) in FLEET_HOOK_EVENTS {
            let groups = v["hooks"][*event].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{event} must not stack duplicates");
            assert_eq!(
                groups[0]["hooks"][0]["url"],
                format!("http://127.0.0.1:50000/fleet/newtok/{event}")
            );
        }
    }

    #[test]
    fn unmerge_removes_exactly_ours() {
        let mut v = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "command", "command": "say done" }] }
                ]
            },
            "permissions": { "allow": ["Bash(ls *)"] }
        });
        merge_fleet_hooks(&mut v, 43917, "tok").unwrap();
        remove_fleet_hooks(&mut v).unwrap();
        assert_eq!(
            v,
            json!({
                "hooks": {
                    "Stop": [
                        { "hooks": [{ "type": "command", "command": "say done" }] }
                    ]
                },
                "permissions": { "allow": ["Bash(ls *)"] }
            })
        );
    }

    #[test]
    fn unmerge_on_a_pure_hobbes_file_leaves_no_trace() {
        let mut v = merged_empty();
        remove_fleet_hooks(&mut v).unwrap();
        assert_eq!(v, json!({}), "an empty hooks object must be removed too");
    }

    #[test]
    fn a_user_http_hook_without_our_path_is_not_ours() {
        let mut v = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "http", "url": "http://127.0.0.1:8080/my-logger" }] },
                    { "hooks": [{ "type": "http", "url": "https://example.com/fleet/x/Stop" }] }
                ]
            }
        });
        remove_fleet_hooks(&mut v).unwrap();
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn malformed_shapes_refuse_without_modification() {
        for bad in [
            json!([1, 2, 3]),
            json!({"hooks": "nope"}),
            json!({"hooks": {"Stop": {"not": "an array"}}}),
            json!({"hooks": {"Stop": ["not an object"]}}),
            json!({"hooks": {"Stop": [{"hooks": "not an array"}]}}),
        ] {
            let mut m = bad.clone();
            assert!(merge_fleet_hooks(&mut m, 1, "t").is_err(), "{bad}");
            let mut u = bad.clone();
            assert!(remove_fleet_hooks(&mut u).is_err(), "{bad}");
            assert_eq!(u, bad, "refusal must not mutate");
        }
    }

    #[test]
    fn connected_port_probes_our_urls_only() {
        assert_eq!(connected_port(&json!({})), None);
        let foreign = json!({
            "hooks": { "Stop": [ { "hooks": [{ "type": "http", "url": "http://127.0.0.1:9/other" }] } ] }
        });
        assert_eq!(connected_port(&foreign), None);
        assert_eq!(connected_port(&merged_empty()), Some(43917));
    }

    // ── File layer (temp dirs only — never the real ~/.claude) ─────────────

    #[test]
    fn connect_creates_file_and_disconnect_restores_absence_of_ours() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        connect_file(&path, 43917, "tok").unwrap();
        assert_eq!(connected_port_file(&path), Some(43917));

        // No backup for a file that didn't exist.
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("hobbes-backup"))
            .collect();
        assert!(backups.is_empty());

        disconnect_file(&path).unwrap();
        assert_eq!(connected_port_file(&path), None);
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after, json!({}));
    }

    #[test]
    fn connect_backs_up_and_preserves_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = json!({
            "model": "opus",
            "hooks": { "Stop": [ { "hooks": [{ "type": "command", "command": "say done" }] } ] }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&original).unwrap()).unwrap();

        connect_file(&path, 43917, "tok").unwrap();

        // Backup carries the pre-merge bytes.
        let backup = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().contains("hobbes-backup"))
            .expect("a timestamped backup must exist");
        let backed: Value =
            serde_json::from_str(&std::fs::read_to_string(backup.path()).unwrap()).unwrap();
        assert_eq!(backed, original);

        // Merged file keeps the user's config.
        let merged: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(merged["model"], "opus");
        assert_eq!(merged["hooks"]["Stop"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn connect_refuses_a_malformed_file_and_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        assert!(connect_file(&path, 43917, "tok").is_err());
        assert!(disconnect_file(&path).is_err());
        // Untouched, and no backup was made for a refused operation.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ this is not json");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn disconnect_of_a_missing_file_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        disconnect_file(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn empty_file_counts_as_empty_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "  \n").unwrap();
        connect_file(&path, 1234, "tok").unwrap();
        assert_eq!(connected_port_file(&path), Some(1234));
    }
}
