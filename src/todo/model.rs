//! The planner domain model.
//!
//! Pure data plus pure logic — no persistence, no Dioxus. Everything here is
//! unit-testable without a database or a UI.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};

// ── Enums ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Open,
    /// Being actively worked right now — focus mode. At most one todo holds
    /// this at a time; `PlannerState::start_focus` enforces it.
    InProgress,
    Completed,
    /// Explicitly abandoned. Distinct from completed so the logbook can tell
    /// "I did this" from "I decided not to".
    Cancelled,
}

impl TodoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TodoStatus::Open => "open",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
            TodoStatus::Cancelled => "cancelled",
        }
    }

    /// Whether the todo is finished, either way. Both leave the active lists.
    pub fn is_closed(self) -> bool {
        matches!(self, TodoStatus::Completed | TodoStatus::Cancelled)
    }
}

/// Which Things-style list an *unscheduled* todo belongs to.
///
/// Orthogonal to `scheduled_for`: a todo with a date shows up in Today or
/// Upcoming regardless of bucket, and falls back to its bucket if the date is
/// cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoBucket {
    /// Captured but not yet triaged.
    #[default]
    Inbox,
    /// Triaged, do it whenever.
    Anytime,
    /// Deliberately deferred out of sight.
    Someday,
}

impl TodoBucket {
    pub fn as_str(self) -> &'static str {
        match self {
            TodoBucket::Inbox => "inbox",
            TodoBucket::Anytime => "anytime",
            TodoBucket::Someday => "someday",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeOfDay {
    Morning,
    Afternoon,
    /// Things' "This Evening" — a separate group at the bottom of Today.
    Evening,
}

/// An Eisenhower-matrix quadrant. Optional on every todo: `None` means "not
/// triaged for priority", which is the default for new and pre-existing
/// todos alike — `Neither` is a deliberate call ("not urgent, not
/// important"), not the absence of one.
///
/// Manual order stays the user's canonical "what first"; priority is a
/// label the planner can sort and band by, never a rewrite of that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Q1 — urgent and important: do it first.
    UrgentImportant,
    /// Q2 — important, not urgent: schedule it.
    Important,
    /// Q3 — urgent, not important: delegate it.
    Urgent,
    /// Q4 — neither: drop or defer it.
    Neither,
}

impl Priority {
    /// Quadrant order, Q1 first — also the ascending sort order.
    pub const ALL: [Priority; 4] = [
        Priority::UrgentImportant,
        Priority::Important,
        Priority::Urgent,
        Priority::Neither,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Priority::UrgentImportant => "urgent_important",
            Priority::Important => "important",
            Priority::Urgent => "urgent",
            Priority::Neither => "neither",
        }
    }

    /// 1–4, the conventional quadrant number.
    pub fn quadrant(self) -> u8 {
        match self {
            Priority::UrgentImportant => 1,
            Priority::Important => 2,
            Priority::Urgent => 3,
            Priority::Neither => 4,
        }
    }

    /// The compact badge text: `"Q1"`…`"Q4"`.
    pub fn short_label(self) -> String {
        format!("Q{}", self.quadrant())
    }

    /// Plain-language name, for menus, band headers and the AI's summary.
    pub fn label(self) -> &'static str {
        match self {
            Priority::UrgentImportant => "Urgent & important",
            Priority::Important => "Important, not urgent",
            Priority::Urgent => "Urgent, not important",
            Priority::Neither => "Neither",
        }
    }

    /// The matrix's verb for the quadrant.
    pub fn action(self) -> &'static str {
        match self {
            Priority::UrgentImportant => "Do first",
            Priority::Important => "Schedule",
            Priority::Urgent => "Delegate",
            Priority::Neither => "Drop",
        }
    }

    /// Accepts the stored names, `q1`–`q4` / `1`–`4`, and the matrix verbs
    /// (`do`, `schedule`, `delegate`, `drop`/`eliminate`). Shared by the
    /// quick-add `!!` token and the AI tools.
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw.trim().to_ascii_lowercase().as_str() {
            "urgent_important" | "urgent-important" | "q1" | "1" | "do" | "do_first" => {
                Priority::UrgentImportant
            }
            "important" | "q2" | "2" | "schedule" | "plan" => Priority::Important,
            "urgent" | "q3" | "3" | "delegate" => Priority::Urgent,
            "neither" | "q4" | "4" | "drop" | "eliminate" => Priority::Neither,
            // The linear scheme's names for the same four levels.
            "p1" => Priority::UrgentImportant,
            "p2" => Priority::Important,
            "p3" => Priority::Urgent,
            "p4" => Priority::Neither,
            _ => return None,
        })
    }

    /// One click / `p` press on a row: Q1 → Q2 → Q3 → Q4 → none → Q1.
    pub fn cycle(current: Option<Priority>) -> Option<Priority> {
        match current {
            None => Some(Priority::UrgentImportant),
            Some(Priority::UrgentImportant) => Some(Priority::Important),
            Some(Priority::Important) => Some(Priority::Urgent),
            Some(Priority::Urgent) => Some(Priority::Neither),
            Some(Priority::Neither) => None,
        }
    }
}

/// How the four priority levels read in the UI. Display only: storage and
/// the AI tools always speak quadrants, so switching scheme never rewrites a
/// todo — P1 *is* Q1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorityScheme {
    /// Q1–Q4, the Eisenhower matrix.
    #[default]
    Eisenhower,
    /// P1–P4, a plain highest-to-lowest scale.
    Linear,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PriorityDisplay {
    #[serde(default)]
    pub scheme: PriorityScheme,
    /// Custom names, level 1 first. Missing or blank = the scheme's default.
    #[serde(default)]
    pub names: Vec<String>,
}

impl PriorityDisplay {
    /// The compact badge: `"Q1"` or `"P1"`.
    pub fn badge(&self, p: Priority) -> String {
        match self.scheme {
            PriorityScheme::Eisenhower => p.short_label(),
            PriorityScheme::Linear => format!("P{}", p.quadrant()),
        }
    }

    pub fn default_name(scheme: PriorityScheme, p: Priority) -> &'static str {
        match scheme {
            PriorityScheme::Eisenhower => p.label(),
            PriorityScheme::Linear => match p {
                Priority::UrgentImportant => "Highest",
                Priority::Important => "High",
                Priority::Urgent => "Medium",
                Priority::Neither => "Low",
            },
        }
    }

    /// The level's name: the user's, else the scheme's.
    pub fn name(&self, p: Priority) -> String {
        self.names
            .get(p.quadrant() as usize - 1)
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
            .unwrap_or(Self::default_name(self.scheme, p))
            .to_string()
    }

    /// A compact caption for tight spots: the matrix verb for an unrenamed
    /// Eisenhower level, else the name.
    pub fn short_name(&self, p: Priority) -> String {
        let custom = self
            .names
            .get(p.quadrant() as usize - 1)
            .is_some_and(|n| !n.trim().is_empty());
        match self.scheme {
            PriorityScheme::Eisenhower if !custom => p.action().to_string(),
            _ => self.name(p),
        }
    }

    /// Tooltip line: the name, plus the matrix verb for Eisenhower.
    pub fn describe(&self, p: Priority) -> String {
        match self.scheme {
            PriorityScheme::Eisenhower => format!("{} — {}", self.name(p), p.action()),
            PriorityScheme::Linear => self.name(p),
        }
    }
}

/// Who created the todo. `Ai` records the originating session so provenance
/// survives even though the list itself is global.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TodoOrigin {
    #[default]
    User,
    Ai {
        session_id: String,
    },
}

/// Who is driving a focus session: the user at the keyboard, or the assistant
/// working on the user's behalf (`HOBBES_TODO_UPDATE` with `status:
/// in_progress`). Mirrors the [`TodoOrigin`] serde shape so old rows and new
/// rows share one idiom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FocusActor {
    #[default]
    Person,
    Agent {
        /// The chat session that drove it — same provenance source as
        /// `TodoOrigin::Ai`.
        #[serde(default)]
        session_id: Option<String>,
    },
}

impl FocusActor {
    pub fn as_str(&self) -> &'static str {
        match self {
            FocusActor::Person => "person",
            FocusActor::Agent { .. } => "agent",
        }
    }

    pub fn is_agent(&self) -> bool {
        matches!(self, FocusActor::Agent { .. })
    }

    pub fn agent_session_id(&self) -> Option<&str> {
        match self {
            FocusActor::Person => None,
            FocusActor::Agent { session_id } => session_id.as_deref(),
        }
    }
}

/// Why a focus session ended. Every banking path records one — the session
/// log is only trustworthy if no exit is anonymous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusEndReason {
    /// Reopened out of focus (banking an in-progress todo).
    Stopped,
    Completed,
    /// Explicit pause (FocusBar Stop, `status: open` from the AI, tray).
    Paused,
    /// Another todo took focus — single-focus preemption.
    Preempted,
    Cancelled,
    /// Closed by `sanitize_stale_focus` after surviving an app quit.
    Recovered,
}

impl FocusEndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FocusEndReason::Stopped => "stopped",
            FocusEndReason::Completed => "completed",
            FocusEndReason::Paused => "paused",
            FocusEndReason::Preempted => "preempted",
            FocusEndReason::Cancelled => "cancelled",
            FocusEndReason::Recovered => "recovered",
        }
    }
}

/// One focus sitting on one todo: who worked, when, for how long, and why it
/// ended. Non-destructive — `Todo.actual_minutes` stays the fast aggregate,
/// but sessions are the source of truth for person-vs-agent attribution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FocusSession {
    pub id: String,
    pub todo_id: String,
    #[serde(default)]
    pub actor: FocusActor,
    pub started_at: DateTime<Utc>,
    /// `None` while the session is live.
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    /// Minutes banked when the session closed. For `recovered` sessions this
    /// is the *clamped* figure (matching what entered `actual_minutes`); the
    /// real elapsed lives in [`Self::unclamped_minutes`].
    #[serde(default)]
    pub minutes: u32,
    #[serde(default)]
    pub end_reason: Option<FocusEndReason>,
    /// Recovery honesty: when `sanitize_stale_focus` clamped the banked
    /// minutes, the real wall-clock elapsed it was clamped down from. The
    /// row keeps its real `started_at`/`ended_at` bounds either way.
    #[serde(default)]
    pub unclamped_minutes: Option<u32>,
}

impl FocusSession {
    pub fn open(todo_id: impl Into<String>, now: DateTime<Utc>, actor: FocusActor) -> Self {
        Self {
            id: format!("fs_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
            todo_id: todo_id.into(),
            actor,
            started_at: now,
            ended_at: None,
            minutes: 0,
            end_reason: None,
            unclamped_minutes: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.ended_at.is_none()
    }

    /// Close the session at `now`, banking the wall-clock minutes.
    pub fn close(&mut self, now: DateTime<Utc>, reason: FocusEndReason) {
        self.ended_at = Some(now.max(self.started_at));
        self.minutes = (now - self.started_at).num_minutes().max(0) as u32;
        self.end_reason = Some(reason);
    }
}

/// A local calendar day's bounds as UTC instants (local midnight to local
/// midnight). DST-safe: a midnight that does not exist locally slides
/// forward to the first instant that does.
fn local_day_bounds_utc(date: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    use chrono::TimeZone;
    let midnight = |d: NaiveDate| {
        let naive = d.and_hms_opt(0, 0, 0).expect("midnight is valid");
        match chrono::Local.from_local_datetime(&naive) {
            chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
                dt.with_timezone(&Utc)
            }
            chrono::LocalResult::None => {
                // DST gap at midnight (rare, but real in some zones): take
                // the first hour boundary that exists.
                (1..=3)
                    .find_map(|h| {
                        chrono::Local
                            .from_local_datetime(&(naive + chrono::Duration::hours(h)))
                            .earliest()
                    })
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|| Utc.from_utc_datetime(&naive))
            }
        }
    };
    (midnight(date), midnight(date + chrono::Duration::days(1)))
}

/// The interval a closed session actually occupied, for day attribution.
/// The end is capped at `started_at + minutes` so a `recovered` session's
/// clamped bank never spreads its full (abandoned) wall-clock span across
/// the calendar. Live sessions attribute nothing until they close.
fn session_interval(s: &FocusSession) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let ended = s.ended_at?;
    let capped = s.started_at + chrono::Duration::minutes(s.minutes as i64);
    let end = ended.min(capped);
    (end > s.started_at).then_some((s.started_at, end))
}

