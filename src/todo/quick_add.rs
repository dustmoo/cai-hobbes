//! Quick-add token grammar.
//!
//! A bounded, deliberate grammar — not natural-language parsing (the design doc
//! rules that out as a scope trap). Tokens can sit anywhere in the input and
//! are stripped from the title; anything that doesn't parse stays in the title,
//! so "email @john about #q3" only consumes the tag.
//!
//! | Token | Meaning |
//! |-------|---------|
//! | `~30m` `~1h30m` `~90` | estimate |
//! | `#tag`                | tag (repeatable) |
//! | `@2pm` `@14:30` `@9`  | place on the timeline at that local time |
//! | `@morning` `@afternoon` `@evening` | time of day group |
//! | `*fri` `*tomorrow` `*today` | scheduled day (same day words as `!`) |
//! | `!fri` `!tomorrow` `!today` | deadline (weekday = next occurrence, today counts) |
//! | `!!1`…`!!4` `!!p1`…`!!p4` `!!do` `!!schedule` `!!delegate` `!!drop` | Eisenhower priority (Q1–Q4) |
//! | `+hobbes` `+hobbes-rc` `+rc` | project (slug / word prefix; must resolve) |
//! | `@me`                 | human lane (explicit) |
//! | `@hobbes` `@<worker>` | agent lane; the rest of the text is the outcome |
//! | `!chore`              | human chore |
//! | `!mine`               | human lane (explicit) |
//!
//! `@` is shared with the clock and time-of-day tokens: a word after `@`
//! that parses as a time (`@2pm`, `@9`, `@14:30`, `@evening`) is ALWAYS a
//! time, checked first; only then `me`, `hobbes`, and the known worker names
//! (slug match, then a unique slug prefix of 3+ characters). An `@word` that
//! is none of these stays in the title ("email @john").
//!
//! Lane rules: an agent marker wins over `!chore`/`!mine`/`@me` (the kind is
//! dropped — kinds belong to human to-dos); the last lane marker otherwise
//! wins. With no lane marker at all, [`suggest_lane`] may offer a
//! [`LaneSuggestion`] — the UI only *suggests*, it never applies one.
//!
//! Shared by the quick-add input; the AI goes through its structured tools and
//! never needs this.

use chrono::{Datelike, NaiveDate, Weekday};

use super::model::{HumanKind, Lane, Priority, TimeOfDay};
use super::project_match::{self, MatchTier, ProjectIndex, ProjectResolution};

/// A `+token` that resolved to an open project.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectPick {
    pub id: String,
    pub title: String,
    pub tier: MatchTier,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuickAddParse {
    pub title: String,
    pub estimate_minutes: Option<u32>,
    pub tags: Vec<String>,
    pub deadline: Option<NaiveDate>,
    /// Scheduled day (`*fri`): overrides the view-context default date.
    pub scheduled: Option<NaiveDate>,
    pub time_of_day: Option<TimeOfDay>,
    /// Local minutes since midnight where the todo should be time-blocked.
    pub block_start: Option<u32>,
    /// Eisenhower quadrant (`!!2`). `!` alone already means deadline, so
    /// priority doubles the sigil rather than claiming a new one.
    pub priority: Option<Priority>,
    /// A `+token` that resolved (last one wins). Consumed from the title.
    pub project: Option<ProjectPick>,
    /// The last `+token` that did NOT resolve. It stays in the title; kept
    /// here only so the preview can show the amber "stays in title" pill.
    pub project_miss: Option<ProjectResolution>,
    /// An explicit lane marker (`@me`/`!mine` → Human, `@hobbes`/`@worker`
    /// → Agent). `None` = no marker (the to-do defaults to the human lane).
    pub lane: Option<Lane>,
    /// `!chore` → `Chore`. Dropped when an agent marker is present.
    pub kind: Option<HumanKind>,
    /// The worker an agent marker named: `"Hobbes"` for `@hobbes`, else the
    /// matched worker name as given in the `workers` list.
    pub worker: Option<String>,
    /// Agent lane only: the outcome — the title text with tokens stripped.
    pub outcome: Option<String>,
    /// With no lane marker: a suggested lane when the text reads like an
    /// outcome (or clearly needs the person). Never applied by the parser.
    pub lane_suggestion: Option<LaneSuggestion>,
}

