//! ICS (RFC 5545) feed support: HTTP fetch with conditional-GET, parsing, and
//! bounded recurrence expansion into [`CalendarEvent`]s.
//!
//! Time semantics:
//! - `...Z` values are UTC; `TZID=`-parameterized values resolve via chrono-tz
//!   (the IANA database — VTIMEZONE blocks in the feed are ignored); floating
//!   values are interpreted in the machine's local timezone; `DATE`-valued
//!   DTSTARTs mark all-day events anchored at local midnight.
//! - Recurrence (RRULE + RDATE + EXDATE) is expanded **only inside the
//!   requested window**, capped at [`MAX_OCCURRENCES_PER_EVENT`] per event.
//!   Overridden instances (RECURRENCE-ID) replace the generated occurrence.
//!
//! Known limitations (deliberate, documented):
//! - `RDATE;VALUE=PERIOD` is not supported (the rrule crate rejects it); an
//!   event carrying one is skipped with a warning.
//! - An unknown `TZID` (e.g. Windows zone names) falls back to local time.
//! - Multi-day events that *start* before the window are not returned — event
//!   identity and windowing are start-based throughout the sync pipeline.
//! - Occurrence duration is the parent's exact `DTEND - DTSTART` (or
//!   `DURATION`) span applied to each occurrence instant, so an occurrence
//!   spanning a DST transition keeps exact rather than wall-clock duration.
//! - `STATUS:CANCELLED` events (and cancelled overrides) are dropped.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::OnceLock;

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use icalendar::{Calendar, CalendarComponent, CalendarDateTime, Component, DatePerhapsTime, Event, EventLike, EventStatus};

use super::calendar_sync::FetchOutcome;
use super::model::CalendarEvent;

/// Runaway guard: no single VEVENT may expand to more occurrences than this
/// inside one window (`u16` because that is what `RRuleSet::all` takes).
pub const MAX_OCCURRENCES_PER_EVENT: u16 = 1000;

// ── URL normalization ───────────────────────────────────────────────────────

/// Rewrite `webcal://` / `webcals://` subscription URLs to `https://`.
/// Anything else passes through untouched.
pub fn normalize_ics_url(url: &str) -> String {
    let trimmed = url.trim();
    for prefix in ["webcals://", "webcal://"] {
        if let Some(rest) = strip_prefix_ignore_case(trimmed, prefix) {
            return format!("https://{}", rest);
        }
    }
    trimmed.to_string()
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    // .get() refuses a slice boundary inside a multibyte char (a pasted
    // non-ASCII URL would otherwise panic here on every sync pass).
    match s.get(..prefix.len()) {
        Some(head) if head.eq_ignore_ascii_case(prefix) => Some(&s[prefix.len()..]),
        _ => None,
    }
}

// ── HTTP fetch ──────────────────────────────────────────────────────────────

/// Long-lived HTTP client (same pattern as `ComposioClient`: build once with a
/// timeout, reuse for every request).
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default()
    })
}

/// Fetch and parse one ICS feed with a conditional GET.
///
/// `etag` / `last_modified` are the validators stored from the previous sync;
/// when the server answers `304 Not Modified` the body is skipped entirely and
/// [`FetchOutcome::NotModified`] tells the caller to keep its cache.
pub async fn fetch_ics(
    url: &str,
    subscription_id: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<FetchOutcome, String> {
    let url = normalize_ics_url(url);
    let mut request = http_client().get(&url);
    if let Some(etag) = etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    if let Some(lm) = last_modified {
        request = request.header(reqwest::header::IF_MODIFIED_SINCE, lm);
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("Calendar feed request failed: {}", e))?;

    let status = response.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(FetchOutcome::NotModified);
    }
    if !status.is_success() {
        return Err(format!(
            "Calendar feed returned HTTP {} for {}",
            status.as_u16(),
            url
        ));
    }

    let header = |name: reqwest::header::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let new_etag = header(reqwest::header::ETAG);
    let new_last_modified = header(reqwest::header::LAST_MODIFIED);

    let body = response
        .text()
        .await
        .map_err(|e| format!("Failed to read calendar feed body: {}", e))?;

    let events = parse_ics(&body, subscription_id, window)?;
    Ok(FetchOutcome::Fetched {
        events,
        etag: new_etag,
        last_modified: new_last_modified,
    })
}

// ── Parsing & expansion ─────────────────────────────────────────────────────