/// Minutes of one closed session that fall on the **local** day `date`.
/// Cross-midnight sessions split at local midnight — each day gets its share.
pub fn session_minutes_on(s: &FocusSession, date: NaiveDate) -> u32 {
    let Some((start, end)) = session_interval(s) else {
        return 0;
    };
    let (day_start, day_end) = local_day_bounds_utc(date);
    let from = start.max(day_start);
    let to = end.min(day_end);
    if to > from {
        (to - from).num_minutes().max(0) as u32
    } else {
        0
    }
}

/// Person-actor focus minutes on a local day, across all todos.
pub fn person_minutes_on(sessions: &[FocusSession], date: NaiveDate) -> u32 {
    sessions
        .iter()
        .filter(|s| !s.actor.is_agent())
        .fold(0u32, |acc, s| acc.saturating_add(session_minutes_on(s, date)))
}

/// Agent-actor focus minutes on a local day, across all todos.
pub fn agent_minutes_on(sessions: &[FocusSession], date: NaiveDate) -> u32 {
    sessions
        .iter()
        .filter(|s| s.actor.is_agent())
        .fold(0u32, |acc, s| acc.saturating_add(session_minutes_on(s, date)))
}

/// Gaps count as "strategic" from this size up — anything shorter is
/// task-switching residue, not usable thinking time.
pub const STRATEGIC_GAP_MIN_MINUTES: u32 = 5;

/// Strategic time v1: `(count, total_minutes)` of gaps of at least
/// [`STRATEGIC_GAP_MIN_MINUTES`] **between person focus sessions** inside the
/// workday window on a local day. `workday` is `(start, end)` in minutes
/// since local midnight (the timeline's `workday_bounds` shape). Sessions are
/// clipped to the window first, so a session running past the workday edge
/// never manufactures a gap outside it. Days with fewer than two distinct
/// person spans have no between-session gaps.
pub fn strategic_gaps(
    sessions: &[FocusSession],
    workday: (u32, u32),
    date: NaiveDate,
) -> (usize, u32) {
    use chrono::Timelike;
    let (day_start, day_end) = workday;
    if day_end <= day_start {
        return (0, 0);
    }
    // Person-session spans on the day, in local minutes-since-midnight,
    // clipped to the workday window.
    let mut spans: Vec<(i64, i64)> = sessions
        .iter()
        .filter(|s| !s.actor.is_agent())
        .filter_map(session_interval)
        .filter_map(|(start, end)| {
            let local_min = |t: DateTime<Utc>| {
                let local = t.with_timezone(&chrono::Local);
                let days = (local.date_naive() - date).num_days();
                days * 24 * 60
                    + local.time().hour() as i64 * 60
                    + local.time().minute() as i64
            };
            let s = local_min(start).max(day_start as i64);
            let e = local_min(end).min(day_end as i64);
            (e > s).then_some((s, e))
        })
        .collect();
    spans.sort_by_key(|(s, _)| *s);
    // Merge overlaps so two interleaved sittings can't fake a gap.
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (s, e) in spans {
        match merged.last_mut() {
            Some((_, ce)) if s <= *ce => *ce = (*ce).max(e),
            _ => merged.push((s, e)),
        }
    }

    let mut count = 0usize;
    let mut total = 0u32;
    for pair in merged.windows(2) {
        let gap = (pair[1].0 - pair[0].1).max(0) as u32;
        if gap >= STRATEGIC_GAP_MIN_MINUTES {
            count += 1;
            total = total.saturating_add(gap);
        }
    }
    (count, total)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChecklistItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub done: bool,
}

// ── Symbiotic planner: lanes, kinds, agent state, decisions ────────────────
//
// SYMBIOTIC_PLANNER.md §2–4. A to-do is either the person's (time-bound,
// on the timeline) or an agent's (an outcome, worked asynchronously). All of
// these default so pre-lanes rows load as untriaged human to-dos.

/// Who spends time on a to-do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// The person's own time — on the timeline, against capacity.
    #[default]
    Human,
    /// An outcome handed to an agent; costs the person judgment minutes only.
    Agent,
}

#[allow(dead_code)] // wave 2: Planner UI / weekly review
impl Lane {
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::Human => "human",
            Lane::Agent => "agent",
        }
    }

    /// Accepts `human`/`me`/`mine`/`yours` and `agent`/`hobbes`/`delegate`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "human" | "me" | "mine" | "yours" | "person" => Some(Lane::Human),
            "agent" | "hobbes" | "delegate" | "delegated" => Some(Lane::Agent),
            _ => None,
        }
    }
}

/// The three kinds of human time (SYMBIOTIC_PLANNER.md §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanKind {
    /// Meetings, calls, work that needs you personally.
    Commitment,
    /// Quick tasks to make smaller and fewer, and batch.
    Chore,
    /// What the rest is *for* — protected, non-commerce time.
    YourTime,
}

#[allow(dead_code)] // wave 2: Planner UI / weekly review
impl HumanKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HumanKind::Commitment => "commitment",
            HumanKind::Chore => "chore",
            HumanKind::YourTime => "your_time",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HumanKind::Commitment => "commitment",
            HumanKind::Chore => "chore",
            HumanKind::YourTime => "your time",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "commitment" | "commit" => Some(HumanKind::Commitment),
            "chore" | "chores" => Some(HumanKind::Chore),
            "your_time" | "yourtime" | "my_time" | "mytime" => Some(HumanKind::YourTime),
            _ => None,
        }
    }
}

/// Where an agent to-do stands. Moves queued → working → needs you → done
/// (or stuck). Derived from the linked worker wherever possible — see
/// `lanes::derive_agent_state`. Reported, never judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Handed over (or about to be) but no worker is running it yet.
    Queued,
    Working,
    /// Waiting on the person: a decision, a permission gate, or input.
    NeedsYou,
    /// The latest brief says blocked and nothing is pending on the person.
    Stuck,
    Done,
}

#[allow(dead_code)] // wave 2: Planner UI / weekly review
impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Queued => "queued",
            AgentState::Working => "working",
            AgentState::NeedsYou => "needs_you",
            AgentState::Stuck => "stuck",
            AgentState::Done => "done",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentState::Queued => "queued",
            AgentState::Working => "working",
            AgentState::NeedsYou => "needs you",
            AgentState::Stuck => "stuck",
            AgentState::Done => "done",
        }
    }
}

/// The re-entry brief attached to a decision: what the worker did, what it
/// is blocked on, and what it will do next. Mirrors the fleet's
/// `SessionBrief` (headline → `did`, `blocked_on` → `blocked`); `next` only
/// exists when a worker states it (`HOBBES_DECISIONS ask`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Brief {
    #[serde(default)]
    pub did: String,
    #[serde(default)]
    pub blocked: Option<String>,
    #[serde(default)]
    pub next: Option<String>,
}

/// What raised a decision. Synthesized decisions (gate / attention) clear
/// themselves when the worker stops waiting; worker-asked ones stay until
/// the person answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DecisionSource {
    /// The worker asked (`HOBBES_DECISIONS ask`).
    #[default]
    Worker,
    /// A held fleet permission gate. Answered by the person in the Claude
    /// Code terminal — never by the model.
    Gate { request_id: String },
    /// A fleet `NeedsAttention` notification (idle prompt, question…).
    Attention,
}

/// How a resolved answer reached (or failed to reach) the worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DecisionDelivery {
    /// Queued as a user message to the worker's Hobbes chat session.
    ChatQueued { session_id: String },
    /// No channel to the worker exists (e.g. a headless Claude Code run):
    /// recorded on the to-do and surfaced in its progress line only.
    NotDelivered { reason: String },
}

/// The person's answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionAnswer {
    pub choice: String,
    #[serde(default)]
    pub note: Option<String>,
    pub at: DateTime<Utc>,
    /// `None` until the dispatch edge has tried to deliver it.
    #[serde(default)]
    pub delivery: Option<DecisionDelivery>,
}

/// A decision an agent needs from the person: a one-line question, the
/// options, how long answering should take, and the brief behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    /// Estimated minutes to decide ("about 2 min").
    #[serde(default)]
    pub est_minutes: Option<u32>,
    #[serde(default)]
    pub brief: Option<Brief>,
    pub asked_at: DateTime<Utc>,
    /// Display name of the worker asking (session title / tab name).
    #[serde(default)]
    pub worker: Option<String>,
    #[serde(default)]
    pub source: DecisionSource,
    #[serde(default)]
    pub resolved: Option<DecisionAnswer>,
}

impl Decision {
    pub fn is_open(&self) -> bool {
        self.resolved.is_none()
    }

    /// Resolve `choice` against the options: a 1-based index ("2"), an exact
    /// (case-insensitive) option, or — when the decision offers no options,
    /// or the person answered in their own words — the text itself.
    pub fn match_choice(&self, choice: &str) -> String {
        let c = choice.trim();
        if let Ok(n) = c.parse::<usize>() {
            if n >= 1 && n <= self.options.len() {
                return self.options[n - 1].clone();
            }
        }
        self.options
            .iter()
            .find(|o| o.trim().eq_ignore_ascii_case(c))
            .cloned()
            .unwrap_or_else(|| c.to_string())
    }
}

/// One answered decision, kept for the weekly review's judgment count and
/// "decisions cleared" (ground covered). Persisted in `todo_decisions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedDecision {
    pub id: String,
    pub todo_id: String,
    pub question: String,
    pub choice: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub worker: Option<String>,
    pub asked_at: DateTime<Utc>,
    pub resolved_at: DateTime<Utc>,
    #[serde(default)]
    pub source: DecisionSource,
    #[serde(default)]
    pub delivery: Option<DecisionDelivery>,
}

// ── Your-time goals ─────────────────────────────────────────────────────────

/// A weekly your-time goal for one user-defined category (Reading, Gaming,
/// Outside…). Personal planner: one set per user (SYMBIOTIC_PLANNER.md §7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeGoal {
    pub id: String,
    pub category: String,
    #[serde(default)]
    pub weekly_target_minutes: u32,
    /// `#RRGGBB`; `None` lets the UI pick from its palette.
    #[serde(default)]
    pub colour: Option<String>,
    #[serde(default)]
    pub sort_order: f64,
}

impl TimeGoal {
    pub fn new(category: impl Into<String>, weekly_target_minutes: u32) -> Self {
        Self {
            id: format!("tg_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
            category: category.into(),
            weekly_target_minutes,
            colour: None,
            sort_order: 0.0,
        }
    }

    pub fn matches(&self, category: &str) -> bool {
        self.category.trim().eq_ignore_ascii_case(category.trim())
    }
}

// ── Review feedback style ───────────────────────────────────────────────────

/// How the weekly review speaks (SYMBIOTIC_PLANNER.md §5). The numbers are
/// the same in every style; only order, wording and the nudge change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStyle {
    /// Leads with the gap and names the drain, with a firm suggestion.
    Direct,
    /// One win, one gap, one next step.
    #[default]
    Balanced,
    /// Leads with what you made room for; the nudge is an invitation.
    Encouraging,
}

#[allow(dead_code)] // wave 2: Planner UI / weekly review
impl ReviewStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewStyle::Direct => "direct",
            ReviewStyle::Balanced => "balanced",
            ReviewStyle::Encouraging => "encouraging",
        }
    }

    /// One step gentler, or `None` at the gentlest.
    pub fn softer(self) -> Option<Self> {
        match self {
            ReviewStyle::Direct => Some(ReviewStyle::Balanced),
            ReviewStyle::Balanced => Some(ReviewStyle::Encouraging),
            ReviewStyle::Encouraging => None,
        }
    }
}

/// Acted-on vs dismissed counts for one style's nudges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NudgeCounts {
    #[serde(default)]
    pub acted: u32,
    #[serde(default)]
    pub dismissed: u32,
}

/// The adaptation signal: counts only, per style. Never switches the style
/// itself — `measures::suggest_review_style` only *suggests*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewSignals {
    #[serde(default)]
    pub direct: NudgeCounts,
    #[serde(default)]
    pub balanced: NudgeCounts,
    #[serde(default)]
    pub encouraging: NudgeCounts,
    /// Weekly reviews left unopened (the "skipping reviews after hard weeks"
    /// signal), per style in force at the time.
    #[serde(default)]
    pub skipped_direct: u32,
    #[serde(default)]
    pub skipped_balanced: u32,
    #[serde(default)]
    pub skipped_encouraging: u32,
}

