//! Composio Google Calendar transport: fetch a subscription's window of
//! events via `GOOGLECALENDAR_EVENTS_LIST` and normalize the Google event
//! shape into [`CalendarEvent`]s.
//!
//! Transport notes:
//! - `singleEvents=true` — recurrence arrives pre-expanded, one item per
//!   occurrence; the item `id` is unique per expanded instance (unlike
//!   `iCalUID`, which is shared across a series), so `id` is the `uid`.
//! - No conditional-GET equivalent: every sync refetches the window (it is
//!   small, structured data). [`FetchOutcome::Fetched`] always carries
//!   `etag`/`last_modified` = `None`.
//! - Pagination via `nextPageToken`, capped at [`MAX_PAGES`] pages.
//! - Auth failures (`ToolExecuteResponse::is_auth_error`) surface as a clear
//!   "reconnect" error for the subscription's `last_error`; the reconnect
//!   flow itself is owned by the existing `execute_tool` lifecycle
//!   (cooldown-guarded auto-reconnect) — never triggered from here.
//!
//! Time semantics mirror `todo::ics`:
//! - `start.dateTime` / `end.dateTime` carry RFC 3339 offsets → UTC instants.
//! - `start.date` / `end.date` (all-day) anchor at **local** midnight; the
//!   end date is exclusive per the Google Calendar API (same as ICS DTEND).
//! - `status == "cancelled"` items are dropped.
//! - Only events **starting** inside the window are returned — Google's
//!   `timeMin`/`timeMax` select by overlap, but the sync pipeline's event
//!   identity and reconciliation are start-based throughout.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{json, Value};

use crate::mcp::composio_client::{ComposioClient, ToolExecuteResponse};

use super::calendar_sync::FetchOutcome;
use super::model::CalendarEvent;

/// Composio tool slug for the Google Calendar events listing.
pub const EVENTS_LIST_TOOL: &str = "GOOGLECALENDAR_EVENTS_LIST";
/// Composio tool slug for listing the account's calendars (Phase 4 picker).
#[allow(dead_code)] // consumer is the Phase 4 settings-UI calendar picker
pub const CALENDARS_LIST_TOOL: &str = "GOOGLECALENDAR_LIST_CALENDARS";

/// Google's documented maximum for `maxResults` on events.list.
const MAX_RESULTS_PER_PAGE: u32 = 2500;
/// Runaway guard on the `nextPageToken` loop. 10 pages × 2500 events is far
/// beyond any real ±14-day window; past it we keep what we have and warn.
const MAX_PAGES: usize = 10;

// ── Production fetch ────────────────────────────────────────────────────────

