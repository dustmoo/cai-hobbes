//! Your-time categories: one list everywhere (timeline menus, the weekly
//! review, the goals editor, and the agent's tools).
//!
//! The list is the goals (in their order), then categories in use on
//! your-time blocks (recently or ahead) that have no goal, then palette
//! suggestions not used yet. Adding a goal never shrinks it, and anything
//! Hobbes files under a new category shows up here so the person can reuse,
//! re-file or promote it. All pure, over in-memory `PlannerState` data.

use chrono::{DateTime, Utc};

use super::model::{BlockSource, Lane, TimeBlock, TimeGoal, Todo};

/// How far back a category still counts as "in use" (8 weeks). Blocks
/// ahead of now always count.
pub const IN_USE_LOOKBACK_DAYS: i64 = 56;

/// The cut-off for "in use": blocks ending after this count.
pub fn in_use_since(now: DateTime<Utc>) -> DateTime<Utc> {
    now - chrono::Duration::days(IN_USE_LOOKBACK_DAYS)
}

/// Where a category in the list comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategorySource {
    /// One of the person's goals.
    Goal,
    /// Used on a your-time block, but no goal.
    InUse,
    /// A palette suggestion nobody has used yet.
    Suggestion,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CategoryOption {
    pub name: String,
    pub source: CategorySource,
    /// The weekly target, for goals.
    pub goal_minutes: Option<u32>,
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Goals in their display order.
pub fn ordered_goals(goals: &[TimeGoal]) -> Vec<&TimeGoal> {
    let mut g: Vec<&TimeGoal> = goals.iter().collect();
    g.sort_by(|a, b| a.sort_order.partial_cmp(&b.sort_order).unwrap_or(std::cmp::Ordering::Equal));
    g
}

/// Categories on your-time blocks ending after `since` (so future blocks
/// always count), most recently started first, each once (case-insensitive)
/// in the spelling of its latest block.
pub fn in_use(blocks: &[TimeBlock], since: DateTime<Utc>) -> Vec<String> {
    let mut used: Vec<(&TimeBlock, &str)> = blocks
        .iter()
        .filter(|b| b.end > since)
        .filter_map(|b| b.your_time_category().map(|c| (b, c.trim())))
        .filter(|(_, c)| !c.is_empty())
        .collect();
    used.sort_by_key(|(b, _)| std::cmp::Reverse(b.start));
    let mut out: Vec<String> = Vec::new();
    for (_, c) in used {
        if !out.iter().any(|o| same(o, c)) {
            out.push(c.to_string());
        }
    }
    out
}

/// In-use categories that aren't goals.
pub fn in_use_without_goal(goals: &[TimeGoal], blocks: &[TimeBlock], since: DateTime<Utc>) -> Vec<String> {
    in_use(blocks, since)
        .into_iter()
        .filter(|c| !goals.iter().any(|g| g.matches(c)))
        .collect()
}

/// The one list: goals (ordered), then in-use categories without a goal,
/// then `suggestions` not already present.
pub fn category_options(
    goals: &[TimeGoal],
    blocks: &[TimeBlock],
    suggestions: &[&str],
    since: DateTime<Utc>,
) -> Vec<CategoryOption> {
    let mut out: Vec<CategoryOption> = Vec::new();
    let has = |out: &[CategoryOption], n: &str| out.iter().any(|o| same(&o.name, n));
    for g in ordered_goals(goals) {
        if g.category.trim().is_empty() || has(&out, &g.category) {
            continue;
        }
        out.push(CategoryOption {
            name: g.category.trim().to_string(),
            source: CategorySource::Goal,
            goal_minutes: Some(g.weekly_target_minutes),
        });
    }
    for c in in_use(blocks, since) {
        if !has(&out, &c) {
            out.push(CategoryOption { name: c, source: CategorySource::InUse, goal_minutes: None });
        }
    }
    for s in suggestions {
        if !has(&out, s) {
            out.push(CategoryOption {
                name: s.to_string(),
                source: CategorySource::Suggestion,
                goal_minutes: None,
            });
        }
    }
    out
}

/// The spelling to file `raw` under: a goal's, else an existing block's
/// (any age), else `raw` trimmed. Keeps "outside" and "Outside" one
/// category.
pub fn canonical(goals: &[TimeGoal], blocks: &[TimeBlock], raw: &str) -> String {
    let raw = raw.trim();
    if let Some(g) = goals.iter().find(|g| g.matches(raw)) {
        return g.category.trim().to_string();
    }
    let mut on_blocks: Vec<&TimeBlock> = blocks
        .iter()
        .filter(|b| b.your_time_category().is_some_and(|c| same(c, raw)))
        .collect();
    on_blocks.sort_by_key(|b| std::cmp::Reverse(b.start));
    on_blocks
        .first()
        .and_then(|b| b.your_time_category())
        .map(|c| c.trim().to_string())
        .unwrap_or_else(|| raw.to_string())
}

/// Re-file one your-time block under `to`. The title follows the category
/// when it was the category's name (the default); a title of its own
/// ("Walk with Sam") is kept. `false` when nothing changed or the block
/// isn't your time.
pub fn set_block_category(block: &mut TimeBlock, to: &str) -> bool {
    let to = to.trim();
    if to.is_empty() {
        return false;
    }
    let BlockSource::YourTime { category } = &mut block.source else {
        return false;
    };
    if category.as_str() == to {
        return false;
    }
    if block.title.trim().is_empty() || same(&block.title, category) {
        block.title = to.to_string();
    }
    *category = to.to_string();
    true
}

/// What a re-file touched, for write-through.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Recategorised {
    pub blocks: Vec<String>,
    pub todos: Vec<String>,
}

