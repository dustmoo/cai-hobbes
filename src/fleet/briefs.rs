//! Re-entry brief and day-rollup engine (pure logic).
//!
//! Everything here is LLM-free and side-effect-free: debounce decisions,
//! task collection, prompt framing, and tolerant mapping of the connector's
//! fixed `summarize_conversation` JSON schema onto [`SessionBrief`]. The
//! actual LLM calls happen in the brief supervisor (`main.rs`, Dioxus side —
//! the only place with hydrated API keys) via the defaulted
//! `LlmConnector::generate_fleet_brief` / `generate_fleet_rollup` methods.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::{truncate_summary, FleetSession, FleetState, FleetStatus, SessionBrief};

/// A Stop marks the session dirty; the brief generates once the session has
/// been quiet this long (SessionEnd rows skip the wait). Coalesces the
/// per-turn Stop spam into one call per work burst.
pub const BRIEF_QUIET_SECS: i64 = 45;
/// The quiet period for a finished dispatched run still owed its hand-back
/// ([`super::awaiting_dispatch_handback`]): its parent chat is waiting on
/// this brief, so it only settles long enough for a headless run's
/// SessionEnd (which follows its last Stop almost at once) to land. Once the
/// continuation fires, later turns debounce on [`BRIEF_QUIET_SECS`] again.
pub const DISPATCH_BRIEF_QUIET_SECS: i64 = 5;
/// Sessions listed in the AI's `planner_today` fleet block.
pub const CTX_MAX_FLEET: usize = 6;
/// Headline clip at generation time (display clips further).
pub const HEADLINE_MAX_CHARS: usize = 200;
const MAX_BULLETS: usize = 5;

/// One unit of brief work for the supervisor.
#[derive(Debug, Clone, PartialEq)]
pub struct BriefTask {
    pub session_id: String,
    pub name: String,
    pub cwd: String,
    pub transcript_path: String,
    pub previous_brief: Option<SessionBrief>,
    pub dirty_at: DateTime<Utc>,
    /// Ended row → final brief, merged into the store (not the live map).
    pub is_final: bool,
    /// Title of the todo linked to this session, when one is — the brief
    /// prompt assesses progress against it.
    pub linked_todo: Option<String>,
    /// The session had background work running when this task was collected
    /// — the prompt is told, so the model stops resurrecting blockers the
    /// running work has already moved past.
    pub background_active: bool,
}

/// Debounce decision for one session.
pub fn brief_due(s: &FleetSession, now: DateTime<Utc>, quiet_secs: i64) -> bool {
    let Some(dirty_at) = s.brief_dirty_at else {
        return false;
    };
    if s.transcript_path.is_none() {
        return false;
    }
    // Already briefed at or after the dirty mark → clean.
    if s.brief.as_ref().is_some_and(|b| b.generated_at >= dirty_at) {
        return false;
    }
    let is_final = s.ended_at.is_some();
    let quiet_secs = if super::awaiting_dispatch_handback(s) {
        quiet_secs.min(DISPATCH_BRIEF_QUIET_SECS)
    } else {
        quiet_secs
    };
    is_final || (now - dirty_at).num_seconds() >= quiet_secs
}

/// Collect due work: live sessions plus today's ended rows, deduped by id
/// (live wins), ended-first so closing briefs land promptly.
pub fn collect_due(
    live: &FleetState,
    ended_rows: &[FleetSession],
    links: &[(String, String)],
    now: DateTime<Utc>,
) -> Vec<BriefTask> {
    let to_task = |s: &FleetSession| -> Option<BriefTask> {
        brief_due(s, now, BRIEF_QUIET_SECS).then(|| BriefTask {
            session_id: s.id.clone(),
            name: s.name.clone(),
            cwd: s.cwd.clone(),
            transcript_path: s.transcript_path.clone().unwrap_or_default(),
            previous_brief: s.brief.clone(),
            dirty_at: s.brief_dirty_at.unwrap_or(now),
            is_final: s.ended_at.is_some(),
            linked_todo: links
                .iter()
                .find(|(sid, _)| sid == &s.id)
                .map(|(_, title)| title.clone()),
            background_active: s.status == FleetStatus::WorkingBackground,
        })
    };
    let mut tasks: Vec<BriefTask> = ended_rows
        .iter()
        .filter(|r| !live.sessions.contains_key(&r.id))
        .filter_map(to_task)
        .collect();
    tasks.extend(live.sessions.values().filter_map(to_task));
    tasks.sort_by_key(|t| (!t.is_final, t.dirty_at));
    tasks
}

/// First retry delay after a failed brief generation; doubles per failure.
pub const BRIEF_RETRY_BASE_SECS: i64 = 30;
/// Longest a failing session waits between attempts.
pub const BRIEF_RETRY_MAX_SECS: i64 = 30 * 60;

/// Per-session exponential backoff for the brief supervisor. It takes one
/// task per tick, oldest dirty first, so a task that keeps failing (a
/// provider outage, a transcript the model chokes on) would sit at the head
/// and starve every other session. A failed session waits out its backoff
/// while the supervisor moves on to the next due task.
#[derive(Debug, Default)]
pub struct BriefBackoff {
    /// session id → (consecutive failures, not before).
    entries: std::collections::HashMap<String, (u32, DateTime<Utc>)>,
}

