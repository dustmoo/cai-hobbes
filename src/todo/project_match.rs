//! Project resolution for the quick-add `+project` token.
//!
//! Pure — no Dioxus, no SQL. A `+token` is matched against each open
//! project's slug (the title lowercased, runs of non-alphanumerics collapsed
//! to `-`), so multi-word titles need no quoting: "Hobbes RC" is
//! `+hobbes-rc`, and word-prefix matching makes `+rc` work too.
//!
//! Only the precise tiers (exact, prefix, word prefix) resolve a typed token
//! on their own, and only when a single project holds the top tier. Initials
//! and fuzzy hits are offered in the autocomplete list, never assumed.

use std::collections::HashMap;

use super::PlannerState;

/// `"Hobbes RC"` → `"hobbes-rc"`. Runs of non-alphanumerics collapse to one
/// `-`; leading and trailing dashes are dropped. Keeps alphanumerics in any
/// script (`"Café Réno"` → `"café-réno"`).
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.extend(c.to_lowercase());
        } else {
            pending_dash = true;
        }
    }
    out
}

/// The first character of each slug word: `"hobbes-rc"` → `"hr"`.
fn initials(slug: &str) -> String {
    slug.split('-').filter_map(|w| w.chars().next()).collect()
}

/// `q`'s characters appear in `hay` in order (not necessarily adjacent).
fn is_subsequence(q: &str, hay: &str) -> bool {
    let mut hay = hay.chars();
    q.chars().all(|c| hay.any(|h| h == c))
}

/// The id a pending "Create project …" entry resolves to.
pub const PENDING_PROJECT_ID: &str = "\u{0}pending-project";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchTier {
    Fuzzy = 30,
    Initials = 50,
    Word = 60,
    Prefix = 80,
    Exact = 100,
}

impl MatchTier {
    /// Tiers precise enough to resolve a typed token without a pick.
    pub fn resolves_unpicked(self) -> bool {
        self >= MatchTier::Word
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub id: String,
    pub title: String,
    pub slug: String,
    pub area: Option<String>,
    pub tier: MatchTier,
}

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    id: String,
    title: String,
    slug: String,
    initials: String,
    area: Option<String>,
    sort_order: f64,
}

impl Entry {
    fn tier(&self, q: &str) -> Option<MatchTier> {
        if self.id == PENDING_PROJECT_ID {
            // A pending create answers only to the exact slug it inserted.
            return (self.slug == q).then_some(MatchTier::Exact);
        }
        if self.slug == q {
            Some(MatchTier::Exact)
        } else if self.slug.starts_with(q) {
            Some(MatchTier::Prefix)
        } else if self.slug.contains(&format!("-{q}")) {
            Some(MatchTier::Word)
        } else if self.initials.starts_with(q) {
            Some(MatchTier::Initials)
        } else if is_subsequence(q, &self.slug) {
            Some(MatchTier::Fuzzy)
        } else {
            None
        }
    }
}

/// Snapshot of open projects, built per keystroke from `PlannerState`
/// (cheap: tens of projects). `pins` records list picks (slug → project id)
/// so a picked token resolves to the chosen project even when two slugs
/// collide.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectIndex {
    entries: Vec<Entry>,
    pins: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectResolution {
    Resolved { id: String, title: String, tier: MatchTier },
    /// Top tier shared by n > 1 projects, or only Initials/Fuzzy hits.
    Ambiguous { query: String, candidates: usize },
    NoMatch { query: String },
}

impl ProjectIndex {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Open projects only — the same filter the row's project picker uses.
    /// Every name a project answers to becomes an entry — its title, the
    /// parts of a `"Base (inner)"` title, and its aliases
    /// (`project_identity`) — so `+cai-hobbes` finds "Hobbes (cai-hobbes)"
    /// or a project carrying that alias. `rank` folds them back to one row
    /// per project.
    pub fn from_state(state: &PlannerState) -> Self {
        let mut entries = Vec::new();
        for p in state.projects.iter().filter(|p| !p.status.is_closed()) {
            let area = p
                .area_id
                .as_ref()
                .and_then(|aid| state.areas.iter().find(|a| &a.id == aid))
                .map(|a| a.title.clone());
            let mut slugs: Vec<String> = vec![slug(&p.title)];
            let names = super::project_identity::name_keys(&p.title)
                .into_iter()
                .chain(p.aliases.iter().flat_map(|a| super::project_identity::name_keys(a)));
            for k in names {
                let s = slug(&k);
                if !s.is_empty() && !slugs.contains(&s) {
                    slugs.push(s);
                }
            }
            for s in slugs {
                entries.push(Entry {
                    id: p.id.clone(),
                    title: p.title.clone(),
                    initials: initials(&s),
                    slug: s,
                    area: area.clone(),
                    sort_order: p.sort_order,
                });
            }
        }
        Self { entries, pins: HashMap::new() }
    }

