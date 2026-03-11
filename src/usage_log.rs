use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Maximum file size for the usage log in bytes (5 MB).
/// Once the serialized log exceeds this threshold, the oldest 20% of
/// entries are trimmed before the next write.  This keeps the file
/// manageable until a proper API-backed metrics pipeline is in place.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// A single usage event recorded at the end of a completed LLM turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageLogEntry {
    pub timestamp: DateTime<Utc>,
    pub session_id: String,
    pub model: String,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
    #[serde(default)]
    pub thoughts_tokens: Option<i32>,
    #[serde(default)]
    pub cached_content_tokens: Option<i32>,
    /// Calculated cost in USD
    #[serde(default)]
    pub cost: Option<f64>,
}

/// Append-only usage ledger persisted to `usage_log.json`.
///
/// Exists independently of session data so usage history survives
/// session deletion, data migration, and backup restoration.
///
/// A 5 MB size cap prevents unbounded growth.  When the cap is
/// exceeded the oldest 20 % of entries are dropped (FIFO).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UsageLog {
    pub entries: Vec<UsageLogEntry>,
}

fn get_usage_log_path() -> Option<PathBuf> {
    dirs::config_dir().and_then(|mut path| {
        path.push("com.hobbes.app");
        fs::create_dir_all(&path).ok()?;
        path.push("usage_log.json");
        Some(path)
    })
}