impl BriefBackoff {
    /// The first due task not backing off. Forgets sessions that are no
    /// longer due (briefed, or their dirty mark was cleared).
    pub fn pick(&mut self, due: Vec<BriefTask>, now: DateTime<Utc>) -> Option<BriefTask> {
        self.entries
            .retain(|id, _| due.iter().any(|t| &t.session_id == id));
        due.into_iter().find(|t| {
            self.entries
                .get(&t.session_id)
                .is_none_or(|(_, not_before)| *not_before <= now)
        })
    }

    /// Record a failed attempt: the next one waits
    /// `BRIEF_RETRY_BASE_SECS · 2^(failures-1)`, capped at
    /// [`BRIEF_RETRY_MAX_SECS`].
    pub fn failed(&mut self, session_id: &str, now: DateTime<Utc>) {
        let entry = self.entries.entry(session_id.to_string()).or_insert((0, now));
        entry.0 = entry.0.saturating_add(1);
        let shift = entry.0.saturating_sub(1).min(16);
        let delay = BRIEF_RETRY_BASE_SECS
            .saturating_mul(1i64 << shift)
            .min(BRIEF_RETRY_MAX_SECS);
        entry.1 = now + chrono::Duration::seconds(delay);
    }

    /// The session got a brief (or its dirty mark was cleared): reset.
    pub fn succeeded(&mut self, session_id: &str) {
        self.entries.remove(session_id);
    }
}

/// `(previous_summary, recent_messages)` framing for
/// `LlmConnector::generate_fleet_brief` — the connector's summarize prompt
/// is fixed, so the brief instructions ride the recent-messages slot (the
/// `summarize_tool_result` precedent).
pub fn brief_framing(
    task: &BriefTask,
    digest: &super::transcript::TranscriptDigest,
) -> (String, String) {
    // The previous brief seeds continuity — but never its blocker: feeding a
    // stale blocker back is how resolved blockers survive regeneration. The
    // model must re-derive blockers from the transcript alone.
    let previous = task
        .previous_brief
        .as_ref()
        .map(|b| {
            let mut b = b.clone();
            b.blocked_on = None;
            b
        })
        .and_then(|b| serde_json::to_string(&b).ok())
        .unwrap_or_default();
    let tracked = task
        .linked_todo
        .as_ref()
        .map(|t| {
            format!(
                " The user's tracked task for this session: '{}' — assess progress against it.",
                truncate_summary(t, 120)
            )
        })
        .unwrap_or_default();
    let background = if task.background_active {
        " This session currently has background work RUNNING (a shell or \
         monitor it launched itself) — it is not blocked or waiting on the \
         user; do not list what that work waits on as a blocker."
    } else {
        ""
    };
    let framed = format!(
        "You are writing a re-entry brief for a Claude Code coding-agent session \
         in project '{}' ({}). Below is the tail of its transcript ({} recent \
         turns). In the 'summary' field write ONE short sentence (under 120 \
         characters) stating what was accomplished or is in progress. Put \
         concrete decisions made into entities.key_decisions and anything \
         blocked or awaiting the user into entities.blockers — but ONLY if \
         the most recent turns still show it unresolved; a question the user \
         has since answered is not a blocker. Do not invent \
         work that is not shown in the transcript.{}{}\n--- transcript ---\n{}",
        task.name, task.cwd, digest.turn_count, tracked, background, digest.text
    );
    (previous, framed)
}

/// Map the connector's summarize schema (`summary`, `current_task`,
/// `entities.{key_decisions,key_topics,blockers}`) onto a [`SessionBrief`].
/// Tolerant of shape drift: any non-null object yields a brief.
pub fn brief_from_summary_value(
    v: &serde_json::Value,
    now: DateTime<Utc>,
    is_final: bool,
) -> Option<SessionBrief> {
    if v.is_null() {
        return None;
    }
    let str_at = |ptr: &str| -> Option<String> {
        v.pointer(ptr)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let list_at = |ptr: &str| -> Vec<String> {
        v.pointer(ptr)
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };

    // A summary that is itself JSON (a reply that wasn't parsed upstream) is
    // unwrapped; one with no usable text is not shown at all.
    let headline = clean_headline(&str_at("/summary").unwrap_or_default())
        .or_else(|| clean_headline(&str_at("/current_task").unwrap_or_default()))?;
    let mut bullets = list_at("/entities/key_decisions");
    bullets.extend(list_at("/entities/key_topics"));
    bullets.truncate(MAX_BULLETS);
    let blocked_on = list_at("/entities/blockers").into_iter().next();

    Some(SessionBrief {
        headline: truncate_summary(&headline, HEADLINE_MAX_CHARS),
        bullets,
        blocked_on,
        generated_at: now,
        final_brief: is_final,
    })
}

/// Longest question carried onto a Hobbes row's brief.
pub const ASK_MAX_CHARS: usize = 160;

/// The question a reply ends on, if it ends on one — the cheap "does this
/// turn ask the person something" test for Hobbes chat rows (no LLM call).
/// Looks at the last line only: its final sentence ending in `?`, or the
/// one before it when a short sign-off follows ("Want me to ship it? Let me
/// know."). A reply that ends in a code block asks nothing.
pub fn question_in_reply(text: &str) -> Option<String> {
    let t = text.trim_end();
    if t.is_empty() || t.ends_with("```") {
        return None;
    }
    let line = t.lines().rev().map(str::trim).find(|l| !l.is_empty())?;
    let line = strip_line_marker(line);
    let trim_wrap = |s: &str| -> String {
        s.trim()
            .trim_matches(|c: char| matches!(c, '*' | '_' | '`' | '(' | ')' | '"' | '\u{201C}' | '\u{201D}'))
            .trim()
            .to_string()
    };
    let sentences: Vec<String> = split_sentences(line)
        .into_iter()
        .map(|s| trim_wrap(&s))
        .filter(|s| !s.is_empty())
        .collect();
    let ask = match sentences.as_slice() {
        [.., last] if last.ends_with('?') => last.clone(),
        [.., q, last] if q.ends_with('?') && last.chars().count() <= 40 => q.clone(),
        _ => return None,
    };
    // A bare "?" (or "??") is punctuation, not a question.
    if ask.chars().filter(|c| c.is_alphanumeric()).count() < 3 {
        return None;
    }
    Some(truncate_summary(&ask, ASK_MAX_CHARS))
}

/// Drop a leading Markdown marker: quote, heading, bullet, numbered item.
fn strip_line_marker(line: &str) -> &str {
    let mut l = line.trim_start_matches('>').trim_start();
    l = l.trim_start_matches('#').trim_start();
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = l.strip_prefix(bullet) {
            return rest.trim_start();
        }
    }
    let digits = l.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &l[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return rest.trim_start();
        }
    }
    l
}

