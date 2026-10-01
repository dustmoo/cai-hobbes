//! Pure display order for the open-tabs sidebar.
//!
//! The same lens idea as the planner's (`todo::views::ListOrder`): the
//! sidebar can sort and group what it shows, but never writes `open_tabs` —
//! the strip's open order (and so Cmd+1..9 / next-tab cycling) is untouched.
//! Kept free of Dioxus so the comparators are testable directly.

use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::todo::views::{natural_key, ListOrder, OrderGroup, OrderKey, SortDir};

/// What the tab list is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TabSortKey {
    /// `open_tabs` order — the order the tabs were opened in, the same order
    /// the top strip shows.
    #[default]
    Manual,
    /// `Session::last_updated`. Its ascending direction is *most recent
    /// first*: a newly picked key starts ascending (the planner control's
    /// rule), and for recency that's the useful end.
    Recent,
    /// `Session::display_title`, natural order ("Step 2" < "Step 10").
    Title,
    /// When the chat started: its first message, or `last_updated` for a chat
    /// with none yet (`Session` has no dedicated creation stamp).
    Created,
}

impl TabSortKey {
    pub const ALL: [TabSortKey; 4] = [
        TabSortKey::Manual,
        TabSortKey::Recent,
        TabSortKey::Title,
        TabSortKey::Created,
    ];
}

impl OrderKey for TabSortKey {
    fn label(self) -> &'static str {
        match self {
            TabSortKey::Manual => "Open order",
            TabSortKey::Recent => "Last active",
            TabSortKey::Title => "A–Z",
            TabSortKey::Created => "Started",
        }
    }

    fn dir_label(self, dir: SortDir) -> Option<&'static str> {
        let (asc, desc) = match self {
            TabSortKey::Manual => return None,
            TabSortKey::Recent => ("Most recent first", "Least recent first"),
            TabSortKey::Title => ("A → Z", "Z → A"),
            TabSortKey::Created => ("Oldest first", "Newest first"),
        };
        Some(if dir == SortDir::Asc { asc } else { desc })
    }
}

/// How the tab list is grouped under headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TabGroupBy {
    #[default]
    None,
    /// Planner project the chat is tagged with (`Session::project_id`).
    Project,
    /// The session's Composio profile.
    Profile,
}

impl OrderGroup for TabGroupBy {
    fn label(self) -> &'static str {
        match self {
            TabGroupBy::None => "None",
            TabGroupBy::Project => "Project",
            TabGroupBy::Profile => "Profile",
        }
    }
}

/// The sidebar's lens: the planner's `ListOrder` over tab vocabulary.
pub type TabListOrder = ListOrder<TabSortKey, TabGroupBy>;

/// Which groupings make sense right now: Project only when the planner is
/// on, Profile only when there's more than one profile to tell apart. A
/// stored choice that drops out falls back via `ListOrder::sanitized`.
pub fn tab_group_options(planner_enabled: bool, profile_count: usize) -> Vec<TabGroupBy> {
    let mut out = vec![TabGroupBy::None];
    if planner_enabled {
        out.push(TabGroupBy::Project);
    }
    if profile_count > 1 {
        out.push(TabGroupBy::Profile);
    }
    out
}

/// Everything the comparators need about one open tab.
#[derive(Debug, Clone, PartialEq)]
pub struct TabFacts {
    /// Position in `open_tabs` — the manual order, and every sort's final
    /// tie-break so an order is fully determined.
    pub idx: usize,
    pub title: String,
    pub last_updated: DateTime<Utc>,
    pub started: DateTime<Utc>,
    /// (rail sort_order, project id, title) of the tagged planner project;
    /// `None` when untagged or the project no longer exists.
    pub project: Option<(f64, String, String)>,
    /// Resolved profile display name.
    pub profile: Option<String>,
}

/// One header's worth of tabs. `label: None` is the ungrouped list.
#[derive(Debug, Clone, PartialEq)]
pub struct TabGroup {
    /// Stable key for Dioxus (`"all"`, `"project-{id}"`, `"profile-none"`).
    pub id: String,
    pub label: Option<String>,
    /// `TabFacts::idx` values, in display order.
    pub rows: Vec<usize>,
}