    pub fn with_pins(mut self, pins: &HashMap<String, String>) -> Self {
        self.pins = pins.clone();
        self
    }

    /// Adds a not-yet-created project (a "Create project …" pick) so its
    /// token resolves like any other. It resolves to [`PENDING_PROJECT_ID`];
    /// the commit creates the real project then. Editing the token away from
    /// its slug stops it resolving, which drops the create — the text stays
    /// authoritative.
    pub fn with_pending(mut self, title: Option<&str>) -> Self {
        if let Some(title) = title {
            let s = slug(title);
            if !s.is_empty() {
                self.entries.push(Entry {
                    id: PENDING_PROJECT_ID.to_string(),
                    title: title.to_string(),
                    initials: initials(&s),
                    slug: s,
                    area: None,
                    sort_order: f64::MAX,
                });
            }
        }
        self
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every candidate ≥ Fuzzy, best first (tier desc, sort_order, title).
    /// An empty query lists every open project in rail order.
    pub fn rank(&self, query: &str) -> Vec<Ranked> {
        let q = slug(query);
        let mut out: Vec<(&Entry, MatchTier)> = self
            .entries
            .iter()
            .filter_map(|e| {
                if q.is_empty() {
                    Some((e, MatchTier::Fuzzy))
                } else {
                    e.tier(&q).map(|t| (e, t))
                }
            })
            .collect();
        out.sort_by(|(a, ta), (b, tb)| {
            tb.cmp(ta)
                .then(a.sort_order.total_cmp(&b.sort_order))
                .then_with(|| a.title.cmp(&b.title))
        });
        // One row per project: its best-matching name (sorted first).
        let mut seen: Vec<&str> = Vec::new();
        out.retain(|(e, _)| {
            if seen.contains(&e.id.as_str()) {
                false
            } else {
                seen.push(e.id.as_str());
                true
            }
        });
        out.into_iter()
            .map(|(e, tier)| Ranked {
                id: e.id.clone(),
                title: e.title.clone(),
                slug: e.slug.clone(),
                area: e.area.clone(),
                tier,
            })
            .collect()
    }

    pub fn resolve(&self, query: &str) -> ProjectResolution {
        let q = slug(query);
        if q.is_empty() {
            return ProjectResolution::NoMatch { query: query.to_string() };
        }
        if let Some(e) = self
            .pins
            .get(&q)
            .and_then(|id| self.entries.iter().find(|e| &e.id == id))
        {
            return ProjectResolution::Resolved {
                id: e.id.clone(),
                title: e.title.clone(),
                tier: MatchTier::Exact,
            };
        }
        let ranked = self.rank(&q);
        let Some(top) = ranked.first() else {
            return ProjectResolution::NoMatch { query: query.to_string() };
        };
        let at_top = ranked.iter().take_while(|r| r.tier == top.tier).count();
        if top.tier.resolves_unpicked() && at_top == 1 {
            ProjectResolution::Resolved {
                id: top.id.clone(),
                title: top.title.clone(),
                tier: top.tier,
            }
        } else {
            ProjectResolution::Ambiguous {
                query: query.to_string(),
                candidates: ranked.len(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::model::{Area, Project, TodoStatus};
    use chrono::Utc;

    fn project(id: &str, title: &str, sort_order: f64) -> Project {
        Project {
            id: id.into(),
            title: title.into(),
            notes: String::new(),
            area_id: None,
            status: TodoStatus::default(),
            deadline: None,
            sort_order,
            path: None,
            aliases: Vec::new(),
            distinct_from: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn index(projects: Vec<Project>) -> ProjectIndex {
        let state = PlannerState { projects, ..Default::default() };
        ProjectIndex::from_state(&state)
    }

    fn resolved_id(r: ProjectResolution) -> Option<String> {
        match r {
            ProjectResolution::Resolved { id, .. } => Some(id),
            _ => None,
        }
    }

    #[test]
    fn slug_collapses_punctuation_and_case() {
        assert_eq!(slug("Hobbes RC"), "hobbes-rc");
        assert_eq!(slug("Taxes 2026 (US)"), "taxes-2026-us");
        assert_eq!(slug("  --x--  "), "x");
        assert_eq!(slug("Café Réno"), "café-réno");
        assert_eq!(slug(""), "");
    }

    #[test]
    fn exact_beats_prefix() {
        let ix = index(vec![project("rc", "Hobbes RC", 1.0), project("h", "Hobbes", 2.0)]);
        assert_eq!(resolved_id(ix.resolve("hobbes")), Some("h".into()));
        assert_eq!(resolved_id(ix.resolve("Hobbes-RC")), Some("rc".into()));
    }

    #[test]
    fn unique_prefix_resolves_shared_prefix_is_ambiguous() {
        let both = index(vec![project("rc", "Hobbes RC", 1.0), project("h", "Hobbes", 2.0)]);
        assert!(matches!(
            both.resolve("hob"),
            ProjectResolution::Ambiguous { candidates: 2, .. }
        ));
        let one = index(vec![project("rc", "Hobbes RC", 1.0)]);
        assert_eq!(resolved_id(one.resolve("hob")), Some("rc".into()));
    }

    #[test]
    fn word_prefix_resolves() {
        let ix = index(vec![project("rc", "Hobbes RC", 1.0), project("reno", "Home renovation", 2.0)]);
        assert_eq!(resolved_id(ix.resolve("rc")), Some("rc".into()));
        assert_eq!(resolved_id(ix.resolve("reno")), Some("reno".into()));
    }

    #[test]
    fn initials_and_fuzzy_rank_but_never_resolve() {
        let ix = index(vec![project("rc", "Hobbes RC", 1.0)]);
        let ranked = ix.rank("hr");
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].tier, MatchTier::Initials);
        assert!(!matches!(ix.resolve("hr"), ProjectResolution::Resolved { .. }));

        assert_eq!(ix.rank("hbrc")[0].tier, MatchTier::Fuzzy);
        assert!(!matches!(ix.resolve("hbrc"), ProjectResolution::Resolved { .. }));
        assert!(matches!(ix.resolve("zzz"), ProjectResolution::NoMatch { .. }));
    }

    #[test]
    fn closed_projects_are_invisible() {
        let mut done = project("old", "Hobbes Beta", 1.0);
        done.status = TodoStatus::Completed;
        let ix = index(vec![done, project("rc", "Hobbes RC", 2.0)]);
        assert_eq!(ix.rank("").len(), 1);
        assert_eq!(resolved_id(ix.resolve("hob")), Some("rc".into()));
    }

    #[test]
    fn pins_disambiguate_colliding_slugs() {
        let ix = index(vec![project("a", "Garden", 1.0), project("b", "garden", 2.0)]);
        assert!(matches!(ix.resolve("garden"), ProjectResolution::Ambiguous { .. }));
        let pins = HashMap::from([("garden".to_string(), "b".to_string())]);
        assert_eq!(resolved_id(ix.with_pins(&pins).resolve("garden")), Some("b".into()));
    }

    #[test]
    fn ties_break_by_sort_order_then_title() {
        let ix = index(vec![
            project("z", "Hobbes Zeta", 1.0),
            project("b", "Hobbes Beta", 2.0),
            project("a", "Hobbes Alpha", 2.0),
        ]);
        let ids: Vec<_> = ix.rank("hob").into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec!["z", "a", "b"]);
    }

    #[test]
    fn a_pending_create_resolves_only_while_its_slug_is_typed() {
        let ix = index(vec![project("gs", "Garden shed", 1.0)]).with_pending(Some("Garden"));
        // Exact on the pending entry beats the real project's prefix match.
        assert_eq!(resolved_id(ix.resolve("garden")), Some(PENDING_PROJECT_ID.into()));
        // Edited away from the inserted slug: nobody is created, and the
        // real project's prefix match takes it.
        assert_eq!(resolved_id(ix.resolve("gard")), Some("gs".into()));
    }

    #[test]
    fn aliases_and_parentheticals_resolve_to_one_project() {
        let mut h = project("h", "Hobbes (cai-hobbes)", 1.0);
        h.aliases.push("The Assistant".into());
        let ix = index(vec![h, project("x", "cai-exceller8", 2.0)]);
        assert_eq!(resolved_id(ix.resolve("cai-hobbes")), Some("h".into()));
        assert_eq!(resolved_id(ix.resolve("cai_hobbes")), Some("h".into()));
        assert_eq!(resolved_id(ix.resolve("the-assistant")), Some("h".into()));
        // The title's own slug still works, and a project appears once.
        assert_eq!(resolved_id(ix.resolve("hobbes")), Some("h".into()));
        let rows = ix.rank("hob");
        assert_eq!(rows.iter().filter(|r| r.id == "h").count(), 1);
        // "cai" prefixes both projects: ambiguous, never guessed.
        assert!(matches!(ix.resolve("cai"), ProjectResolution::Ambiguous { .. }));
    }

    #[test]
    fn area_title_is_carried_for_the_list() {
        let mut p = project("rc", "Hobbes RC", 1.0);
        p.area_id = Some("w".into());
        let state = PlannerState {
            projects: vec![p],
            areas: vec![Area {
                id: "w".into(),
                title: "Work".into(),
                sort_order: 0.0,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }],
            ..Default::default()
        };
        let ix = ProjectIndex::from_state(&state);
        assert_eq!(ix.rank("rc")[0].area.as_deref(), Some("Work"));
    }
}