/// Split at sentence ends: `.`, `!` or `?` followed by whitespace.
fn split_sentences(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        cur.push(c);
        if matches!(c, '.' | '!' | '?') && chars.peek().is_some_and(|n| n.is_whitespace()) {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// The brief for a Hobbes chat row at a turn boundary: headline and bullets
/// from the rolling conversation summary, but the blocker ONLY from the
/// turn that just ended (`ask`). The summary's `entities.blockers` is a
/// rolling list the summarizer is told to preserve — feeding it in as the
/// row's ask kept answered questions on the board for days. With no
/// summary yet, a question still makes a brief (its own headline).
pub fn hobbes_turn_brief(
    summary: Option<&serde_json::Value>,
    ask: Option<String>,
    now: DateTime<Utc>,
    is_final: bool,
) -> Option<SessionBrief> {
    let ask = ask.map(|a| a.trim().to_string()).filter(|a| !a.is_empty());
    match summary.and_then(|v| brief_from_summary_value(v, now, is_final)) {
        Some(mut b) => {
            b.blocked_on = ask;
            Some(b)
        }
        None => ask.map(|a| SessionBrief {
            headline: truncate_summary(&a, HEADLINE_MAX_CHARS),
            bullets: Vec::new(),
            blocked_on: Some(a),
            generated_at: now,
            final_brief: is_final,
        }),
    }
}

/// A display-safe headline: prose passes through; text that is (or wraps) a
/// JSON object is parsed and its `summary`/`current_task` taken, salvaging a
/// truncated `"summary"` string; nothing recoverable gives `None`. Never
/// returns a string that itself looks like a JSON object.
pub fn clean_headline(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if !crate::llm::looks_like_json_object(t) {
        return Some(t.to_string());
    }
    let inner = crate::llm::extract_json_object(t)
        .and_then(|v| {
            ["/summary", "/current_task"].iter().find_map(|p| {
                v.pointer(p)
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && !crate::llm::looks_like_json_object(s))
                    .map(str::to_string)
            })
        })
        .or_else(|| crate::llm::salvage_summary_text(t))?;
    (!crate::llm::looks_like_json_object(&inner)).then_some(inner)
}

/// One display line from a brief: headline plus blocker, clipped — the shape
/// written onto linked todos' `latest_progress`. The blocker is passed in
/// rather than read off the brief: callers hand over
/// [`FleetSession::blocked_on`] (or a brief they know is current), so a
/// blocker from a turn the user already answered never reaches a to-do.
pub fn progress_line(brief: &SessionBrief, blocked: Option<&str>) -> String {
    // Briefs persisted before the headline was cleaned may still hold JSON.
    let headline = clean_headline(&brief.headline).unwrap_or_default();
    let mut line = truncate_summary(&headline, 160);
    if let Some(blocked) = blocked.map(str::trim).filter(|b| !b.is_empty()) {
        let sep = if line.is_empty() { "" } else { "; " };
        line.push_str(&format!("{sep}blocked: {}", truncate_summary(blocked, 80)));
    }
    truncate_summary(&line, 200)
}

// ── Day rollup ──────────────────────────────────────────────────────────────

/// A cached end-of-day narrative (session_store `meta` table, keyed by date).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayRollup {
    pub date: NaiveDate,
    pub narrative: String,
    pub generated_at: DateTime<Utc>,
    pub session_count: usize,
    pub total_minutes: u32,
}

pub fn rollup_meta_key(date: NaiveDate) -> String {
    format!("fleet_rollup_{}", date.format("%Y-%m-%d"))
}

pub use crate::todo::model::format_minutes;

/// Rich per-session lines for the rollup prompt: name — time — headline
/// (+ blocked), minutes-desc, capped.
pub fn rollup_lines(
    rows: &[FleetSession],
    date: NaiveDate,
    now: DateTime<Utc>,
    cap: usize,
) -> Vec<String> {
    session_lines(rows, date, now, cap, 1)
}

/// Step the AI-facing fleet minutes are rounded down to.
pub const CTX_MINUTES_STEP: u32 = 15;