/// Fetch one subscription's window from Google Calendar via Composio.
///
/// The caller (`Fetcher::Composio`) resolved the client Arc before any await;
/// no MCP locks are held here (P-010) — `execute_tool` manages its own.
pub async fn fetch_composio(
    client: &ComposioClient,
    calendar_id: &str,
    subscription_id: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Result<FetchOutcome, String> {
    let events = fetch_events_paginated(subscription_id, window, |page_token| {
        let client = client.clone();
        let calendar_id = calendar_id.to_string();
        async move {
            let mut args = json!({
                "calendarId": calendar_id,
                "singleEvents": true,
                "timeMin": window.0.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "timeMax": window.1.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "maxResults": MAX_RESULTS_PER_PAGE,
            });
            if let Some(token) = page_token {
                args["pageToken"] = Value::String(token);
            }
            let response = client.execute_tool(EVENTS_LIST_TOOL, args).await?;
            page_payload(response)
        }
    })
    .await?;

    Ok(FetchOutcome::Fetched {
        events,
        etag: None,
        last_modified: None,
    })
}

/// Unwrap a Composio tool response into its `data` payload, converting
/// failures into honest subscription errors.
pub fn page_payload(response: ToolExecuteResponse) -> Result<Value, String> {
    if response.is_auth_error() {
        return Err(
            "Google Calendar authorization expired or was revoked — reconnect Google Calendar \
             for this profile, then sync again."
                .to_string(),
        );
    }
    if !response.successful {
        return Err(match response.error {
            Some(e) if !e.trim().is_empty() => format!("Google Calendar request failed: {}", e),
            _ => "Google Calendar request failed without error detail.".to_string(),
        });
    }
    unwrap_proxy_envelope(response.data)
}

/// The MCP proxy wraps tool output in a JSON-RPC `tools/call` envelope:
/// `{"jsonrpc":…,"result":{"content":[{"type":"text","text":"<json string>"}],"isError":…}}`.
/// `execute_tool` returns that envelope verbatim as `data` on success — its
/// normalization only inspects the nested text for *auth* signals, because its
/// only consumer until now was the LLM, which reads the text itself. This
/// module parses the payload programmatically, so the envelope must be
/// unwrapped here: dig out `result.content[].text`, surface `isError` and
/// inner `successful: false` as real errors, and hand back the inner data.
/// Non-envelope payloads (tests, future transports) pass through untouched.
fn unwrap_proxy_envelope(data: Value) -> Result<Value, String> {
    // JSON-RPC protocol error member.
    if data.get("jsonrpc").is_some() {
        if let Some(err) = data.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| err.to_string());
            return Err(format!("Google Calendar request failed: {}", brief(&msg)));
        }
    }
    let Some(result) = data.get("result") else {
        return Ok(data);
    };
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find_map(|i| i.get("text").and_then(Value::as_str))
        });
    let Some(text) = text else {
        // An envelope without text content: trust isError, else use the
        // result object itself (structured_content-style relays).
        return if is_error {
            Err("Google Calendar request failed without error detail.".to_string())
        } else {
            Ok(result.clone())
        };
    };
    if is_error {
        return Err(format!("Google Calendar request failed: {}", brief(text)));
    }
    match serde_json::from_str::<Value>(text) {
        // The text is usually a stringified Composio execute response
        // (`{successful, data, error}`); honor its verdict, then let
        // `split_events_page` probe the remaining nesting.
        Ok(inner) => {
            if inner.get("successful").and_then(Value::as_bool) == Some(false) {
                let e = inner
                    .get("error")
                    .and_then(Value::as_str)
                    .filter(|e| !e.trim().is_empty())
                    .unwrap_or("no error detail");
                return Err(format!("Google Calendar request failed: {}", brief(e)));
            }
            Ok(inner)
        }
        Err(_) => Err(format!(
            "Google Calendar returned an unexpected response: {}",
            brief(text)
        )),
    }
}

/// First ~200 chars of an error/text blob, so `last_error` stays readable.
fn brief(text: &str) -> String {
    let trimmed = text.trim();
    let mut out: String = trimmed.chars().take(200).collect();
    if out.len() < trimmed.len() {
        out.push('…');
    }
    out
}

// ── Pagination (testable seam) ──────────────────────────────────────────────

/// Drive the `nextPageToken` loop over an injected page fetcher, mapping and
/// window-filtering every item. `fetch_page` receives the previous page's
/// token (`None` for the first page) and returns that page's `data` payload.
pub async fn fetch_events_paginated<F, Fut>(
    subscription_id: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
    mut fetch_page: F,
) -> Result<Vec<CalendarEvent>, String>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    let mut out: Vec<CalendarEvent> = Vec::new();
    let mut page_token: Option<String> = None;

    for _ in 0..MAX_PAGES {
        let payload = fetch_page(page_token.take()).await?;
        let (items, next_token) = split_events_page(&payload);
        for item in items {
            if let Some(event) = map_google_event(item, subscription_id) {
                // Start-based windowing, matching the ICS path: Google's
                // timeMin/timeMax select by overlap, so an event *starting*
                // before the window can still arrive — drop it.
                if event.start >= window.0 && event.start < window.1 {
                    out.push(event);
                }
            }
        }
        match next_token {
            Some(token) if !token.is_empty() => page_token = Some(token),
            _ => return Ok(out),
        }
    }

    tracing::warn!(
        "Google Calendar sync hit the {}-page pagination cap for subscription '{}'; truncating",
        MAX_PAGES,
        subscription_id
    );
    Ok(out)
}

