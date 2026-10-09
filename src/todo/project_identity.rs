//! Project identity: when do two names mean the same project?
//!
//! Pure — no Dioxus, no SQL. Every place that matches a name to a project
//! (AI upsert dedupe, `HOBBES_PROJECT_LIST` filtering, the session tagger,
//! quick-add `+project`, fleet cwd mapping, the "Possible duplicates" group)
//! goes through here, so a project's title and its aliases are treated alike
//! everywhere.
//!
//! Two strengths:
//! - **Exact** — the normalized names are equal (case, whitespace, `-`, `_`,
//!   `.` and edge punctuation ignored; a trailing parenthetical also yields
//!   its base and its contents as names). Exact means "same project": an AI
//!   upsert without an id updates it instead of creating a twin.
//! - **Similar** — one name contains the other as whole words, a small edit
//!   distance on short names, or a shared distinctive word. Similar is only
//!   ever *suggested* (a refusal listing candidates, a duplicates group);
//!   nothing merges or resolves on similarity alone.

use super::model::Project;

/// Shortest name (in chars) that counts by containment or edit distance —
/// "AI" must not be similar to everything with "ai" in it.
const MIN_SIMILAR_LEN: usize = 4;
/// Shortest shared word that alone makes two names similar.
const MIN_SHARED_WORD_LEN: usize = 5;
/// Edit distance applies to short names only; on long ones two edits are
/// noise, not a typo.
const MAX_EDIT_NAME_LEN: usize = 16;

/// Words too generic to make two names similar on their own.
const STOPWORDS: &[&str] = &[
    "project", "projects", "planning", "personal", "general", "research", "website", "notes",
    "stuff", "admin", "misc", "other", "work", "client", "clients", "launch",
];

/// `"Hobbes Core & Prod…"` → `"hobbes core & prod"`. Lowercased; runs of
/// whitespace, `-`, `_` and `.` collapse to one space; leading and trailing
/// punctuation is dropped. Inner punctuation that isn't a separator (`&`,
/// `+`) is kept, so "R&D" and "R D" stay distinct.
pub fn normalize(s: &str) -> String {
    let lowered: String = s
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| if c.is_whitespace() || matches!(c, '-' | '_' | '.') { ' ' } else { c })
        .collect();
    let trimmed = lowered.trim_matches(|c: char| !c.is_alphanumeric());
    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `"Hobbes (cai-hobbes)"` → `Some(("Hobbes", "cai-hobbes"))`. Only a
/// trailing parenthetical with a non-empty base counts.
fn split_parenthetical(s: &str) -> Option<(&str, &str)> {
    let t = s.trim().trim_end_matches(|c: char| !c.is_alphanumeric() && c != ')');
    let t = t.strip_suffix(')')?;
    let open = t.rfind('(')?;
    let base = t[..open].trim();
    let inner = t[open + 1..].trim();
    (!base.is_empty() && !inner.is_empty()).then_some((base, inner))
}

/// Every normalized name one written name stands for, strongest first: the
/// whole name, then (for `"Base (inner)"`) the base and the inner text.
pub fn name_keys(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |k: String| {
        if !k.is_empty() && !out.contains(&k) {
            out.push(k);
        }
    };
    push(normalize(name));
    if let Some((base, inner)) = split_parenthetical(name) {
        push(normalize(base));
        push(normalize(inner));
    }
    out
}