/// What the person did with one review nudge.
#[allow(dead_code)] // wave 2: weekly review records it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeOutcome {
    Acted,
    Dismissed,
    /// The review itself went unopened that week.
    Skipped,
}

#[allow(dead_code)] // wave 2: Planner UI / weekly review
impl ReviewSignals {
    pub fn counts(&self, style: ReviewStyle) -> NudgeCounts {
        match style {
            ReviewStyle::Direct => self.direct,
            ReviewStyle::Balanced => self.balanced,
            ReviewStyle::Encouraging => self.encouraging,
        }
    }

    pub fn skipped(&self, style: ReviewStyle) -> u32 {
        match style {
            ReviewStyle::Direct => self.skipped_direct,
            ReviewStyle::Balanced => self.skipped_balanced,
            ReviewStyle::Encouraging => self.skipped_encouraging,
        }
    }

    /// Record one outcome under the style that was in force.
    pub fn record(&mut self, style: ReviewStyle, outcome: NudgeOutcome) {
        let (counts, skipped) = match style {
            ReviewStyle::Direct => (&mut self.direct, &mut self.skipped_direct),
            ReviewStyle::Balanced => (&mut self.balanced, &mut self.skipped_balanced),
            ReviewStyle::Encouraging => (&mut self.encouraging, &mut self.skipped_encouraging),
        };
        match outcome {
            NudgeOutcome::Acted => counts.acted = counts.acted.saturating_add(1),
            NudgeOutcome::Dismissed => counts.dismissed = counts.dismissed.saturating_add(1),
            NudgeOutcome::Skipped => *skipped = skipped.saturating_add(1),
        }
    }

    /// Start a style's counts afresh — called when the person switches
    /// style, so old evidence doesn't immediately re-suggest a switch.
    pub fn reset(&mut self, style: ReviewStyle) {
        match style {
            ReviewStyle::Direct => {
                self.direct = NudgeCounts::default();
                self.skipped_direct = 0;
            }
            ReviewStyle::Balanced => {
                self.balanced = NudgeCounts::default();
                self.skipped_balanced = 0;
            }
            ReviewStyle::Encouraging => {
                self.encouraging = NudgeCounts::default();
                self.skipped_encouraging = 0;
            }
        }
    }
}

// ── Agent time ──────────────────────────────────────────────────────────────

/// What agent time may touch (SYMBIOTIC_PLANNER.md §3 "Scoped"). Reads stay
/// within what the person shared; writes only ever land in Hobbes-owned
/// space (proposals). The runtime gate applies
/// `agent_time::agent_time_scope_allows` on every call regardless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AgentTimeScope {
    /// Projects agent time may read around. Empty = everything shared.
    #[serde(default)]
    pub project_ids: Vec<String>,
    /// Proposal kinds it may file. Empty = all kinds.
    #[serde(default)]
    pub proposal_kinds: Vec<ProposalKind>,
    /// The person's own words for the scope, shown in the panel.
    #[serde(default)]
    pub note: String,
}

fn default_grant_days() -> [bool; 7] {
    [true; 7]
}

fn default_grant_minutes() -> u32 {
    30
}

fn default_grant_cap() -> f64 {
    1.0
}

/// The human-granted agent-time budget. Off by default; zero is valid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTimeGrant {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_grant_minutes")]
    pub daily_minutes: u32,
    /// Spend cap per day, USD.
    #[serde(default = "default_grant_cap")]
    pub spend_cap_usd: f64,
    /// Granted days, Monday first.
    #[serde(default = "default_grant_days")]
    pub days: [bool; 7],
    #[serde(default)]
    pub scope: AgentTimeScope,
    /// The connector agent time runs on (a `Settings::llm_connectors` id).
    /// `None` = "Same as your chats" (the user's default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector_id: Option<String>,
    /// The model agent time runs on, from that connector's allowed models
    /// (`Settings::allowed_models`). `None` = the connector's own model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The scheduled window: on a granted day, once local time reaches this,
    /// the runtime starts agent time by itself (once a day, never after the
    /// person paused or stopped a run that day). `None` = manual Start only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_start: Option<NaiveTime>,
}

impl Default for AgentTimeGrant {
    fn default() -> Self {
        Self {
            enabled: false,
            daily_minutes: default_grant_minutes(),
            spend_cap_usd: default_grant_cap(),
            days: default_grant_days(),
            scope: AgentTimeScope::default(),
            connector_id: None,
            model: None,
            window_start: None,
        }
    }
}

impl AgentTimeGrant {
    pub fn grants_day(&self, date: NaiveDate) -> bool {
        use chrono::Datelike;
        self.days[date.weekday().num_days_from_monday() as usize]
    }
}

/// One line of an agent-time session's log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTimeLogEntry {
    pub at: DateTime<Utc>,
    pub tool: String,
    pub summary: String,
    /// What the line records. Rows saved before this field read as `Call`.
    #[serde(default, skip_serializing_if = "AgentTimeLogKind::is_call")]
    pub kind: AgentTimeLogKind,
    /// The model's own `log` note for this call: a note it wrote just before
    /// the call is folded in here instead of standing as its own row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What an agent-time log line records.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTimeLogKind {
    /// A tool call the model made (recorded by the runtime gate, or a
    /// `HOBBES_AGENT_TIME` start / propose / end). The only kind counted as
    /// a tool call.
    #[default]
    Call,
    /// The model's optional `log` note that no call has followed yet. The
    /// next recorded call absorbs it (see `AgentTimeLogEntry::note`).
    Note,
    /// The runtime's own line (approval given, held calls expired, run
    /// closed). Never merged, never counted.
    Event,
}

impl AgentTimeLogKind {
    pub fn is_call(&self) -> bool {
        *self == AgentTimeLogKind::Call
    }
}

impl AgentTimeSession {
    /// Real tool calls on the log (notes and runtime events excluded).
    pub fn call_count(&self) -> usize {
        self.tool_calls.iter().filter(|e| e.kind.is_call()).count()
    }
}

/// Why an agent-time session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTimeEndReason {
    /// The agent called `end`.
    Completed,
    /// The person paused it.
    Paused,
    /// The person stopped it.
    Stopped,
    /// Today's granted minutes ran out.
    BudgetExhausted,
    /// Today's spend cap was reached.
    SpendCapReached,
    /// Found open after an app restart.
    Recovered,
}

#[allow(dead_code)] // wave 2: Agent time panel
impl AgentTimeEndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentTimeEndReason::Completed => "completed",
            AgentTimeEndReason::Paused => "paused",
            AgentTimeEndReason::Stopped => "stopped",
            AgentTimeEndReason::BudgetExhausted => "budget_exhausted",
            AgentTimeEndReason::SpendCapReached => "spend_cap_reached",
            AgentTimeEndReason::Recovered => "recovered",
        }
    }
}

/// One logged agent-time sitting: declared intent, every tool call, what it
/// produced, and what it cost. Nothing happens off the record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTimeSession {
    pub id: String,
    pub intent: String,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub tool_calls: Vec<AgentTimeLogEntry>,
    /// Proposal ids filed during the session.
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub end_reason: Option<AgentTimeEndReason>,
    /// The agent's closing summary (`end(summary)`).
    #[serde(default)]
    pub summary: Option<String>,
    /// The Hobbes chat session the runtime ran it in.
    #[serde(default)]
    pub chat_session_id: Option<String>,
    /// Calls the scope gate refused or sent for approval (evidence).
    #[serde(default)]
    pub out_of_scope_attempts: u32,
    /// The agent-lane to-do this session works on (`start(todo_id)`);
    /// `None` = work in Hobbes-owned space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_id: Option<String>,
    /// When the person read this run in the agent-time review
    /// (`PlannerState::mark_reviewed`). `None` = not reviewed yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_at: Option<DateTime<Utc>>,
}

impl AgentTimeSession {
    pub fn is_open(&self) -> bool {
        self.ended_at.is_none()
    }

    /// Wall-clock minutes used, counting a live session up to `now`.
    pub fn minutes_used(&self, now: DateTime<Utc>) -> u32 {
        let end = self.ended_at.unwrap_or(now);
        (end - self.started_at).num_minutes().max(0) as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Note,
    Todo,
    Skill,
    Research,
    Other,
    /// An offer to take one of the person's own (human-lane) to-dos into the
    /// agent lane. `Proposal::todo_id` names it; only the person's
    /// acceptance hands it over.
    Offer,
}

impl ProposalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProposalKind::Note => "note",
            ProposalKind::Todo => "todo",
            ProposalKind::Skill => "skill",
            ProposalKind::Research => "research",
            ProposalKind::Other => "other",
            ProposalKind::Offer => "offer",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "note" | "notes" => Some(ProposalKind::Note),
            "todo" | "to-do" | "to_do" | "task" => Some(ProposalKind::Todo),
            "skill" | "draft_skill" => Some(ProposalKind::Skill),
            "research" | "digest" | "research_digest" => Some(ProposalKind::Research),
            "other" => Some(ProposalKind::Other),
            "offer" | "offer_to_take" => Some(ProposalKind::Offer),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    #[default]
    Open,
    Accepted,
    /// Accepted after the person edited it.
    Edited,
    Dismissed,
    /// A graded result (Prepare / Finish / Tend) the person sent back:
    /// neither kept nor discarded. Never used for Notice proposals.
    NeedsWork,
}

#[allow(dead_code)] // wave 2: Planner UI / weekly review
impl ProposalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProposalStatus::Open => "open",
            ProposalStatus::Accepted => "accepted",
            ProposalStatus::Edited => "edited",
            ProposalStatus::Dismissed => "dismissed",
            ProposalStatus::NeedsWork => "needs_work",
        }
    }

    /// The verdict in words for a result of `job`: graded jobs read
    /// kept / needs work / discarded, Notice proposals accepted / dismissed.
    pub fn verdict_word(self, job: Job) -> &'static str {
        match (job.is_graded(), self) {
            (_, ProposalStatus::Open) => {
                if job.is_graded() {
                    "ungraded"
                } else {
                    "open"
                }
            }
            (_, ProposalStatus::NeedsWork) => "needs work",
            (true, ProposalStatus::Accepted | ProposalStatus::Edited) => "kept",
            (true, ProposalStatus::Dismissed) => "discarded",
            (false, ProposalStatus::Accepted | ProposalStatus::Edited) => "accepted",
            (false, ProposalStatus::Dismissed) => "dismissed",
        }
    }
}

/// Which of agent time's four jobs a result belongs to.
///
/// - `Prepare`: briefs / drafts / background for a commitment the person
///   flagged "Prep this" (`Todo::prep_requested`).
/// - `Finish`: finished work on the agent-lane queue.
/// - `Notice`: a proposal for a change to the person's stuff (accept /
///   dismiss). Rows saved before jobs existed load as `Notice`.
/// - `Tend`: Hobbes's own upkeep (corrections, its own friction, learning
///   its tools, tidying memory). Read-only outside Hobbes.
///
/// Prepare / Finish / Tend results are *graded*: `Accepted` = Keep,
/// `NeedsWork` = Needs work, `Dismissed` = Discard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Job {
    Prepare,
    Finish,
    #[default]
    Notice,
    Tend,
}

#[allow(dead_code)] // label: consumer is the agent-time review UI
impl Job {
    pub fn as_str(self) -> &'static str {
        match self {
            Job::Prepare => "prepare",
            Job::Finish => "finish",
            Job::Notice => "notice",
            Job::Tend => "tend",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Job::Prepare => "Prepare",
            Job::Finish => "Finish",
            Job::Notice => "Notice",
            Job::Tend => "Tend",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "prepare" | "prep" => Some(Job::Prepare),
            "finish" | "finished" => Some(Job::Finish),
            "notice" => Some(Job::Notice),
            "tend" | "upkeep" => Some(Job::Tend),
            _ => None,
        }
    }

    /// Whether results of this job are graded Keep / Needs work / Discard
    /// (everything but Notice).
    pub fn is_graded(self) -> bool {
        self != Job::Notice
    }

    pub fn is_notice(&self) -> bool {
        *self == Job::Notice
    }
}

/// The person's comment on a result. Kept on the result and shown to the
/// agent in the next window's opening context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultNote {
    pub at: DateTime<Utc>,
    pub text: String,
}

