//! The trust decision log — every approve/deny/rule-hit, append-only, in its
//! own migration domain of the shared `sessions.db` (the `fleet::store`
//! shape). Survives session deletion (unlike the session journal) and is
//! queryable for the future "you approved X twelve times — add a rule?"
//! proposal layer.
//!
//! **max_seq rule**: `trust_decisions` is folded into [`max_seq`], which
//! `session_store::seed_seq_from_db` consults — omitting it means a
//! restarted process draws seqs below existing rows and every write silently
//! vanishes.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::session_store::{is_available, next_seq, with_conn};

const DOMAIN: &str = "trust";

const MIGRATION_V1: &str = "
    CREATE TABLE IF NOT EXISTS trust_decisions (
        id          TEXT PRIMARY KEY,
        ts          TEXT NOT NULL,
        session_id  TEXT,
        project_id  TEXT,
        server      TEXT NOT NULL,
        tool        TEXT NOT NULL,
        arg_summary TEXT NOT NULL,
        decision    TEXT NOT NULL,
        rule_id     TEXT,
        seq         INTEGER NOT NULL,
        data        TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_trust_decisions_server_tool
        ON trust_decisions(server, tool, ts);
";

const MIGRATIONS: &[(i32, &str)] = &[(1, MIGRATION_V1)];

pub fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             domain  TEXT PRIMARY KEY,
             version INTEGER NOT NULL
         );",
    )?;
    let current: i32 = conn
        .query_row(
            "SELECT version FROM schema_migrations WHERE domain = ?1",
            params![DOMAIN],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    for (version, sql) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (domain, version) VALUES (?1, ?2)
             ON CONFLICT(domain) DO UPDATE SET version = excluded.version",
            params![DOMAIN, version],
        )?;
        tx.commit()?;
        tracing::info!("Trust schema migrated to v{}", version);
    }
    Ok(())
}

pub(crate) fn max_seq(conn: &Connection) -> i64 {
    conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM trust_decisions", [], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// A trust rule auto-approved the call.
    RuleHit,
    /// The user clicked Approve.
    Approved,
    /// The user clicked Approve and authored a rule in the same gesture.
    ApprovedRuleCreated,
    /// The user clicked Deny.
    Denied,
}

impl DecisionKind {
    fn as_str(&self) -> &'static str {
        match self {
            DecisionKind::RuleHit => "rule_hit",
            DecisionKind::Approved => "approved",
            DecisionKind::ApprovedRuleCreated => "approved_rule_created",
            DecisionKind::Denied => "denied",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrustDecision {
    pub id: String,
    pub ts: DateTime<Utc>,
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    pub server: String,
    pub tool: String,
    pub arg_summary: String,
    pub decision: DecisionKind,
    pub rule_id: Option<String>,
}

impl TrustDecision {
    pub fn new(
        decision: DecisionKind,
        server: &str,
        tool: &str,
        arg_summary: String,
        session_id: Option<String>,
        project_id: Option<String>,
        rule_id: Option<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            ts: Utc::now(),
            session_id,
            project_id,
            server: server.to_string(),
            tool: tool.to_string(),
            arg_summary,
            decision,
            rule_id,
        }
    }
}

/// A compact one-line summary of a call's arguments for the log — the
/// command for terminal calls, compact JSON otherwise, clipped.
pub fn arg_summary(args: &serde_json::Value) -> String {
    let raw = args
        .get("command")
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| args.to_string());
    if raw.chars().count() > 200 {
        format!("{}…", raw.chars().take(199).collect::<String>())
    } else {
        raw
    }
}

fn fmt_ts(ts: &DateTime<Utc>) -> String {
    ts.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

fn insert_conn(conn: &Connection, d: &TrustDecision, seq: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO trust_decisions
         (id, ts, session_id, project_id, server, tool, arg_summary, decision, rule_id, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            d.id,
            fmt_ts(&d.ts),
            d.session_id,
            d.project_id,
            d.server,
            d.tool,
            d.arg_summary,
            d.decision.as_str(),
            d.rule_id,
            seq,
            serde_json::to_string(d).unwrap_or_default(),
        ],
    )?;
    Ok(())
}