/// Locate the events container in a tool payload and return its `items` plus
/// `nextPageToken`. Composio sometimes nests the raw Google response one
/// level down (`data.data` / `data.response_data`), so probe those too.
fn split_events_page(payload: &Value) -> (&[Value], Option<String>) {
    let container = [Some(payload), payload.get("data"), payload.get("response_data")]
        .into_iter()
        .flatten()
        .find(|c| c.get("items").is_some_and(Value::is_array))
        .unwrap_or(payload);

    let items = container
        .get("items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let next_token = container
        .get("nextPageToken")
        .and_then(Value::as_str)
        .map(str::to_string);
    (items, next_token)
}

// ── Event mapping ───────────────────────────────────────────────────────────

/// Map one Google Calendar event resource to a [`CalendarEvent`], or `None`
/// if it is cancelled or structurally unusable (no id, no start).
pub fn map_google_event(item: &Value, subscription_id: &str) -> Option<CalendarEvent> {
    if item.get("status").and_then(Value::as_str) == Some("cancelled") {
        return None;
    }
    // `id` — unique per expanded instance under singleEvents (iCalUID is not).
    let uid = match item.get("id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            tracing::warn!("Google Calendar: skipping event without an id");
            return None;
        }
    };
    let title = match item.get("summary").and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => "(untitled)".to_string(),
    };

    let start_obj = item.get("start")?;
    let end_obj = item.get("end");

    let (start, end, all_day) = if let Some(raw) = start_obj.get("dateTime").and_then(Value::as_str)
    {
        // Timed event: RFC 3339 with offset.
        let start = parse_rfc3339(raw).or_else(|| {
            tracing::warn!("Google Calendar: unparsable start.dateTime '{}' on '{}'", raw, uid);
            None
        })?;
        let end = end_obj
            .and_then(|e| e.get("dateTime"))
            .and_then(Value::as_str)
            .and_then(parse_rfc3339)
            .unwrap_or(start);
        (start, end, false)
    } else if let Some(raw) = start_obj.get("date").and_then(Value::as_str) {
        // All-day: date-valued, anchored at local midnight like the ICS path.
        let start_date: NaiveDate = raw.parse().ok().or_else(|| {
            tracing::warn!("Google Calendar: unparsable start.date '{}' on '{}'", raw, uid);
            None
        })?;
        let start = super::ics::local_midnight(start_date)?;
        // Google's all-day end.date is exclusive (same as ICS DTEND).
        let end = end_obj
            .and_then(|e| e.get("date"))
            .and_then(Value::as_str)
            .and_then(|d| d.parse::<NaiveDate>().ok())
            .and_then(super::ics::local_midnight)
            .filter(|end| *end > start)
            .unwrap_or(start + Duration::days(1));
        (start, end, true)
    } else {
        tracing::warn!("Google Calendar: skipping event '{}' without a start time", uid);
        return None;
    };

    // Busy/free: Google marks free events `transparency: "transparent"`
    // (omitted for busy ones) and Focus Time blocks `eventType: "focusTime"`;
    // the shared title heuristic is the fallback for calendars that surface
    // focus time as a plain event.
    let busy = item.get("transparency").and_then(Value::as_str) != Some("transparent")
        && item.get("eventType").and_then(Value::as_str) != Some("focusTime")
        && !super::model::is_focus_time_title(&title);

    // Tentative invitations (`status: "tentative"`) still block time but
    // don't count as planned meeting minutes. "confirmed" or absent is firm.
    let tentative = item.get("status").and_then(Value::as_str) == Some("tentative");

    Some(CalendarEvent {
        uid,
        subscription_id: subscription_id.to_string(),
        title,
        start,
        end,
        all_day,
        url: item.get("htmlLink").and_then(Value::as_str).map(str::to_string),
        location: item
            .get("location")
            .and_then(Value::as_str)
            .map(str::to_string),
        busy,
        tentative,
    })
}

fn parse_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

// ── Calendar listing (Phase 4 picker) ───────────────────────────────────────

/// List the connected account's calendars as `(id, summary)` pairs, for the
/// Phase 4 subscription picker. Single page (Google's calendarList default
/// covers typical accounts; the picker is not a sync path).
#[allow(dead_code)] // consumer is the Phase 4 settings-UI calendar picker
pub async fn list_google_calendars(
    client: &ComposioClient,
) -> Result<Vec<(String, String)>, String> {
    let response = client
        .execute_tool(CALENDARS_LIST_TOOL, json!({ "maxResults": 250 }))
        .await?;
    let payload = page_payload(response)?;
    Ok(parse_calendar_list(&payload))
}