/// A suggested lane for unmarked input. The UI offers it ("Hand to
/// Hobbes?" / "Keep it yours?"); the person decides.
#[derive(Debug, Clone, PartialEq)]
pub struct LaneSuggestion {
    pub lane: Lane,
    /// For a human suggestion: the kind the verb implies.
    pub kind: Option<HumanKind>,
    /// Why, in a few words, for the hint line.
    pub reason: &'static str,
}

impl QuickAddParse {
    /// Whether any token was recognised (drives the chip preview row).
    pub fn has_tokens(&self) -> bool {
        self.estimate_minutes.is_some()
            || !self.tags.is_empty()
            || self.deadline.is_some()
            || self.scheduled.is_some()
            || self.time_of_day.is_some()
            || self.block_start.is_some()
            || self.priority.is_some()
            || self.project.is_some()
            || self.project_miss.is_some()
            || self.lane.is_some()
            || self.kind.is_some()
    }
}

// ── Lane suggestion heuristic ───────────────────────────────────────────────
//
// Deliberately small and predictable — word lists, not NLP. Checked in
// order, first match wins:
//
// 1. An outcome phrase anywhere ("tests passing", "ready for", "so that",
//    "pr open"…) → agent: the text states a definition of done.
// 2. The first word is a delegable work verb (research, draft, summarize,
//    compare, fix, refactor, benchmark…) → agent.
// 3. The first word needs the person there (call, meet, attend… →
//    commitment; pay, buy, renew, pick… → chore; play, watch, walk, hike…
//    → your time) → human, with that kind.
// 4. Otherwise no suggestion.

const OUTCOME_PHRASES: &[&str] = &[
    "tests passing", "tests pass", "ready for", "ready by", "so that", "pr open",
    "open a pr", "pull request", "with a summary", "end to end", "definition of done",
];

const AGENT_VERBS: &[&str] = &[
    "research", "investigate", "summarize", "summarise", "compare", "draft", "compile",
    "analyze", "analyse", "benchmark", "refactor", "implement", "fix", "debug", "migrate",
    "triage", "audit", "prototype", "document", "outline", "collect", "gather", "scrape",
    "transcribe", "translate", "ship", "build", "test", "find", "update",
];

const COMMITMENT_VERBS: &[&str] = &[
    "call", "meet", "attend", "visit", "interview", "present", "talk", "discuss", "lunch",
    "dinner", "coffee",
];

const CHORE_VERBS: &[&str] = &[
    "pay", "buy", "pick", "renew", "sign", "return", "book", "order", "file", "drop",
    "collect", "clean", "wash", "mail",
];

const YOUR_TIME_VERBS: &[&str] = &[
    "play", "watch", "read", "walk", "hike", "run", "game", "garden", "paint", "bike", "swim",
];

/// Suggest a lane for unmarked capture text (see the rules above).
pub fn suggest_lane(title: &str) -> Option<LaneSuggestion> {
    let lower = title.trim().to_lowercase();
    if lower.is_empty() {
        return None;
    }
    if OUTCOME_PHRASES.iter().any(|p| lower.contains(p)) {
        return Some(LaneSuggestion {
            lane: Lane::Agent,
            kind: None,
            reason: "reads like an outcome",
        });
    }
    let first = lower
        .split_whitespace()
        .next()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .unwrap_or("");
    // Chore verbs are checked before agent verbs so "collect the dry
    // cleaning" stays the person's errand.
    if CHORE_VERBS.contains(&first) {
        return Some(LaneSuggestion {
            lane: Lane::Human,
            kind: Some(HumanKind::Chore),
            reason: "a quick errand",
        });
    }
    if AGENT_VERBS.contains(&first) {
        return Some(LaneSuggestion {
            lane: Lane::Agent,
            kind: None,
            reason: "work an agent can take on",
        });
    }
    if COMMITMENT_VERBS.contains(&first) {
        return Some(LaneSuggestion {
            lane: Lane::Human,
            kind: Some(HumanKind::Commitment),
            reason: "needs you there",
        });
    }
    if YOUR_TIME_VERBS.contains(&first) {
        return Some(LaneSuggestion {
            lane: Lane::Human,
            kind: Some(HumanKind::YourTime),
            reason: "your time",
        });
    }
    None
}