/// Best-effort append — never blocks or fails the caller.
pub fn log_decision(d: TrustDecision) {
    if !is_available() {
        return;
    }
    if let Err(e) = with_conn(|conn| insert_conn(conn, &d, next_seq())) {
        tracing::error!("trust: failed to log decision: {e}");
    }
}

fn row_to_decision(data: String) -> Option<TrustDecision> {
    serde_json::from_str(&data).ok()
}

/// Most recent decisions, newest first. Reserved for the aggregated
/// proposal layer ("you approved X twelve times — add a rule?").
#[allow(dead_code)]
pub fn recent(limit: usize) -> Vec<TrustDecision> {
    if !is_available() {
        return Vec::new();
    }
    with_conn(|conn| {
        let mut stmt =
            conn.prepare("SELECT data FROM trust_decisions ORDER BY ts DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |r| r.get::<_, String>(0))?;
        Ok(rows.flatten().filter_map(row_to_decision).collect())
    })
    .unwrap_or_default()
}

/// Per-rule hit statistics: rule_id → (hit count, last hit ts). Feeds the
/// settings list — the rules themselves carry no counters.
pub fn rule_stats() -> std::collections::HashMap<String, (u32, String)> {
    if !is_available() {
        return Default::default();
    }
    with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT rule_id, COUNT(*), MAX(ts) FROM trust_decisions
             WHERE rule_id IS NOT NULL GROUP BY rule_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)? as u32, r.get::<_, String>(2)?),
            ))
        })?;
        Ok(rows.flatten().collect())
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::test_support::with_test_db;

    fn decision(kind: DecisionKind, rule: Option<&str>) -> TrustDecision {
        TrustDecision::new(
            kind,
            "hobbes-terminal",
            "HOBBES_TERMINAL_EXEC",
            "cargo test".into(),
            Some("sess-1".into()),
            Some("proj-1".into()),
            rule.map(str::to_string),
        )
    }

    #[test]
    fn log_recent_and_stats_round_trip() {
        with_test_db(|conn| {
            insert_conn(conn, &decision(DecisionKind::Approved, None), 1).unwrap();
            insert_conn(conn, &decision(DecisionKind::RuleHit, Some("r1")), 2).unwrap();
            insert_conn(conn, &decision(DecisionKind::RuleHit, Some("r1")), 3).unwrap();
            insert_conn(conn, &decision(DecisionKind::Denied, None), 4).unwrap();

            let mut stmt = conn.prepare("SELECT data FROM trust_decisions ORDER BY seq").unwrap();
            let all: Vec<TrustDecision> = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .flatten()
                .filter_map(row_to_decision)
                .collect();
            assert_eq!(all.len(), 4);
            assert_eq!(all[1].decision, DecisionKind::RuleHit);
            assert_eq!(all[1].rule_id.as_deref(), Some("r1"));

            // Stats aggregate per rule.
            let stats: std::collections::HashMap<String, i64> = conn
                .prepare("SELECT rule_id, COUNT(*) FROM trust_decisions WHERE rule_id IS NOT NULL GROUP BY rule_id")
                .unwrap()
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .flatten()
                .collect();
            assert_eq!(stats.get("r1"), Some(&2));

            // The silent-data-loss guard: seq seeding must see our writes.
            assert!(max_seq(conn) >= 4);
        });
    }

    #[test]
    fn arg_summaries_prefer_the_command_and_clip() {
        assert_eq!(
            arg_summary(&serde_json::json!({"command": "cargo test", "timeout_secs": 30})),
            "cargo test"
        );
        assert!(arg_summary(&serde_json::json!({"q": "x"})).contains("\"q\""));
        let long = arg_summary(&serde_json::json!({"command": "x".repeat(500)}));
        assert!(long.chars().count() <= 200);
        assert!(long.ends_with('…'));
    }
}