/// A project's names: its title's keys, then each alias's.
pub fn project_keys(p: &Project) -> Vec<String> {
    let mut out = name_keys(&p.title);
    for a in &p.aliases {
        for k in name_keys(a) {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

fn compact(k: &str) -> String {
    k.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Two normalized keys name the same thing: equal, or equal once spaces are
/// dropped ("hobbes rc" / "hobbesrc") when long enough to be distinctive.
fn keys_equal(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (ca, cb) = (compact(a), compact(b));
    ca == cb && ca.chars().count() >= MIN_SIMILAR_LEN
}

/// Whole-word containment: `"cai hobbes"` contains `"hobbes"`, but
/// `"chart"` does not contain `"art"`.
fn contains_words(longer: &str, shorter: &str) -> bool {
    format!(" {longer} ").contains(&format!(" {shorter} "))
}

/// Levenshtein distance over chars.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn distinctive_words(k: &str) -> impl Iterator<Item = &str> {
    k.split(' ').filter(|w| {
        w.chars().count() >= MIN_SHARED_WORD_LEN
            && !w.chars().all(|c| c.is_ascii_digit())
            && !STOPWORDS.contains(w)
    })
}

/// Similar (but not necessarily exact) keys.
fn keys_similar(a: &str, b: &str) -> bool {
    let (short, long) = if a.chars().count() <= b.chars().count() { (a, b) } else { (b, a) };
    let short_len = compact(short).chars().count();
    if short_len >= MIN_SIMILAR_LEN && contains_words(long, short) {
        return true;
    }
    let (cs, cl) = (compact(short), compact(long));
    if short_len >= MIN_SIMILAR_LEN && cl.chars().count() <= MAX_EDIT_NAME_LEN {
        let limit = if short_len < 6 { 1 } else { 2 };
        if edit_distance(&cs, &cl) <= limit {
            return true;
        }
    }
    distinctive_words(a).any(|w| distinctive_words(b).any(|v| v == w))
}

/// How strongly a name matches a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NameMatch {
    Similar = 1,
    /// Exact via an alias or a parenthetical part.
    ExactAlias = 2,
    /// Exact on the project's whole title.
    ExactTitle = 3,
}

impl NameMatch {
    pub fn is_exact(self) -> bool {
        self >= NameMatch::ExactAlias
    }
}

/// How `name` matches `p`, if at all.
pub fn match_name(p: &Project, name: &str) -> Option<NameMatch> {
    let wanted = name_keys(name);
    if wanted.is_empty() {
        return None;
    }
    let keys = project_keys(p);
    let title_key = normalize(&p.title);
    if wanted.iter().any(|w| keys_equal(w, &title_key)) {
        return Some(NameMatch::ExactTitle);
    }
    if wanted.iter().any(|w| keys.iter().any(|k| keys_equal(w, k))) {
        return Some(NameMatch::ExactAlias);
    }
    if wanted.iter().any(|w| keys.iter().any(|k| keys_similar(w, k))) {
        return Some(NameMatch::Similar);
    }
    None
}

/// How two projects relate by name (title + aliases on both sides).
pub fn match_projects(a: &Project, b: &Project) -> Option<NameMatch> {
    let ka = project_keys(a);
    let kb = project_keys(b);
    if ka.iter().any(|x| kb.iter().any(|y| keys_equal(x, y))) {
        return Some(NameMatch::ExactAlias);
    }
    if ka.iter().any(|x| kb.iter().any(|y| keys_similar(x, y))) {
        return Some(NameMatch::Similar);
    }
    None
}

/// What an exact lookup of a name found.
#[derive(Debug, Clone, PartialEq)]
pub enum ExactLookup {
    None,
    One(String),
    /// Several projects tie at the strongest exact strength (existing
    /// duplicates) — the caller must ask for an id.
    Many(Vec<String>),
}

/// The project a name exactly names. Open projects are preferred over
/// closed ones; within a status class the strongest strength wins, and a
/// tie at the top is `Many`.
pub fn lookup_exact(projects: &[Project], name: &str) -> ExactLookup {
    for open in [true, false] {
        let hits: Vec<(&Project, NameMatch)> = projects
            .iter()
            .filter(|p| p.status.is_closed() != open)
            .filter_map(|p| match_name(p, name).filter(|m| m.is_exact()).map(|m| (p, m)))
            .collect();
        let Some(best) = hits.iter().map(|(_, m)| *m).max() else { continue };
        let top: Vec<String> = hits
            .iter()
            .filter(|(_, m)| *m == best)
            .map(|(p, _)| p.id.clone())
            .collect();
        return if top.len() == 1 {
            ExactLookup::One(top[0].clone())
        } else {
            ExactLookup::Many(top)
        };
    }
    ExactLookup::None
}

/// [`lookup_exact`] over open projects only — for surfaces that can only
/// file into an open project (quick add).
pub fn lookup_exact_open(projects: &[Project], name: &str) -> ExactLookup {
    let open: Vec<Project> = projects.iter().filter(|p| !p.status.is_closed()).cloned().collect();
    lookup_exact(&open, name)
}

/// Projects whose names are similar (not exact) to `name`, open first, then
/// by rail order.
pub fn similar_projects<'a>(projects: &'a [Project], name: &str) -> Vec<&'a Project> {
    let mut out: Vec<&Project> = projects
        .iter()
        .filter(|p| match_name(p, name) == Some(NameMatch::Similar))
        .collect();
    out.sort_by(|a, b| {
        a.status
            .is_closed()
            .cmp(&b.status.is_closed())
            .then(a.sort_order.total_cmp(&b.sort_order))
    });
    out
}