fn cmp_tabs(a: &TabFacts, b: &TabFacts, order: TabListOrder) -> Ordering {
    let primary = match order.key {
        TabSortKey::Manual => Ordering::Equal,
        // Ascending = most recent first (see `TabSortKey::Recent`).
        TabSortKey::Recent => b.last_updated.cmp(&a.last_updated),
        TabSortKey::Title => natural_key(&a.title).cmp(&natural_key(&b.title)),
        TabSortKey::Created => a.started.cmp(&b.started),
    };
    let primary = match order.dir {
        SortDir::Asc => primary,
        SortDir::Desc => primary.reverse(),
    };
    primary.then_with(|| a.idx.cmp(&b.idx))
}

/// Open tabs → the groups to render, sorted within each group. Pure.
///
/// Named groups follow their natural order (projects by planner rail order,
/// profiles A–Z); the "No project" / "No profile" catch-all is always last.
pub fn arrange_tabs(facts: &[TabFacts], order: TabListOrder) -> Vec<TabGroup> {
    let mut sorted: Vec<&TabFacts> = facts.iter().collect();
    sorted.sort_by(|a, b| cmp_tabs(a, b, order));

    // (sort position, id suffix, label); `None` = the catch-all.
    type GroupKey = Option<(f64, String, String)>;
    let key_of = |t: &TabFacts| -> GroupKey {
        match order.group {
            TabGroupBy::None => None,
            TabGroupBy::Project => t.project.clone(),
            TabGroupBy::Profile => t.profile.as_ref().map(|p| (0.0, p.clone(), p.clone())),
        }
    };

    if order.group == TabGroupBy::None {
        if sorted.is_empty() {
            return Vec::new();
        }
        return vec![TabGroup {
            id: "all".to_string(),
            label: None,
            rows: sorted.iter().map(|t| t.idx).collect(),
        }];
    }

    let mut groups: Vec<(GroupKey, Vec<usize>)> = Vec::new();
    for t in sorted {
        let k = key_of(t);
        let id = k.as_ref().map(|k| k.1.clone());
        match groups.iter_mut().find(|g| g.0.as_ref().map(|k| &k.1) == id.as_ref()) {
            Some(g) => g.1.push(t.idx),
            None => groups.push((k, vec![t.idx])),
        }
    }
    groups.sort_by(|a, b| match (&a.0, &b.0) {
        (Some(x), Some(y)) => match order.group {
            TabGroupBy::Profile => natural_key(&x.2).cmp(&natural_key(&y.2)),
            _ => x
                .0
                .partial_cmp(&y.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| x.2.cmp(&y.2)),
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });

    let (prefix, none_label) = match order.group {
        TabGroupBy::Profile => ("profile", "No profile"),
        _ => ("project", "No project"),
    };
    groups
        .into_iter()
        .map(|(k, rows)| match k {
            Some((_, id, label)) => TabGroup {
                id: format!("{prefix}-{id}"),
                label: Some(label),
                rows,
            },
            None => TabGroup {
                id: format!("{prefix}-none"),
                label: Some(none_label.to_string()),
                rows,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_790_000_000 + secs, 0).unwrap()
    }

    fn tab(idx: usize, title: &str, updated: i64, started: i64) -> TabFacts {
        TabFacts {
            idx,
            title: title.to_string(),
            last_updated: at(updated),
            started: at(started),
            project: None,
            profile: None,
        }
    }

    fn order(key: TabSortKey, dir: SortDir, group: TabGroupBy) -> TabListOrder {
        ListOrder { key, dir, group }
    }

    fn flat(groups: &[TabGroup]) -> Vec<usize> {
        groups.iter().flat_map(|g| g.rows.iter().copied()).collect()
    }

    fn sample() -> Vec<TabFacts> {
        vec![
            tab(0, "Step 10", 30, 0),
            tab(1, "alpha", 10, 20),
            tab(2, "Step 2", 20, 10),
        ]
    }

    #[test]
    fn manual_keeps_open_order_in_one_headerless_group() {
        let groups = arrange_tabs(&sample(), TabListOrder::default());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, None);
        assert_eq!(groups[0].rows, vec![0, 1, 2]);
    }

    #[test]
    fn recent_ascending_is_most_recent_first() {
        let asc = arrange_tabs(&sample(), order(TabSortKey::Recent, SortDir::Asc, TabGroupBy::None));
        assert_eq!(flat(&asc), vec![0, 2, 1]);
        let desc = arrange_tabs(&sample(), order(TabSortKey::Recent, SortDir::Desc, TabGroupBy::None));
        assert_eq!(flat(&desc), vec![1, 2, 0]);
    }

    #[test]
    fn title_sorts_naturally_and_case_insensitively() {
        let groups = arrange_tabs(&sample(), order(TabSortKey::Title, SortDir::Asc, TabGroupBy::None));
        assert_eq!(flat(&groups), vec![1, 2, 0], "alpha < Step 2 < Step 10");
    }

    #[test]
    fn created_ascending_is_oldest_first() {
        let groups = arrange_tabs(&sample(), order(TabSortKey::Created, SortDir::Asc, TabGroupBy::None));
        assert_eq!(flat(&groups), vec![0, 2, 1]);
    }

    #[test]
    fn ties_break_on_open_order_in_either_direction() {
        let tabs = vec![tab(0, "Same", 5, 5), tab(1, "Same", 5, 5), tab(2, "Same", 5, 5)];
        for dir in [SortDir::Asc, SortDir::Desc] {
            let groups = arrange_tabs(&tabs, order(TabSortKey::Title, dir, TabGroupBy::None));
            assert_eq!(flat(&groups), vec![0, 1, 2], "{dir:?} must not reverse ties");
        }
    }

    #[test]
    fn project_groups_follow_rail_order_with_untagged_last() {
        let mut tabs = sample();
        tabs[0].project = Some((2.0, "p-b".into(), "Beta".into()));
        tabs[2].project = Some((1.0, "p-a".into(), "Zulu first on rail".into()));
        let groups = arrange_tabs(&tabs, order(TabSortKey::Manual, SortDir::Asc, TabGroupBy::Project));
        let labels: Vec<_> = groups.iter().map(|g| g.label.clone().unwrap()).collect();
        assert_eq!(labels, vec!["Zulu first on rail", "Beta", "No project"]);
        assert_eq!(groups[2].id, "project-none");
        assert_eq!(groups[2].rows, vec![1]);
    }

    #[test]
    fn profile_groups_are_alphabetical_and_sorted_within() {
        let mut tabs = sample();
        tabs[0].profile = Some("Work".into());
        tabs[1].profile = Some("personal".into());
        tabs[2].profile = Some("Work".into());
        let groups = arrange_tabs(&tabs, order(TabSortKey::Title, SortDir::Asc, TabGroupBy::Profile));
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].label.as_deref(), Some("personal"));
        assert_eq!(groups[1].label.as_deref(), Some("Work"));
        assert_eq!(groups[1].rows, vec![2, 0], "Step 2 before Step 10 inside the group");
    }

    #[test]
    fn empty_list_has_no_groups() {
        assert!(arrange_tabs(&[], TabListOrder::default()).is_empty());
    }

    #[test]
    fn stored_order_drops_groupings_no_longer_offered() {
        let stored = order(TabSortKey::Recent, SortDir::Desc, TabGroupBy::Profile);
        let groups = tab_group_options(true, 1);
        assert_eq!(groups, vec![TabGroupBy::None, TabGroupBy::Project]);
        let clean = stored.sanitized(&TabSortKey::ALL, &groups);
        assert_eq!(clean, order(TabSortKey::Recent, SortDir::Desc, TabGroupBy::None));
        assert!(order(TabSortKey::Manual, SortDir::Desc, TabGroupBy::None)
            .sanitized(&TabSortKey::ALL, &groups)
            .is_manual());
    }

    #[test]
    fn tab_order_round_trips_through_json() {
        let o = order(TabSortKey::Created, SortDir::Desc, TabGroupBy::Project);
        let json = serde_json::to_string(&o).unwrap();
        assert_eq!(json, r#"{"key":"created","dir":"desc","group":"project"}"#);
        let back: TabListOrder = serde_json::from_str(&json).unwrap();
        assert_eq!(back, o);
        let empty: TabListOrder = serde_json::from_str("{}").unwrap();
        assert!(empty.is_manual());
    }
}