/// Lines for the AI's `planner_today` fleet block: the rollup lines, with
/// each session's active time rounded down to [`CTX_MINUTES_STEP`] minutes
/// ("<15m" below one step). `planner_today` reaches the model through the
/// turn context ledger, which records it again whenever its rendering
/// changes; exact minutes would change it on every request while any coding
/// session is active.
pub fn fleet_context_lines(
    rows: &[FleetSession],
    date: NaiveDate,
    now: DateTime<Utc>,
    cap: usize,
) -> Vec<String> {
    session_lines(rows, date, now, cap, CTX_MINUTES_STEP)
}

/// Shared body of [`rollup_lines`] / [`fleet_context_lines`]: minutes are
/// rounded down to `step` before sorting and formatting.
fn session_lines(
    rows: &[FleetSession],
    date: NaiveDate,
    now: DateTime<Utc>,
    cap: usize,
    step: u32,
) -> Vec<String> {
    let step = step.max(1);
    let mut with_minutes: Vec<(&FleetSession, u32)> = rows
        .iter()
        .map(|s| (s, s.minutes_on(date, now) / step * step))
        .collect();
    with_minutes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
    with_minutes
        .into_iter()
        .take(cap)
        .map(|(s, m)| {
            let time = if m == 0 && step > 1 {
                format!("<{step}m")
            } else {
                format_minutes(m)
            };
            let mut line = format!("{} — {}", truncate_summary(s.display_name(), 60), time);
            if let Some(b) = &s.brief {
                line.push_str(&format!(" — {}", truncate_summary(&b.headline, 140)));
                if let Some(blocked) = s.blocked_on() {
                    line.push_str(&format!("; blocked: {}", truncate_summary(blocked, 80)));
                }
            }
            line
        })
        .collect()
}

/// Sessions included in a `HOBBES_FLEET_STATUS` report.
pub const STATUS_MAX_SESSIONS: usize = 20;

/// The `HOBBES_FLEET_STATUS` payload: live + ended-today sessions (deduped,
/// live wins), optionally filtered by a case-insensitive name/cwd substring.
/// Pure — callers hand in the snapshot and the day rows.
pub fn status_report(
    live: &FleetState,
    ended_today: &[FleetSession],
    filter: Option<&str>,
    links: &[(String, String)],
    now: DateTime<Utc>,
    today: NaiveDate,
) -> serde_json::Value {
    let needle = filter.map(str::to_lowercase).filter(|f| !f.is_empty());
    let matches = |s: &FleetSession| {
        needle.as_ref().is_none_or(|n| {
            s.name.to_lowercase().contains(n) || s.cwd.to_lowercase().contains(n)
        })
    };
    let mut rows: Vec<&FleetSession> = ended_today
        .iter()
        .filter(|r| !live.sessions.contains_key(&r.id))
        .chain(live.sessions.values())
        .filter(|s| matches(s))
        .collect();
    // Attention first, then live-before-ended, then most recent.
    rows.sort_by(|a, b| {
        b.asks_person()
            .cmp(&a.asks_person())
            .then(a.ended_at.is_some().cmp(&b.ended_at.is_some()))
            .then(b.last_event_at.cmp(&a.last_event_at))
    });
    let total = rows.len();
    let sessions: Vec<serde_json::Value> = rows
        .into_iter()
        .take(STATUS_MAX_SESSIONS)
        .map(|s| {
            let (status, attention): (&str, Option<String>) = match &s.status {
                super::FleetStatus::Working => ("working", None),
                super::FleetStatus::WorkingBackground => ("working_background", None),
                super::FleetStatus::Waiting => ("waiting_on_timer", None),
                super::FleetStatus::Idle if s.ended_at.is_some() => ("ended", None),
                super::FleetStatus::Idle => ("idle", None),
                super::FleetStatus::NeedsAttention(kind) => (
                    "needs_attention",
                    Some(match kind {
                        super::AttentionKind::Gate => "waiting on a permission approval".into(),
                        super::AttentionKind::Notification { message, .. } => {
                            truncate_summary(message, 160)
                        }
                    }),
                ),
            };
            let mut v = serde_json::json!({
                "id": s.id,
                "name": s.display_name(),
                "origin": match s.origin {
                    super::FleetOrigin::External => "claude_code",
                    super::FleetOrigin::Hobbes => "hobbes_tab",
                },
                "status": status,
                "minutes_today": s.minutes_on(today, now),
                "last_event_minutes_ago": (now - s.last_event_at).num_minutes().max(0),
            });
            if let Some((_, title)) = links.iter().find(|(sid, _)| sid == &s.id) {
                v["linked_todo"] = serde_json::json!(title);
            }
            if s.origin == super::FleetOrigin::External && !s.cwd.is_empty() {
                v["cwd"] = serde_json::json!(s.cwd);
            }
            if let Some(a) = attention {
                v["attention"] = serde_json::json!(a);
            }
            if let Some(b) = &s.brief {
                let mut brief = serde_json::json!({
                    "headline": truncate_summary(&b.headline, HEADLINE_MAX_CHARS),
                });
                if !b.bullets.is_empty() {
                    brief["details"] = serde_json::json!(b.bullets);
                }
                if let Some(blocked) = s.blocked_on() {
                    brief["blocked_on"] = serde_json::json!(truncate_summary(blocked, 160));
                }
                v["brief"] = brief;
            }
            if let Some(g) = &s.pending_gate {
                v["pending_approval"] = serde_json::json!({
                    "tool": g.tool_name,
                    "input": truncate_summary(&g.input_summary, 160),
                });
            }
            v
        })
        .collect();

    let mut report = serde_json::json!({
        "date": today.format("%Y-%m-%d").to_string(),
        "session_count": total,
        "agent_minutes_today": super::live_minutes_on(live, today, now)
            .saturating_add(ended_today.iter().map(|s| s.minutes_on(today, now)).sum()),
        "sessions": sessions,
    });
    if total > STATUS_MAX_SESSIONS {
        report["note"] =
            serde_json::json!(format!("showing {STATUS_MAX_SESSIONS} of {total} sessions"));
    }
    report
}

