//! Pure layout solver: given the window's logical width and the widths the
//! user asked for, decide which side columns give way so the main column
//! keeps its protected minimum.
//!
//! The general form is [`solve`]: a list of side columns, each with a
//! requested width, a floor, and an optional collapse step, plus an ordered
//! list of [`GiveWay`] steps. Steps run in order only while the layout still
//! overflows, so a roomy window applies none of them.
//!
//! [`solve_chat`] is the chat-mode policy built on top of it:
//! 1. the drawer's inner nav (Settings) becomes a 44px icon rail,
//! 2. the tab sidebar collapses to a 48px emoji rail,
//! 3. the drawer shrinks toward its 420px minimum,
//!
//! keeping the chat column at 520px or wider. [`solve_planner`] is the
//! Planner policy over the same [`solve`]: Planner nav → 52px rail, tab
//! sidebar → rail, timeline → 300px, task list ≥ 520px.
//!
//! Nothing here mutates saved widths: the outcome is the *effective* layout
//! for this frame. When space returns, solving again with the same requests
//! hands the user's widths straight back.

/// The main column (chat or task list) never drops below this in chat mode.
pub const CHAT_MIN_WIDTH: f64 = 520.0;
/// Tab sidebar in emoji-rail mode.
pub const SIDEBAR_RAIL_WIDTH: f64 = 48.0;
/// Tab sidebar's resizable range and default (full mode).
pub const SIDEBAR_MIN_WIDTH: f64 = 180.0;
pub const SIDEBAR_MAX_WIDTH: f64 = 400.0;
pub const SIDEBAR_DEFAULT_WIDTH: f64 = 256.0;
/// Dragging the sidebar narrower than this snaps it to rail mode.
pub const SIDEBAR_SNAP_TO_RAIL_BELOW: f64 = 140.0;
/// Drawer (Settings / History / MCP) range and default.
pub const DRAWER_MIN_WIDTH: f64 = 420.0;
pub const DRAWER_MAX_WIDTH: f64 = 800.0;
pub const DRAWER_DEFAULT_WIDTH: f64 = 420.0;
/// The Settings nav: full width (`w-48`) and icon-rail width.
pub const DRAWER_NAV_FULL_WIDTH: f64 = 192.0;
pub const DRAWER_NAV_RAIL_WIDTH: f64 = 44.0;
/// The Settings nav rails whenever the drawer itself is narrower than this,
/// whatever the window does.
pub const DRAWER_NAV_RAIL_BELOW: f64 = 600.0;

/// How a side column can collapse in one discrete step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Collapse {
    /// No collapse step; the column can only shrink toward its `min`.
    None,
    /// The whole column becomes a rail of this width (tab sidebar, Planner nav).
    Rail { width: f64 },
    /// An inner nav inside the column rails, freeing up to `saving` px of the
    /// column's width (never below the column's `min`). The column takes only
    /// as much of the saving as the overflow needs.
    InnerRail { saving: f64 },
}

/// One side column's request.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnSpec {
    /// The user's saved width.
    pub requested: f64,
    /// Floor for `GiveWay::Shrink` (and for an inner rail's saving).
    pub min: f64,
    pub collapse: Collapse,
}