/// A question agent time saved instead of stopping the run to ask
/// (`HOBBES_AGENT_TIME ask`). The person answers it in the review; the
/// answer reaches the next window's opening context once, then is
/// `consumed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentQuestion {
    pub id: String,
    /// The agent-time session that asked it.
    pub session_id: String,
    pub question: String,
    /// Suggested answers (may be empty: a free answer).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    pub asked_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<DateTime<Utc>>,
    /// The answer was delivered into a window's opening context.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub consumed: bool,
}

impl AgentQuestion {
    pub fn is_open(&self) -> bool {
        self.answer.is_none()
    }
}

/// An agent-time output. Proposals are the only thing agent time produces;
/// accepting one is a UI action, never a tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub kind: ProposalKind,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub status: ProposalStatus,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub resolved_at: Option<DateTime<Utc>>,
    /// The agent-time session that filed it.
    pub session_id: String,
    /// `Offer`: the person's to-do agent time offers to take. Absent on
    /// every other kind (and on rows saved before offers existed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_id: Option<String>,
    /// `Todo`: the lane the suggested to-do belongs in. `Some(Agent)` when
    /// the agent filed it as its own work (`propose(kind: todo, lane:
    /// agent)`); absent = a suggestion for the person (human lane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<Lane>,
    /// The person's later changes to the verdict, oldest first. The agent
    /// sees them (with any note) in the next run's opening context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revisions: Vec<ProposalRevision>,
    /// Taken off the trust ledger by the person: it no longer counts and
    /// leaves the lists, but stays on disk with its revision record.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    /// Which job produced it. Rows saved before jobs existed are `Notice`.
    #[serde(default, skip_serializing_if = "Job::is_notice")]
    pub job: Job,
    /// A Tend result correcting something Hobbes told the person. Shown
    /// first in the review.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub correction: bool,
    /// The person's comments on it, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<ResultNote>,
}

impl Proposal {
    /// A graded result (Prepare / Finish / Tend), not a Notice proposal.
    pub fn is_result(&self) -> bool {
        self.job.is_graded()
    }
}

/// One after-the-fact change to a resolved proposal (Trust ledger → Review).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposalRevision {
    pub at: DateTime<Utc>,
    /// The verdict before the change.
    pub from: ProposalStatus,
    /// The verdict after it; `None` = removed from the ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<ProposalStatus>,
    /// The person's reason, for the agent. May be empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// Trust-ledger evidence (SYMBIOTIC_PLANNER.md §3). Evidence only: it never
/// grants anything — `measures::suggest_scope_widening` can only suggest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TrustLedger {
    #[serde(default)]
    pub proposals_accepted: u32,
    /// Accepted after editing (counts as accepted for the rate, noted apart).
    #[serde(default)]
    pub proposals_edited: u32,
    #[serde(default)]
    pub proposals_dismissed: u32,
    #[serde(default)]
    pub in_scope_attempts: u32,
    #[serde(default)]
    pub out_of_scope_attempts: u32,
    #[serde(default)]
    pub promises_kept: u32,
    #[serde(default)]
    pub promises_broken: u32,
    /// Sessions that ended with zero out-of-scope attempts.
    #[serde(default)]
    pub clean_sessions: u32,
    /// Graded results (Prepare / Finish / Tend) the person kept.
    #[serde(default)]
    pub results_kept: u32,
    /// Graded results sent back as "needs work".
    #[serde(default)]
    pub results_needs_work: u32,
    /// Graded results discarded.
    #[serde(default)]
    pub results_discarded: u32,
    /// Corrections Hobbes delivered (Tend results flagged `correction`).
    #[serde(default)]
    pub corrections_delivered: u32,
}

#[allow(dead_code)] // wave 2: Planner UI / weekly review
impl TrustLedger {
    /// Accepted (incl. edited) over everything decided; `None` before any.
    pub fn acceptance_rate(&self) -> Option<f64> {
        let ok = self.proposals_accepted + self.proposals_edited;
        let total = ok + self.proposals_dismissed;
        (total > 0).then(|| ok as f64 / total as f64)
    }

    /// Kept over every graded result; `None` before any.
    pub fn keep_rate(&self) -> Option<f64> {
        let total = self.results_kept + self.results_needs_work + self.results_discarded;
        (total > 0).then(|| self.results_kept as f64 / total as f64)
    }

    pub fn in_scope_rate(&self) -> Option<f64> {
        let total = self.in_scope_attempts + self.out_of_scope_attempts;
        (total > 0).then(|| self.in_scope_attempts as f64 / total as f64)
    }
}

// ── Todo ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub status: TodoStatus,
    #[serde(default)]
    pub bucket: TodoBucket,

    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub area_id: Option<String>,

    /// The day you intend to work on it. Deliberately distinct from `deadline`
    /// — collapsing the two is what makes most to-do apps nag.
    #[serde(default)]
    pub scheduled_for: Option<NaiveDate>,
    #[serde(default)]
    pub time_of_day: Option<TimeOfDay>,
    /// The day it is actually due.
    #[serde(default)]
    pub deadline: Option<NaiveDate>,

    #[serde(default)]
    pub estimate_minutes: Option<u32>,
    #[serde(default)]
    pub actual_minutes: u32,
    /// Eisenhower quadrant; `None` = not triaged (the default, so rows
    /// written before this field existed load unchanged).
    #[serde(default)]
    pub priority: Option<Priority>,

    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub checklist: Vec<ChecklistItem>,

    /// When the current focus session began. `Some` only while status is
    /// `InProgress`; folding into `actual_minutes` happens on pause/close.
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    /// Fractional index — see [`sort_between`].
    #[serde(default)]
    pub sort_order: f64,
    #[serde(default)]
    pub origin: TodoOrigin,

    /// The fleet session carrying this todo (terminal Claude Code uuid, or a
    /// Hobbes chat session id). Set by the AI tools or auto-set when agent
    /// focus starts; the fleet side derives its view by scanning todos.
    #[serde(default)]
    pub linked_fleet_session: Option<String>,
    /// Display-only: the linked session's latest brief line ("did X;
    /// blocked: Y"). Never drives status — progress is reported, not judged.
    #[serde(default)]
    pub latest_progress: Option<String>,

    // ── Symbiotic planner (SYMBIOTIC_PLANNER.md §4) ─────────────────────────
    // Every field defaults, so rows written before the lanes existed load as
    // untriaged human to-dos.
    /// Who spends time on it: the person (default) or an agent.
    #[serde(default)]
    pub lane: Lane,
    /// For human to-dos: commitment, chore or your time. `None` = not
    /// triaged. Ignored (and cleared by the tools) on agent to-dos.
    #[serde(default)]
    pub kind: Option<HumanKind>,
    /// Your-time category ("Reading", "Outside"…) for `kind = YourTime`
    /// to-dos — matched case-insensitively against `TimeGoal.category`. When
    /// unset, the first tag naming a goal's category is used instead.
    #[serde(default)]
    pub time_category: Option<String>,
    /// For agent to-dos: the definition of done the agent works toward.
    #[serde(default)]
    pub outcome: Option<String>,
    /// For agent to-dos: where the work stands. Derived from the linked
    /// worker's fleet status and brief wherever possible (see
    /// `lanes::derive_agent_state`) — reported, never judged.
    #[serde(default)]
    pub agent_state: Option<AgentState>,
    /// The decision the agent needs from the person (open while
    /// `resolved` is `None`).
    #[serde(default)]
    pub decision: Option<Decision>,
    /// For agent to-dos: the model its worker runs on (a slug, e.g.
    /// `claude-haiku-5-5`), recorded at dispatch so the lane card can show
    /// it. `None` when unknown (a Claude Code run on its default model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_model: Option<String>,
    /// "Prep this": the person asked agent time to prepare for this human
    /// to-do (briefs, drafts, background). Only the person sets it — the
    /// model's to-do tools refuse it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prep_requested: bool,

    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

impl Todo {
    /// Assign (or clear) the project. Filing an Inbox todo into a project is
    /// a triage decision (Things does the same), so it leaves Inbox for
    /// Anytime. Clearing never moves it back.
    pub fn set_project(&mut self, project_id: Option<String>) {
        if project_id.is_some() && self.bucket == TodoBucket::Inbox {
            self.bucket = TodoBucket::Anytime;
        }
        self.project_id = project_id;
    }

    /// Whether an agent is waiting on an unanswered decision.
    pub fn has_open_decision(&self) -> bool {
        self.decision.as_ref().is_some_and(Decision::is_open)
    }

    /// Move the to-do between lanes, keeping the per-lane fields coherent:
    /// entering the agent lane drops the human `kind` and starts `Queued`
    /// (unless a state is already known); returning to the human lane
    /// clears the agent state and any open decision. `outcome` is kept
    /// either way — it is the person's words.
    pub fn set_lane(&mut self, lane: Lane) {
        if self.lane == lane {
            return;
        }
        self.lane = lane;
        match lane {
            Lane::Agent => {
                self.kind = None;
                self.time_category = None;
                // "Prep this" is for the person's own commitments.
                self.prep_requested = false;
                if self.agent_state.is_none() {
                    self.agent_state = Some(AgentState::Queued);
                }
            }
            Lane::Human => {
                self.agent_state = None;
                if self.has_open_decision() {
                    self.decision = None;
                }
            }
        }
    }

    /// A new todo in the Inbox, sorted to the end.
    pub fn new(title: impl Into<String>, sort_order: f64) -> Self {
        let now = Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.into(),
            notes: String::new(),
            status: TodoStatus::Open,
            bucket: TodoBucket::Inbox,
            project_id: None,
            area_id: None,
            scheduled_for: None,
            time_of_day: None,
            deadline: None,
            estimate_minutes: None,
            actual_minutes: 0,
            priority: None,
            tags: Vec::new(),
            checklist: Vec::new(),
            started_at: None,
            sort_order,
            origin: TodoOrigin::User,
            linked_fleet_session: None,
            latest_progress: None,
            lane: Lane::Human,
            kind: None,
            time_category: None,
            outcome: None,
            agent_state: None,
            decision: None,
            worker_model: None,
            prep_requested: false,
            created_at: now,
            updated_at: now,
            completed_at: None,
        }
    }

    /// Past its deadline and still open. Uses the *deadline*, never the
    /// scheduled date — missing a day you planned something is not a failure.
    pub fn is_overdue(&self, today: NaiveDate) -> bool {
        !self.status.is_closed() && self.deadline.is_some_and(|d| d < today)
    }

    /// Fold the running focus session (if any) into `actual_minutes` and clear
    /// the start marker. Safe to call in any state.
    pub fn fold_elapsed(&mut self, now: DateTime<Utc>) {
        if let Some(started) = self.started_at.take() {
            let mins = (now - started).num_minutes().max(0) as u32;
            self.actual_minutes = self.actual_minutes.saturating_add(mins);
        }
    }

    /// Minutes actually spent, including the live session when focused.
    pub fn elapsed_minutes(&self, now: DateTime<Utc>) -> u32 {
        let live = self
            .started_at
            .map(|s| (now - s).num_minutes().max(0) as u32)
            .unwrap_or(0);
        self.actual_minutes.saturating_add(live)
    }

    pub fn mark_completed(&mut self, now: DateTime<Utc>) {
        self.fold_elapsed(now);
        self.status = TodoStatus::Completed;
        self.completed_at = Some(now);
        self.updated_at = now;
    }

    pub fn reopen(&mut self, now: DateTime<Utc>) {
        self.fold_elapsed(now);
        self.status = TodoStatus::Open;
        self.completed_at = None;
        self.updated_at = now;
    }

    /// Leave focus without closing: back to Open with the session banked.
    pub fn pause(&mut self, now: DateTime<Utc>) {
        self.fold_elapsed(now);
        if self.status == TodoStatus::InProgress {
            self.status = TodoStatus::Open;
        }
        self.updated_at = now;
    }

    /// One-line summary for AI tool responses. Line-oriented on purpose: tool
    /// results are re-fed into the next prompt, so this stays cheap.
    pub fn summary(&self) -> String {
        let mark = match self.status {
            TodoStatus::Open => "○",
            TodoStatus::InProgress => "▶",
            TodoStatus::Completed => "✓",
            TodoStatus::Cancelled => "✗",
        };
        let mut parts: Vec<String> = Vec::new();
        if let Some(m) = self.estimate_minutes {
            parts.push(format_minutes(m));
        }
        if let Some(d) = self.scheduled_for {
            parts.push(d.to_string());
        }
        if let Some(d) = self.deadline {
            parts.push(format!("due {}", d));
        }
        if let Some(p) = self.priority {
            parts.push(format!("{} {}", p.short_label(), p.label().to_lowercase()));
        }
        for tag in &self.tags {
            parts.push(format!("#{}", tag));
        }
        // Lane markers only when they say something: an untriaged human
        // to-do (every pre-lanes row) reads exactly as before.
        match self.lane {
            Lane::Agent => parts.push(match self.agent_state {
                Some(s) => format!("agent: {}", s.label()),
                None => "agent".to_string(),
            }),
            Lane::Human => {
                if let Some(k) = self.kind {
                    match (k, self.time_category.as_deref()) {
                        (HumanKind::YourTime, Some(c)) => parts.push(format!("your time: {}", c)),
                        _ => parts.push(k.label().to_string()),
                    }
                }
            }
        }

        let mut out = if parts.is_empty() {
            format!("[{}] {} {}", self.id, mark, self.title)
        } else {
            format!(
                "[{}] {} {} — {}",
                self.id,
                mark,
                self.title,
                parts.join(", ")
            )
        };
        if self.lane == Lane::Agent {
            if let Some(o) = self.outcome.as_deref().filter(|o| !o.trim().is_empty()) {
                out.push_str(&format!(" → outcome: {}", o.trim()));
            }
        }
        // Closed work reports its actuals so the AI sees estimate accuracy
        // (groundwork for the shutdown ritual).
        if self.status.is_closed() && self.actual_minutes > 0 {
            match self.estimate_minutes {
                Some(est) => out.push_str(&format!(
                    " (took {} of {})",
                    format_minutes(self.actual_minutes),
                    format_minutes(est)
                )),
                None => out.push_str(&format!(" (took {})", format_minutes(self.actual_minutes))),
            }
        }
        out
    }
}