/// Resolve an `@word` against the known workers: slug equality first, then a
/// unique slug prefix of at least 3 characters. Returns the name as given.
fn match_worker<'a>(word: &str, workers: &'a [String]) -> Option<&'a str> {
    let w = project_match::slug(word);
    if w.is_empty() {
        return None;
    }
    if let Some(exact) = workers.iter().find(|n| project_match::slug(n) == w) {
        return Some(exact.as_str());
    }
    if w.chars().count() < 3 {
        return None;
    }
    let mut hits = workers.iter().filter(|n| project_match::slug(n).starts_with(&w));
    match (hits.next(), hits.next()) {
        (Some(one), None) => Some(one.as_str()),
        _ => None,
    }
}

/// Unchanged signature: an empty index means `+` never consumes, so every
/// caller that doesn't know about projects behaves exactly as before.
/// (The UI now parses through `parse_quick_add_with`; this is the stable
/// project-less entry point the tests pin.)
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_quick_add(input: &str, today: NaiveDate) -> QuickAddParse {
    parse_quick_add_with(input, today, &ProjectIndex::empty())
}

/// `+word` whose first character could start a project token. Digit-led
/// words (`+1`, `+15%`) are plain text.
fn project_query(word: &str) -> Option<&str> {
    let rest = word.strip_prefix('+')?;
    rest.chars()
        .next()
        .filter(|c| c.is_alphanumeric() && !c.is_ascii_digit())
        .map(|_| rest)
}

pub fn parse_quick_add_with(input: &str, today: NaiveDate, projects: &ProjectIndex) -> QuickAddParse {
    parse_capture(input, today, projects, &[])
}

/// The full capture grammar: [`parse_quick_add_with`] plus `@worker` names.
/// `workers` are the names an `@word` may resolve to (live fleet sessions,
/// chat tab names…); `@hobbes` and `@me` work without it.
pub fn parse_capture(
    input: &str,
    today: NaiveDate,
    projects: &ProjectIndex,
    workers: &[String],
) -> QuickAddParse {
    let mut out = QuickAddParse::default();
    let mut title_words: Vec<&str> = Vec::new();
    let mut agent_marked = false;

    for word in input.split_whitespace() {
        // Split after the first CHARACTER, not the first byte — a multibyte
        // leading char (é, 🎉, CJK) makes byte index 1 a non-boundary and
        // split_at would panic mid-keystroke.
        let sigil_len = word.chars().next().map_or(0, char::len_utf8);
        let consumed = match word.split_at(sigil_len) {
            ("~", rest) => match parse_duration_minutes(rest) {
                Some(m) => {
                    out.estimate_minutes = Some(m);
                    true
                }
                None => false,
            },
            ("#", rest) => {
                if !rest.is_empty()
                    && rest.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                {
                    let tag = rest.to_string();
                    if !out.tags.contains(&tag) {
                        out.tags.push(tag);
                    }
                    true
                } else {
                    false
                }
            }
            // Times first: `@2pm` must never read as a worker.
            ("@", rest) => match parse_time_word(rest) {
                Some(TimeToken::TimeOfDay(t)) => {
                    out.time_of_day = Some(t);
                    true
                }
                Some(TimeToken::Clock(minutes)) => {
                    out.block_start = Some(minutes);
                    true
                }
                None => match rest.to_ascii_lowercase().as_str() {
                    "me" => {
                        if !agent_marked {
                            out.lane = Some(Lane::Human);
                        }
                        true
                    }
                    "hobbes" => {
                        agent_marked = true;
                        out.lane = Some(Lane::Agent);
                        out.worker = Some("Hobbes".to_string());
                        true
                    }
                    _ => match match_worker(rest, workers) {
                        Some(name) => {
                            agent_marked = true;
                            out.lane = Some(Lane::Agent);
                            out.worker = Some(name.to_string());
                            true
                        }
                        None => false,
                    },
                },
            },
            ("!", rest) if rest.eq_ignore_ascii_case("chore") || rest.eq_ignore_ascii_case("chores") => {
                out.kind = Some(HumanKind::Chore);
                if !agent_marked {
                    out.lane = Some(Lane::Human);
                }
                true
            }
            ("!", rest) if rest.eq_ignore_ascii_case("mine") => {
                if !agent_marked {
                    out.lane = Some(Lane::Human);
                }
                true
            }
            // Checked before the single-`!` deadline arm, which would
            // otherwise see "!2" and leave the word in the title.
            ("!", rest) if rest.starts_with('!') => match Priority::parse(&rest[1..]) {
                Some(p) => {
                    out.priority = Some(p);
                    true
                }
                None => false,
            },
            ("!", rest) => match parse_day_word(rest, today) {
                Some(d) => {
                    out.deadline = Some(d);
                    true
                }
                None => false,
            },
            // Same day-word grammar as `!`, but for the *scheduled* day —
            // mirroring the deadline token so Upcoming stops misleading.
            ("*", rest) => match parse_day_word(rest, today) {
                Some(d) => {
                    out.scheduled = Some(d);
                    true
                }
                None => false,
            },
            // `+project`: consumed only when it resolves. A miss stays in the
            // title (and flags the amber preview pill); an empty index means
            // the caller doesn't know about projects, so no pill either.
            ("+", _) if !projects.is_empty() => match project_query(word) {
                Some(q) => match projects.resolve(q) {
                    ProjectResolution::Resolved { id, title, tier } => {
                        out.project = Some(ProjectPick { id, title, tier });
                        true
                    }
                    miss => {
                        out.project_miss = Some(miss);
                        false
                    }
                },
                None => false,
            },
            _ => false,
        };
        if !consumed {
            title_words.push(word);
        }
    }

    out.title = title_words.join(" ");
    if agent_marked {
        // Kinds belong to human to-dos; the rest of the text is the outcome.
        out.kind = None;
        out.outcome = Some(out.title.clone()).filter(|t| !t.is_empty());
    } else if out.lane.is_none() {
        out.lane_suggestion = suggest_lane(&out.title);
    }
    out
}