/// Extract `(id, summary)` pairs from a calendarList payload. `summary`
/// falls back to the id so an unnamed calendar is still pickable.
///
/// `GOOGLECALENDAR_LIST_CALENDARS` returns its array under `"calendars"`
/// (unlike events, which use Google's raw `"items"` key); both are probed,
/// at every nesting level the events path handles.
#[allow(dead_code)] // via list_google_calendars (Phase 4); unit-tested now
pub fn parse_calendar_list(payload: &Value) -> Vec<(String, String)> {
    let container = [Some(payload), payload.get("data"), payload.get("response_data")]
        .into_iter()
        .flatten()
        .find(|c| {
            c.get("calendars").is_some_and(Value::is_array)
                || c.get("items").is_some_and(Value::is_array)
        })
        .unwrap_or(payload);

    let items = container
        .get("calendars")
        .or_else(|| container.get("items"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    items
        .iter()
        .filter_map(|c| {
            let id = c.get("id").and_then(Value::as_str)?;
            if id.is_empty() {
                return None;
            }
            let summary = c
                .get("summaryOverride")
                .or_else(|| c.get("summary"))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(id);
            Some((id.to_string(), summary.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const SUB: &str = "sub1";

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// Local wall-clock instant, mirroring the all-day anchoring; keeps
    /// assertions machine-timezone-independent.
    fn local(s: &str) -> DateTime<Utc> {
        let naive: chrono::NaiveDateTime = s.parse().unwrap();
        chrono::Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    /// A wide UTC window covering August 2026.
    fn aug_window() -> (DateTime<Utc>, DateTime<Utc>) {
        (utc("2026-08-01T00:00:00Z"), utc("2026-09-01T00:00:00Z"))
    }

    // ── Event mapping ───────────────────────────────────────────────────────

    #[test]
    fn timed_event_with_offset_maps_to_utc() {
        let item = json!({
            "id": "abc123_20260810T140000Z",
            "status": "confirmed",
            "summary": "Design review",
            "start": { "dateTime": "2026-08-10T10:00:00-04:00" },
            "end": { "dateTime": "2026-08-10T11:00:00-04:00" },
            "htmlLink": "https://www.google.com/calendar/event?eid=abc",
            "location": "Room 4"
        });
        let e = map_google_event(&item, SUB).unwrap();
        assert_eq!(e.uid, "abc123_20260810T140000Z");
        assert_eq!(e.subscription_id, SUB);
        assert_eq!(e.title, "Design review");
        assert_eq!(e.start, utc("2026-08-10T14:00:00Z"));
        assert_eq!(e.end, utc("2026-08-10T15:00:00Z"));
        assert!(!e.all_day);
        assert_eq!(
            e.url.as_deref(),
            Some("https://www.google.com/calendar/event?eid=abc")
        );
        assert_eq!(e.location.as_deref(), Some("Room 4"));
    }

    #[test]
    fn all_day_single_event_spans_one_local_day() {
        // Google marks a one-day all-day event with an exclusive next-day end.
        let item = json!({
            "id": "allday1",
            "summary": "Offsite",
            "start": { "date": "2026-08-12" },
            "end": { "date": "2026-08-13" }
        });
        let e = map_google_event(&item, SUB).unwrap();
        assert!(e.all_day);
        assert_eq!(e.start, local("2026-08-12T00:00:00"));
        assert_eq!(e.end - e.start, Duration::days(1));
    }

    #[test]
    fn all_day_multi_day_event_uses_exclusive_end_date() {
        let item = json!({
            "id": "conf1",
            "summary": "Conference",
            "start": { "date": "2026-08-12" },
            "end": { "date": "2026-08-14" }
        });
        let e = map_google_event(&item, SUB).unwrap();
        assert!(e.all_day);
        assert_eq!(e.end - e.start, Duration::days(2));
    }

    #[test]
    fn all_day_without_end_defaults_to_one_day() {
        let item = json!({
            "id": "noend",
            "summary": "Holiday",
            "start": { "date": "2026-08-12" }
        });
        let e = map_google_event(&item, SUB).unwrap();
        assert_eq!(e.end - e.start, Duration::days(1));
    }

    #[test]
    fn busy_free_semantics_map_from_google_fields() {
        let base = |id: &str| {
            json!({
                "id": id,
                "summary": "Anything",
                "start": { "dateTime": "2026-08-10T14:00:00Z" },
                "end": { "dateTime": "2026-08-10T15:00:00Z" }
            })
        };

        // Default: busy.
        assert!(map_google_event(&base("plain"), SUB).unwrap().busy);

        // transparency: "transparent" → free.
        let mut transparent = base("transp");
        transparent["transparency"] = json!("transparent");
        assert!(!map_google_event(&transparent, SUB).unwrap().busy);
        // transparency: "opaque" stays busy.
        let mut opaque = base("opaque");
        opaque["transparency"] = json!("opaque");
        assert!(map_google_event(&opaque, SUB).unwrap().busy);

        // eventType: "focusTime" → free even without transparency.
        let mut focus_type = base("ft");
        focus_type["eventType"] = json!("focusTime");
        assert!(!map_google_event(&focus_type, SUB).unwrap().busy);

        // Title heuristic fallback.
        let mut titled = base("title");
        titled["summary"] = json!("Focus time");
        assert!(!map_google_event(&titled, SUB).unwrap().busy);
        let mut near_miss = base("near");
        near_miss["summary"] = json!("Focused discussion");
        assert!(map_google_event(&near_miss, SUB).unwrap().busy);
    }

    #[test]
    fn tentative_status_maps_from_google_fields() {
        let base = |id: &str, status: Option<&str>| {
            let mut v = json!({
                "id": id,
                "summary": "Anything",
                "start": { "dateTime": "2026-08-10T14:00:00Z" },
                "end": { "dateTime": "2026-08-10T15:00:00Z" }
            });
            if let Some(s) = status {
                v["status"] = json!(s);
            }
            v
        };

        let maybe = map_google_event(&base("maybe", Some("tentative")), SUB).unwrap();
        assert!(maybe.tentative, "status \"tentative\" → tentative");
        assert!(maybe.busy, "tentative is orthogonal to busy/free");

        assert!(!map_google_event(&base("firm", Some("confirmed")), SUB).unwrap().tentative);
        assert!(!map_google_event(&base("plain", None), SUB).unwrap().tentative);
    }

    #[test]
    fn cancelled_events_are_skipped() {
        let item = json!({
            "id": "gone",
            "status": "cancelled",
            "start": { "dateTime": "2026-08-10T14:00:00Z" }
        });
        assert!(map_google_event(&item, SUB).is_none());
    }

    #[test]
    fn missing_summary_falls_back_to_untitled() {
        let item = json!({
            "id": "untitled1",
            "start": { "dateTime": "2026-08-10T14:00:00Z" },
            "end": { "dateTime": "2026-08-10T14:30:00Z" }
        });
        assert_eq!(map_google_event(&item, SUB).unwrap().title, "(untitled)");
    }

    #[test]
    fn missing_end_yields_zero_length_event() {
        let item = json!({
            "id": "zero",
            "summary": "Ping",
            "start": { "dateTime": "2026-08-10T14:00:00Z" }
        });
        let e = map_google_event(&item, SUB).unwrap();
        assert_eq!(e.end, e.start);
    }

    #[test]
    fn events_without_id_or_start_are_skipped() {
        let no_id = json!({ "summary": "?", "start": { "dateTime": "2026-08-10T14:00:00Z" } });
        let no_start = json!({ "id": "x", "summary": "?" });
        let bad_start = json!({ "id": "y", "start": { "dateTime": "not a date" } });
        assert!(map_google_event(&no_id, SUB).is_none());
        assert!(map_google_event(&no_start, SUB).is_none());
        assert!(map_google_event(&bad_start, SUB).is_none());
    }

    // ── Pagination ──────────────────────────────────────────────────────────

    fn timed_item(id: &str, start: &str, end: &str) -> Value {
        json!({
            "id": id,
            "summary": format!("Event {}", id),
            "start": { "dateTime": start },
            "end": { "dateTime": end }
        })
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn paginator_assembles_pages_and_replays_tokens() {
        use std::cell::RefCell;
        let seen_tokens: RefCell<Vec<Option<String>>> = RefCell::new(Vec::new());

        let pages = vec![
            json!({
                "items": [timed_item("a", "2026-08-03T10:00:00Z", "2026-08-03T11:00:00Z")],
                "nextPageToken": "page2"
            }),
            json!({
                "items": [timed_item("b", "2026-08-04T10:00:00Z", "2026-08-04T11:00:00Z")]
                // no nextPageToken → last page
            }),
        ];

        let events = block_on(fetch_events_paginated(SUB, aug_window(), |token| {
            let mut tokens = seen_tokens.borrow_mut();
            let page = pages[tokens.len()].clone();
            tokens.push(token);
            async move { Ok(page) }
        }))
        .unwrap();

        assert_eq!(
            *seen_tokens.borrow(),
            vec![None, Some("page2".to_string())],
            "the second request replays the first page's token"
        );
        let uids: Vec<&str> = events.iter().map(|e| e.uid.as_str()).collect();
        assert_eq!(uids, vec!["a", "b"]);
    }

    #[test]
    fn paginator_filters_events_starting_outside_the_window() {
        // timeMin/timeMax select by overlap, so Google can return an event
        // starting before the window; the paginator drops it.
        let page = json!({
            "items": [
                timed_item("before", "2026-07-31T23:00:00Z", "2026-08-01T01:00:00Z"),
                timed_item("inside", "2026-08-10T10:00:00Z", "2026-08-10T11:00:00Z"),
                timed_item("at-end", "2026-09-01T00:00:00Z", "2026-09-01T01:00:00Z"),
            ]
        });
        let events =
            block_on(fetch_events_paginated(SUB, aug_window(), |_| {
                let page = page.clone();
                async move { Ok(page) }
            }))
            .unwrap();
        let uids: Vec<&str> = events.iter().map(|e| e.uid.as_str()).collect();
        assert_eq!(uids, vec!["inside"]);
    }

    #[test]
    fn paginator_stops_at_the_page_cap() {
        use std::cell::Cell;
        let calls = Cell::new(0usize);
        // Every page claims another follows — the cap must break the loop.
        let events = block_on(fetch_events_paginated(SUB, aug_window(), |_| {
            let n = calls.get();
            calls.set(n + 1);
            let page = json!({
                "items": [timed_item(
                    &format!("e{}", n),
                    "2026-08-10T10:00:00Z",
                    "2026-08-10T11:00:00Z"
                )],
                "nextPageToken": "more"
            });
            async move { Ok(page) }
        }))
        .unwrap();
        assert_eq!(calls.get(), 10, "MAX_PAGES requests, then truncate");
        assert_eq!(events.len(), 10);
    }

    #[test]
    fn paginator_propagates_page_errors() {
        let err = block_on(fetch_events_paginated(SUB, aug_window(), |_| async {
            Err("boom".to_string())
        }))
        .unwrap_err();
        assert_eq!(err, "boom");
    }

    #[test]
    fn nested_composio_envelope_is_unwrapped() {
        // Composio sometimes nests the raw Google response under `data`.
        let page = json!({
            "data": {
                "items": [timed_item("nested", "2026-08-10T10:00:00Z", "2026-08-10T11:00:00Z")],
                "nextPageToken": ""
            }
        });
        let events =
            block_on(fetch_events_paginated(SUB, aug_window(), |_| {
                let page = page.clone();
                async move { Ok(page) }
            }))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "nested");
    }

    // ── Response envelope / errors ──────────────────────────────────────────

    fn response(successful: bool, data: Value, error: Option<&str>) -> ToolExecuteResponse {
        ToolExecuteResponse {
            data,
            error: error.map(str::to_string),
            successful,
            log_id: None,
            session_info: None,
        }
    }

    #[test]
    fn auth_error_response_asks_for_reconnect() {
        let resp = response(false, json!({ "status_code": 401 }), None);
        let err = page_payload(resp).unwrap_err();
        assert!(err.contains("reconnect"), "{}", err);
    }

    #[test]
    fn failed_response_surfaces_its_error_string() {
        let resp = response(false, json!({}), Some("calendar not found"));
        let err = page_payload(resp).unwrap_err();
        assert!(err.contains("calendar not found"), "{}", err);

        let no_detail = response(false, json!({}), None);
        assert!(page_payload(no_detail).is_err());
    }

    #[test]
    fn successful_response_yields_its_data() {
        let resp = response(true, json!({ "items": [] }), None);
        assert_eq!(page_payload(resp).unwrap(), json!({ "items": [] }));
    }

    /// What production actually receives: `execute_tool` posts a JSON-RPC
    /// `tools/call` to the MCP proxy and hands back the whole envelope as
    /// `data`, with the real Composio execute response stringified inside
    /// `result.content[0].text`.
    #[test]
    fn mcp_proxy_envelope_is_unwrapped_to_the_inner_payload() {
        let inner = json!({
            "successful": true,
            "data": { "calendars": [ { "id": "primary", "summary": "Me" } ] }
        });
        let envelope = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "result": {
                "content": [ { "type": "text", "text": inner.to_string() } ],
                "isError": false
            }
        });
        let payload = page_payload(response(true, envelope, None)).unwrap();
        assert_eq!(
            parse_calendar_list(&payload),
            vec![("primary".to_string(), "Me".to_string())]
        );
    }

    #[test]
    fn proxy_iserror_text_becomes_an_error_not_an_empty_success() {
        let envelope = json!({
            "jsonrpc": "2.0",
            "result": {
                "content": [ { "type": "text",
                               "text": "Tool GOOGLECALENDAR_CALENDARS_LIST not found" } ],
                "isError": true
            }
        });
        let err = page_payload(response(true, envelope, None)).unwrap_err();
        assert!(err.contains("not found"), "{}", err);
    }

    #[test]
    fn inner_unsuccessful_execute_response_becomes_an_error() {
        let inner = json!({ "successful": false, "error": "calendar not found: nope@x" });
        let envelope = json!({
            "jsonrpc": "2.0",
            "result": { "content": [ { "type": "text", "text": inner.to_string() } ] }
        });
        let err = page_payload(response(true, envelope, None)).unwrap_err();
        assert!(err.contains("calendar not found"), "{}", err);
    }

    #[test]
    fn jsonrpc_error_member_becomes_an_error() {
        let envelope = json!({
            "jsonrpc": "2.0",
            "error": { "code": -32602, "message": "Unknown tool" }
        });
        let err = page_payload(response(true, envelope, None)).unwrap_err();
        assert!(err.contains("Unknown tool"), "{}", err);
    }

    #[test]
    fn envelope_with_non_json_text_is_an_error_not_empty() {
        let envelope = json!({
            "jsonrpc": "2.0",
            "result": { "content": [ { "type": "text", "text": "<html>proxy said no</html>" } ] }
        });
        let err = page_payload(response(true, envelope, None)).unwrap_err();
        assert!(err.contains("unexpected response"), "{}", err);
    }

    #[test]
    fn envelope_unwrapping_flows_through_the_event_paginator() {
        let inner = json!({
            "successful": true,
            "data": {
                "items": [timed_item("wrapped", "2026-08-10T10:00:00Z", "2026-08-10T11:00:00Z")]
            }
        });
        let envelope = json!({
            "jsonrpc": "2.0",
            "result": {
                "content": [ { "type": "text", "text": inner.to_string() } ],
                "isError": false
            }
        });
        let events = block_on(fetch_events_paginated(SUB, aug_window(), |_| {
            let envelope = envelope.clone();
            async move { unwrap_proxy_envelope(envelope) }
        }))
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "wrapped");
    }

    // ── Calendar listing ────────────────────────────────────────────────────

    /// The real `GOOGLECALENDAR_LIST_CALENDARS` shape: array under
    /// `data.calendars`, not Google's raw `items`.
    #[test]
    fn calendar_list_reads_the_composio_calendars_key() {
        let payload = json!({
            "data": {
                "calendars": [
                    { "id": "user@example.com", "summary": "user@example.com", "primary": true },
                    { "id": "team@group.calendar.google.com", "summary": "Team" }
                ]
            }
        });
        assert_eq!(
            parse_calendar_list(&payload),
            vec![
                ("user@example.com".to_string(), "user@example.com".to_string()),
                ("team@group.calendar.google.com".to_string(), "Team".to_string()),
            ]
        );
    }

    #[test]
    fn calendar_list_maps_id_and_summary() {
        let payload = json!({
            "items": [
                { "id": "primary", "summary": "user@example.com" },
                { "id": "team@group.calendar.google.com", "summaryOverride": "Team",
                  "summary": "Clearmirror Team" },
                { "id": "unnamed@group.calendar.google.com" },
                { "summary": "no id — dropped" }
            ]
        });
        assert_eq!(
            parse_calendar_list(&payload),
            vec![
                ("primary".to_string(), "user@example.com".to_string()),
                ("team@group.calendar.google.com".to_string(), "Team".to_string()),
                (
                    "unnamed@group.calendar.google.com".to_string(),
                    "unnamed@group.calendar.google.com".to_string()
                ),
            ]
        );
    }
}