impl UsageLog {
    /// Load the usage log from disk, or return an empty log if the file
    /// doesn't exist or can't be parsed.
    pub fn load() -> Self {
        let Some(path) = get_usage_log_path() else {
            tracing::warn!("Could not determine usage log path");
            return Self::default();
        };

        match fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
                tracing::warn!("Failed to parse usage log, starting fresh: {}", e);
                Self::default()
            }),
            Err(_) => {
                // File doesn't exist yet — that's fine
                Self::default()
            }
        }
    }

    /// Append an entry and persist to disk asynchronously.
    ///
    /// If the log has grown beyond [`MAX_LOG_BYTES`] the oldest 20 %
    /// of entries are trimmed before serialisation.
    pub fn record(&mut self, entry: UsageLogEntry) {
        self.entries.push(entry);
        self.enforce_size_cap();
        self.save_async();
    }

    /// Sum of all recorded costs.
    #[allow(dead_code)] // Future: wired in when UsageLog drives the All-Time UI
    pub fn total_cost(&self) -> f64 {
        self.entries.iter().filter_map(|e| e.cost).sum()
    }

    /// Sum of all recorded tokens.
    #[allow(dead_code)] // Future: wired in when UsageLog drives the All-Time UI
    pub fn total_tokens(&self) -> i64 {
        self.entries.iter().map(|e| e.total_tokens as i64).sum()
    }

    /// Drop the oldest 20 % of entries when the serialized size exceeds
    /// [`MAX_LOG_BYTES`].  Uses a quick byte-length estimate to avoid
    /// serializing twice on the hot path.
    fn enforce_size_cap(&mut self) {
        self.enforce_size_cap_limit(MAX_LOG_BYTES);
    }

    /// Testable inner implementation — accepts a custom cap.
    #[cfg(not(test))]
    fn enforce_size_cap_limit(&mut self, cap: u64) {
        self.enforce_size_cap_inner(cap);
    }

    /// Public in test builds so unit tests can exercise with a small cap.
    #[cfg(test)]
    pub fn enforce_size_cap_limit(&mut self, cap: u64) {
        self.enforce_size_cap_inner(cap);
    }

    fn enforce_size_cap_inner(&mut self, cap: u64) {
        // Quick estimate: serialized size ≈ current byte count.
        // We do a real check only when there are enough entries to matter.
        if self.entries.len() < 100 {
            return; // Definitely under cap
        }

        // Estimate: each entry serializes to ~250 bytes on average (pretty-printed).
        let estimate = self.entries.len() as u64 * 250;
        if estimate < cap {
            return;
        }

        // Confirm with actual serialization
        let actual = serde_json::to_vec(self)
            .map(|v| v.len() as u64)
            .unwrap_or(0);
        if actual > cap {
            let trim_count = self.entries.len() / 5; // 20 %
            let trim_count = trim_count.max(1);
            tracing::info!(
                "Usage log exceeds {} bytes (actual: {}). Trimming oldest {} entries.",
                cap,
                actual,
                trim_count
            );
            self.entries.drain(..trim_count);
        }
    }

    /// Persist the log to disk using the atomic serialize-then-move pattern.
    fn save_async(&self) {
        let bytes = match serde_json::to_vec_pretty(self) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("Failed to serialize usage log: {}", e);
                return;
            }
        };

        crate::async_persist::persist_bytes_async(bytes, Self::save_bytes, "usage log", None);
    }

    /// Write pre-serialized bytes to the usage log file atomically.
    fn save_bytes(bytes: Vec<u8>) -> Result<(), std::io::Error> {
        use std::io::Write;

        let path = get_usage_log_path().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "Could not find usage log path")
        })?;
        let parent_dir = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Could not find parent directory",
            )
        })?;

        let mut temp_file = tempfile::NamedTempFile::new_in(parent_dir)?;
        temp_file.write_all(&bytes)?;
        temp_file.as_file().sync_all()?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::Permissions::from_mode(0o600);
            temp_file.as_file().set_permissions(permissions)?;
        }

        temp_file.persist(&path).map_err(|e| e.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_usage_log_append_and_read() {
        let mut log = UsageLog::default();
        assert!(log.entries.is_empty());

        log.entries.push(UsageLogEntry {
            timestamp: Utc::now(),
            session_id: "test-session".to_string(),
            model: "gemini-2.5-flash".to_string(),
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            thoughts_tokens: Some(10),
            cached_content_tokens: None,
            cost: Some(0.0023),
        });

        assert_eq!(log.entries.len(), 1);
        assert_eq!(log.entries[0].session_id, "test-session");
        assert_eq!(log.entries[0].total_tokens, 150);
    }

    #[test]
    fn test_usage_log_total_cost() {
        let mut log = UsageLog::default();

        log.entries.push(UsageLogEntry {
            timestamp: Utc::now(),
            session_id: "s1".to_string(),
            model: "gemini-2.5-flash".to_string(),
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            thoughts_tokens: None,
            cached_content_tokens: None,
            cost: Some(0.01),
        });

        log.entries.push(UsageLogEntry {
            timestamp: Utc::now(),
            session_id: "s2".to_string(),
            model: "gemini-3.0-pro".to_string(),
            prompt_tokens: 200,
            completion_tokens: 100,
            total_tokens: 300,
            thoughts_tokens: None,
            cached_content_tokens: None,
            cost: Some(0.05),
        });

        assert!((log.total_cost() - 0.06).abs() < 0.0001);
        assert_eq!(log.total_tokens(), 450);
    }

    #[test]
    fn test_usage_log_serialization_roundtrip() {
        let mut log = UsageLog::default();
        log.entries.push(UsageLogEntry {
            timestamp: Utc::now(),
            session_id: "roundtrip-test".to_string(),
            model: "test-model".to_string(),
            prompt_tokens: 42,
            completion_tokens: 13,
            total_tokens: 55,
            thoughts_tokens: Some(5),
            cached_content_tokens: Some(10),
            cost: Some(0.001),
        });

        let json = serde_json::to_string_pretty(&log).expect("serialize");
        let loaded: UsageLog = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].session_id, "roundtrip-test");
        assert_eq!(loaded.entries[0].thoughts_tokens, Some(5));
        assert!((loaded.total_cost() - 0.001).abs() < 0.00001);
    }

    #[test]
    fn test_enforce_size_cap_trims_oldest() {
        let mut log = UsageLog::default();

        // Use 200 entries with a small cap (10 KB) to reliably trigger FIFO trim.
        for i in 0..200 {
            log.entries.push(UsageLogEntry {
                timestamp: Utc::now(),
                session_id: format!("session-{:05}", i),
                model: "gemini-2.5-flash-preview-04-17".to_string(),
                prompt_tokens: 10_000,
                completion_tokens: 5_000,
                total_tokens: 15_000,
                thoughts_tokens: Some(1_000),
                cached_content_tokens: Some(2_000),
                cost: Some(0.0123),
            });
        }

        let original_len = log.entries.len();
        // Use a 10 KB cap so the trim definitely fires
        log.enforce_size_cap_limit(10 * 1024);

        // Should have trimmed 20 %
        assert!(log.entries.len() < original_len);
        let expected_remaining = original_len - (original_len / 5);
        assert_eq!(log.entries.len(), expected_remaining);

        // The remaining entries should be the NEWEST ones (FIFO trim)
        assert_eq!(
            log.entries[0].session_id,
            format!("session-{:05}", original_len / 5)
        );
    }

    /// Verify that enforce_size_cap is truly a no-op for small logs.
    #[test]
    fn test_enforce_size_cap_noop_when_small() {
        let mut log = UsageLog::default();
        for i in 0..10 {
            log.entries.push(UsageLogEntry {
                timestamp: Utc::now(),
                session_id: format!("s-{}", i),
                model: "test".to_string(),
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                thoughts_tokens: None,
                cached_content_tokens: None,
                cost: Some(0.0001),
            });
        }
        log.enforce_size_cap();
        assert_eq!(log.entries.len(), 10, "Small log should not be trimmed");
    }
}