/// One "Possible duplicates" group.
#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateGroup {
    /// Member ids in rail order.
    pub ids: Vec<String>,
    /// Every member exactly names another (same name / alias), as opposed
    /// to merely similar names.
    pub exact: bool,
}

fn find(parent: &mut [usize], i: usize) -> usize {
    let mut r = i;
    while parent[r] != r {
        r = parent[r];
    }
    let mut c = i;
    while parent[c] != r {
        let next = parent[c];
        parent[c] = r;
        c = next;
    }
    r
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[rb] = ra;
    }
}

/// Possible duplicates among the open projects: connected groups of
/// projects whose names match exactly or are similar, minus pairs the
/// person said are distinct (`Project::distinct_from`, either side).
pub fn duplicate_groups(projects: &[Project]) -> Vec<DuplicateGroup> {
    let mut open: Vec<&Project> = projects.iter().filter(|p| !p.status.is_closed()).collect();
    open.sort_by(|a, b| a.sort_order.total_cmp(&b.sort_order).then_with(|| a.title.cmp(&b.title)));
    let n = open.len();
    let mut any: Vec<usize> = (0..n).collect();
    let mut exact: Vec<usize> = (0..n).collect();
    for i in 0..n {
        for j in i + 1..n {
            let (a, b) = (open[i], open[j]);
            if a.distinct_from.contains(&b.id) || b.distinct_from.contains(&a.id) {
                continue;
            }
            match match_projects(a, b) {
                Some(m) if m.is_exact() => {
                    union(&mut any, i, j);
                    union(&mut exact, i, j);
                }
                Some(_) => union(&mut any, i, j),
                None => {}
            }
        }
    }
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    for i in 0..n {
        let r = find(&mut any, i);
        match groups.iter_mut().find(|(root, _)| *root == r) {
            Some((_, members)) => members.push(i),
            None => groups.push((r, vec![i])),
        }
    }
    groups
        .into_iter()
        .filter(|(_, m)| m.len() > 1)
        .map(|(_, members)| {
            let r0 = find(&mut exact, members[0]);
            let all_exact = members.iter().all(|&i| find(&mut exact, i) == r0);
            DuplicateGroup {
                ids: members.iter().map(|&i| open[i].id.clone()).collect(),
                exact: all_exact,
            }
        })
        .collect()
}