// ── Projects & areas ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub area_id: Option<String>,
    #[serde(default)]
    pub status: TodoStatus,
    #[serde(default)]
    pub deadline: Option<NaiveDate>,
    #[serde(default)]
    pub sort_order: f64,
    /// Repo/folder root for this project — lets fleet terminal sessions map
    /// to the project by cwd (longest-prefix match).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Other names this project answers to (merged-in titles, a repo name,
    /// a nickname). Every name match — AI upsert dedupe, the session tagger,
    /// quick-add `+project`, fleet cwd mapping — treats them like the title
    /// (`todo::project_identity`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Project ids the person said are NOT duplicates of this one ("Not
    /// duplicates" in Manage projects) — keeps the pair out of "Possible
    /// duplicates".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub distinct_from: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Area {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub sort_order: f64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Serde default for the `busy` flags: rows serialized before busy/free
/// semantics existed must load as busy — treating an unknown meeting as free
/// time is the dangerous direction.
fn default_busy() -> bool {
    true
}

/// Focus-time title heuristic, shared by both calendar transports (ICS and
/// Composio). Google exports Focus Time to ICS as an ordinary event titled
/// "Focus time", so the title is the only signal the ICS path gets. A title
/// that (case-insensitively, trimmed) equals "focus" or "focus time", or
/// starts with "focus time", marks the event as non-blocking.
pub fn is_focus_time_title(title: &str) -> bool {
    let t = title.trim().to_lowercase();
    t == "focus" || t == "focus time" || t.starts_with("focus time")
}

// ── Time blocks & day plans ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum BlockSource {
    /// The user dragged it onto the timeline.
    Manual,
    /// Placed by the planner.
    Auto,
    /// Mirrored from a real calendar; read-only in the UI. The serde defaults
    /// keep blocks serialized before subscriptions existed loading.
    /// `subscription_id` drives the color tint (and the sync reconciler's GC);
    /// `url` drives click-to-open.
    External {
        uid: String,
        #[serde(default)]
        subscription_id: Option<String>,
        #[serde(default)]
        url: Option<String>,
        /// Whether the event blocks time (Busy) or not (Free / Focus Time).
        /// Non-busy blocks still render on the timeline but don't join the
        /// auto-placement busy set, don't trigger overlap warnings, and don't
        /// count as planned time. Defaults to busy for pre-existing rows.
        #[serde(default = "default_busy")]
        busy: bool,
        /// Whether the invitation is tentative (ICS `STATUS:TENTATIVE`,
        /// Google `status: "tentative"`). Tentative meetings still block
        /// auto-placement and warn on overlap (standard free-busy
        /// convention) but contribute nothing to planned time. Defaults to
        /// firm for rows serialized before the flag existed.
        #[serde(default)]
        tentative: bool,
    },
    /// Protected your-time (SYMBIOTIC_PLANNER.md §4): placed by the person
    /// (or by the AI when asked), in a your-time category. The planner plans
    /// around these and never moves or deletes one without the person's say
    /// so (`HOBBES_TIME_BLOCK` requires `user_confirmed`).
    YourTime { category: String },
    /// A batched chores slot: one block holding several quick chore to-dos
    /// (`todo_ids`), so chores happen together instead of fragmenting the
    /// day.
    Chores {
        #[serde(default)]
        todo_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeBlock {
    pub id: String,
    /// `None` for meetings and other blocks that aren't a todo.
    #[serde(default)]
    pub todo_id: Option<String>,
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub source: BlockSource,
}

impl TimeBlock {
    // Consumed by P4's inline cards (elapsed-vs-estimate display).
    #[allow(dead_code)]
    pub fn duration_minutes(&self) -> u32 {
        (self.end - self.start).num_minutes().max(0) as u32
    }

    /// Protected your-time: the planner plans around it.
    pub fn is_protected(&self) -> bool {
        matches!(self.source, BlockSource::YourTime { .. })
    }

    /// The your-time category, for protected blocks.
    pub fn your_time_category(&self) -> Option<&str> {
        match &self.source {
            BlockSource::YourTime { category } => Some(category.as_str()),
            _ => None,
        }
    }

    /// Whether two blocks overlap in time. Touching edges do not overlap, so a
    /// 09:00–10:00 block and a 10:00–11:00 block sit back to back cleanly.
    pub fn overlaps(&self, other: &TimeBlock) -> bool {
        self.start < other.end && other.start < self.end
    }
}

// ── Calendar events ─────────────────────────────────────────────────────────

/// One occurrence of an external calendar event, normalized to UTC instants.
///
/// This is the cache record (`todo_calendar_events.data`), the unit the
/// fetchers produce, and what the materializer turns into external
/// `TimeBlock`s. Recurring events arrive pre-expanded — one `CalendarEvent`
/// per occurrence, keyed `(subscription_id, uid, start)` — so recurrence rules
/// never reach this type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub uid: String,
    pub subscription_id: String,
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    #[serde(default)]
    pub all_day: bool,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    /// Busy/Free semantics: `false` for events marked transparent/free
    /// (ICS `TRANSP:TRANSPARENT`, Google `transparency: "transparent"` or
    /// `eventType: "focusTime"`, or a focus-time title). Cached rows from
    /// before this field existed load as busy.
    #[serde(default = "default_busy")]
    pub busy: bool,
    /// Tentative invitation status (ICS `STATUS:TENTATIVE`, Google
    /// `status: "tentative"`). Cached rows from before this field existed
    /// load as firm.
    #[serde(default)]
    pub tentative: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayPlan {
    pub date: NaiveDate,
    pub capacity_minutes: u32,
    #[serde(default)]
    pub planned_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub shutdown_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub reflection: String,
}

impl DayPlan {
    pub fn new(date: NaiveDate, capacity_minutes: u32) -> Self {
        Self {
            date,
            capacity_minutes,
            planned_at: None,
            shutdown_at: None,
            reflection: String::new(),
        }
    }
}

/// The result of measuring a day's planned work against its capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    /// The day's total: open work still owed PLUS work already finished
    /// (and, via [`measure_capacity_with_meetings`], firm busy meetings).
    /// Counting only open todos makes a fully executed day read "0m planned"
    /// — finishing work must never shrink the plan.
    pub planned_minutes: u32,
    /// The finished share of `planned_minutes`: todos completed on this day.
    pub done_minutes: u32,
    pub capacity_minutes: u32,
    /// Open todos scheduled for the day with no estimate — they make
    /// `planned` an undercount, so the UI and the AI should say so rather
    /// than imply the day fits.
    pub unestimated: usize,
    /// Agent-actor focus minutes on the day. Informational only: never
    /// consumes `capacity_minutes` and never joins `planned_minutes` — the
    /// agent working does not use up the person's day. Zero unless capacity
    /// was measured with focus sessions.
    pub agent_minutes: u32,
}

impl Capacity {
    pub fn over_by(&self) -> u32 {
        self.planned_minutes.saturating_sub(self.capacity_minutes)
    }

    pub fn remaining(&self) -> u32 {
        self.capacity_minutes.saturating_sub(self.planned_minutes)
    }

    pub fn is_overcommitted(&self) -> bool {
        self.planned_minutes > self.capacity_minutes
    }

    /// Human summary for tool responses and the capacity bar's tooltip.
    pub fn summary(&self) -> String {
        let mut out = format!("{} planned", format_minutes(self.planned_minutes));
        // Zero-state stays clean: an untouched day shouldn't advertise "0m done".
        if self.done_minutes > 0 {
            out.push_str(&format!(" · {} done", format_minutes(self.done_minutes)));
        }
        if self.agent_minutes > 0 {
            out.push_str(&format!(" · {} agent", format_minutes(self.agent_minutes)));
        }
        if self.is_overcommitted() {
            out.push_str(&format!(" — over by {}", format_minutes(self.over_by())));
        } else {
            out.push_str(&format!(" · {} free", format_minutes(self.remaining())));
        }
        if self.unestimated > 0 {
            out.push_str(&format!(
                " ({} unestimated, so the real total is higher)",
                self.unestimated
            ));
        }
        out
    }
}

/// Measure the todos scheduled for a day against its capacity.
///
/// `date` is a **local** calendar day (planner days are user-local
/// everywhere). Open todos contribute their estimates; todos completed on
/// that local day contribute `max(estimate, actual_minutes)` — the actual
/// wins when the work overran, and unestimated finished work still counts
/// for the time it took.
#[allow(dead_code)] // shipping callers use the sessions-aware variants; tests pin the base math
pub fn measure_capacity(todos: &[Todo], date: NaiveDate, capacity_minutes: u32) -> Capacity {
    measure_capacity_with_sessions(todos, &[], date, capacity_minutes)
}

/// [`measure_capacity`] with focus-session attribution. The person math keeps
/// its shape; only the "spent" source changes: a completed todo that has ANY
/// session rows derives its spent time from its **person-actor** session
/// minutes (agent work does not masquerade as the person's), while a todo
/// with no rows — pre-migration data — falls back to `actual_minutes`.
/// Never both, so nothing double-counts. `agent_minutes` is filled with the
/// day's agent-actor session minutes; with `sessions` empty this is exactly
/// the old math.
pub fn measure_capacity_with_sessions(
    todos: &[Todo],
    sessions: &[FocusSession],
    date: NaiveDate,
    capacity_minutes: u32,
) -> Capacity {
    let mut open = 0u32;
    let mut done = 0u32;
    let mut unestimated = 0usize;
    // The agent lane is not the person's day: agent work never consumes
    // capacity (its cost to the person is judgment minutes, not hours).
    for todo in todos
        .iter()
        .filter(|t| t.scheduled_for == Some(date) && t.lane == Lane::Human)
    {
        if todo.status.is_closed() {
            let completed_on_day = todo
                .completed_at
                .is_some_and(|c| c.with_timezone(&chrono::Local).date_naive() == date);
            if completed_on_day {
                let has_rows = sessions.iter().any(|s| s.todo_id == todo.id);
                let spent = if has_rows {
                    sessions
                        .iter()
                        .filter(|s| s.todo_id == todo.id && !s.actor.is_agent())
                        .fold(0u32, |acc, s| acc.saturating_add(s.minutes))
                } else {
                    todo.actual_minutes
                };
                done = done.saturating_add(todo.estimate_minutes.unwrap_or(0).max(spent));
            }
        } else {
            match todo.estimate_minutes {
                Some(m) => open = open.saturating_add(m),
                None => unestimated += 1,
            }
        }
    }

    Capacity {
        planned_minutes: open.saturating_add(done),
        done_minutes: done,
        capacity_minutes,
        unestimated,
        agent_minutes: agent_minutes_on(sessions, date),
    }
}