/// The `+word` being typed at the end of the input, if any: drives the
/// autocomplete list. v1 treats the caret as being at the end, so a trailing
/// space closes it. `"x +ho"` → `Some("ho")`; a bare `"x +"` → `Some("")`.
pub fn active_project_query(input: &str) -> Option<&str> {
    if input.ends_with(char::is_whitespace) {
        return None;
    }
    let last = input.split_whitespace().last()?;
    if last == "+" {
        return Some("");
    }
    project_query(last)
}

/// Replace the last `+project` word with `+{slug}` (appending one if there's
/// none), or remove every `+project` word when `slug` is `None`. The pill and
/// the list both edit the text through this, so the text stays the source of
/// truth. A bare trailing `+` counts as the word being replaced.
pub fn set_project_token(input: &str, slug: Option<&str>) -> String {
    let mut words: Vec<&str> = input.split_whitespace().collect();
    let is_token = |w: &str| w == "+" || project_query(w).is_some();
    let token = slug.map(|s| format!("+{}", project_match::slug(s)));
    match token {
        None => words.retain(|w| !is_token(w)),
        Some(ref t) => match words.iter().rposition(|w| is_token(w)) {
            Some(i) => words[i] = t,
            None => words.push(t),
        },
    }
    words.join(" ")
}

/// `"30m"`, `"1h30m"`, `"2h"`, or bare `"90"` (minutes). Rejects zero.
/// Also the grammar of the detail card's estimate field — one duration
/// syntax everywhere.
pub(crate) fn parse_duration_minutes(s: &str) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<u32>() {
        return (n > 0).then_some(n);
    }
    let mut total: u32 = 0;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let per_unit = match c.to_ascii_lowercase() {
            'h' => 60,
            'm' => 1,
            _ => return None,
        };
        if num.is_empty() {
            return None;
        }
        total = total.checked_add(num.parse::<u32>().ok()?.checked_mul(per_unit)?)?;
        num.clear();
    }
    // Trailing digits without a unit ("1h30") are ambiguous — reject.
    if !num.is_empty() || total == 0 {
        return None;
    }
    Some(total)
}

enum TimeToken {
    TimeOfDay(TimeOfDay),
    /// Local minutes since midnight.
    Clock(u32),
}

fn parse_time_word(s: &str) -> Option<TimeToken> {
    match s.to_ascii_lowercase().as_str() {
        "morning" => return Some(TimeToken::TimeOfDay(TimeOfDay::Morning)),
        "afternoon" => return Some(TimeToken::TimeOfDay(TimeOfDay::Afternoon)),
        "evening" | "eve" | "tonight" => return Some(TimeToken::TimeOfDay(TimeOfDay::Evening)),
        _ => {}
    }
    parse_clock(s).map(TimeToken::Clock)
}

