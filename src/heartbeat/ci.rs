//! CI watcher: the latest GitHub Actions runs for projects with a path.
//!
//! Shells out to the `gh` CLI (no tokens handled here: `gh` owns its own
//! auth), parses the JSON, and keeps the newest completed run per
//! (workflow, branch). The runtime diffs successive polls into events.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::process::Command;

use super::{CiConclusion, CiRun};

/// How often the Heartbeat polls GitHub for CI runs. Five minutes keeps a
/// handful of projects well inside `gh`'s rate limits.
pub const CI_POLL_MINUTES: i64 = 5;

/// Per-project `gh` timeout.
const GH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhRun {
    #[serde(default)]
    workflow_name: Option<String>,
    #[serde(default)]
    head_branch: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

fn map_conclusion(s: &str) -> CiConclusion {
    match s {
        "success" => CiConclusion::Success,
        "failure" => CiConclusion::Failure,
        "cancelled" => CiConclusion::Cancelled,
        _ => CiConclusion::Other,
    }
}

/// Parse `gh run list --json workflowName,headBranch,status,conclusion,url,updatedAt`
/// output. Only completed runs count; the newest per (workflow, branch) wins.
/// Malformed input yields an empty vec.
pub fn parse_gh_runs(json: &str, project_id: &str, project_title: &str) -> Vec<CiRun> {
    let raw: Vec<GhRun> = match serde_json::from_str(json) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("ci: unparseable gh output: {e}");
            return Vec::new();
        }
    };
    let mut newest: HashMap<(String, String), CiRun> = HashMap::new();
    for r in raw {
        if r.status.as_deref() != Some("completed") {
            continue;
        }
        let Some(finished_at) = r
            .updated_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
        else {
            continue;
        };
        let workflow = r.workflow_name.unwrap_or_default();
        let branch = r.head_branch.unwrap_or_default();
        let run = CiRun {
            project_id: project_id.to_string(),
            project_title: project_title.to_string(),
            workflow: workflow.clone(),
            branch: branch.clone(),
            conclusion: map_conclusion(r.conclusion.as_deref().unwrap_or("")),
            url: r.url.unwrap_or_default(),
            finished_at,
        };
        match newest.get(&(workflow.clone(), branch.clone())) {
            Some(existing) if existing.finished_at >= run.finished_at => {}
            _ => {
                newest.insert((workflow, branch), run);
            }
        }
    }
    let mut out: Vec<CiRun> = newest.into_values().collect();
    out.sort_by(|a, b| {
        b.finished_at
            .cmp(&a.finished_at)
            .then_with(|| a.workflow.cmp(&b.workflow))
            .then_with(|| a.branch.cmp(&b.branch))
    });
    out
}

/// GUI launches get a minimal PATH, so look for `gh` in the usual install
/// spots before falling back to a bare `gh` (resolved via PATH).
fn locate_gh() -> PathBuf {
    let mut dirs: Vec<PathBuf> = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
    ];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/bin"));
    }
    let exe = if cfg!(windows) { "gh.exe" } else { "gh" };
    dirs.into_iter()
        .map(|d| d.join(exe))
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("gh"))
}

/// PATH for the child: the usual dev dirs plus whatever we inherited, so
/// `gh` can find `git`.
fn child_path() -> String {
    let mut parts = vec!["/opt/homebrew/bin".to_string(), "/usr/local/bin".to_string()];
    if let Ok(p) = std::env::var("PATH") {
        parts.push(p);
    }
    parts.push("/usr/bin:/bin".to_string());
    parts.join(":")
}

/// Run `gh run list` in `repo_path` and parse the result.
pub async fn fetch_project_runs(
    project_id: &str,
    project_title: &str,
    repo_path: &str,
) -> Result<Vec<CiRun>, String> {
    let dir = Path::new(repo_path);
    if !dir.is_dir() {
        return Err("project folder not found".to_string());
    }
    let mut cmd = Command::new(locate_gh());
    cmd.args([
        "run",
        "list",
        "--limit",
        "10",
        "--json",
        "databaseId,workflowName,headBranch,status,conclusion,url,updatedAt",
    ])
    .current_dir(dir)
    .env("PATH", child_path())
    .env("GH_PROMPT_DISABLED", "1")
    .env("NO_COLOR", "1")
    .stdin(std::process::Stdio::null())
    .kill_on_drop(true);

    let output = match tokio::time::timeout(GH_TIMEOUT, cmd.output()).await {
        Err(_) => return Err("gh timed out".to_string()),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("gh CLI not installed".to_string());
        }
        Ok(Err(e)) => return Err(format!("could not run gh: {e}")),
        Ok(Ok(o)) => o,
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lower = stderr.to_lowercase();
        let msg = if lower.contains("gh auth login") || lower.contains("not logged in") {
            "gh is not authenticated".to_string()
        } else if lower.contains("not a git repository")
            || lower.contains("no git remotes")
            || lower.contains("could not determine")
            || lower.contains("none of the git remotes")
            || lower.contains("could not resolve to a repository")
        {
            "folder is not a GitHub repository".to_string()
        } else {
            format!("gh failed: {}", crate::log_utils::truncate_for_log(stderr.trim(), 160))
        };
        return Err(msg);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_gh_runs(&stdout, project_id, project_title))
}