/// The one-shot framing for `LlmConnector::generate_fleet_rollup`.
pub fn rollup_framing(date: NaiveDate, lines: &[String], total_minutes: u32) -> String {
    format!(
        "Write a brief end-of-day review for {} of the user's coding-agent \
         sessions. In the 'summary' field write 2-4 sentences: what got done, \
         what is blocked, and what to pick up tomorrow. Do not invent work not \
         listed. Sessions:\n{}\nTotal agent time: {}.",
        date.format("%A, %-d %B %Y"),
        lines.join("\n"),
        format_minutes(total_minutes)
    )
}

/// Read a cached day rollup out of the `meta` table. The single reader for
/// every surface that shows one (the Fleet review band, the planner's End the
/// day card) — the cache key lives next to its writer.
pub fn load_rollup(date: NaiveDate) -> Option<DayRollup> {
    crate::session_store::meta_get(&rollup_meta_key(date))
        .and_then(|s| serde_json::from_str::<DayRollup>(&s).ok())
        .map(|mut rollup| {
            rollup.narrative = unwrap_narrative(rollup.narrative);
            rollup
        })
}

/// A narrative cached before the summary parser tolerated wrapped JSON holds
/// the model's whole reply (`{{"summary": …}`); show its `summary` instead.
fn unwrap_narrative(narrative: String) -> String {
    if !narrative.trim_start().starts_with('{') {
        return narrative;
    }
    crate::llm::extract_json_object(&narrative)
        .and_then(|v| v.get("summary").and_then(|s| s.as_str()).map(str::to_string))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(narrative)
}

