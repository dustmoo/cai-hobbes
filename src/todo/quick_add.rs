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
//!
//! Shared by the quick-add input; the AI goes through its structured tools and
//! never needs this.

use chrono::{Datelike, NaiveDate, Weekday};

use super::model::{Priority, TimeOfDay};
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
    let mut out = QuickAddParse::default();
    let mut title_words: Vec<&str> = Vec::new();

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
            ("@", rest) => match parse_time_word(rest) {
                Some(TimeToken::TimeOfDay(t)) => {
                    out.time_of_day = Some(t);
                    true
                }
                Some(TimeToken::Clock(minutes)) => {
                    out.block_start = Some(minutes);
                    true
                }
                None => false,
            },
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