/// Poll every `(id, title, path)` project one at a time (gentle on rate
/// limits). Projects that error are skipped; each distinct error is logged
/// once at debug.
pub async fn fetch_all(projects: Vec<(String, String, String)>) -> Vec<CiRun> {
    let mut all = Vec::new();
    for (id, title, path) in projects {
        match fetch_project_runs(&id, &title, &path).await {
            Ok(runs) => all.extend(runs),
            Err(e) => {
                if crate::log_utils::first_time(&format!("ci:{id}:{e}")) {
                    tracing::debug!("ci: skipping project {id}: {e}");
                }
            }
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {"databaseId":101,"workflowName":"CI","headBranch":"main","status":"completed","conclusion":"success","url":"https://github.com/o/r/actions/runs/101","updatedAt":"2026-10-20T10:00:00Z"},
      {"databaseId":100,"workflowName":"CI","headBranch":"main","status":"completed","conclusion":"failure","url":"https://github.com/o/r/actions/runs/100","updatedAt":"2026-10-19T10:00:00Z"},
      {"databaseId":102,"workflowName":"CI","headBranch":"feat/x","status":"completed","conclusion":"failure","url":"https://github.com/o/r/actions/runs/102","updatedAt":"2026-10-20T11:00:00Z"},
      {"databaseId":103,"workflowName":"Release","headBranch":"main","status":"completed","conclusion":"cancelled","url":"https://github.com/o/r/actions/runs/103","updatedAt":"2026-10-20T12:00:00Z"},
      {"databaseId":104,"workflowName":"Clippy","headBranch":"main","status":"completed","conclusion":"skipped","url":"https://github.com/o/r/actions/runs/104","updatedAt":"2026-10-20T09:00:00Z"},
      {"databaseId":105,"workflowName":"CI","headBranch":"feat/y","status":"in_progress","conclusion":"","url":"https://github.com/o/r/actions/runs/105","updatedAt":"2026-10-20T13:00:00Z"}
    ]"#;

    fn find<'a>(runs: &'a [CiRun], wf: &str, br: &str) -> Option<&'a CiRun> {
        runs.iter().find(|r| r.workflow == wf && r.branch == br)
    }

    #[test]
    fn maps_conclusions_and_skips_in_progress() {
        let runs = parse_gh_runs(SAMPLE, "p1", "Proj");
        assert_eq!(runs.len(), 4);
        assert_eq!(find(&runs, "CI", "feat/x").unwrap().conclusion, CiConclusion::Failure);
        assert_eq!(find(&runs, "Release", "main").unwrap().conclusion, CiConclusion::Cancelled);
        assert_eq!(find(&runs, "Clippy", "main").unwrap().conclusion, CiConclusion::Other);
        assert!(find(&runs, "CI", "feat/y").is_none());
        let r = find(&runs, "CI", "main").unwrap();
        assert_eq!(r.project_id, "p1");
        assert_eq!(r.project_title, "Proj");
        assert!(r.url.ends_with("/101"));
    }

    #[test]
    fn keeps_newest_per_workflow_and_branch() {
        let runs = parse_gh_runs(SAMPLE, "p1", "Proj");
        let r = find(&runs, "CI", "main").unwrap();
        assert_eq!(r.conclusion, CiConclusion::Success);
        assert_eq!(r.finished_at.to_rfc3339(), "2026-10-20T10:00:00+00:00");
        // Order-independent: newest wins even when listed oldest-first.
        let rev = r#"[
          {"workflowName":"CI","headBranch":"main","status":"completed","conclusion":"failure","url":"u1","updatedAt":"2026-10-19T10:00:00Z"},
          {"workflowName":"CI","headBranch":"main","status":"completed","conclusion":"success","url":"u2","updatedAt":"2026-10-20T10:00:00Z"}
        ]"#;
        let runs = parse_gh_runs(rev, "p", "P");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].url, "u2");
    }

    #[test]
    fn malformed_json_is_empty() {
        assert!(parse_gh_runs("not json", "p", "P").is_empty());
        assert!(parse_gh_runs("{\"a\":1}", "p", "P").is_empty());
        assert!(parse_gh_runs("", "p", "P").is_empty());
        assert!(parse_gh_runs("[]", "p", "P").is_empty());
    }

    #[test]
    fn tolerates_missing_fields() {
        let json = r#"[
          {"status":"completed","updatedAt":"2026-10-20T10:00:00Z"},
          {"status":"completed","workflowName":"CI","headBranch":"main"},
          {"workflowName":"CI","headBranch":"main","conclusion":"success","updatedAt":"2026-10-20T10:00:00Z"}
        ]"#;
        let runs = parse_gh_runs(json, "p", "P");
        // Only the first is usable: completed with a timestamp.
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].workflow, "");
        assert_eq!(runs[0].conclusion, CiConclusion::Other);
    }
}