/// Add `name` to `aliases` unless it already names the project (any name
/// of its title — including a parenthetical's parts — or of an existing
/// alias, normalized). Returns whether it was added.
pub fn add_alias(title: &str, aliases: &mut Vec<String>, name: &str) -> bool {
    let name = name.trim();
    let key = normalize(name);
    let held = name_keys(title)
        .into_iter()
        .chain(aliases.iter().flat_map(|a| name_keys(a)))
        .any(|k| k == key);
    if key.is_empty() || held {
        return false;
    }
    aliases.push(name.to_string());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::model::TodoStatus;

    pub(crate) fn project(id: &str, title: &str) -> Project {
        Project {
            id: id.into(),
            title: title.into(),
            notes: String::new(),
            area_id: None,
            status: TodoStatus::Open,
            deadline: None,
            sort_order: 0.0,
            path: None,
            aliases: Vec::new(),
            distinct_from: Vec::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn normalize_ignores_case_separators_and_edge_punctuation() {
        assert_eq!(normalize("cai-hobbes"), "cai hobbes");
        assert_eq!(normalize("  CAI_hobbes. "), "cai hobbes");
        assert_eq!(normalize("Cai   Hobbes"), "cai hobbes");
        assert_eq!(normalize("Hobbes Core & Prod…"), "hobbes core & prod");
        assert_eq!(normalize("\"Garbage Vibe Kids!\""), "garbage vibe kids");
        assert_eq!(normalize("..."), "");
    }

    #[test]
    fn a_trailing_parenthetical_yields_base_and_inner_names() {
        assert_eq!(
            name_keys("Hobbes (cai-hobbes)"),
            vec![normalize("Hobbes (cai-hobbes)"), "hobbes".into(), "cai hobbes".into()]
        );
        assert_eq!(name_keys("(just parens)"), vec!["just parens"]);
        assert_eq!(name_keys("Garbage Vibe Kids"), vec!["garbage vibe kids"]);
    }

    #[test]
    fn exact_matches_title_alias_and_parenthetical() {
        let mut p = project("h", "Hobbes (cai-hobbes)");
        assert_eq!(match_name(&p, "Hobbes (CAI hobbes)"), Some(NameMatch::ExactTitle));
        assert_eq!(match_name(&p, "cai_hobbes"), Some(NameMatch::ExactAlias));
        assert_eq!(match_name(&p, "hobbes"), Some(NameMatch::ExactAlias));
        p.aliases.push("The Assistant".into());
        assert_eq!(match_name(&p, "the-assistant"), Some(NameMatch::ExactAlias));
        // Space-insensitive only when distinctive.
        assert_eq!(match_name(&project("rc", "Hobbes RC"), "hobbesrc"), Some(NameMatch::ExactTitle));
    }

    #[test]
    fn similar_is_not_exact() {
        let p = project("x", "Exceller8 AC-3 Chart…");
        assert_eq!(match_name(&p, "cai-exceller8"), Some(NameMatch::Similar));
        let h = project("core", "Hobbes Core & Prod…");
        assert_eq!(match_name(&h, "Hobbes"), Some(NameMatch::Similar));
        // Typo on a short name.
        assert_eq!(match_name(&project("g", "Garden"), "Gardn"), Some(NameMatch::Similar));
        // Unrelated / too short / substring inside a word.
        assert_eq!(match_name(&project("c", "Chart Review"), "art"), None);
        assert_eq!(match_name(&project("a", "AI Strategy"), "AI"), None);
        assert_eq!(match_name(&project("t", "Taxes 2026"), "Garbage Vibe Kids"), None);
    }

    #[test]
    fn lookup_exact_prefers_open_and_strongest() {
        let mut closed = project("old", "cai-hobbes");
        closed.status = TodoStatus::Completed;
        let projects = vec![closed, project("h", "Hobbes (cai-hobbes)")];
        assert_eq!(lookup_exact(&projects, "cai hobbes"), ExactLookup::One("h".into()));

        let twins = vec![project("a", "cai-hobbes"), project("b", "cai-hobbes")];
        assert_eq!(
            lookup_exact(&twins, "CAI-Hobbes"),
            ExactLookup::Many(vec!["a".into(), "b".into()])
        );
        // A title match beats an alias-only match.
        let mut aliased = project("p", "Puget");
        aliased.aliases.push("cai-hobbes".into());
        let mixed = vec![aliased, project("t", "cai-hobbes")];
        assert_eq!(lookup_exact(&mixed, "cai-hobbes"), ExactLookup::One("t".into()));
        assert_eq!(lookup_exact(&mixed, "nothing like it"), ExactLookup::None);
    }

    /// The person's real nav, 2026-10.
    fn real_names() -> Vec<Project> {
        let names = [
            ("h1", "cai-hobbes"),
            ("h2", "cai-hobbes"),
            ("x1", "cai-exceller8"),
            ("x2", "cai-exceller8"),
            ("x3", "Exceller8 AC-3 Chart…"),
            ("g1", "Garbage Vibe Kids"),
            ("g2", "Garbage Vibe Kids"),
            ("hb", "Hobbes"),
            ("hp", "Hobbes (cai-hobbes)"),
            ("hc", "Hobbes Core & Prod…"),
            ("tx", "Taxes 2026"),
        ];
        names
            .iter()
            .enumerate()
            .map(|(i, (id, t))| {
                let mut p = project(id, t);
                p.sort_order = i as f64;
                p
            })
            .collect()
    }

    #[test]
    fn duplicate_groups_on_the_real_names() {
        let groups = duplicate_groups(&real_names());
        let ids = |g: &DuplicateGroup| g.ids.clone();
        assert_eq!(groups.len(), 3, "{groups:?}");
        let hobbes = groups.iter().find(|g| g.ids.contains(&"h1".to_string())).unwrap();
        assert_eq!(ids(hobbes), vec!["h1", "h2", "hb", "hp", "hc"]);
        assert!(!hobbes.exact, "Hobbes Core & Prod is only similar");
        let exc = groups.iter().find(|g| g.ids.contains(&"x1".to_string())).unwrap();
        assert_eq!(ids(exc), vec!["x1", "x2", "x3"]);
        let gvk = groups.iter().find(|g| g.ids.contains(&"g1".to_string())).unwrap();
        assert_eq!(ids(gvk), vec!["g1", "g2"]);
        assert!(gvk.exact);
        assert!(!groups.iter().any(|g| g.ids.contains(&"tx".to_string())));
    }

    #[test]
    fn exact_subgroup_and_distinct_pairs() {
        let mut ps = real_names();
        ps.retain(|p| ["h1", "h2", "hb", "hp"].contains(&p.id.as_str()));
        let g = duplicate_groups(&ps);
        assert_eq!(g.len(), 1);
        assert!(g[0].exact, "cai-hobbes ×2, Hobbes, Hobbes (cai-hobbes) all name each other");

        // Marked distinct: the pair no longer groups.
        let mut a = project("a", "Garden");
        let b = project("b", "Gardn");
        assert_eq!(duplicate_groups(&[a.clone(), b.clone()]).len(), 1);
        a.distinct_from.push("b".into());
        assert!(duplicate_groups(&[a, b]).is_empty());
    }

    #[test]
    fn closed_projects_are_not_duplicates() {
        let mut done = project("a", "Garbage Vibe Kids");
        done.status = TodoStatus::Cancelled;
        assert!(duplicate_groups(&[done, project("b", "Garbage Vibe Kids")]).is_empty());
    }

    #[test]
    fn add_alias_skips_names_already_held() {
        let mut aliases = vec!["cai-hobbes".to_string()];
        assert!(!add_alias("Hobbes", &mut aliases, "HOBBES"));
        assert!(!add_alias("Hobbes", &mut aliases, "cai_hobbes"));
        assert!(add_alias("Hobbes", &mut aliases, "Hobbes Core"));
        assert_eq!(aliases, vec!["cai-hobbes", "Hobbes Core"]);
        // A parenthetical title already answers to its parts.
        let mut none: Vec<String> = Vec::new();
        assert!(!add_alias("Hobbes (cai-hobbes)", &mut none, "cai_hobbes"));
        assert!(!add_alias("Hobbes (cai-hobbes)", &mut none, "Hobbes"));
        assert!(add_alias("Hobbes", &mut none, "Hobbes (cai-hobbes)"));
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", "abc"), 0);
    }
}