/// One step in the give-way order, naming a column by its index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GiveWay {
    /// Apply the column's collapse step (no-op for `Collapse::None`).
    Rail(usize),
    /// Shrink the column continuously toward its `min`.
    Shrink(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Budget {
    /// Window logical width. `<= 0` (unknown yet) means "don't constrain".
    pub available: f64,
    /// Protected minimum for the flex main column.
    pub main_min: f64,
    pub columns: Vec<ColumnSpec>,
    pub order: Vec<GiveWay>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnOutcome {
    pub width: f64,
    /// The column's collapse step was applied (whole-column rail, or inner
    /// nav railed).
    pub railed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Solution {
    /// Same order as `Budget::columns`.
    pub columns: Vec<ColumnOutcome>,
    /// What's left for the main column. Can fall below `main_min` only when
    /// every step has been taken and the window is still too narrow.
    pub main_width: f64,
}

/// Run the give-way steps in order until the main column fits.
pub fn solve(budget: &Budget) -> Solution {
    let mut out: Vec<ColumnOutcome> = budget
        .columns
        .iter()
        .map(|c| ColumnOutcome { width: c.requested, railed: false })
        .collect();
    let used = |out: &[ColumnOutcome]| out.iter().map(|c| c.width).sum::<f64>();

    if budget.available <= 0.0 {
        return Solution { main_width: f64::INFINITY, columns: out };
    }

    for step in &budget.order {
        let overflow = used(&out) + budget.main_min - budget.available;
        if overflow <= 0.0 {
            break;
        }
        match *step {
            GiveWay::Rail(i) => {
                let Some(spec) = budget.columns.get(i) else { continue };
                let col = &mut out[i];
                if col.railed {
                    continue;
                }
                match spec.collapse {
                    Collapse::None => {}
                    Collapse::Rail { width } => {
                        col.width = width;
                        col.railed = true;
                    }
                    Collapse::InnerRail { saving } => {
                        let room = (col.width - spec.min).max(0.0);
                        col.width -= saving.min(overflow).min(room);
                        col.railed = true;
                    }
                }
            }
            GiveWay::Shrink(i) => {
                let Some(spec) = budget.columns.get(i) else { continue };
                let col = &mut out[i];
                // A whole-column rail is already as small as it gets.
                if col.railed && matches!(spec.collapse, Collapse::Rail { .. }) {
                    continue;
                }
                let room = (col.width - spec.min).max(0.0);
                col.width -= overflow.min(room);
            }
        }
    }

    let main_width = budget.available - used(&out);
    Solution { columns: out, main_width }
}

// ── Chat-mode policy ──────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SidebarRequest {
    /// The user's saved full width (180–400).
    pub width: f64,
    /// The user snapped the sidebar to rail mode on purpose.
    pub user_railed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DrawerRequest {
    /// The user's saved drawer width.
    pub width: f64,
    /// The drawer has an inner nav that can rail (Settings does; History
    /// and MCP don't).
    pub has_inner_nav: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChatLayoutInput {
    pub window_width: f64,
    /// `None` when the tab sidebar isn't showing (top-tabs mode).
    pub sidebar: Option<SidebarRequest>,
    /// `None` when no drawer is open.
    pub drawer: Option<DrawerRequest>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarMode {
    Hidden,
    Full,
    /// Rail because the user chose it (persisted).
    UserRail,
    /// Rail because the window ran out of room (temporary, never persisted).
    AutoRail,
}

impl SidebarMode {
    pub fn is_rail(self) -> bool {
        matches!(self, SidebarMode::UserRail | SidebarMode::AutoRail)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChatLayout {
    pub sidebar_mode: SidebarMode,
    /// 0 when hidden.
    pub sidebar_width: f64,
    /// `None` when no drawer is open.
    pub drawer_width: Option<f64>,
    /// The drawer's inner nav shows as an icon rail — because the solver
    /// needed the room, or because the drawer is narrower than
    /// [`DRAWER_NAV_RAIL_BELOW`].
    pub drawer_nav_railed: bool,
    pub main_width: f64,
}

/// Chat-mode layout: drawer nav rails first, then the sidebar rails, then
/// the drawer shrinks toward 420px; the chat column keeps 520px.
pub fn solve_chat(input: &ChatLayoutInput) -> ChatLayout {
    let mut columns = Vec::new();
    let mut order = Vec::new();

    let sidebar_idx = input.sidebar.map(|s| {
        columns.push(if s.user_railed {
            ColumnSpec {
                requested: SIDEBAR_RAIL_WIDTH,
                min: SIDEBAR_RAIL_WIDTH,
                collapse: Collapse::None,
            }
        } else {
            ColumnSpec {
                requested: s.width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH),
                min: SIDEBAR_MIN_WIDTH,
                collapse: Collapse::Rail { width: SIDEBAR_RAIL_WIDTH },
            }
        });
        columns.len() - 1
    });
    let drawer_idx = input.drawer.map(|d| {
        columns.push(ColumnSpec {
            requested: d.width.clamp(DRAWER_MIN_WIDTH, DRAWER_MAX_WIDTH),
            min: DRAWER_MIN_WIDTH,
            collapse: if d.has_inner_nav {
                Collapse::InnerRail { saving: DRAWER_NAV_FULL_WIDTH - DRAWER_NAV_RAIL_WIDTH }
            } else {
                Collapse::None
            },
        });
        columns.len() - 1
    });

    if let Some(d) = drawer_idx {
        order.push(GiveWay::Rail(d));
    }
    if let Some(s) = sidebar_idx {
        order.push(GiveWay::Rail(s));
    }
    if let Some(d) = drawer_idx {
        order.push(GiveWay::Shrink(d));
    }

    let solution = solve(&Budget {
        available: input.window_width,
        main_min: CHAT_MIN_WIDTH,
        columns,
        order,
    });

    let (sidebar_mode, sidebar_width) = match (input.sidebar, sidebar_idx) {
        (Some(s), Some(i)) => {
            let c = solution.columns[i];
            let mode = if s.user_railed {
                SidebarMode::UserRail
            } else if c.railed {
                SidebarMode::AutoRail
            } else {
                SidebarMode::Full
            };
            (mode, c.width)
        }
        _ => (SidebarMode::Hidden, 0.0),
    };
    let (drawer_width, drawer_nav_railed) = match (input.drawer, drawer_idx) {
        (Some(d), Some(i)) => {
            let c = solution.columns[i];
            let railed = d.has_inner_nav && (c.railed || c.width < DRAWER_NAV_RAIL_BELOW);
            (Some(c.width), railed)
        }
        _ => (None, false),
    };

    ChatLayout {
        sidebar_mode,
        sidebar_width,
        drawer_width,
        drawer_nav_railed,
        main_width: solution.main_width,
    }
}

// ── Planner policy ────────────────────────────────────────────────────────

/// Planner nav (smart lists + projects): resizable range, reset default and
/// icon-rail width.
pub const PLANNER_NAV_MIN_WIDTH: f64 = 160.0;
pub const PLANNER_NAV_MAX_WIDTH: f64 = 320.0;
pub const PLANNER_NAV_DEFAULT_WIDTH: f64 = 220.0;
pub const PLANNER_NAV_RAIL_WIDTH: f64 = 52.0;
/// Day timeline column: resizable range and reset default.
pub const PLANNER_TIMELINE_MIN_WIDTH: f64 = 300.0;
pub const PLANNER_TIMELINE_MAX_WIDTH: f64 = 520.0;
pub const PLANNER_TIMELINE_DEFAULT_WIDTH: f64 = 380.0;
/// The task list never drops below this.
pub const PLANNER_MAIN_MIN_WIDTH: f64 = 520.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlannerLayoutInput {
    pub window_width: f64,
    /// `None` when the tab sidebar isn't showing (top-tabs mode).
    pub sidebar: Option<SidebarRequest>,
    /// A Settings/History/MCP drawer's width, if one is open. The Planner
    /// policy never shrinks it; it just takes its space first.
    pub drawer_width: Option<f64>,
    /// The user's saved Planner nav width.
    pub nav_width: f64,
    /// The timeline column only exists on the Today view.
    pub show_timeline: bool,
    /// The user's saved timeline width.
    pub timeline_width: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlannerLayout {
    pub sidebar_mode: SidebarMode,
    /// 0 when hidden.
    pub sidebar_width: f64,
    pub drawer_width: Option<f64>,
    /// The Settings nav inside an open drawer shows as an icon rail
    /// (drawer narrower than [`DRAWER_NAV_RAIL_BELOW`]); the Planner policy
    /// never rails it for space.
    pub drawer_nav_railed: bool,
    /// The Planner nav shows its 52px icon rail (temporary, never persisted).
    pub nav_railed: bool,
    /// Effective nav width (the rail width when railed).
    pub nav_width: f64,
    /// 0 when the timeline isn't showing.
    pub timeline_width: f64,
    pub main_width: f64,
}

/// Planner layout: the Planner nav rails first, then the tab sidebar rails,
/// then the timeline shrinks toward 300px; the task list keeps 520px.
pub fn solve_planner(input: &PlannerLayoutInput) -> PlannerLayout {
    let mut columns = Vec::new();
    let mut order = Vec::new();

    let drawer_idx = input.drawer_width.map(|w| {
        let w = w.clamp(DRAWER_MIN_WIDTH, DRAWER_MAX_WIDTH);
        columns.push(ColumnSpec { requested: w, min: w, collapse: Collapse::None });
        columns.len() - 1
    });
    let sidebar_idx = input.sidebar.map(|s| {
        columns.push(if s.user_railed {
            ColumnSpec {
                requested: SIDEBAR_RAIL_WIDTH,
                min: SIDEBAR_RAIL_WIDTH,
                collapse: Collapse::None,
            }
        } else {
            ColumnSpec {
                requested: s.width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH),
                min: SIDEBAR_MIN_WIDTH,
                collapse: Collapse::Rail { width: SIDEBAR_RAIL_WIDTH },
            }
        });
        columns.len() - 1
    });
    columns.push(ColumnSpec {
        requested: input.nav_width.clamp(PLANNER_NAV_MIN_WIDTH, PLANNER_NAV_MAX_WIDTH),
        min: PLANNER_NAV_MIN_WIDTH,
        collapse: Collapse::Rail { width: PLANNER_NAV_RAIL_WIDTH },
    });
    let nav_idx = columns.len() - 1;
    let timeline_idx = input.show_timeline.then(|| {
        columns.push(ColumnSpec {
            requested: input
                .timeline_width
                .clamp(PLANNER_TIMELINE_MIN_WIDTH, PLANNER_TIMELINE_MAX_WIDTH),
            min: PLANNER_TIMELINE_MIN_WIDTH,
            collapse: Collapse::None,
        });
        columns.len() - 1
    });

    order.push(GiveWay::Rail(nav_idx));
    if let Some(s) = sidebar_idx {
        order.push(GiveWay::Rail(s));
    }
    if let Some(t) = timeline_idx {
        order.push(GiveWay::Shrink(t));
    }

    let solution = solve(&Budget {
        available: input.window_width,
        main_min: PLANNER_MAIN_MIN_WIDTH,
        columns,
        order,
    });

    let (sidebar_mode, sidebar_width) = match (input.sidebar, sidebar_idx) {
        (Some(s), Some(i)) => {
            let c = solution.columns[i];
            let mode = if s.user_railed {
                SidebarMode::UserRail
            } else if c.railed {
                SidebarMode::AutoRail
            } else {
                SidebarMode::Full
            };
            (mode, c.width)
        }
        _ => (SidebarMode::Hidden, 0.0),
    };
    let nav = solution.columns[nav_idx];
    let drawer = drawer_idx.map(|i| solution.columns[i].width);

    PlannerLayout {
        sidebar_mode,
        sidebar_width,
        drawer_width: drawer,
        drawer_nav_railed: drawer.is_some_and(|w| w < DRAWER_NAV_RAIL_BELOW),
        nav_railed: nav.railed,
        nav_width: nav.width,
        timeline_width: timeline_idx.map_or(0.0, |i| solution.columns[i].width),
        main_width: solution.main_width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(window: f64, sidebar: bool, drawer: Option<(f64, bool)>) -> ChatLayoutInput {
        ChatLayoutInput {
            window_width: window,
            sidebar: sidebar.then_some(SidebarRequest { width: 256.0, user_railed: false }),
            drawer: drawer.map(|(width, has_inner_nav)| DrawerRequest { width, has_inner_nav }),
        }
    }

    #[test]
    fn roomy_windows_take_no_steps() {
        for w in [2000.0, 1440.0, 1280.0] {
            let l = solve_chat(&input(w, true, Some((420.0, true))));
            assert_eq!(l.sidebar_mode, SidebarMode::Full, "{w}");
            assert_eq!(l.sidebar_width, 256.0);
            assert_eq!(l.drawer_width, Some(420.0));
            // Nav still rails: the drawer is narrower than 600.
            assert!(l.drawer_nav_railed);
            assert_eq!(l.main_width, w - 676.0);
        }
    }

    #[test]
    fn wide_drawer_keeps_full_nav_when_roomy() {
        let l = solve_chat(&input(2000.0, true, Some((700.0, true))));
        assert_eq!(l.drawer_width, Some(700.0));
        assert!(!l.drawer_nav_railed);
        assert_eq!(l.sidebar_mode, SidebarMode::Full);
        assert_eq!(l.main_width, 2000.0 - 956.0);
    }

    #[test]
    fn drawer_closed_never_constrains_at_common_sizes() {
        for w in [2000.0, 1440.0, 1280.0, 1024.0] {
            for sidebar in [true, false] {
                let l = solve_chat(&input(w, sidebar, None));
                assert_eq!(l.drawer_width, None);
                assert!(!l.drawer_nav_railed);
                if sidebar {
                    assert_eq!(l.sidebar_mode, SidebarMode::Full, "{w}");
                    assert_eq!(l.main_width, w - 256.0);
                } else {
                    assert_eq!(l.sidebar_mode, SidebarMode::Hidden);
                    assert_eq!(l.sidebar_width, 0.0);
                    assert_eq!(l.main_width, w);
                }
            }
        }
    }

    #[test]
    fn sidebar_off_drawer_open_fits_down_to_1024() {
        for w in [2000.0, 1440.0, 1280.0, 1024.0] {
            let l = solve_chat(&input(w, false, Some((420.0, true))));
            assert_eq!(l.drawer_width, Some(420.0));
            assert!(l.main_width >= CHAT_MIN_WIDTH, "{w}");
        }
    }

    #[test]
    fn at_1024_with_sidebar_and_drawer_the_sidebar_rails() {
        let l = solve_chat(&input(1024.0, true, Some((420.0, true))));
        // Drawer already at its floor: the nav rail frees nothing, so the
        // sidebar rails next.
        assert!(l.drawer_nav_railed);
        assert_eq!(l.sidebar_mode, SidebarMode::AutoRail);
        assert_eq!(l.sidebar_width, SIDEBAR_RAIL_WIDTH);
        assert_eq!(l.drawer_width, Some(420.0));
        assert_eq!(l.main_width, 1024.0 - 48.0 - 420.0);
        assert!(l.main_width >= CHAT_MIN_WIDTH);
    }

    #[test]
    fn nav_rail_comes_first_and_takes_only_what_is_needed() {
        // 256 + 800 + 520 = 1576; at 1500 we're 76 over. The nav rail alone
        // covers it, so the sidebar stays full.
        let l = solve_chat(&input(1500.0, true, Some((800.0, true))));
        assert!(l.drawer_nav_railed);
        assert_eq!(l.sidebar_mode, SidebarMode::Full);
        assert_eq!(l.drawer_width, Some(724.0));
        assert_eq!(l.main_width, CHAT_MIN_WIDTH);
    }

    #[test]
    fn sidebar_rails_second_then_drawer_shrinks() {
        // 1280: 256 + 800 + 520 = 1576, 296 over. Nav rail −148, sidebar rail
        // −208: 60 to spare, drawer stays 652.
        let l = solve_chat(&input(1280.0, true, Some((800.0, true))));
        assert_eq!(l.sidebar_mode, SidebarMode::AutoRail);
        assert_eq!(l.drawer_width, Some(652.0));
        assert_eq!(l.main_width, 1280.0 - 48.0 - 652.0);

        // 1024: after both rails still 196 over → drawer shrinks to 456.
        let l = solve_chat(&input(1024.0, true, Some((800.0, true))));
        assert_eq!(l.sidebar_mode, SidebarMode::AutoRail);
        assert_eq!(l.drawer_width, Some(456.0));
        assert_eq!(l.main_width, CHAT_MIN_WIDTH);
    }

    #[test]
    fn drawer_without_inner_nav_skips_the_nav_step() {
        let l = solve_chat(&input(1500.0, true, Some((800.0, false))));
        assert!(!l.drawer_nav_railed);
        assert_eq!(l.sidebar_mode, SidebarMode::AutoRail);
        assert_eq!(l.drawer_width, Some(800.0));
        assert_eq!(l.main_width, 1500.0 - 848.0);

        // History/MCP never report a railed nav, even narrow.
        let l = solve_chat(&input(2000.0, true, Some((420.0, false))));
        assert!(!l.drawer_nav_railed);
    }

    #[test]
    fn drawer_floor_holds_even_if_chat_must_go_under() {
        let l = solve_chat(&input(800.0, true, Some((700.0, true))));
        assert_eq!(l.sidebar_mode, SidebarMode::AutoRail);
        assert_eq!(l.drawer_width, Some(DRAWER_MIN_WIDTH));
        assert_eq!(l.main_width, 800.0 - 48.0 - 420.0);
    }

    #[test]
    fn user_rail_is_reported_as_such_and_never_auto() {
        let mut i = input(2000.0, true, None);
        i.sidebar = Some(SidebarRequest { width: 300.0, user_railed: true });
        let l = solve_chat(&i);
        assert_eq!(l.sidebar_mode, SidebarMode::UserRail);
        assert_eq!(l.sidebar_width, SIDEBAR_RAIL_WIDTH);
        assert_eq!(l.main_width, 2000.0 - 48.0);
    }

    #[test]
    fn saved_widths_come_back_when_space_returns() {
        let req = |w| ChatLayoutInput {
            window_width: w,
            sidebar: Some(SidebarRequest { width: 320.0, user_railed: false }),
            drawer: Some(DrawerRequest { width: 640.0, has_inner_nav: true }),
        };
        let tight = solve_chat(&req(1024.0));
        assert_eq!(tight.sidebar_mode, SidebarMode::AutoRail);
        assert!(tight.drawer_width.unwrap() < 640.0);
        let roomy = solve_chat(&req(2000.0));
        assert_eq!(roomy.sidebar_mode, SidebarMode::Full);
        assert_eq!(roomy.sidebar_width, 320.0);
        assert_eq!(roomy.drawer_width, Some(640.0));
        assert!(!roomy.drawer_nav_railed);

        // Closing the drawer also gives the sidebar back.
        let closed = solve_chat(&ChatLayoutInput { drawer: None, ..req(1024.0) });
        assert_eq!(closed.sidebar_mode, SidebarMode::Full);
        assert_eq!(closed.sidebar_width, 320.0);
    }

    #[test]
    fn requests_are_clamped_to_ranges() {
        let l = solve_chat(&ChatLayoutInput {
            window_width: 3000.0,
            sidebar: Some(SidebarRequest { width: 999.0, user_railed: false }),
            drawer: Some(DrawerRequest { width: 100.0, has_inner_nav: false }),
        });
        assert_eq!(l.sidebar_width, SIDEBAR_MAX_WIDTH);
        assert_eq!(l.drawer_width, Some(DRAWER_MIN_WIDTH));
    }

    #[test]
    fn unknown_window_width_leaves_requests_alone() {
        let l = solve_chat(&input(0.0, true, Some((800.0, true))));
        assert_eq!(l.sidebar_mode, SidebarMode::Full);
        assert_eq!(l.drawer_width, Some(800.0));
        assert!(!l.drawer_nav_railed);
    }

    #[test]
    fn generic_solver_supports_a_planner_style_policy() {
        // Sidebar, planner nav (rail 52), timeline (shrink to 300); task list ≥ 520.
        let budget = |w| Budget {
            available: w,
            main_min: 520.0,
            columns: vec![
                ColumnSpec { requested: 256.0, min: 180.0, collapse: Collapse::Rail { width: 48.0 } },
                ColumnSpec { requested: 208.0, min: 160.0, collapse: Collapse::Rail { width: 52.0 } },
                ColumnSpec { requested: 320.0, min: 300.0, collapse: Collapse::None },
            ],
            order: vec![GiveWay::Rail(1), GiveWay::Rail(0), GiveWay::Shrink(2)],
        };
        let s = solve(&budget(1440.0));
        assert!(s.columns.iter().all(|c| !c.railed));
        let s = solve(&budget(1200.0)); // 1304 → 104 over: planner nav rails
        assert!(s.columns[1].railed && !s.columns[0].railed);
        assert_eq!(s.columns[1].width, 52.0);
        let s = solve(&budget(1024.0)); // still over after nav rail → sidebar rails
        assert!(s.columns[0].railed && s.columns[1].railed);
        assert_eq!(s.main_width, 1024.0 - 48.0 - 52.0 - 320.0);
        let s = solve(&budget(900.0)); // timeline shrinks to its floor
        assert_eq!(s.columns[2].width, 300.0);
        assert_eq!(s.main_width, 900.0 - 400.0);
    }

    #[test]
    fn shrink_skips_a_railed_column_and_bad_indices() {
        let s = solve(&Budget {
            available: 600.0,
            main_min: 520.0,
            columns: vec![ColumnSpec {
                requested: 256.0,
                min: 180.0,
                collapse: Collapse::Rail { width: 48.0 },
            }],
            order: vec![GiveWay::Rail(0), GiveWay::Shrink(0), GiveWay::Shrink(7), GiveWay::Rail(9)],
        });
        assert_eq!(s.columns[0].width, 48.0);
        assert_eq!(s.main_width, 552.0);
    }
    // ── Planner policy ──

    fn planner(window: f64, sidebar: bool) -> PlannerLayout {
        solve_planner(&PlannerLayoutInput {
            window_width: window,
            sidebar: sidebar.then_some(SidebarRequest { width: 256.0, user_railed: false }),
            drawer_width: None,
            nav_width: PLANNER_NAV_DEFAULT_WIDTH,
            show_timeline: true,
            timeline_width: PLANNER_TIMELINE_DEFAULT_WIDTH,
        })
    }

    #[test]
    fn planner_roomy_windows_take_no_steps() {
        for w in [2000.0, 1440.0] {
            let l = planner(w, true);
            assert_eq!(l.sidebar_mode, SidebarMode::Full, "{w}");
            assert!(!l.nav_railed);
            assert_eq!(l.nav_width, 220.0);
            assert_eq!(l.timeline_width, 380.0);
            assert_eq!(l.main_width, w - 256.0 - 220.0 - 380.0);
        }
    }

    #[test]
    fn planner_1280_rails_only_the_nav_with_sidebar() {
        let l = planner(1280.0, true);
        assert!(l.nav_railed);
        assert_eq!(l.nav_width, 52.0);
        assert_eq!(l.sidebar_mode, SidebarMode::Full);
        assert_eq!(l.timeline_width, 380.0);
        assert_eq!(l.main_width, 1280.0 - 256.0 - 52.0 - 380.0);
        assert!(l.main_width >= PLANNER_MAIN_MIN_WIDTH);
    }

    #[test]
    fn planner_1024_rails_nav_then_sidebar() {
        let l = planner(1024.0, true);
        assert!(l.nav_railed);
        assert_eq!(l.sidebar_mode, SidebarMode::AutoRail);
        assert_eq!(l.sidebar_width, 48.0);
        assert_eq!(l.timeline_width, 380.0);
        assert_eq!(l.main_width, 1024.0 - 48.0 - 52.0 - 380.0);
        assert!(l.main_width >= PLANNER_MAIN_MIN_WIDTH);
    }

    #[test]
    fn planner_without_sidebar_needs_only_the_nav_rail_at_1024() {
        for w in [2000.0, 1440.0, 1280.0] {
            let l = planner(w, false);
            assert_eq!(l.sidebar_mode, SidebarMode::Hidden);
            assert_eq!(l.sidebar_width, 0.0);
            assert!(!l.nav_railed, "{w}");
            assert_eq!(l.main_width, w - 220.0 - 380.0);
        }
        let l = planner(1024.0, false);
        assert!(l.nav_railed);
        assert_eq!(l.timeline_width, 380.0);
        assert_eq!(l.main_width, 1024.0 - 52.0 - 380.0);
    }

    #[test]
    fn planner_timeline_shrinks_last_and_floor_holds() {
        let l = solve_planner(&PlannerLayoutInput {
            window_width: 900.0,
            sidebar: Some(SidebarRequest { width: 256.0, user_railed: false }),
            drawer_width: None,
            nav_width: 220.0,
            show_timeline: true,
            timeline_width: 520.0,
        });
        assert!(l.nav_railed);
        assert_eq!(l.sidebar_mode, SidebarMode::AutoRail);
        assert_eq!(l.timeline_width, PLANNER_TIMELINE_MIN_WIDTH);
    }

    #[test]
    fn planner_without_timeline_reserves_nothing_for_it() {
        let l = solve_planner(&PlannerLayoutInput {
            window_width: 1280.0,
            sidebar: Some(SidebarRequest { width: 256.0, user_railed: false }),
            drawer_width: None,
            nav_width: 220.0,
            show_timeline: false,
            timeline_width: 380.0,
        });
        assert!(!l.nav_railed);
        assert_eq!(l.timeline_width, 0.0);
        assert_eq!(l.main_width, 1280.0 - 256.0 - 220.0);
    }

    #[test]
    fn planner_saved_widths_come_back_when_space_returns() {
        let tight = planner(1024.0, true);
        assert!(tight.nav_railed);
        let roomy = planner(2000.0, true);
        assert!(!roomy.nav_railed);
        assert_eq!(roomy.nav_width, 220.0);
        assert_eq!(roomy.sidebar_mode, SidebarMode::Full);
    }

    #[test]
    fn planner_open_drawer_takes_space_first() {
        let l = solve_planner(&PlannerLayoutInput {
            window_width: 1440.0,
            sidebar: Some(SidebarRequest { width: 256.0, user_railed: false }),
            drawer_width: Some(420.0),
            nav_width: 220.0,
            show_timeline: true,
            timeline_width: 380.0,
        });
        assert_eq!(l.drawer_width, Some(420.0));
        assert!(l.drawer_nav_railed);
        assert!(l.nav_railed);
        assert_eq!(l.sidebar_mode, SidebarMode::AutoRail);
    }

    #[test]
    fn planner_unknown_window_width_leaves_requests_alone() {
        let l = planner(0.0, true);
        assert!(!l.nav_railed);
        assert_eq!(l.sidebar_mode, SidebarMode::Full);
        assert_eq!(l.timeline_width, 380.0);
    }
}