/// `"9"` → 09:00, `"9:30"`, `"14:30"`, `"9am"`, `"2pm"`, `"2:15pm"`,
/// `"12am"` → 00:00, `"12pm"` → 12:00.
fn parse_clock(s: &str) -> Option<u32> {
    let lower = s.to_ascii_lowercase();
    let (body, meridiem) = if let Some(b) = lower.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = lower.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (lower.as_str(), None)
    };

    let (h_str, m_str) = match body.split_once(':') {
        Some((h, m)) => (h, m),
        None => (body, "0"),
    };
    let hour: u32 = h_str.parse().ok()?;
    let minute: u32 = m_str.parse().ok()?;
    if minute > 59 {
        return None;
    }

    let hour = match meridiem {
        None => {
            if hour > 23 {
                return None;
            }
            hour
        }
        Some(pm) => {
            if hour == 0 || hour > 12 {
                return None;
            }
            match (pm, hour) {
                (false, 12) => 0,  // 12am
                (false, h) => h,   // 9am
                (true, 12) => 12,  // 12pm
                (true, h) => h + 12,
            }
        }
    };
    Some(hour * 60 + minute)
}

/// `today`/`tomorrow`, or a weekday name → the next occurrence, where today
/// itself counts ("!fri" on a Friday means today, matching Todoist).
fn parse_day_word(s: &str, today: NaiveDate) -> Option<NaiveDate> {
    let lower = s.to_ascii_lowercase();
    match lower.as_str() {
        "today" | "tod" => return Some(today),
        "tomorrow" | "tom" | "tmrw" => return today.succ_opt(),
        _ => {}
    }
    let target = match lower.as_str() {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    };
    let ahead = (target.num_days_from_monday() as i64
        - today.weekday().num_days_from_monday() as i64)
        .rem_euclid(7);
    today.checked_add_days(chrono::Days::new(ahead as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-08-12 is a Wednesday.
    fn today() -> NaiveDate {
        "2026-08-12".parse().unwrap()
    }

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn plain_titles_pass_through_untouched() {
        let p = parse_quick_add("Review the quarterly report", today());
        assert_eq!(p.title, "Review the quarterly report");
        assert!(!p.has_tokens());
    }

    #[test]
    fn multibyte_leading_chars_do_not_panic() {
        // Regression: split_at(1) panicked on a non-ASCII first char, crashing
        // the app on every keystroke of words like these.
        let p = parse_quick_add("über Docs émail 🎉party 日本語 привет", today());
        assert_eq!(p.title, "über Docs émail 🎉party 日本語 привет");
        assert!(!p.has_tokens());
    }

    #[test]
    fn double_bang_sets_the_eisenhower_quadrant() {
        let p = parse_quick_add("Call the bank !!1 !fri", today());
        assert_eq!(p.title, "Call the bank");
        assert_eq!(p.priority, Some(Priority::UrgentImportant));
        assert_eq!(p.deadline, Some(date("2026-08-14")), "!fri is still a deadline");
        assert!(p.has_tokens());

        assert_eq!(parse_quick_add("x !!schedule", today()).priority, Some(Priority::Important));
        assert_eq!(parse_quick_add("x !!Q3", today()).priority, Some(Priority::Urgent));
        assert_eq!(parse_quick_add("x !!drop", today()).priority, Some(Priority::Neither));

        // Unknown quadrants and a bare "!!" stay in the title.
        let p = parse_quick_add("wow !! really !!9", today());
        assert_eq!(p.title, "wow !! really !!9");
        assert_eq!(p.priority, None);
    }

    #[test]
    fn the_screenshot_case_parses() {
        // The exact input that motivated this: "Review Emails ~30m".
        let p = parse_quick_add("Review Emails ~30m", today());
        assert_eq!(p.title, "Review Emails");
        assert_eq!(p.estimate_minutes, Some(30));
    }

    #[test]
    fn estimates_accept_the_common_shapes() {
        assert_eq!(parse_quick_add("x ~90", today()).estimate_minutes, Some(90));
        assert_eq!(parse_quick_add("x ~1h30m", today()).estimate_minutes, Some(90));
        assert_eq!(parse_quick_add("x ~2h", today()).estimate_minutes, Some(120));
        // Ambiguous trailing digits stay in the title.
        let p = parse_quick_add("x ~1h30", today());
        assert_eq!(p.estimate_minutes, None);
        assert_eq!(p.title, "x ~1h30");
    }

    #[test]
    fn tags_accumulate_and_dedupe() {
        let p = parse_quick_add("write #q3 report #writing #q3", today());
        assert_eq!(p.title, "write report");
        assert_eq!(p.tags, vec!["q3", "writing"]);
    }

    #[test]
    fn clock_times_place_on_the_timeline() {
        assert_eq!(parse_quick_add("x @2pm", today()).block_start, Some(14 * 60));
        assert_eq!(parse_quick_add("x @14:30", today()).block_start, Some(14 * 60 + 30));
        assert_eq!(parse_quick_add("x @9", today()).block_start, Some(9 * 60));
        assert_eq!(parse_quick_add("x @2:15pm", today()).block_start, Some(14 * 60 + 15));
        assert_eq!(parse_quick_add("x @12am", today()).block_start, Some(0));
        assert_eq!(parse_quick_add("x @12pm", today()).block_start, Some(12 * 60));
    }

    #[test]
    fn time_of_day_words_set_the_group() {
        assert_eq!(
            parse_quick_add("read @evening", today()).time_of_day,
            Some(TimeOfDay::Evening)
        );
        assert_eq!(
            parse_quick_add("gym @morning", today()).time_of_day,
            Some(TimeOfDay::Morning)
        );
    }

    #[test]
    fn deadlines_resolve_weekdays_with_today_counting() {
        // Today is Wednesday.
        assert_eq!(parse_quick_add("x !fri", today()).deadline, Some(date("2026-08-14")));
        assert_eq!(parse_quick_add("x !wed", today()).deadline, Some(date("2026-08-12")));
        assert_eq!(parse_quick_add("x !mon", today()).deadline, Some(date("2026-08-17")));
        assert_eq!(parse_quick_add("x !tomorrow", today()).deadline, Some(date("2026-08-13")));
    }

    #[test]
    fn scheduled_token_resolves_day_words_like_the_deadline_token() {
        // Today is Wednesday.
        assert_eq!(parse_quick_add("x *fri", today()).scheduled, Some(date("2026-08-14")));
        assert_eq!(parse_quick_add("x *tomorrow", today()).scheduled, Some(date("2026-08-13")));
        assert_eq!(parse_quick_add("x *today", today()).scheduled, Some(date("2026-08-12")));
        // Scheduled and deadline are independent axes.
        let p = parse_quick_add("x *mon !fri", today());
        assert_eq!(p.scheduled, Some(date("2026-08-17")));
        assert_eq!(p.deadline, Some(date("2026-08-14")));
        assert!(p.has_tokens());
    }

    #[test]
    fn scheduled_token_composes_with_a_clock() {
        // The submit path anchors the @clock block on the *day.
        let p = parse_quick_add("standup @9 *mon", today());
        assert_eq!(p.title, "standup");
        assert_eq!(p.block_start, Some(9 * 60));
        assert_eq!(p.scheduled, Some(date("2026-08-17")));
    }

    #[test]
    fn unrecognised_tokens_stay_in_the_title() {
        let p = parse_quick_add("email @john about #q3 !soon *soon ~later", today());
        assert_eq!(p.title, "email @john about !soon *soon ~later");
        assert_eq!(p.tags, vec!["q3"]);
        assert_eq!(p.deadline, None);
        assert_eq!(p.scheduled, None);
        assert_eq!(p.estimate_minutes, None);
        assert_eq!(p.block_start, None);
    }

    #[test]
    fn a_fully_tokenised_line_composes() {
        let p = parse_quick_add("Draft the proposal ~45m #writing @2pm !fri", today());
        assert_eq!(p.title, "Draft the proposal");
        assert_eq!(p.estimate_minutes, Some(45));
        assert_eq!(p.tags, vec!["writing"]);
        assert_eq!(p.block_start, Some(14 * 60));
        assert_eq!(p.deadline, Some(date("2026-08-14")));
        assert!(p.has_tokens());
    }

    #[test]
    fn tokens_only_leaves_an_empty_title() {
        let p = parse_quick_add("~30m #tag", today());
        assert!(p.title.is_empty());
        assert!(p.has_tokens());
    }

    #[test]
    fn clock_rejects_nonsense() {
        assert_eq!(parse_quick_add("x @25", today()).block_start, None);
        assert_eq!(parse_quick_add("x @9:75", today()).block_start, None);
        assert_eq!(parse_quick_add("x @13pm", today()).block_start, None);
        assert_eq!(parse_quick_add("x @0pm", today()).block_start, None);
    }

    fn projects() -> ProjectIndex {
        use crate::todo::model::Project;
        let p = |id: &str, title: &str, sort_order: f64| Project {
            id: id.into(),
            title: title.into(),
            notes: String::new(),
            area_id: None,
            status: Default::default(),
            deadline: None,
            sort_order,
            path: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let state = crate::todo::PlannerState {
            projects: vec![p("rc", "Hobbes RC", 1.0), p("reno", "Home renovation", 2.0)],
            ..Default::default()
        };
        ProjectIndex::from_state(&state)
    }

    #[test]
    fn plus_token_is_consumed_when_it_resolves() {
        let p = parse_quick_add_with("Ship notes +hobbes-rc ~30m", today(), &projects());
        assert_eq!(p.title, "Ship notes");
        assert_eq!(p.project.as_ref().map(|p| p.id.as_str()), Some("rc"));
        assert_eq!(p.estimate_minutes, Some(30));
        assert!(p.project_miss.is_none());
    }

    #[test]
    fn unresolved_plus_stays_in_title() {
        for input in ["+1 on the proposal", "C++ refactor", "budget +15%"] {
            let p = parse_quick_add_with(input, today(), &projects());
            assert_eq!(p.title, input);
            assert_eq!(p.project, None);
            assert_eq!(p.project_miss, None, "digit-led / non-token: no amber noise");
        }
        let p = parse_quick_add_with("buy +garden-shed", today(), &projects());
        assert_eq!(p.title, "buy +garden-shed");
        assert!(matches!(p.project_miss, Some(ProjectResolution::NoMatch { .. })));
        assert!(p.has_tokens(), "the amber pill needs the chip row");
    }

    #[test]
    fn empty_index_never_consumes_plus() {
        let p = parse_quick_add("Ship +hobbes", today());
        assert_eq!(p.title, "Ship +hobbes");
        assert_eq!(p.project, None);
        assert_eq!(p.project_miss, None);
        assert!(!p.has_tokens());
    }

    #[test]
    fn last_resolving_plus_wins_and_all_resolving_are_consumed() {
        let p = parse_quick_add_with("tiles +rc +reno", today(), &projects());
        assert_eq!(p.title, "tiles");
        assert_eq!(p.project.map(|p| p.id), Some("reno".into()));
    }

    #[test]
    fn bare_plus_stays_in_title() {
        let p = parse_quick_add_with("a + b", today(), &projects());
        assert_eq!(p.title, "a + b");
        assert!(!p.has_tokens());
    }

    #[test]
    fn set_project_token_replaces_appends_and_removes() {
        assert_eq!(set_project_token("x +ho", Some("Hobbes RC")), "x +hobbes-rc");
        assert_eq!(set_project_token("x +", Some("hobbes-rc")), "x +hobbes-rc");
        assert_eq!(set_project_token("x ~30m", Some("reno")), "x ~30m +reno");
        assert_eq!(set_project_token("x +rc ~30m", None), "x ~30m");
        assert_eq!(set_project_token("+1 idea +rc", None), "+1 idea", "+1 is text");
    }

    fn workers() -> Vec<String> {
        vec!["clearmirror-site".into(), "Scott Marsh Phase 2".into(), "puget-app-packs".into(), "puget-bench".into()]
    }

    fn cap(input: &str) -> QuickAddParse {
        parse_capture(input, today(), &ProjectIndex::empty(), &workers())
    }

    #[test]
    fn at_hobbes_routes_to_the_agent_lane_with_the_rest_as_outcome() {
        let p = cap("@hobbes ship the Docker app-packs release with tests passing ~2h");
        assert_eq!(p.lane, Some(Lane::Agent));
        assert_eq!(p.worker.as_deref(), Some("Hobbes"));
        assert_eq!(p.title, "ship the Docker app-packs release with tests passing");
        assert_eq!(p.outcome.as_deref(), Some("ship the Docker app-packs release with tests passing"));
        assert_eq!(p.estimate_minutes, Some(120));
        assert!(p.lane_suggestion.is_none(), "an explicit marker never gets a suggestion");
        assert!(p.has_tokens());
        // Case-insensitive, any position.
        assert_eq!(cap("fix the build @HOBBES").lane, Some(Lane::Agent));
    }

    #[test]
    fn at_worker_names_resolve_by_slug_and_unique_prefix() {
        assert_eq!(cap("rename Monica @clearmirror-site").worker.as_deref(), Some("clearmirror-site"));
        assert_eq!(cap("scope doc @scott-marsh-phase-2").worker.as_deref(), Some("Scott Marsh Phase 2"));
        assert_eq!(cap("scope doc @scott").worker.as_deref(), Some("Scott Marsh Phase 2"));
        // Ambiguous prefix ("puget" → two workers) and short prefixes stay text.
        let p = cap("tag release @puget");
        assert_eq!(p.lane, None);
        assert_eq!(p.title, "tag release @puget");
        assert_eq!(cap("x @sc").lane, None);
        // Unknown @names stay in the title, as before.
        let p = cap("email @john about it");
        assert_eq!(p.title, "email @john about it");
        assert_eq!(p.worker, None);
        // Without a worker list only @hobbes / @me are lane markers.
        assert_eq!(parse_quick_add("x @clearmirror-site", today()).lane, None);
    }

    #[test]
    fn at_times_still_parse_as_times() {
        for (input, mins) in [("call @2pm", 14 * 60), ("call @9", 9 * 60), ("call @14:30", 14 * 60 + 30), ("call @12am", 0)] {
            let p = cap(input);
            assert_eq!(p.block_start, Some(mins), "{input}");
            assert_eq!(p.lane, None, "{input}");
            assert_eq!(p.title, "call", "{input}");
        }
        assert_eq!(cap("read @evening").time_of_day, Some(TimeOfDay::Evening));
        // A time and a lane marker compose.
        let p = cap("standup @me @9");
        assert_eq!((p.lane, p.block_start), (Some(Lane::Human), Some(9 * 60)));
    }

    #[test]
    fn chore_and_mine_markers() {
        let p = cap("pay Comcast bill !chore ~5m");
        assert_eq!(p.title, "pay Comcast bill");
        assert_eq!(p.kind, Some(HumanKind::Chore));
        assert_eq!(p.lane, Some(Lane::Human));
        assert_eq!(p.estimate_minutes, Some(5));
        assert!(p.lane_suggestion.is_none());

        let p = cap("call mom !mine");
        assert_eq!((p.lane, p.kind), (Some(Lane::Human), None));
        let p = cap("plan the trip @me");
        assert_eq!(p.lane, Some(Lane::Human));
        assert_eq!(p.title, "plan the trip");

        // !chore / !mine never collide with the deadline day words.
        assert_eq!(cap("x !mon").deadline, Some(date("2026-08-17")));
        assert_eq!(cap("x !mon").kind, None);
    }

    #[test]
    fn agent_marker_wins_and_drops_the_kind() {
        let p = cap("renew passport !chore @hobbes");
        assert_eq!(p.lane, Some(Lane::Agent));
        assert_eq!(p.kind, None);
        assert_eq!(p.outcome.as_deref(), Some("renew passport"));
        let p = cap("@hobbes renew passport @me");
        assert_eq!(p.lane, Some(Lane::Agent), "@me after an agent marker doesn't flip it back");
    }

    #[test]
    fn unmarked_text_gets_a_lane_suggestion() {
        let s = cap("ship the release with tests passing").lane_suggestion.unwrap();
        assert_eq!((s.lane, s.kind), (Lane::Agent, None));
        let s = cap("Research KV-cache offload approaches").lane_suggestion.unwrap();
        assert_eq!(s.lane, Lane::Agent);
        let s = cap("call mom ~20m").lane_suggestion.unwrap();
        assert_eq!((s.lane, s.kind), (Lane::Human, Some(HumanKind::Commitment)));
        let s = cap("pay the water bill").lane_suggestion.unwrap();
        assert_eq!((s.lane, s.kind), (Lane::Human, Some(HumanKind::Chore)));
        let s = cap("collect the dry cleaning").lane_suggestion.unwrap();
        assert_eq!(s.kind, Some(HumanKind::Chore), "chore verbs outrank agent verbs");
        let s = cap("walk by the river").lane_suggestion.unwrap();
        assert_eq!(s.kind, Some(HumanKind::YourTime));
        assert!(cap("Quarterly numbers").lane_suggestion.is_none());
        assert!(cap("").lane_suggestion.is_none());
        // A suggestion is not a token: the chip row stays quiet.
        assert!(!parse_quick_add("Research things", today()).has_tokens());
    }

    #[test]
    fn active_project_query_only_for_trailing_word() {
        assert_eq!(active_project_query("x +ho"), Some("ho"));
        assert_eq!(active_project_query("x +"), Some(""));
        assert_eq!(active_project_query("x +ho "), None);
        assert_eq!(active_project_query("x +1"), None);
        assert_eq!(active_project_query("x ho"), None);
        assert_eq!(active_project_query(""), None);
    }
}