/// Parse an ICS document and expand every VEVENT into concrete occurrences
/// inside `window` (`[start, end)`, UTC instants — an occurrence starting
/// exactly at `window.0` is included, one starting at `window.1` is not).
pub fn parse_ics(
    text: &str,
    subscription_id: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Result<Vec<CalendarEvent>, String> {
    let calendar =
        Calendar::from_str(text).map_err(|e| format!("Failed to parse ICS feed: {}", e))?;

    // Split VEVENTs into recurrence parents and RECURRENCE-ID overrides.
    let mut parents: Vec<&Event> = Vec::new();
    // (uid, original occurrence start in UTC) → override event
    let mut overrides: HashMap<(String, DateTime<Utc>), &Event> = HashMap::new();
    for component in &calendar.components {
        let CalendarComponent::Event(event) = component else {
            continue; // VTIMEZONE (TZIDs resolve via chrono-tz), VTODO, …
        };
        let uid = event.get_uid().unwrap_or_default().to_string();
        match event.get_recurrence_id().and_then(|r| resolve_start(&r).map(|(dt, _)| dt)) {
            Some(recurrence_id) if !uid.is_empty() => {
                overrides.insert((uid, recurrence_id), event);
            }
            _ => parents.push(event),
        }
    }

    let mut out: Vec<CalendarEvent> = Vec::new();
    let mut consumed_overrides: HashSet<(String, DateTime<Utc>)> = HashSet::new();

    for event in parents {
        expand_event(
            event,
            subscription_id,
            window,
            &overrides,
            &mut consumed_overrides,
            &mut out,
        );
    }

    // Orphan overrides (their parent is outside the feed, or the override
    // moved into the window while the original slot was outside it) still
    // represent real occurrences — emit them standalone.
    for (key, event) in &overrides {
        if !consumed_overrides.contains(key) {
            if let Some(ev) = single_event(event, subscription_id, window) {
                out.push(ev);
            }
        }
    }

    // Occurrence identity is (uid, start); a moved override colliding with a
    // generated occurrence must not produce siblings.
    let mut seen: HashSet<(String, DateTime<Utc>)> = HashSet::new();
    out.retain(|e| seen.insert((e.uid.clone(), e.start)));
    out.sort_by_key(|e| e.start);
    Ok(out)
}

/// Expand one parent VEVENT into occurrences within the window.
fn expand_event(
    event: &Event,
    subscription_id: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
    overrides: &HashMap<(String, DateTime<Utc>), &Event>,
    consumed_overrides: &mut HashSet<(String, DateTime<Utc>)>,
    out: &mut Vec<CalendarEvent>,
) {
    let uid = match event.get_uid() {
        Some(uid) if !uid.is_empty() => uid.to_string(),
        _ => {
            tracing::warn!("ICS: skipping VEVENT without a UID");
            return;
        }
    };
    if event.get_status() == Some(EventStatus::Cancelled) {
        return;
    }
    let Some(start_prop) = event.get_start() else {
        tracing::warn!("ICS: skipping VEVENT '{}' without DTSTART", uid);
        return;
    };
    let Some((start, all_day)) = resolve_start(&start_prop) else {
        tracing::warn!("ICS: skipping VEVENT '{}' with unresolvable DTSTART", uid);
        return;
    };
    let duration = event_duration(event, start, all_day);

    let is_recurring = event.property_value("RRULE").is_some()
        || event.multi_properties().contains_key("RDATE");

    let occurrence_starts: Vec<DateTime<Utc>> = if is_recurring {
        match event.get_recurrence() {
            Ok(set) => {
                let result = set
                    .after(rrule::Tz::UTC.from_utc_datetime(&window.0.naive_utc()))
                    .before(rrule::Tz::UTC.from_utc_datetime(&window.1.naive_utc()))
                    .all(MAX_OCCURRENCES_PER_EVENT);
                if result.limited {
                    tracing::warn!(
                        "ICS: VEVENT '{}' hit the {}-occurrence expansion cap; truncating",
                        uid,
                        MAX_OCCURRENCES_PER_EVENT
                    );
                }
                result
                    .dates
                    .into_iter()
                    .map(|d| d.with_timezone(&Utc))
                    // `before` is inclusive; the window end is exclusive.
                    .filter(|d| *d >= window.0 && *d < window.1)
                    .collect()
            }
            Err(e) => {
                tracing::warn!("ICS: skipping recurring VEVENT '{}': {}", uid, e);
                return;
            }
        }
    } else {
        vec![start]
    };

    for occurrence in occurrence_starts {
        // An override replaces the generated occurrence at its RECURRENCE-ID.
        if let Some(override_event) = overrides.get(&(uid.clone(), occurrence)) {
            consumed_overrides.insert((uid.clone(), occurrence));
            if let Some(ev) = single_event(override_event, subscription_id, window) {
                out.push(ev);
            }
            continue;
        }
        if occurrence < window.0 || occurrence >= window.1 {
            continue;
        }
        let title = event_title(event);
        let busy = event_busy(event, &title);
        out.push(CalendarEvent {
            uid: uid.clone(),
            subscription_id: subscription_id.to_string(),
            title,
            start: occurrence,
            end: occurrence + duration,
            all_day,
            url: event.get_url().map(str::to_string),
            location: event.get_location().map(str::to_string),
            busy,
            tentative: event_tentative(event),
        });
    }
}

/// Build the `CalendarEvent` for a non-recurring VEVENT (or an override
/// instance), or `None` if it is cancelled, unresolvable, or out of window.
fn single_event(
    event: &Event,
    subscription_id: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Option<CalendarEvent> {
    if event.get_status() == Some(EventStatus::Cancelled) {
        return None;
    }
    let uid = event.get_uid().filter(|u| !u.is_empty())?.to_string();
    let (start, all_day) = resolve_start(&event.get_start()?)?;
    if start < window.0 || start >= window.1 {
        return None;
    }
    let duration = event_duration(event, start, all_day);
    let title = event_title(event);
    let busy = event_busy(event, &title);
    Some(CalendarEvent {
        uid,
        subscription_id: subscription_id.to_string(),
        title,
        start,
        end: start + duration,
        all_day,
        url: event.get_url().map(str::to_string),
        location: event.get_location().map(str::to_string),
        busy,
        tentative: event_tentative(event),
    })
}

fn event_title(event: &Event) -> String {
    match event.get_summary() {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => "(untitled)".to_string(),
    }
}

/// Busy/free semantics for a VEVENT: `TRANSP:TRANSPARENT` marks the event as
/// free (OPAQUE or absent means busy), and the shared focus-time title
/// heuristic catches Google's Focus Time blocks, which export to ICS as
/// ordinary events titled "Focus time".
fn event_busy(event: &Event, title: &str) -> bool {
    if event
        .property_value("TRANSP")
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("TRANSPARENT"))
    {
        return false;
    }
    !super::model::is_focus_time_title(title)
}

/// Tentative invitation status: `STATUS:TENTATIVE` → tentative.
/// `STATUS:CONFIRMED` or an absent STATUS is firm (CANCELLED never gets
/// here — cancelled events are dropped upstream).
fn event_tentative(event: &Event) -> bool {
    event.get_status() == Some(EventStatus::Tentative)
}

/// Resolve a DTSTART / DTEND / RECURRENCE-ID value to a UTC instant.
/// Returns `(instant, is_date_valued)`.
fn resolve_start(value: &DatePerhapsTime) -> Option<(DateTime<Utc>, bool)> {
    match value {
        // All-day: anchored at local midnight (the planner's days are local).
        DatePerhapsTime::Date(date) => Some((local_midnight(*date)?, true)),
        DatePerhapsTime::DateTime(dt) => Some((resolve_date_time(dt)?, false)),
    }
}

fn resolve_date_time(dt: &CalendarDateTime) -> Option<DateTime<Utc>> {
    match dt {
        CalendarDateTime::Utc(utc) => Some(*utc),
        // Floating: RFC 5545 says "local time of the observer".
        CalendarDateTime::Floating(naive) => local_naive_to_utc(&chrono::Local, naive),
        CalendarDateTime::WithTimezone { date_time, tzid } => {
            match chrono_tz::Tz::from_str(tzid) {
                Ok(tz) => local_naive_to_utc(&tz, date_time),
                Err(_) => {
                    // Non-IANA TZID (e.g. Windows zone names): fall back to
                    // treating the value as floating local time.
                    tracing::warn!("ICS: unknown TZID '{}', treating as local time", tzid);
                    local_naive_to_utc(&chrono::Local, date_time)
                }
            }
        }
    }
}

/// Interpret a naive wall-clock time in `tz` and convert to UTC. Ambiguous
/// times (DST fall-back) take the earlier instant; nonexistent times (DST
/// spring-forward gap) slide forward an hour, matching common calendar UX.
///
/// This is the planner's ONE local→UTC policy — tool handlers, the timeline
/// UI, and calendar sync all route through it so a block never lands on a
/// different instant depending on which surface placed it.
pub(crate) fn local_naive_to_utc<T: TimeZone>(tz: &T, naive: &NaiveDateTime) -> Option<DateTime<Utc>> {
    tz.from_local_datetime(naive)
        .earliest()
        .or_else(|| tz.from_local_datetime(&(*naive + Duration::hours(1))).earliest())
        .map(|dt| dt.with_timezone(&Utc))
}

/// Local midnight of `date` as a UTC instant — all-day anchoring, shared with
/// the Composio transport so both paths agree on all-day semantics.
pub(crate) fn local_midnight(date: NaiveDate) -> Option<DateTime<Utc>> {
    local_naive_to_utc(&chrono::Local, &date.and_hms_opt(0, 0, 0)?)
}

/// The event's duration: `DTEND - DTSTART`, else `DURATION`, else the RFC 5545
/// defaults (one day for all-day events, zero-length otherwise).
fn event_duration(event: &Event, start: DateTime<Utc>, all_day: bool) -> Duration {
    if let Some(end_prop) = event.get_end() {
        if let Some((end, _)) = resolve_start(&end_prop) {
            if end > start {
                return end - start;
            }
        }
    }
    if let Some(raw) = event.property_value("DURATION") {
        if let Some(d) = parse_ics_duration(raw) {
            if d > Duration::zero() {
                return d;
            }
        } else {
            tracing::warn!("ICS: unparsable DURATION '{}'", raw);
        }
    }
    if all_day {
        Duration::days(1)
    } else {
        Duration::zero()
    }
}

/// Parse an RFC 5545 DURATION value (`P2W`, `P1DT12H`, `PT45M`, `-PT15M`, …).
fn parse_ics_duration(raw: &str) -> Option<Duration> {
    let s = raw.trim();
    let (negative, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let s = s.strip_prefix('P')?;

    let mut seconds: i64 = 0;
    let mut number = String::new();
    let mut in_time = false;
    let mut matched_any = false;
    for c in s.chars() {
        match c {
            '0'..='9' => number.push(c),
            'T' | 't' => {
                if !number.is_empty() {
                    return None;
                }
                in_time = true;
            }
            unit => {
                let n: i64 = number.parse().ok()?;
                number.clear();
                let factor = match (unit.to_ascii_uppercase(), in_time) {
                    ('W', false) => 7 * 86_400,
                    ('D', false) => 86_400,
                    ('H', true) => 3_600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
                seconds = seconds.checked_add(n.checked_mul(factor)?)?;
                matched_any = true;
            }
        }
    }
    if !number.is_empty() || !matched_any {
        return None;
    }
    Some(if negative {
        Duration::seconds(-seconds)
    } else {
        Duration::seconds(seconds)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    const SUB: &str = "sub1";

    /// A wide window covering August 2026 (UTC).
    fn aug_window() -> (DateTime<Utc>, DateTime<Utc>) {
        (
            "2026-08-01T00:00:00Z".parse().unwrap(),
            "2026-09-01T00:00:00Z".parse().unwrap(),
        )
    }

    fn wrap(body: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n{}\r\nEND:VCALENDAR\r\n",
            body.trim()
        )
    }

    fn parse(body: &str) -> Vec<CalendarEvent> {
        parse_ics(&wrap(body), SUB, aug_window()).unwrap()
    }

    #[test]
    fn normalize_ics_url_survives_multibyte_prefixes() {
        // Regression: byte-slicing s[..prefix.len()] panicked when the boundary
        // fell mid-char in a pasted non-ASCII URL (2-byte Cyrillic chars put
        // every boundary at an even offset; "webcal://" is 9 bytes).
        let cyrillic = "календарь://example.com/feed.ics";
        assert_eq!(normalize_ics_url(cyrillic), cyrillic);
        // Case-insensitive rewrite still works.
        assert_eq!(normalize_ics_url("Webcal://x/y.ics"), "https://x/y.ics");
    }

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// Local wall-clock time as a UTC instant, mirroring the parser's
    /// floating/all-day semantics; keeps assertions machine-tz-independent.
    fn local(s: &str) -> DateTime<Utc> {
        let naive: NaiveDateTime = s.parse().unwrap();
        local_naive_to_utc(&chrono::Local, &naive).unwrap()
    }

    // ── URL rewrite ─────────────────────────────────────────────────────────

    #[test]
    fn webcal_urls_rewrite_to_https() {
        assert_eq!(
            normalize_ics_url("webcal://example.com/feed.ics"),
            "https://example.com/feed.ics"
        );
        assert_eq!(
            normalize_ics_url("WEBCALS://example.com/feed.ics"),
            "https://example.com/feed.ics"
        );
        assert_eq!(
            normalize_ics_url("  https://example.com/feed.ics  "),
            "https://example.com/feed.ics"
        );
        assert_eq!(normalize_ics_url("http://example.com/x"), "http://example.com/x");
    }

    // ── Plain events ────────────────────────────────────────────────────────

    #[test]
    fn single_timed_utc_event_with_url_and_location() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:one@test\r\nDTSTART:20260810T140000Z\r\nDTEND:20260810T150000Z\r\nSUMMARY:Design review\r\nLOCATION:Room 4\r\nURL:https://cal.example/e/1\r\nEND:VEVENT",
        );
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.uid, "one@test");
        assert_eq!(e.subscription_id, SUB);
        assert_eq!(e.title, "Design review");
        assert_eq!(e.start, utc("2026-08-10T14:00:00Z"));
        assert_eq!(e.end, utc("2026-08-10T15:00:00Z"));
        assert!(!e.all_day);
        assert_eq!(e.url.as_deref(), Some("https://cal.example/e/1"));
        assert_eq!(e.location.as_deref(), Some("Room 4"));
    }

    #[test]
    fn missing_summary_falls_back_to_untitled() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:untitled@test\r\nDTSTART:20260810T140000Z\r\nDTEND:20260810T143000Z\r\nEND:VEVENT",
        );
        assert_eq!(events[0].title, "(untitled)");
    }

    #[test]
    fn missing_dtend_yields_zero_length_event() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:zero@test\r\nDTSTART:20260810T140000Z\r\nSUMMARY:Ping\r\nEND:VEVENT",
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].end, events[0].start);
    }

    #[test]
    fn duration_based_event() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:dur@test\r\nDTSTART:20260810T140000Z\r\nDURATION:PT1H30M\r\nSUMMARY:Workshop\r\nEND:VEVENT",
        );
        assert_eq!(events[0].end - events[0].start, Duration::minutes(90));
    }

    #[test]
    fn duration_parser_handles_rfc5545_forms() {
        assert_eq!(parse_ics_duration("PT45M"), Some(Duration::minutes(45)));
        assert_eq!(parse_ics_duration("P1DT12H"), Some(Duration::hours(36)));
        assert_eq!(parse_ics_duration("P2W"), Some(Duration::weeks(2)));
        assert_eq!(parse_ics_duration("-PT15M"), Some(Duration::minutes(-15)));
        assert_eq!(parse_ics_duration("PT1H30M"), Some(Duration::minutes(90)));
        assert_eq!(parse_ics_duration("P"), None);
        assert_eq!(parse_ics_duration("PT"), None);
        assert_eq!(parse_ics_duration("P1X"), None);
        assert_eq!(parse_ics_duration("nonsense"), None);
        // Date units after T (and time units outside T) are invalid.
        assert_eq!(parse_ics_duration("PT1D"), None);
        assert_eq!(parse_ics_duration("P1H"), None);
    }

    #[test]
    fn floating_time_is_local() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:float@test\r\nDTSTART:20260810T090000\r\nDTEND:20260810T093000\r\nSUMMARY:Floating\r\nEND:VEVENT",
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].start, local("2026-08-10T09:00:00"));
        assert!(!events[0].all_day);
    }

    #[test]
    fn transp_transparent_marks_the_event_free() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:free@test\r\nDTSTART:20260810T140000Z\r\nDTEND:20260810T150000Z\r\nTRANSP:TRANSPARENT\r\nSUMMARY:Hold\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:opaque@test\r\nDTSTART:20260810T150000Z\r\nDTEND:20260810T160000Z\r\nTRANSP:OPAQUE\r\nSUMMARY:Real meeting\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:plain@test\r\nDTSTART:20260810T160000Z\r\nDTEND:20260810T170000Z\r\nSUMMARY:No TRANSP\r\nEND:VEVENT",
        );
        let busy_by_uid: Vec<(&str, bool)> =
            events.iter().map(|e| (e.uid.as_str(), e.busy)).collect();
        assert_eq!(
            busy_by_uid,
            vec![("free@test", false), ("opaque@test", true), ("plain@test", true)],
            "TRANSPARENT → free; OPAQUE or absent → busy"
        );
    }

    #[test]
    fn focus_time_title_marks_the_event_free() {
        // Google exports Focus Time to ICS as a plain OPAQUE event titled
        // "Focus time" — the title heuristic is the only signal.
        let events = parse(
            "BEGIN:VEVENT\r\nUID:focus@test\r\nDTSTART:20260810T090000Z\r\nDTEND:20260810T110000Z\r\nSUMMARY:Focus time\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:focused@test\r\nDTSTART:20260810T110000Z\r\nDTEND:20260810T120000Z\r\nSUMMARY:Focused discussion\r\nEND:VEVENT",
        );
        assert!(!events[0].busy, "Focus time is free");
        assert!(events[1].busy, "'Focused discussion' is a real meeting");
    }

    #[test]
    fn recurring_free_event_stays_free_across_occurrences() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:focusweek@test\r\nDTSTART:20260803T090000Z\r\nDTEND:20260803T110000Z\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\nTRANSP:TRANSPARENT\r\nSUMMARY:Focus time\r\nEND:VEVENT",
        );
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|e| !e.busy));
    }

    #[test]
    fn status_tentative_marks_the_event_tentative() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:maybe@test\r\nDTSTART:20260810T140000Z\r\nDTEND:20260810T150000Z\r\nSTATUS:TENTATIVE\r\nSUMMARY:Maybe\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:firm@test\r\nDTSTART:20260810T150000Z\r\nDTEND:20260810T160000Z\r\nSTATUS:CONFIRMED\r\nSUMMARY:Firm\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:plainstatus@test\r\nDTSTART:20260810T160000Z\r\nDTEND:20260810T170000Z\r\nSUMMARY:No STATUS\r\nEND:VEVENT",
        );
        let tentative_by_uid: Vec<(&str, bool)> =
            events.iter().map(|e| (e.uid.as_str(), e.tentative)).collect();
        assert_eq!(
            tentative_by_uid,
            vec![
                ("maybe@test", true),
                ("firm@test", false),
                ("plainstatus@test", false)
            ],
            "TENTATIVE → tentative; CONFIRMED or absent → firm"
        );
        assert!(events[0].busy, "tentative is orthogonal to busy/free");
    }

    #[test]
    fn recurring_tentative_event_stays_tentative_across_occurrences() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:maybeweek@test\r\nDTSTART:20260803T090000Z\r\nDTEND:20260803T100000Z\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\nSTATUS:TENTATIVE\r\nSUMMARY:Maybe 1:1\r\nEND:VEVENT",
        );
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|e| e.tentative));
    }

    #[test]
    fn cancelled_events_are_dropped() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:gone@test\r\nDTSTART:20260810T140000Z\r\nSTATUS:CANCELLED\r\nSUMMARY:Nope\r\nEND:VEVENT",
        );
        assert!(events.is_empty());
    }

    // ── Timezones ───────────────────────────────────────────────────────────

    #[test]
    fn tzid_event_resolves_via_chrono_tz() {
        // 10:00 in New York during EDT is 14:00 UTC.
        let events = parse(
            "BEGIN:VEVENT\r\nUID:tz@test\r\nDTSTART;TZID=America/New_York:20260810T100000\r\nDTEND;TZID=America/New_York:20260810T110000\r\nSUMMARY:NY call\r\nEND:VEVENT",
        );
        assert_eq!(events[0].start, utc("2026-08-10T14:00:00Z"));
        assert_eq!(events[0].end, utc("2026-08-10T15:00:00Z"));
    }

    #[test]
    fn recurring_tz_event_crossing_dst_boundary_keeps_wall_clock_time() {
        // Daily 09:00 America/New_York across the 2026-03-08 spring-forward
        // (02:00 that morning). 09:00 EST = 14:00 UTC before; 09:00 EDT =
        // 13:00 UTC from the 8th on — the wall-clock time holds, the UTC
        // instant shifts.
        let window = (utc("2026-03-07T00:00:00Z"), utc("2026-03-10T00:00:00Z"));
        let ics = wrap(
            "BEGIN:VEVENT\r\nUID:dst@test\r\nDTSTART;TZID=America/New_York:20260307T090000\r\nDTEND;TZID=America/New_York:20260307T100000\r\nRRULE:FREQ=DAILY;COUNT=3\r\nSUMMARY:Standup\r\nEND:VEVENT",
        );
        let events = parse_ics(&ics, SUB, window).unwrap();
        let starts: Vec<DateTime<Utc>> = events.iter().map(|e| e.start).collect();
        assert_eq!(
            starts,
            vec![
                utc("2026-03-07T14:00:00Z"), // 09:00 EST
                utc("2026-03-08T13:00:00Z"), // 09:00 EDT — DST began at 02:00
                utc("2026-03-09T13:00:00Z"), // 09:00 EDT
            ]
        );
    }

    // ── All-day events ──────────────────────────────────────────────────────

    #[test]
    fn all_day_single_event_spans_one_local_day() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:allday@test\r\nDTSTART;VALUE=DATE:20260812\r\nSUMMARY:Offsite\r\nEND:VEVENT",
        );
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert!(e.all_day);
        assert_eq!(e.start, local("2026-08-12T00:00:00"));
        assert_eq!(e.end - e.start, Duration::days(1));
    }

    #[test]
    fn all_day_event_with_dtend_uses_exclusive_end_date() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:multi@test\r\nDTSTART;VALUE=DATE:20260812\r\nDTEND;VALUE=DATE:20260814\r\nSUMMARY:Conference\r\nEND:VEVENT",
        );
        assert_eq!(events[0].end - events[0].start, Duration::days(2));
    }

    #[test]
    fn all_day_recurring_weekly() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:planning@test\r\nDTSTART;VALUE=DATE:20260803\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nSUMMARY:Planning day\r\nEND:VEVENT",
        );
        assert_eq!(events.len(), 4);
        for (i, e) in events.iter().enumerate() {
            assert!(e.all_day);
            assert_eq!(
                e.start,
                local("2026-08-03T00:00:00") + Duration::weeks(i as i64)
            );
            assert_eq!(e.end - e.start, Duration::days(1));
        }
    }

    // ── Recurrence ──────────────────────────────────────────────────────────

    #[test]
    fn weekly_rrule_with_exdate_skips_the_excluded_instance() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:weekly@test\r\nDTSTART:20260803T100000Z\r\nDTEND:20260803T103000Z\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nEXDATE:20260817T100000Z\r\nSUMMARY:1:1\r\nEND:VEVENT",
        );
        let starts: Vec<DateTime<Utc>> = events.iter().map(|e| e.start).collect();
        assert_eq!(
            starts,
            vec![
                utc("2026-08-03T10:00:00Z"),
                utc("2026-08-10T10:00:00Z"),
                utc("2026-08-24T10:00:00Z"),
            ]
        );
        assert!(events.iter().all(|e| e.uid == "weekly@test"));
        assert!(events.iter().all(|e| e.end - e.start == Duration::minutes(30)));
    }

    #[test]
    fn rrule_with_until_stops_at_until() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:until@test\r\nDTSTART:20260803T100000Z\r\nDTEND:20260803T110000Z\r\nRRULE:FREQ=DAILY;UNTIL=20260806T100000Z\r\nSUMMARY:Daily\r\nEND:VEVENT",
        );
        assert_eq!(events.len(), 4, "3rd through 6th inclusive");
        assert_eq!(events.last().unwrap().start, utc("2026-08-06T10:00:00Z"));
    }

    #[test]
    fn rdate_adds_extra_occurrences() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:rdate@test\r\nDTSTART:20260803T100000Z\r\nDTEND:20260803T110000Z\r\nRDATE:20260820T160000Z\r\nSUMMARY:Ad-hoc\r\nEND:VEVENT",
        );
        let starts: Vec<DateTime<Utc>> = events.iter().map(|e| e.start).collect();
        assert_eq!(
            starts,
            vec![utc("2026-08-03T10:00:00Z"), utc("2026-08-20T16:00:00Z")]
        );
    }

    #[test]
    fn recurrence_id_override_replaces_the_generated_instance() {
        // Weekly Mondays; the Aug 17 instance is moved to Aug 18 15:00 and renamed.
        let events = parse(
            "BEGIN:VEVENT\r\nUID:weekly@test\r\nDTSTART:20260803T100000Z\r\nDTEND:20260803T103000Z\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\nSUMMARY:1:1\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:weekly@test\r\nRECURRENCE-ID:20260817T100000Z\r\nDTSTART:20260818T150000Z\r\nDTEND:20260818T153000Z\r\nSUMMARY:1:1 (moved)\r\nEND:VEVENT",
        );
        let mut summary: Vec<(DateTime<Utc>, &str)> =
            events.iter().map(|e| (e.start, e.title.as_str())).collect();
        summary.sort();
        assert_eq!(
            summary,
            vec![
                (utc("2026-08-03T10:00:00Z"), "1:1"),
                (utc("2026-08-10T10:00:00Z"), "1:1"),
                (utc("2026-08-18T15:00:00Z"), "1:1 (moved)"),
            ],
            "the generated 08-17 instance is replaced, not duplicated"
        );
    }

    #[test]
    fn cancelled_override_removes_the_instance() {
        let events = parse(
            "BEGIN:VEVENT\r\nUID:weekly@test\r\nDTSTART:20260803T100000Z\r\nDTEND:20260803T103000Z\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\nSUMMARY:1:1\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:weekly@test\r\nRECURRENCE-ID:20260810T100000Z\r\nDTSTART:20260810T100000Z\r\nDTEND:20260810T103000Z\r\nSTATUS:CANCELLED\r\nSUMMARY:1:1\r\nEND:VEVENT",
        );
        let starts: Vec<DateTime<Utc>> = events.iter().map(|e| e.start).collect();
        assert_eq!(
            starts,
            vec![utc("2026-08-03T10:00:00Z"), utc("2026-08-17T10:00:00Z")]
        );
    }

    #[test]
    fn orphan_override_is_emitted_standalone() {
        // An override whose parent series is absent from the feed.
        let events = parse(
            "BEGIN:VEVENT\r\nUID:orphan@test\r\nRECURRENCE-ID:20260805T100000Z\r\nDTSTART:20260805T110000Z\r\nDTEND:20260805T113000Z\r\nSUMMARY:Rescheduled\r\nEND:VEVENT",
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].start, utc("2026-08-05T11:00:00Z"));
    }

    // ── Window edges & expansion bounds ─────────────────────────────────────

    #[test]
    fn window_start_is_inclusive_and_end_is_exclusive() {
        let (ws, we) = aug_window();
        let ics = wrap(
            "BEGIN:VEVENT\r\nUID:at-start@test\r\nDTSTART:20260801T000000Z\r\nDTEND:20260801T003000Z\r\nSUMMARY:At start\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:at-end@test\r\nDTSTART:20260901T000000Z\r\nDTEND:20260901T003000Z\r\nSUMMARY:At end\r\nEND:VEVENT",
        );
        let events = parse_ics(&ics, SUB, (ws, we)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "at-start@test");
    }

    #[test]
    fn recurrence_expands_only_inside_the_window() {
        // Unbounded daily rule from 2020: only August 2026 comes back.
        let events = parse(
            "BEGIN:VEVENT\r\nUID:forever@test\r\nDTSTART:20200101T090000Z\r\nDTEND:20200101T091500Z\r\nRRULE:FREQ=DAILY\r\nSUMMARY:Forever\r\nEND:VEVENT",
        );
        assert_eq!(events.len(), 31);
        assert_eq!(events.first().unwrap().start, utc("2026-08-01T09:00:00Z"));
        assert_eq!(events.last().unwrap().start, utc("2026-08-31T09:00:00Z"));
    }

    #[test]
    fn expansion_is_capped_per_event() {
        // Minutely unbounded: a 31-day window holds ~44k occurrences; the cap
        // keeps it at MAX_OCCURRENCES_PER_EVENT.
        let events = parse(
            "BEGIN:VEVENT\r\nUID:runaway@test\r\nDTSTART:20260801T000000Z\r\nDTEND:20260801T000500Z\r\nRRULE:FREQ=MINUTELY\r\nSUMMARY:Runaway\r\nEND:VEVENT",
        );
        assert_eq!(events.len(), MAX_OCCURRENCES_PER_EVENT as usize);
    }

    // ── Robustness ──────────────────────────────────────────────────────────

    #[test]
    fn garbage_input_is_an_error_not_a_panic() {
        assert!(parse_ics("not an ics feed at all", SUB, aug_window()).is_err());
    }

    #[test]
    fn events_without_uid_or_dtstart_are_skipped() {
        let events = parse(
            "BEGIN:VEVENT\r\nDTSTART:20260810T140000Z\r\nSUMMARY:No UID\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:no-start@test\r\nSUMMARY:No DTSTART\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:ok@test\r\nDTSTART:20260810T140000Z\r\nDTEND:20260810T143000Z\r\nSUMMARY:Fine\r\nEND:VEVENT",
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "ok@test");
    }

    #[test]
    fn folded_lines_unfold() {
        // RFC 5545 line folding: continuation lines start with a space.
        let events = parse(
            "BEGIN:VEVENT\r\nUID:folded@test\r\nDTSTART:20260810T140000Z\r\nDTEND:20260810T143000Z\r\nSUMMARY:A rather long su\r\n mmary that was folded\r\nEND:VEVENT",
        );
        assert_eq!(events[0].title, "A rather long summary that was folded");
    }

    // ── HTTP fetch (wiremock — no real network) ─────────────────────────────

    const FEED_BODY: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:http@test\r\nDTSTART:20260810T140000Z\r\nDTEND:20260810T150000Z\r\nSUMMARY:Fetched\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[tokio::test]
    async fn fetch_200_returns_events_and_captures_validators() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/feed.ics"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(FEED_BODY)
                    .insert_header("ETag", "\"v1\"")
                    .insert_header("Last-Modified", "Mon, 10 Aug 2026 12:00:00 GMT"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/feed.ics", server.uri());
        let outcome = fetch_ics(&url, SUB, aug_window(), None, None).await.unwrap();
        match outcome {
            FetchOutcome::Fetched {
                events,
                etag,
                last_modified,
            } => {
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].uid, "http@test");
                assert_eq!(events[0].subscription_id, SUB);
                assert_eq!(etag.as_deref(), Some("\"v1\""));
                assert_eq!(
                    last_modified.as_deref(),
                    Some("Mon, 10 Aug 2026 12:00:00 GMT")
                );
            }
            other => panic!("expected Fetched, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn conditional_get_replays_validators_and_304_short_circuits() {
        use wiremock::matchers::{header, headers, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // The 304 arm matches ONLY when both validators are replayed, proving
        // the second request actually sent them. (`headers` because wiremock's
        // exact matcher splits on commas, and HTTP dates contain one.)
        Mock::given(method("GET"))
            .and(path("/feed.ics"))
            .and(header("If-None-Match", "\"v1\""))
            .and(headers(
                "If-Modified-Since",
                vec!["Mon", "10 Aug 2026 12:00:00 GMT"],
            ))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/feed.ics", server.uri());
        let outcome = fetch_ics(
            &url,
            SUB,
            aug_window(),
            Some("\"v1\""),
            Some("Mon, 10 Aug 2026 12:00:00 GMT"),
        )
        .await
        .unwrap();
        assert_eq!(outcome, FetchOutcome::NotModified);
    }

    #[tokio::test]
    async fn http_errors_produce_descriptive_error_outcomes() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing.ics"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/broken.ics"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let err404 = fetch_ics(
            &format!("{}/missing.ics", server.uri()),
            SUB,
            aug_window(),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err404.contains("404"), "{}", err404);

        let err500 = fetch_ics(
            &format!("{}/broken.ics", server.uri()),
            SUB,
            aug_window(),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err500.contains("500"), "{}", err500);
    }

    #[tokio::test]
    async fn garbage_feed_body_is_a_fetch_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/garbage.ics"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>not a calendar</html>"))
            .mount(&server)
            .await;

        let err = fetch_ics(
            &format!("{}/garbage.ics", server.uri()),
            SUB,
            aug_window(),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("parse"), "{}", err);
    }

    #[test]
    fn local_midnight_matches_local_conversion() {
        // Sanity-check the test helper itself against the local timezone.
        let m = local_midnight("2026-08-12".parse().unwrap()).unwrap();
        let back = m.with_timezone(&chrono::Local);
        assert_eq!(back.date_naive(), "2026-08-12".parse::<NaiveDate>().unwrap());
        assert_eq!(back.time().hour(), 0);
    }
}