/// Generate (and cache) the day rollup for `date` with the same connector
/// call the Fleet review band makes.
///
/// Extracted here so the planner's End-the-day card generates *the same*
/// narrative, cached under the same key, instead of a second parallel
/// notion of "the day in review". Callers own the in-flight guard and the
/// error surface; this function is one shot.
pub async fn generate_day_rollup(
    settings: &crate::settings::Settings,
    date: NaiveDate,
) -> Result<DayRollup, String> {
    let instance = settings
        .active_connector()
        .filter(|i| settings.is_connector_configured(i))
        .or_else(|| {
            settings
                .chat_connectors()
                .find(|i| settings.is_connector_configured(i))
        })
        .cloned()
        .ok_or_else(|| "No configured LLM connector.".to_string())?;
    let now = Utc::now();
    let state = super::shared().snapshot();
    // Ended rows first, then the live map — the store query and the live map
    // overlap while a session is running, so the live copy wins.
    let mut rows: Vec<FleetSession> = super::store::sessions_active_on(date)
        .into_iter()
        .filter(|r| !state.sessions.contains_key(&r.id))
        .collect();
    rows.extend(state.sessions.values().cloned());
    if rows.is_empty() {
        return Err("No sessions to roll up.".to_string());
    }
    let total = super::fleet_agent_minutes_on(&state, date, now);
    let lines = rollup_lines(&rows, date, now, 12);
    let framed = rollup_framing(date, &lines, total);
    let connector = crate::llm::build_connector_for_instance(&instance, None);
    let narrative = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        connector.generate_fleet_rollup(framed),
    )
    .await
    .map_err(|_| "Rollup timed out.".to_string())?
    .map_err(|e| e.to_string())?;
    let rollup = DayRollup {
        date,
        narrative,
        generated_at: now,
        session_count: rows.len(),
        total_minutes: total,
    };
    if let Ok(json) = serde_json::to_string(&rollup) {
        let _ = crate::session_store::meta_set(&rollup_meta_key(date), &json);
    }
    Ok(rollup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_wrapped_narrative_is_unwrapped() {
        let wrapped = r#"{{"summary":"Shipped briefs.","sentiment":"good"}"#.to_string();
        assert_eq!(unwrap_narrative(wrapped), "Shipped briefs.");
        let plain = "Today's sessions shipped briefs.".to_string();
        assert_eq!(unwrap_narrative(plain.clone()), plain);
        // Unparseable braces stay as they were rather than vanishing.
        let odd = "{not json".to_string();
        assert_eq!(unwrap_narrative(odd.clone()), odd);
    }
    use crate::fleet::transcript::TranscriptDigest;
    use chrono::TimeZone;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn session(id: &str) -> FleetSession {
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 10, 0, 0).unwrap();
        let mut state = FleetState::default();
        super::super::reduce(
            &mut state,
            &super::super::events::FleetEvent::Stop {
                session_id: id.into(),
                cwd: format!("/dev/{id}"),
                background_tasks: 0,
            },
            now,
        );
        state.sessions[id].clone()
    }

    fn brief_at(t: &str) -> SessionBrief {
        SessionBrief {
            headline: "did things".into(),
            bullets: vec![],
            blocked_on: None,
            generated_at: utc(t),
            final_brief: false,
        }
    }

    #[test]
    fn brief_due_matrix() {
        let t0 = utc("2026-09-01T10:00:00Z");
        let mut s = session("s1");
        // Not dirty → not due.
        assert!(!brief_due(&s, t0, 45));
        s.brief_dirty_at = Some(t0);
        // Dirty but no transcript path → not due.
        assert!(!brief_due(&s, t0 + chrono::Duration::seconds(60), 45));
        s.transcript_path = Some("/tmp/t.jsonl".into());
        // Dirty, path, quiet not elapsed → not due.
        assert!(!brief_due(&s, t0 + chrono::Duration::seconds(30), 45));
        // Quiet elapsed → due.
        assert!(brief_due(&s, t0 + chrono::Duration::seconds(46), 45));
        // Ended overrides the quiet period.
        s.ended_at = Some(t0);
        assert!(brief_due(&s, t0, 45));
        s.ended_at = None;
        // Brief generated after the dirty mark → clean.
        s.brief = Some(brief_at("2026-09-01T10:00:01Z"));
        assert!(!brief_due(&s, t0 + chrono::Duration::seconds(60), 45));
        // Brief older than the dirty mark → due again.
        s.brief = Some(brief_at("2026-09-01T09:00:00Z"));
        assert!(brief_due(&s, t0 + chrono::Duration::seconds(60), 45));
    }

    #[test]
    fn finished_dispatched_run_skips_most_of_the_quiet_period() {
        let t0 = utc("2026-09-01T10:00:00Z");
        let mut s = session("run");
        s.transcript_path = Some("/tmp/t.jsonl".into());
        s.brief_dirty_at = Some(t0);
        s.dispatched_from = Some("parent".into());
        let settle = t0 + chrono::Duration::seconds(DISPATCH_BRIEF_QUIET_SECS);
        assert!(!brief_due(&s, settle - chrono::Duration::seconds(1), BRIEF_QUIET_SECS));
        assert!(brief_due(&s, settle, BRIEF_QUIET_SECS));
        // Still working → not finished, the normal debounce applies.
        s.working_since = Some(t0);
        assert!(!brief_due(&s, settle, BRIEF_QUIET_SECS));
        s.working_since = None;
        // Hand-back already delivered → later turns debounce normally.
        s.continuation_sent = true;
        assert!(!brief_due(&s, settle, BRIEF_QUIET_SECS));
    }

    #[test]
    fn collect_due_dedupes_live_over_ended_and_finals_first() {
        let now = utc("2026-09-01T12:00:00Z");
        let dirty = now - chrono::Duration::seconds(120);

        let mut live = FleetState::default();
        let mut a = session("a");
        a.transcript_path = Some("/t/a.jsonl".into());
        a.brief_dirty_at = Some(dirty);
        live.sessions.insert("a".into(), a.clone());

        // Stale ended copy of "a" (should be shadowed by the live row) plus a
        // genuinely ended "b".
        let mut a_ended = a.clone();
        a_ended.ended_at = Some(dirty);
        let mut b = session("b");
        b.transcript_path = Some("/t/b.jsonl".into());
        b.brief_dirty_at = Some(dirty);
        b.ended_at = Some(now - chrono::Duration::seconds(10));

        let tasks = collect_due(&live, &[a_ended, b], &[], now);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].session_id, "b");
        assert!(tasks[0].is_final);
        assert_eq!(tasks[1].session_id, "a");
        assert!(!tasks[1].is_final);
    }

    #[test]
    fn brief_from_full_schema() {
        let v = serde_json::json!({
            "summary": "Refactored the fleet store and added brief plumbing.",
            "current_task": "briefs",
            "entities": {
                "key_decisions": ["no migration needed", "merge via seq guard"],
                "key_topics": ["fleet", "briefs", "store", "extra", "more", "overflow"],
                "blockers": ["waiting on user review"]
            }
        });
        let b = brief_from_summary_value(&v, utc("2026-09-01T10:00:00Z"), true).unwrap();
        assert_eq!(b.headline, "Refactored the fleet store and added brief plumbing.");
        assert_eq!(b.bullets.len(), MAX_BULLETS);
        assert_eq!(b.bullets[0], "no migration needed");
        assert_eq!(b.blocked_on.as_deref(), Some("waiting on user review"));
        assert!(b.final_brief);
    }

    #[test]
    fn brief_falls_back_current_task_then_stringified() {
        let v = serde_json::json!({"summary": "", "current_task": "fixing tests"});
        let b = brief_from_summary_value(&v, utc("2026-09-01T10:00:00Z"), false).unwrap();
        assert_eq!(b.headline, "fixing tests");

        // No summary text at all: no brief, never the stringified JSON.
        let odd = serde_json::json!({"unexpected": "shape"});
        assert!(brief_from_summary_value(&odd, utc("2026-09-01T10:00:00Z"), false).is_none());
        assert!(brief_from_summary_value(&serde_json::Value::Null, utc("2026-09-01T10:00:00Z"), false).is_none());
    }

    #[test]
    fn json_in_a_summary_is_unwrapped_or_dropped() {
        let now = utc("2026-09-01T10:00:00Z");
        // The reported bug: a truncated JSON object stored as the summary.
        let cut = r#"{"summary":"Active Composio profile is 'Clearmirror.ai'. Hobbes was asked to verify the cal"#;
        let v = serde_json::json!({ "summary": cut });
        let b = brief_from_summary_value(&v, now, false).unwrap();
        assert_eq!(b.headline, "Active Composio profile is 'Clearmirror.ai'. Hobbes was asked to verify the cal");
        assert!(!progress_line(&b, None).contains('{'));

        // A complete object (prose-wrapped, fenced) as the summary string.
        let fenced = "Sure!\n```json\n{\"summary\":\"Shipped the digest\",\"current_task\":\"qa\"}\n```";
        let b = brief_from_summary_value(&serde_json::json!({ "summary": fenced }), now, false).unwrap();
        assert_eq!(b.headline, "Shipped the digest");

        // current_task only.
        let only_task = r#"{"summary":"","current_task":"fixing tests"}"#;
        assert_eq!(clean_headline(only_task).as_deref(), Some("fixing tests"));

        // Nothing recoverable: dropped.
        assert_eq!(clean_headline(r#"{"entities":{"key_topics":["a"]}}"#), None);
        assert!(brief_from_summary_value(&serde_json::json!({"summary": r#"{"entities":{}}"#}), now, false).is_none());

        // A stored legacy brief headline is cleaned at display time.
        let legacy = SessionBrief {
            headline: cut.to_string(),
            bullets: vec![],
            blocked_on: None,
            generated_at: now,
            final_brief: false,
        };
        assert!(progress_line(&legacy, None).starts_with("Active Composio"));
    }

    #[test]
    fn progress_and_context_lines_drop_an_overtaken_blocker() {
        let b = SessionBrief {
            headline: "Built the exporter".into(),
            bullets: vec![],
            blocked_on: Some("needs the API key".into()),
            generated_at: utc("2026-09-01T10:00:00Z"),
            final_brief: false,
        };
        assert_eq!(progress_line(&b, Some("needs the API key")), "Built the exporter; blocked: needs the API key");
        assert_eq!(progress_line(&b, None), "Built the exporter");
        assert_eq!(progress_line(&b, Some("  ")), "Built the exporter");

        let mut s = session("s1");
        s.brief = Some(b);
        let date = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let now = utc("2026-09-01T12:00:00Z");
        assert!(fleet_context_lines(std::slice::from_ref(&s), date, now, 5)[0].contains("blocked:"));
        // A turn stopped after the brief: its question was overtaken.
        s.brief_dirty_at = Some(utc("2026-09-01T11:00:00Z"));
        assert!(!fleet_context_lines(std::slice::from_ref(&s), date, now, 5)[0].contains("blocked:"));
        let live = FleetState { sessions: [(s.id.clone(), s.clone())].into(), ..Default::default() };
        let report = status_report(&live, &[], None, &[], now, date);
        assert!(report["sessions"][0]["brief"].get("blocked_on").is_none());
    }

    #[test]
    fn question_in_reply_reads_the_closing_question() {
        assert_eq!(question_in_reply("All set. Want me to ship it?").as_deref(), Some("Want me to ship it?"));
        assert_eq!(
            question_in_reply("Two options:\n- A\n- B\n\n**Which one do you prefer?**").as_deref(),
            Some("Which one do you prefer?")
        );
        assert_eq!(question_in_reply("Should I merge it? Let me know.").as_deref(), Some("Should I merge it?"));
        assert_eq!(question_in_reply("> Deploy now?\n").as_deref(), Some("Deploy now?"));
        assert_eq!(question_in_reply("1. Do you want the CSV too?").as_deref(), Some("Do you want the CSV too?"));
        // Not a closing question.
        assert_eq!(question_in_reply("Why did it fail? The cache was cold, so the first request timed out."), None);
        assert_eq!(question_in_reply("Why did it fail?\n\nThe cache was cold."), None);
        assert_eq!(question_in_reply("Done. The schedule is clear."), None);
        assert_eq!(question_in_reply("```rust\nfn f() -> Option<u8>?\n```"), None);
        assert_eq!(question_in_reply("???"), None);
        assert_eq!(question_in_reply(""), None);
        // Long questions are clipped.
        let long = format!("{}?", "word ".repeat(80));
        assert!(question_in_reply(&long).unwrap().chars().count() <= ASK_MAX_CHARS + 1);
    }

    #[test]
    fn hobbes_turn_brief_without_a_summary_still_carries_the_ask() {
        let now = utc("2026-09-01T10:00:00Z");
        let b = hobbes_turn_brief(None, Some("Ship it?".into()), now, false).unwrap();
        assert_eq!(b.blocked_on.as_deref(), Some("Ship it?"));
        assert_eq!(b.generated_at, now);
        assert!(hobbes_turn_brief(None, None, now, false).is_none());
        assert!(hobbes_turn_brief(None, Some("  ".into()), now, false).is_none());
    }

    fn task(id: &str, dirty: &str) -> BriefTask {
        BriefTask {
            session_id: id.into(),
            name: id.into(),
            cwd: String::new(),
            transcript_path: String::new(),
            previous_brief: None,
            dirty_at: utc(dirty),
            is_final: false,
            linked_todo: None,
            background_active: false,
        }
    }

    #[test]
    fn a_failing_brief_backs_off_and_lets_the_others_through() {
        let mut b = BriefBackoff::default();
        let t0 = utc("2026-09-01T10:00:00Z");
        let due = || vec![task("old", "2026-09-01T09:00:00Z"), task("new", "2026-09-01T09:30:00Z")];
        assert_eq!(b.pick(due(), t0).unwrap().session_id, "old");
        b.failed("old", t0);
        // Next tick: the failing head is skipped, the next session runs.
        let t1 = t0 + chrono::Duration::seconds(15);
        assert_eq!(b.pick(due(), t1).unwrap().session_id, "new");
        // Once its backoff runs out it gets another try.
        let t2 = t0 + chrono::Duration::seconds(BRIEF_RETRY_BASE_SECS);
        assert_eq!(b.pick(due(), t2).unwrap().session_id, "old");
        // Delays double, capped.
        b.failed("old", t2);
        assert_eq!(b.pick(vec![task("old", "2026-09-01T09:00:00Z")], t2 + chrono::Duration::seconds(59)), None);
        assert!(b.pick(vec![task("old", "2026-09-01T09:00:00Z")], t2 + chrono::Duration::seconds(60)).is_some());
        for _ in 0..20 {
            b.failed("old", t2);
        }
        let cap = t2 + chrono::Duration::seconds(BRIEF_RETRY_MAX_SECS);
        assert!(b.pick(vec![task("old", "2026-09-01T09:00:00Z")], cap - chrono::Duration::seconds(1)).is_none());
        assert!(b.pick(vec![task("old", "2026-09-01T09:00:00Z")], cap).is_some());
        // Success resets; a session that's no longer due is forgotten.
        b.succeeded("old");
        assert_eq!(b.pick(due(), t2).unwrap().session_id, "old");
        b.failed("new", t2);
        b.pick(vec![task("old", "2026-09-01T09:00:00Z")], t2);
        assert_eq!(b.pick(due(), t2).unwrap().session_id, "old");
        assert_eq!(b.pick(vec![task("new", "2026-09-01T09:30:00Z")], t2).unwrap().session_id, "new", "forgotten once it left the due list");
    }

    #[test]
    fn rollup_lines_sort_clip_and_cap() {
        let day: NaiveDate = "2026-09-01".parse().unwrap();
        let now = utc("2026-09-01T20:00:00Z");
        let mut small = session("small");
        small.day_minutes.insert(day, 5);
        let mut big = session("big");
        big.day_minutes.insert(day, 90);
        big.brief = Some(SessionBrief {
            headline: "Shipped the thing".into(),
            bullets: vec![],
            blocked_on: Some("code review".into()),
            generated_at: now,
            final_brief: false,
        });
        let mut extra = session("extra");
        extra.day_minutes.insert(day, 1);

        let lines = rollup_lines(&[small.clone(), big.clone(), extra.clone()], day, now, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("big — 1h 30m — Shipped the thing; blocked: code review"));
        assert!(lines[1].starts_with("small — 5m"));
    }

    #[test]
    fn fleet_context_minutes_round_down_to_fifteen() {
        let day: NaiveDate = "2026-09-01".parse().unwrap();
        let now = utc("2026-09-01T20:00:00Z");
        let line = |minutes: u32| {
            let mut s = session("s");
            s.day_minutes.insert(day, minutes);
            fleet_context_lines(&[s], day, now, 5).remove(0)
        };
        assert_eq!(line(5), "s — <15m");
        assert_eq!(line(15), "s — 15m");
        assert_eq!(line(29), "s — 15m");
        assert_eq!(line(97), "s — 1h 30m");
        // Ticking within one step leaves the rendering unchanged.
        assert_eq!(line(91), line(104));
        // The rollup keeps exact minutes.
        let mut s = session("s");
        s.day_minutes.insert(day, 97);
        assert_eq!(rollup_lines(&[s], day, now, 5)[0], "s — 1h 37m");
    }

    #[test]
    fn brief_framing_carries_project_digest_and_previous() {
        let mut s = session("s1");
        s.transcript_path = Some("/t/s1.jsonl".into());
        s.brief_dirty_at = Some(utc("2026-09-01T10:00:00Z"));
        s.brief = Some(brief_at("2026-09-01T09:00:00Z"));
        s.status = FleetStatus::WorkingBackground;
        let task = collect_due(
            &FleetState {
                sessions: [("s1".to_string(), s)].into_iter().collect(),
                ..Default::default()
            },
            &[],
            &[("s1".to_string(), "Ship the briefs".to_string())],
            utc("2026-09-01T10:01:00Z"),
        )
        .remove(0);
        let digest = TranscriptDigest {
            text: "USER: hi\nASSISTANT: done".into(),
            turn_count: 1,
            title: None,
        };
        let (prev, framed) = brief_framing(&task, &digest);
        assert!(prev.contains("did things"), "previous brief rides the summary slot");
        assert!(framed.contains("'s1'"));
        assert!(framed.contains("/dev/s1"));
        assert!(framed.contains("USER: hi"));
        assert!(framed.contains("1 recent"));
        assert!(
            framed.contains("tracked task") && framed.contains("Ship the briefs"),
            "linked todo rides the framing"
        );
        assert!(
            task.background_active && framed.contains("background work RUNNING"),
            "a live background phase is declared to the model"
        );
    }

    #[test]
    fn rollup_meta_key_and_framing() {
        let day: NaiveDate = "2026-09-01".parse().unwrap();
        assert_eq!(rollup_meta_key(day), "fleet_rollup_2026-09-01");
        let f = rollup_framing(day, &["a — 5m".into()], 65);
        assert!(f.contains("a — 5m"));
        assert!(f.contains("1h 5m"));
        assert!(f.contains("September 2026"));
    }

}