impl Recategorised {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.todos.is_empty()
    }
}

/// Re-file every your-time block (all weeks) and human your-time to-do in
/// category `from` under `to`.
pub fn recategorise(
    blocks: &mut [TimeBlock],
    todos: &mut [Todo],
    from: &str,
    to: &str,
    now: DateTime<Utc>,
) -> Recategorised {
    let mut out = Recategorised::default();
    let to = to.trim();
    if to.is_empty() || from.trim().is_empty() {
        return out;
    }
    for b in blocks.iter_mut() {
        if b.your_time_category().is_some_and(|c| same(c, from)) && set_block_category(b, to) {
            out.blocks.push(b.id.clone());
        }
    }
    for t in todos.iter_mut().filter(|t| t.lane == Lane::Human) {
        if t.time_category.as_deref().is_some_and(|c| same(c, from) && c != to) {
            t.time_category = Some(to.to_string());
            t.updated_at = now;
            out.todos.push(t.id.clone());
        }
    }
    out
}

/// Rename the goal for `from` to `to`. `Ok(None)` when `from` has no goal
/// (a plain category re-file); refused when `to` is empty or already
/// another goal (merging two goals is a remove + re-file, not a rename).
pub fn rename_goal(goals: &mut [TimeGoal], from: &str, to: &str) -> Result<Option<TimeGoal>, String> {
    let to = to.trim();
    if to.is_empty() {
        return Err("the new name is empty".to_string());
    }
    let Some(idx) = goals.iter().position(|g| g.matches(from)) else {
        return Ok(None);
    };
    if goals.iter().enumerate().any(|(i, g)| i != idx && g.matches(to)) {
        return Err(format!("'{}' is already a goal", to));
    }
    goals[idx].category = to.to_string();
    Ok(Some(goals[idx].clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::model::{HumanKind, Todo};
    use chrono::TimeZone;

    fn t(day: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, h, 0, 0).unwrap()
    }

    fn yt(id: &str, cat: &str, day: u32) -> TimeBlock {
        TimeBlock {
            id: id.into(),
            todo_id: None,
            title: cat.into(),
            start: t(day, 12),
            end: t(day, 13),
            source: BlockSource::YourTime { category: cat.into() },
        }
    }

    fn goal(cat: &str, order: f64) -> TimeGoal {
        TimeGoal { sort_order: order, ..TimeGoal::new(cat, 300) }
    }

    const PALETTE: [&str; 5] = ["Reading", "Outside", "Experimenting", "Gaming", "Shows & movies"];

    #[test]
    fn adding_a_goal_never_shrinks_the_list() {
        let since = t(1, 0);
        let none = category_options(&[], &[], &PALETTE, since);
        assert_eq!(none.len(), 5);
        assert!(none.iter().all(|o| o.source == CategorySource::Suggestion));

        let goals = vec![goal("Outside", 1.0)];
        let blocks = vec![yt("b1", "Break", 12)];
        let opts = category_options(&goals, &blocks, &PALETTE, since);
        let names: Vec<&str> = opts.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["Outside", "Break", "Reading", "Experimenting", "Gaming", "Shows & movies"]);
        assert_eq!(opts[0].source, CategorySource::Goal);
        assert_eq!(opts[0].goal_minutes, Some(300));
        assert_eq!(opts[1].source, CategorySource::InUse);
        assert!(opts.len() >= none.len());
    }

    #[test]
    fn goals_keep_their_order_and_in_use_is_most_recent_first() {
        let goals = vec![goal("Gaming", 2.0), goal("Reading", 1.0)];
        let blocks = vec![yt("a", "nap", 3), yt("b", "Break", 10), yt("c", "Nap", 11), yt("d", "reading", 12)];
        let opts = category_options(&goals, &blocks, &[], t(1, 0));
        let names: Vec<&str> = opts.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["Reading", "Gaming", "Nap", "Break"], "dedup is case-insensitive, latest spelling wins");
    }

    #[test]
    fn old_categories_drop_out_but_future_ones_count() {
        let blocks = vec![yt("old", "Knitting", 1), yt("ahead", "Sailing", 30)];
        assert_eq!(in_use(&blocks, t(5, 0)), vec!["Sailing".to_string()]);
        assert_eq!(in_use_without_goal(&[goal("sailing", 0.0)], &blocks, t(5, 0)), Vec::<String>::new());
    }

    #[test]
    fn canonical_snaps_case_variants_to_the_existing_spelling() {
        let goals = vec![goal("Outside", 0.0)];
        let blocks = vec![yt("b", "Break", 12)];
        assert_eq!(canonical(&goals, &blocks, " outside "), "Outside");
        assert_eq!(canonical(&goals, &blocks, "break"), "Break");
        assert_eq!(canonical(&goals, &blocks, "Piano"), "Piano");
    }

    #[test]
    fn re_filing_a_block_retitles_only_a_default_title() {
        let mut b = yt("b", "Break", 12);
        assert!(set_block_category(&mut b, "Outside"));
        assert_eq!(b.title, "Outside");
        assert_eq!(b.your_time_category(), Some("Outside"));
        assert!(!set_block_category(&mut b, "Outside"), "no-op");

        let mut walk = TimeBlock { title: "Walk with Sam".into(), ..yt("w", "Break", 12) };
        assert!(set_block_category(&mut walk, "Outside"));
        assert_eq!(walk.title, "Walk with Sam");

        let mut work = TimeBlock { source: BlockSource::Manual, ..yt("m", "x", 12) };
        assert!(!set_block_category(&mut work, "Outside"), "work blocks have no category");
    }

    #[test]
    fn recategorise_moves_every_block_and_todo_of_the_category() {
        let mut blocks = vec![yt("a", "Break", 1), yt("b", "break", 12), yt("c", "Reading", 12)];
        let mut todo = Todo::new("Stretch", 1.0);
        todo.kind = Some(HumanKind::YourTime);
        todo.time_category = Some("Break".into());
        let mut todos = vec![todo];
        let r = recategorise(&mut blocks, &mut todos, "BREAK", "Outside", t(13, 0));
        assert_eq!(r.blocks, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(r.todos.len(), 1);
        assert_eq!(todos[0].time_category.as_deref(), Some("Outside"));
        assert_eq!(blocks[2].your_time_category(), Some("Reading"));
        assert!(recategorise(&mut blocks, &mut todos, "Break", "Outside", t(13, 0)).is_empty());
    }

    #[test]
    fn a_rename_can_fix_the_spelling_of_its_own_category() {
        let mut blocks = vec![yt("a", "outside", 12)];
        let r = recategorise(&mut blocks, &mut [], "outside", "Outside", t(13, 0));
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(blocks[0].title, "Outside");
    }

    #[test]
    fn renaming_a_goal_refuses_to_collide() {
        let mut goals = vec![goal("Outside", 0.0), goal("Reading", 1.0)];
        assert_eq!(rename_goal(&mut goals, "outside", "Getting outside").unwrap().unwrap().category, "Getting outside");
        assert!(rename_goal(&mut goals, "Getting outside", "reading").is_err());
        assert!(rename_goal(&mut goals, "Reading", "  ").is_err());
        assert_eq!(rename_goal(&mut goals, "Break", "Rest"), Ok(None));
        assert_eq!(rename_goal(&mut goals, "Reading", "READING").unwrap().unwrap().category, "READING", "case fix of itself is fine");
    }
}