/// Merge a set of half-open intervals into a sorted, disjoint union.
/// Touching edges coalesce (`start <= current_end`), so back-to-back
/// meetings merge into one span for free.
fn merge_intervals(
    mut intervals: Vec<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    intervals.sort_by_key(|(s, _)| *s);
    let mut merged: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    for (start, end) in intervals {
        match merged.last_mut() {
            Some((_, ce)) if start <= *ce => *ce = (*ce).max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Minutes of planned meeting time on a **local** day: the union of firm busy
/// external (calendar-mirrored) meeting intervals, minus any overlap with
/// task blocks (Manual/Auto — the todo timeboxes), summed.
///
/// - Free/focus-time events (`busy: false`) and tentative invitations
///   (`tentative: true`) contribute nothing.
/// - Overlapping meetings merge before summing — sitting in two meetings at
///   once does not cost twice the time.
/// - Where a task block overlaps a meeting, the task wins: the todo's
///   estimate already claims that slot in `planned_minutes`, so only the
///   non-overlapped meeting minutes add on top.
pub fn meeting_planned_minutes(blocks: &[TimeBlock], date: NaiveDate) -> u32 {
    let on_day = |b: &&TimeBlock| {
        b.end > b.start && b.start.with_timezone(&chrono::Local).date_naive() == date
    };
    let meetings = merge_intervals(
        blocks
            .iter()
            .filter(|b| {
                matches!(
                    b.source,
                    BlockSource::External {
                        busy: true,
                        tentative: false,
                        ..
                    }
                )
            })
            .filter(on_day)
            .map(|b| (b.start, b.end))
            .collect(),
    );
    // Task blocks are the todo timeboxes (and batched chores). Protected
    // your-time is not work: a meeting over it still costs the day.
    let tasks = merge_intervals(
        blocks
            .iter()
            .filter(|b| {
                matches!(
                    b.source,
                    BlockSource::Manual | BlockSource::Auto | BlockSource::Chores { .. }
                )
            })
            .filter(on_day)
            .map(|b| (b.start, b.end))
            .collect(),
    );

    // Both unions are sorted and disjoint, so the meeting minutes minus the
    // pairwise intersections can never go negative — the clamps are belt and
    // braces against arithmetic surprises, not expected paths.
    let mut total = 0i64;
    for (ms, me) in &meetings {
        total += (*me - *ms).num_minutes();
        for (ts, te) in &tasks {
            let start = (*ms).max(*ts);
            let end = (*me).min(*te);
            if end > start {
                total -= (end - start).num_minutes();
            }
        }
    }
    total.max(0) as u32
}

/// [`measure_capacity`] with firm busy external meetings **added to the
/// day's planned time** when `meetings_count` is on — a day with 4h of
/// meetings has 4h of its capacity already spoken for, and the plan should
/// say so. Capacity itself stays the full configured day; overlap with task
/// blocks is deduplicated with the task winning ([`meeting_planned_minutes`]).
/// With `meetings_count` off this is exactly `measure_capacity`.
#[allow(dead_code)] // shipping callers use measure_capacity_full; tests pin the meeting math
pub fn measure_capacity_with_meetings(
    todos: &[Todo],
    blocks: &[TimeBlock],
    date: NaiveDate,
    capacity_minutes: u32,
    meetings_count: bool,
) -> Capacity {
    measure_capacity_full(todos, blocks, &[], date, capacity_minutes, meetings_count)
}

/// The whole picture: todos + meetings + focus-session attribution. This is
/// what the Today rail and the planner context use; the narrower functions
/// above are this with parts absent.
pub fn measure_capacity_full(
    todos: &[Todo],
    blocks: &[TimeBlock],
    sessions: &[FocusSession],
    date: NaiveDate,
    capacity_minutes: u32,
    meetings_count: bool,
) -> Capacity {
    let mut cap = measure_capacity_with_sessions(todos, sessions, date, capacity_minutes);
    if meetings_count {
        cap.planned_minutes = cap
            .planned_minutes
            .saturating_add(meeting_planned_minutes(blocks, date));
    }
    cap
}

// ── Fractional ordering ─────────────────────────────────────────────────────

/// Gap between freshly appended items. Large enough that thousands of
/// in-between insertions never need a renormalise.
pub const SORT_STEP: f64 = 1024.0;

/// Below this gap, `sort_between` can no longer split cleanly and the list
/// should be renormalised.
pub const SORT_MIN_GAP: f64 = 1e-6;

/// A sort key placing an item between two neighbours.
///
/// `None` means "no neighbour on that side". Writing one row instead of
/// renumbering the whole list is what keeps drag-and-drop responsive.
pub fn sort_between(before: Option<f64>, after: Option<f64>) -> f64 {
    match (before, after) {
        (None, None) => 0.0,
        (Some(b), None) => b + SORT_STEP,
        (None, Some(a)) => a - SORT_STEP,
        (Some(b), Some(a)) => (b + a) / 2.0,
    }
}

/// Whether the neighbours have collapsed too close together to split again.
/// The caller should renormalise the list and retry.
pub fn needs_renormalise(before: Option<f64>, after: Option<f64>) -> bool {
    match (before, after) {
        (Some(b), Some(a)) => (a - b).abs() < SORT_MIN_GAP,
        _ => false,
    }
}

/// Rewrite an ordered run of todos' sort keys onto a fresh evenly-spaced
/// ladder from 0.
pub fn renormalise<'a>(todos: impl IntoIterator<Item = &'a mut Todo>) {
    for (i, todo) in todos.into_iter().enumerate() {
        todo.sort_order = i as f64 * SORT_STEP;
    }
}

// ── Formatting ──────────────────────────────────────────────────────────────

/// `95` → `"1h 35m"`, `45` → `"45m"`, `120` → `"2h"`.
pub fn format_minutes(minutes: u32) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    match (h, m) {
        (0, m) => format!("{}m", m),
        (h, 0) => format!("{}h", h),
        (h, m) => format!("{}h {}m", h, m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_saved_before_model_choice_means_same_as_your_chats() {
        let old = r#"{"enabled":true,"daily_minutes":45,"spend_cap_usd":2.0,"days":[true,true,true,true,true,true,true]}"#;
        let g: AgentTimeGrant = serde_json::from_str(old).unwrap();
        assert_eq!((g.connector_id, g.model), (None, None));
        // Unset fields stay out of the saved JSON; set ones round-trip.
        let json = serde_json::to_string(&AgentTimeGrant::default()).unwrap();
        assert!(!json.contains("connector_id") && !json.contains("\"model\""), "{json}");
        let g = AgentTimeGrant {
            connector_id: Some("c1".into()),
            model: Some("claude-haiku-5-5".into()),
            ..Default::default()
        };
        let back: AgentTimeGrant = serde_json::from_str(&serde_json::to_string(&g).unwrap()).unwrap();
        assert_eq!(back, g);
        // Old to-do and proposal rows load without the new fields too.
        let t: Todo = serde_json::from_value(serde_json::to_value(Todo::new("x", 0.0)).unwrap()).unwrap();
        assert_eq!(t.worker_model, None);
    }

    #[test]
    fn filing_an_inbox_todo_into_a_project_triages_it() {
        let mut t = Todo::new("x", 0.0);
        t.set_project(Some("p".into()));
        assert_eq!(t.bucket, TodoBucket::Anytime);
        t.set_project(None);
        assert_eq!(t.bucket, TodoBucket::Anytime, "clearing never un-triages");

        let mut s = Todo::new("y", 0.0);
        s.bucket = TodoBucket::Someday;
        s.set_project(Some("p".into()));
        assert_eq!(s.bucket, TodoBucket::Someday);
    }

    #[test]
    fn priority_cycle_walks_the_quadrants_then_clears() {
        let mut p = None;
        let mut seen = Vec::new();
        for _ in 0..5 {
            p = Priority::cycle(p);
            seen.push(p);
        }
        assert_eq!(
            seen,
            vec![
                Some(Priority::UrgentImportant),
                Some(Priority::Important),
                Some(Priority::Urgent),
                Some(Priority::Neither),
                None
            ]
        );
    }

    #[test]
    fn priority_display_schemes_and_custom_names() {
        let q = PriorityDisplay::default();
        assert_eq!(q.badge(Priority::Important), "Q2");
        assert_eq!(q.name(Priority::Important), "Important, not urgent");
        assert_eq!(q.describe(Priority::Important), "Important, not urgent — Schedule");

        let p = PriorityDisplay {
            scheme: PriorityScheme::Linear,
            names: vec!["Now".into(), "  ".into()],
        };
        assert_eq!(p.badge(Priority::UrgentImportant), "P1");
        assert_eq!(p.name(Priority::UrgentImportant), "Now");
        assert_eq!(p.name(Priority::Important), "High", "blank falls back");
        assert_eq!(p.name(Priority::Neither), "Low", "missing falls back");
        assert_eq!(Priority::parse("p3"), Some(Priority::Urgent));

        // Old settings files carry no priority block at all.
        let d: PriorityDisplay = serde_json::from_str("{}").unwrap();
        assert_eq!(d, PriorityDisplay::default());
    }

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn todo_with(estimate: Option<u32>, scheduled: Option<NaiveDate>) -> Todo {
        let mut t = Todo::new("t", 0.0);
        t.estimate_minutes = estimate;
        t.scheduled_for = scheduled;
        t
    }

    #[test]
    fn format_minutes_reads_naturally() {
        assert_eq!(format_minutes(0), "0m");
        assert_eq!(format_minutes(45), "45m");
        assert_eq!(format_minutes(60), "1h");
        assert_eq!(format_minutes(95), "1h 35m");
        assert_eq!(format_minutes(120), "2h");
    }

    #[test]
    fn overdue_uses_deadline_not_scheduled_date() {
        let today = date("2026-08-12");
        let mut t = Todo::new("x", 0.0);

        // A scheduled date in the past is not overdue — plans slip, that's fine.
        t.scheduled_for = Some(date("2026-08-01"));
        assert!(!t.is_overdue(today));

        t.deadline = Some(date("2026-08-11"));
        assert!(t.is_overdue(today));

        // Due today is not yet overdue.
        t.deadline = Some(today);
        assert!(!t.is_overdue(today));

        // Closed todos are never overdue.
        t.deadline = Some(date("2026-08-01"));
        t.mark_completed(Utc::now());
        assert!(!t.is_overdue(today));
    }

    /// A UTC instant whose *local* date is `date`, matching how completion
    /// stamps are compared. Keeps these tests independent of the machine's
    /// timezone.
    fn local_noon(date: NaiveDate) -> DateTime<Utc> {
        use chrono::TimeZone;
        chrono::Local
            .from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn capacity_counts_open_and_done_work_scheduled_that_day() {
        let today = date("2026-08-12");
        let other = date("2026-08-13");
        let mut done = todo_with(Some(60), Some(today));
        done.mark_completed(local_noon(today));

        let todos = vec![
            todo_with(Some(45), Some(today)),
            todo_with(Some(30), Some(today)),
            todo_with(Some(600), Some(other)), // different day
            todo_with(None, Some(today)),      // unestimated
            done,                              // finished today — still planned work
            todo_with(Some(90), None),         // unscheduled
        ];

        let cap = measure_capacity(&todos, today, 360);
        assert_eq!(
            cap.planned_minutes, 135,
            "finishing work must not shrink the plan: open (75) + done (60)"
        );
        assert_eq!(cap.done_minutes, 60);
        assert_eq!(cap.unestimated, 1);
        assert_eq!(cap.remaining(), 225);
        assert!(!cap.is_overcommitted());
        assert_eq!(cap.summary(), "2h 15m planned · 1h done · 3h 45m free (1 unestimated, so the real total is higher)");
    }

    #[test]
    fn capacity_done_uses_the_larger_of_estimate_and_actual() {
        let today = date("2026-08-12");
        let yesterday = date("2026-08-11");

        // Overran its estimate: the actual wins.
        let mut overran = todo_with(Some(30), Some(today));
        overran.actual_minutes = 50;
        overran.mark_completed(local_noon(today));
        // No estimate: the actual is all we know.
        let mut unestimated_done = todo_with(None, Some(today));
        unestimated_done.actual_minutes = 20;
        unestimated_done.mark_completed(local_noon(today));
        // Completed on a *different* local day than it was scheduled: it
        // wasn't this day's work, so it contributes nothing here.
        let mut stale = todo_with(Some(90), Some(today));
        stale.mark_completed(local_noon(yesterday));

        let cap = measure_capacity(&[overran, unestimated_done, stale], today, 360);
        assert_eq!(cap.done_minutes, 70);
        assert_eq!(cap.planned_minutes, 70);
        assert_eq!(cap.unestimated, 0, "unestimated counts OPEN todos only");
    }

    #[test]
    fn capacity_reports_overcommitment() {
        let today = date("2026-08-12");
        let todos = vec![todo_with(Some(400), Some(today))];
        let cap = measure_capacity(&todos, today, 360);

        assert!(cap.is_overcommitted());
        assert_eq!(cap.over_by(), 40);
        assert_eq!(cap.remaining(), 0);
        assert_eq!(cap.summary(), "6h 40m planned — over by 40m");
    }

    #[test]
    fn capacity_summary_flags_unestimated_work() {
        let today = date("2026-08-12");
        let todos = vec![todo_with(Some(60), Some(today)), todo_with(None, Some(today))];
        let cap = measure_capacity(&todos, today, 360);

        // A day that "fits" while hiding unestimated work is the exact lie the
        // planner exists to prevent.
        assert!(cap.summary().contains("1 unestimated"));
    }

    /// External block on `date` from `from_h`:00 to `to_h`:00 **local** time,
    /// so the local-day filter is exercised the same way the timeline uses it.
    fn ext_block(id: &str, date: NaiveDate, from_h: u32, to_h: u32) -> TimeBlock {
        TimeBlock {
            id: id.into(),
            todo_id: None,
            title: id.into(),
            start: local_noon(date) + chrono::Duration::hours(from_h as i64 - 12),
            end: local_noon(date) + chrono::Duration::hours(to_h as i64 - 12),
            source: BlockSource::External {
                uid: id.into(),
                subscription_id: Some("sub1".into()),
                url: None,
                busy: true,
                tentative: false,
            },
        }
    }

    /// A Hobbes-owned task block (Manual) on `date`, local hours.
    fn task_block(id: &str, date: NaiveDate, from_h: u32, to_h: u32) -> TimeBlock {
        let mut b = ext_block(id, date, from_h, to_h);
        b.todo_id = Some(format!("td_{}", id));
        b.source = BlockSource::Manual;
        b
    }

    #[test]
    fn meetings_add_to_planned_not_subtract_from_capacity() {
        let today = date("2026-08-12");
        let todos = vec![todo_with(Some(240), Some(today))];
        // 9–10 and 14–16: 3h of meetings.
        let blocks = vec![
            ext_block("standup", today, 9, 10),
            ext_block("review", today, 14, 16),
        ];

        let cap = measure_capacity_with_meetings(&todos, &blocks, today, 360, true);
        assert_eq!(cap.capacity_minutes, 360, "capacity stays the full day");
        assert_eq!(
            cap.planned_minutes, 420,
            "4h of tasks + 3h of meetings is the real plan"
        );
        assert!(cap.is_overcommitted(), "7h planned into 6h does not fit");
        assert_eq!(cap.over_by(), 60);

        // Toggle off restores the old todo-only math exactly.
        let off = measure_capacity_with_meetings(&todos, &blocks, today, 360, false);
        assert_eq!(off, measure_capacity(&todos, today, 360));
        assert!(!off.is_overcommitted());
    }

    #[test]
    fn overlapping_meetings_do_not_double_count() {
        let today = date("2026-08-12");
        // 9–11 and 10–12 overlap: merged span is 9–12 = 3h, not 4h.
        // 12–13 touches 11? No — separate; and 13–13 is degenerate (skipped).
        let blocks = vec![
            ext_block("a", today, 9, 11),
            ext_block("b", today, 10, 12),
            ext_block("c", today, 13, 14),
            ext_block("degenerate", today, 15, 15),
        ];
        assert_eq!(meeting_planned_minutes(&blocks, today), 240);

        // Touching edges coalesce without double counting the boundary.
        let touching = vec![ext_block("a", today, 9, 10), ext_block("b", today, 10, 11)];
        assert_eq!(meeting_planned_minutes(&touching, today), 120);
    }

    #[test]
    fn meeting_minutes_ignore_other_days_and_non_external_blocks() {
        let today = date("2026-08-12");
        let other = date("2026-08-13");

        let blocks = vec![
            task_block("manual", today, 15, 17),
            ext_block("tomorrow", other, 9, 17),
            ext_block("mine", today, 9, 10),
        ];
        assert_eq!(meeting_planned_minutes(&blocks, today), 60);
    }

    #[test]
    fn task_blocks_win_meeting_overlaps() {
        let today = date("2026-08-12");
        // Meeting 9–11; a task block 10–12 overlaps its second hour. The
        // todo's estimate already claims 10–11, so only 9–10 adds.
        let blocks = vec![
            ext_block("planning", today, 9, 11),
            task_block("deep", today, 10, 12),
        ];
        assert_eq!(
            meeting_planned_minutes(&blocks, today),
            60,
            "task wins the overlap: only the non-overlapped meeting hour adds"
        );

        // A task block fully covering the meeting zeroes it out...
        let covered = vec![
            ext_block("standup", today, 9, 10),
            task_block("marathon", today, 8, 12),
        ];
        assert_eq!(meeting_planned_minutes(&covered, today), 0);

        // ...and overlapping task blocks are merged first, so a slot covered
        // by two tasks is still only subtracted once.
        let double_task = vec![
            ext_block("sync", today, 9, 12),
            task_block("a", today, 9, 11),
            task_block("b", today, 10, 12),
        ];
        assert_eq!(meeting_planned_minutes(&double_task, today), 0);
    }

    #[test]
    fn non_busy_meetings_do_not_add_to_planned() {
        let today = date("2026-08-12");
        // A 2h focus-time mirror marked free must not cost the day anything.
        let mut focus = ext_block("focus", today, 9, 11);
        if let BlockSource::External { ref mut busy, .. } = focus.source {
            *busy = false;
        }
        let blocks = vec![focus, ext_block("standup", today, 14, 15)];

        assert_eq!(
            meeting_planned_minutes(&blocks, today),
            60,
            "only the busy meeting counts"
        );
        let cap = measure_capacity_with_meetings(&[], &blocks, today, 360, true);
        assert_eq!(cap.planned_minutes, 60, "the 1h busy meeting only");
        assert_eq!(cap.capacity_minutes, 360);
    }

    #[test]
    fn tentative_meetings_do_not_add_to_planned() {
        let today = date("2026-08-12");
        let mut maybe = ext_block("maybe", today, 9, 11);
        if let BlockSource::External {
            ref mut tentative, ..
        } = maybe.source
        {
            *tentative = true;
        }
        let blocks = vec![maybe, ext_block("standup", today, 14, 15)];

        assert_eq!(
            meeting_planned_minutes(&blocks, today),
            60,
            "busy && tentative is still absent from planned"
        );
        let cap = measure_capacity_with_meetings(&[], &blocks, today, 360, true);
        assert_eq!(cap.planned_minutes, 60);
    }

    #[test]
    fn calendar_event_old_json_defaults_to_busy() {
        let json = serde_json::json!({
            "uid": "e1",
            "subscription_id": "s1",
            "title": "Old row",
            "start": "2026-08-12T09:00:00Z",
            "end": "2026-08-12T10:00:00Z"
        });
        let e: CalendarEvent = serde_json::from_value(json).unwrap();
        assert!(e.busy, "cached rows from before the flag must load as busy");
        assert!(
            !e.tentative,
            "cached rows from before the flag must load as firm"
        );
    }

    #[test]
    fn focus_time_title_heuristic_matches_google_exports() {
        assert!(is_focus_time_title("Focus time"));
        assert!(is_focus_time_title("  FOCUS TIME  "));
        assert!(is_focus_time_title("Focus"));
        assert!(is_focus_time_title("Focus time — deep work"));
        assert!(!is_focus_time_title("Focused discussion"));
        assert!(!is_focus_time_title("Team focus review"));
        assert!(!is_focus_time_title("Standup"));
        assert!(!is_focus_time_title(""));
    }

    #[test]
    fn meeting_heavy_day_overflows_planned_not_capacity() {
        let today = date("2026-08-12");
        let blocks = vec![ext_block("offsite", today, 8, 18)]; // 10h
        let cap = measure_capacity_with_meetings(&[], &blocks, today, 360, true);
        assert_eq!(cap.capacity_minutes, 360, "capacity is never shrunk");
        assert_eq!(cap.planned_minutes, 600, "the offsite IS the plan");
        assert!(cap.is_overcommitted());
        assert_eq!(cap.over_by(), 240);
        assert_eq!(cap.remaining(), 0);
    }

    #[test]
    fn external_block_source_old_json_still_deserializes() {
        // Blocks serialized before subscriptions existed carry only the uid.
        let json = serde_json::json!({"kind": "external", "uid": "cal-99"});
        let source: BlockSource = serde_json::from_value(json).unwrap();
        assert_eq!(
            source,
            BlockSource::External {
                uid: "cal-99".into(),
                subscription_id: None,
                url: None,
                busy: true,
                tentative: false,
            },
            "pre-busy rows must load as busy and firm — never as free time"
        );

        // And the new shape round-trips.
        let full = BlockSource::External {
            uid: "u1".into(),
            subscription_id: Some("sub1".into()),
            url: Some("https://cal.example/e/1".into()),
            busy: false,
            tentative: true,
        };
        let round: BlockSource =
            serde_json::from_value(serde_json::to_value(&full).unwrap()).unwrap();
        assert_eq!(round, full);
    }

    #[test]
    fn focus_time_accrues_and_folds() {
        let start: DateTime<Utc> = "2026-08-13T10:00:00Z".parse().unwrap();
        let later: DateTime<Utc> = "2026-08-13T10:25:00Z".parse().unwrap();

        let mut t = Todo::new("deep work", 0.0);
        t.status = TodoStatus::InProgress;
        t.started_at = Some(start);

        // Live elapsed counts the running session.
        assert_eq!(t.elapsed_minutes(later), 25);

        // Pausing banks it and returns to Open.
        t.pause(later);
        assert_eq!(t.status, TodoStatus::Open);
        assert_eq!(t.actual_minutes, 25);
        assert!(t.started_at.is_none());
        assert_eq!(t.elapsed_minutes(later), 25);

        // A second session stacks on top, and completing folds it.
        t.status = TodoStatus::InProgress;
        t.started_at = Some(later);
        let end: DateTime<Utc> = "2026-08-13T10:40:00Z".parse().unwrap();
        t.mark_completed(end);
        assert_eq!(t.actual_minutes, 40);
        assert_eq!(t.status, TodoStatus::Completed);

        // In-progress is active, not closed: it stays in Today and capacity.
        assert!(!TodoStatus::InProgress.is_closed());
    }

    // ── Focus sessions ──────────────────────────────────────────────────────

    /// A closed session for `todo` starting at a *local* hour:minute on
    /// `date`, lasting `mins`. Local-built like the block helpers so the
    /// midnight-split tests hold in any timezone.
    fn closed_session(
        todo: &str,
        actor: FocusActor,
        date: NaiveDate,
        hour: u32,
        minute: u32,
        mins: i64,
    ) -> FocusSession {
        use chrono::TimeZone;
        let start = chrono::Local
            .from_local_datetime(&date.and_hms_opt(hour, minute, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&Utc);
        let mut s = FocusSession::open(todo, start, actor);
        s.close(start + chrono::Duration::minutes(mins), FocusEndReason::Paused);
        s
    }

    fn agent(session: &str) -> FocusActor {
        FocusActor::Agent {
            session_id: Some(session.into()),
        }
    }

    #[test]
    fn focus_session_serde_round_trips_and_defaults() {
        // Round trip with an agent actor and every field set.
        let mut s = closed_session("td_1", agent("sess-9"), date("2026-08-12"), 9, 0, 50);
        s.unclamped_minutes = Some(300);
        let round: FocusSession =
            serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(round, s);
        assert_eq!(round.actor.agent_session_id(), Some("sess-9"));
        assert_eq!(round.end_reason, Some(FocusEndReason::Paused));

        // A minimal row (as an older or hand-written build might store it)
        // defaults to a live person session.
        let json = serde_json::json!({
            "id": "fs_1",
            "todo_id": "td_1",
            "started_at": "2026-08-12T09:00:00Z"
        });
        let s: FocusSession = serde_json::from_value(json).unwrap();
        assert_eq!(s.actor, FocusActor::Person);
        assert!(!s.actor.is_agent());
        assert!(s.is_open());
        assert_eq!(s.minutes, 0);
        assert_eq!(s.end_reason, None);
        assert_eq!(s.unclamped_minutes, None);
    }

    #[test]
    fn session_minutes_split_at_local_midnight() {
        let day = date("2026-08-12");
        let next = date("2026-08-13");
        // 23:00 local for two hours: one hour on each side of midnight.
        let s = closed_session("td_1", FocusActor::Person, day, 23, 0, 120);
        assert_eq!(session_minutes_on(&s, day), 60);
        assert_eq!(session_minutes_on(&s, next), 60);
        assert_eq!(session_minutes_on(&s, date("2026-08-14")), 0);

        // Fully inside one day: all of it lands there.
        let s = closed_session("td_1", FocusActor::Person, day, 9, 0, 45);
        assert_eq!(session_minutes_on(&s, day), 45);
        assert_eq!(session_minutes_on(&s, next), 0);

        // A live session attributes nothing until it closes.
        let open = FocusSession::open("td_1", Utc::now(), FocusActor::Person);
        assert_eq!(session_minutes_on(&open, chrono::Local::now().date_naive()), 0);
    }

    #[test]
    fn per_actor_day_aggregation_separates_person_from_agent() {
        let day = date("2026-08-12");
        let sessions = vec![
            closed_session("td_1", FocusActor::Person, day, 9, 0, 30),
            closed_session("td_2", FocusActor::Person, day, 14, 0, 25),
            closed_session("td_1", agent("sess-1"), day, 10, 0, 40),
            // Another day: excluded from this day's totals.
            closed_session("td_1", FocusActor::Person, date("2026-08-13"), 9, 0, 90),
        ];
        assert_eq!(person_minutes_on(&sessions, day), 55);
        assert_eq!(agent_minutes_on(&sessions, day), 40);
        assert_eq!(person_minutes_on(&sessions, date("2026-08-13")), 90);
        assert_eq!(agent_minutes_on(&sessions, date("2026-08-13")), 0);
    }

    #[test]
    fn recovered_sessions_attribute_only_their_clamped_minutes() {
        let day = date("2026-08-12");
        // A 14-hour abandoned session, clamped to 120 banked minutes: the row
        // keeps its real bounds, but day attribution follows the honest bank —
        // 14 hours of "focus" spreading over the calendar would be the exact
        // lie the clamp exists to prevent.
        let mut s = closed_session("td_1", FocusActor::Person, day, 9, 0, 14 * 60);
        s.end_reason = Some(FocusEndReason::Recovered);
        s.unclamped_minutes = Some(s.minutes);
        s.minutes = 120;
        assert_eq!(person_minutes_on(&[s.clone()], day), 120);
        assert_eq!(person_minutes_on(&[s], date("2026-08-13")), 0);
    }

    #[test]
    fn strategic_gaps_need_two_spans_and_five_minutes() {
        let day = date("2026-08-12");
        let workday = (9 * 60, 17 * 60);

        // Empty day, and a single session: nothing between to measure.
        assert_eq!(strategic_gaps(&[], workday, day), (0, 0));
        let single = vec![closed_session("td_1", FocusActor::Person, day, 9, 0, 60)];
        assert_eq!(strategic_gaps(&single, workday, day), (0, 0));

        // A 4-minute breather is task-switching residue, not strategic time.
        let tight = vec![
            closed_session("td_1", FocusActor::Person, day, 9, 0, 60),
            closed_session("td_2", FocusActor::Person, day, 10, 4, 30),
        ];
        assert_eq!(strategic_gaps(&tight, workday, day), (0, 0));

        // Two real gaps: 10:00→10:30 (30m) and 11:00→12:00 (60m).
        let spaced = vec![
            closed_session("td_1", FocusActor::Person, day, 9, 0, 60),
            closed_session("td_2", FocusActor::Person, day, 10, 30, 30),
            closed_session("td_3", FocusActor::Person, day, 12, 0, 60),
        ];
        assert_eq!(strategic_gaps(&spaced, workday, day), (2, 90));

        // Agent sessions are invisible here: the agent filling a gap does not
        // spend the *person's* strategic time.
        let mut with_agent = spaced.clone();
        with_agent.push(closed_session("td_9", agent("s"), day, 11, 0, 60));
        assert_eq!(strategic_gaps(&with_agent, workday, day), (2, 90));
    }

    #[test]
    fn strategic_gaps_clip_to_the_workday_window() {
        let day = date("2026-08-12");
        let workday = (9 * 60, 17 * 60);

        // An early sitting entirely before the workday is clipped out: the
        // remaining single span has no between-session gap.
        let early = vec![
            closed_session("td_1", FocusActor::Person, day, 7, 0, 60),
            closed_session("td_2", FocusActor::Person, day, 10, 0, 60),
        ];
        assert_eq!(strategic_gaps(&early, workday, day), (0, 0));

        // Sessions straddling the edges are clipped to them: 8:00–9:30 and
        // 16:30–18:00 leave exactly the 9:30→16:30 gap (7h), never counting
        // minutes outside the window.
        let straddle = vec![
            closed_session("td_1", FocusActor::Person, day, 8, 0, 90),
            closed_session("td_2", FocusActor::Person, day, 16, 30, 90),
        ];
        assert_eq!(strategic_gaps(&straddle, workday, day), (1, 7 * 60));

        // Overlapping sittings merge before gap math — no phantom gaps.
        let overlap = vec![
            closed_session("td_1", FocusActor::Person, day, 9, 0, 90),
            closed_session("td_2", FocusActor::Person, day, 10, 0, 60),
            closed_session("td_3", FocusActor::Person, day, 12, 0, 30),
        ];
        assert_eq!(strategic_gaps(&overlap, workday, day), (1, 60));

        // A degenerate window measures nothing.
        assert_eq!(strategic_gaps(&overlap, (600, 600), day), (0, 0));
    }

    #[test]
    fn capacity_prefers_session_derived_person_minutes_over_actuals() {
        let today = date("2026-08-12");
        // Done with session rows: 30m person + 40m agent. actual_minutes (70)
        // holds the destructive total; the person share must win, and the
        // agent share must not masquerade as the person's.
        let mut tracked = todo_with(None, Some(today));
        tracked.id = "td_tracked".into();
        tracked.actual_minutes = 70;
        tracked.mark_completed(local_noon(today));
        // Pre-migration done todo: no rows, actual_minutes is all we know.
        let mut legacy = todo_with(None, Some(today));
        legacy.id = "td_legacy".into();
        legacy.actual_minutes = 20;
        legacy.mark_completed(local_noon(today));

        let sessions = vec![
            closed_session("td_tracked", FocusActor::Person, today, 9, 0, 30),
            closed_session("td_tracked", agent("s1"), today, 10, 0, 40),
        ];

        let cap = measure_capacity_with_sessions(
            &[tracked.clone(), legacy.clone()],
            &sessions,
            today,
            360,
        );
        assert_eq!(
            cap.done_minutes, 50,
            "session-derived 30m (never the 70m aggregate, never the agent's 40m) + legacy 20m"
        );
        assert_eq!(cap.agent_minutes, 40);
        // The agent lane never consumes the person's day.
        assert_eq!(cap.planned_minutes, 50);
        assert_eq!(cap.remaining(), 310);
        assert!(!cap.is_overcommitted());

        // With no session rows at all this is exactly the old math.
        let old = measure_capacity(&[tracked, legacy], today, 360);
        assert_eq!(old.done_minutes, 90);
        assert_eq!(old.agent_minutes, 0);
    }

    #[test]
    fn capacity_summary_reports_agent_minutes_when_nonzero() {
        let cap = Capacity {
            planned_minutes: 90,
            done_minutes: 60,
            capacity_minutes: 360,
            unestimated: 0,
            agent_minutes: 45,
        };
        assert_eq!(
            cap.summary(),
            "1h 30m planned · 1h done · 45m agent · 4h 30m free"
        );
        // Zero agent minutes stay silent — the free tier and untouched days
        // keep today's wording exactly.
        let quiet = Capacity {
            agent_minutes: 0,
            ..cap
        };
        assert!(!quiet.summary().contains("agent"));
    }

    #[test]
    fn sort_between_splits_neighbours() {
        assert_eq!(sort_between(None, None), 0.0);
        assert_eq!(sort_between(Some(100.0), None), 100.0 + SORT_STEP);
        assert_eq!(sort_between(None, Some(100.0)), 100.0 - SORT_STEP);
        assert_eq!(sort_between(Some(0.0), Some(100.0)), 50.0);
    }

    #[test]
    fn renormalise_when_neighbours_collapse() {
        assert!(!needs_renormalise(Some(0.0), Some(1.0)));
        assert!(needs_renormalise(Some(1.0), Some(1.0 + 1e-9)));
        // Open-ended sides can always be extended.
        assert!(!needs_renormalise(None, Some(1.0)));

        let mut todos = vec![Todo::new("a", 5.0), Todo::new("b", 5.000001)];
        renormalise(&mut todos);
        assert_eq!(todos[0].sort_order, 0.0);
        assert_eq!(todos[1].sort_order, SORT_STEP);
    }

    #[test]
    fn blocks_touching_edges_do_not_overlap() {
        let base = "2026-08-12T09:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let block = |from_h: i64, to_h: i64| TimeBlock {
            id: "b".into(),
            todo_id: None,
            title: "b".into(),
            start: base + chrono::Duration::hours(from_h),
            end: base + chrono::Duration::hours(to_h),
            source: BlockSource::Manual,
        };

        assert!(!block(0, 1).overlaps(&block(1, 2)));
        assert!(block(0, 2).overlaps(&block(1, 3)));
        assert_eq!(block(0, 2).duration_minutes(), 120);
    }

    #[test]
    fn todo_roundtrips_with_missing_optional_fields() {
        // Rows written by an older build must still load.
        let json = serde_json::json!({
            "id": "td_1",
            "title": "Draft the proposal",
            "created_at": "2026-08-12T09:00:00Z",
            "updated_at": "2026-08-12T09:00:00Z"
        });
        let t: Todo = serde_json::from_value(json).unwrap();
        assert_eq!(t.status, TodoStatus::Open);
        assert_eq!(t.bucket, TodoBucket::Inbox);
        assert_eq!(t.origin, TodoOrigin::User);
        assert!(t.tags.is_empty());
        assert!(t.scheduled_for.is_none());
    }

    #[test]
    fn summary_is_line_oriented() {
        let mut t = Todo::new("Draft the proposal", 0.0);
        t.id = "td_1a2b".into();
        t.estimate_minutes = Some(45);
        t.scheduled_for = Some(date("2026-08-12"));
        t.tags = vec!["writing".into()];

        assert_eq!(
            t.summary(),
            "[td_1a2b] ○ Draft the proposal — 45m, 2026-08-12, #writing"
        );

        let bare = Todo::new("Bare", 0.0);
        assert!(bare.summary().ends_with("○ Bare"));
    }

    #[test]
    fn summary_reports_actuals_only_for_closed_work() {
        let done_at: DateTime<Utc> = "2026-08-13T10:52:00Z".parse().unwrap();

        let mut t = Todo::new("Draft", 0.0);
        t.estimate_minutes = Some(60);
        t.actual_minutes = 52;
        // Open work never shows actuals — the focus bar owns the live readout.
        assert!(!t.summary().contains("took"));

        t.mark_completed(done_at);
        assert!(t.summary().ends_with("(took 52m of 1h)"), "{}", t.summary());

        // Unestimated closed work reports the bare actual.
        let mut bare = Todo::new("Email", 0.0);
        bare.actual_minutes = 20;
        bare.mark_completed(done_at);
        assert!(bare.summary().ends_with("(took 20m)"), "{}", bare.summary());

        // Closed without any tracked time stays clean.
        let mut untracked = Todo::new("Call", 0.0);
        untracked.mark_completed(done_at);
        assert!(!untracked.summary().contains("took"));
    }
}
