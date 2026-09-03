//! Trust-rule evaluation: user-authored allow-rules checked BEFORE the
//! permission prompt. Pure — the gate in `mcp::manager` calls [`evaluate`]
//! inside the branch that would otherwise prompt; a hit skips the prompt,
//! anything else falls through unchanged. Rules never touch the
//! `always_allow` or bypass paths.
//!
//! SECURITY: the command-prefix matcher is deliberately conservative. A
//! prefix rule auto-allows only a command that (a) matches the prefix at
//! token boundaries and (b) contains NO shell metacharacters anywhere —
//! `cargo test && rm -rf /` must always fall through to the prompt, even
//! under a `cargo test` rule. When in doubt, prompt.

use crate::context::permissions::TrustRule;

/// Session-derived context for scoped rules.
pub struct TrustCtx<'a> {
    pub project_id: Option<&'a str>,
}

/// Any character that can chain, substitute, or redirect: its presence
/// anywhere in the command disqualifies prefix-rule auto-approval.
fn has_shell_metachars(command: &str) -> bool {
    command.contains(';')
        || command.contains('&')
        || command.contains('|')
        || command.contains('`')
        || command.contains("$(")
        || command.contains('>')
        || command.contains('<')
        || command.contains('\n')
        || command.contains('\r')
}

/// Token-boundary prefix match with the metachar guard. "cargo test"
/// matches "cargo test" and "cargo test --release"; never "cargo testx",
/// never anything containing a metacharacter.
pub fn command_matches_prefix(command: &str, prefix: &str) -> bool {
    let prefix_tokens: Vec<&str> = prefix.split_whitespace().collect();
    if prefix_tokens.is_empty() {
        return false;
    }
    if has_shell_metachars(command) {
        return false;
    }
    let cmd_tokens: Vec<&str> = command.split_whitespace().collect();
    if cmd_tokens.len() < prefix_tokens.len() {
        return false;
    }
    prefix_tokens
        .iter()
        .zip(cmd_tokens.iter())
        .all(|(p, c)| p == c)
}

/// First matching rule wins.
pub fn evaluate<'a>(
    rules: &'a [TrustRule],
    server: &str,
    tool: &str,
    args: &serde_json::Value,
    ctx: &TrustCtx,
) -> Option<&'a TrustRule> {
    rules.iter().find(|rule| {
        if rule.server != server {
            return false;
        }
        if let Some(rule_tool) = rule.tool.as_deref() {
            if rule_tool != tool {
                return false;
            }
        }
        if let Some(rule_project) = rule.project_id.as_deref() {
            if ctx.project_id != Some(rule_project) {
                return false;
            }
        }
        if let Some(prefix) = rule.command_prefix.as_deref() {
            // A prefix rule with no command argument present never matches.
            let Some(command) = args.get("command").and_then(|c| c.as_str()) else {
                return false;
            };
            if !command_matches_prefix(command, prefix) {
                return false;
            }
        }
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn rule(
        server: &str,
        tool: Option<&str>,
        prefix: Option<&str>,
        project: Option<&str>,
    ) -> TrustRule {
        TrustRule {
            id: uuid::Uuid::new_v4().to_string(),
            server: server.into(),
            tool: tool.map(str::to_string),
            command_prefix: prefix.map(str::to_string),
            project_id: project.map(str::to_string),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn prefix_matches_at_token_boundaries_only() {
        assert!(command_matches_prefix("cargo test", "cargo test"));
        assert!(command_matches_prefix("cargo test --release", "cargo test"));
        assert!(command_matches_prefix("  cargo   test  -q ", "cargo test"));
        assert!(!command_matches_prefix("cargo testx", "cargo test"));
        assert!(!command_matches_prefix("cargotest", "cargo test"));
        assert!(!command_matches_prefix("cargo", "cargo test"));
        assert!(!command_matches_prefix("cargo test", ""));
        assert!(!command_matches_prefix("cargo test", "   "));
    }

    #[test]
    fn every_metachar_disqualifies() {
        for cmd in [
            "cargo test; rm -rf /",
            "cargo test && rm -rf /",
            "cargo test & echo x",
            "cargo test | tee /etc/passwd",
            "cargo test `whoami`",
            "cargo test $(whoami)",
            "cargo test > /etc/hosts",
            "cargo test < secrets",
            "cargo test\nrm -rf /",
            "cargo test\rrm -rf /",
        ] {
            assert!(
                !command_matches_prefix(cmd, "cargo test"),
                "must fall through to the prompt: {cmd:?}"
            );
        }
    }

    #[test]
    fn evaluate_matrix() {
        let rules = vec![
            rule("hobbes-terminal", Some("HOBBES_TERMINAL_EXEC"), Some("cargo test"), None),
            rule("composio-gmail", None, None, Some("proj-1")),
        ];
        let ctx_none = TrustCtx { project_id: None };
        let ctx_p1 = TrustCtx { project_id: Some("proj-1") };
        let cmd = |c: &str| serde_json::json!({ "command": c });

        // Terminal prefix rule.
        assert!(evaluate(&rules, "hobbes-terminal", "HOBBES_TERMINAL_EXEC", &cmd("cargo test -q"), &ctx_none).is_some());
        assert!(evaluate(&rules, "hobbes-terminal", "HOBBES_TERMINAL_EXEC", &cmd("cargo build"), &ctx_none).is_none());
        assert!(
            evaluate(&rules, "hobbes-terminal", "HOBBES_TERMINAL_EXEC", &cmd("cargo test && rm -rf /"), &ctx_none).is_none(),
            "metachars fall through"
        );
        // Prefix rule with no command arg present never matches.
        assert!(evaluate(&rules, "hobbes-terminal", "HOBBES_TERMINAL_EXEC", &serde_json::json!({}), &ctx_none).is_none());
        // Wrong tool / wrong server.
        assert!(evaluate(&rules, "hobbes-terminal", "HOBBES_TERMINAL_RESET", &cmd("cargo test"), &ctx_none).is_none());
        assert!(evaluate(&rules, "other-server", "HOBBES_TERMINAL_EXEC", &cmd("cargo test"), &ctx_none).is_none());

        // Project-scoped server-wide rule (tool wildcard).
        assert!(evaluate(&rules, "composio-gmail", "GMAIL_SEND", &serde_json::json!({}), &ctx_p1).is_some());
        assert!(evaluate(&rules, "composio-gmail", "GMAIL_SEND", &serde_json::json!({}), &ctx_none).is_none());
        assert!(
            evaluate(
                &rules,
                "composio-gmail",
                "GMAIL_SEND",
                &serde_json::json!({}),
                &TrustCtx { project_id: Some("proj-2") }
            )
            .is_none()
        );

        // First match wins: duplicate broader rule after a narrower one.
        let mut ordered = rules.clone();
        ordered.push(rule("hobbes-terminal", None, None, None));
        let hit = evaluate(&ordered, "hobbes-terminal", "HOBBES_TERMINAL_EXEC", &cmd("cargo test"), &ctx_none).unwrap();
        assert_eq!(hit.id, ordered[0].id);
    }
}
