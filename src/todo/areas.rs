//! Area management: the Planner nav's group headers. Pure state changes on
//! `PlannerState`; callers persist what each returns (Manage projects writes
//! the rows off the UI thread, the AI upsert path inline).
//!
//! Areas are matched by name the way projects are: normalized (case,
//! punctuation and spacing insensitive — `project_identity::normalize`), so
//! "Deep work" and "deep-work" are the same area and the AI's upsert updates
//! the existing one instead of creating a duplicate.

use chrono::{DateTime, Utc};

use super::model::{Area, SORT_STEP};
use super::project_identity::normalize;
use super::PlannerState;

/// What deleting an area touched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AreaDeleteOutcome {
    /// Projects whose `area_id` was cleared (they move to No area).
    pub projects: Vec<String>,
    /// To-dos whose `area_id` was cleared.
    pub todos: Vec<String>,
}

/// The existing area whose normalized title equals `name`'s, if any.
pub fn area_by_name<'a>(areas: &'a [Area], name: &str) -> Option<&'a Area> {
    let key = normalize(name);
    if key.is_empty() {
        return None;
    }
    areas.iter().find(|a| normalize(&a.title) == key)
}

impl PlannerState {
    /// Areas in nav order.
    pub fn areas_sorted(&self) -> Vec<&Area> {
        let mut v: Vec<&Area> = self.areas.iter().collect();
        v.sort_by(|a, b| a.sort_order.total_cmp(&b.sort_order).then_with(|| a.title.cmp(&b.title)));
        v
    }

    /// Create an area, or return the existing one with the same normalized
    /// name. `Ok((id, created))`; an empty title is refused.
    pub fn create_area(&mut self, title: &str, now: DateTime<Utc>) -> Result<(String, bool), String> {
        let title = title.trim();
        if title.is_empty() {
            return Err("an area needs a name".to_string());
        }
        if let Some(existing) = area_by_name(&self.areas, title) {
            return Ok((existing.id.clone(), false));
        }
        let sort_order = self.areas.iter().map(|a| a.sort_order).fold(0.0f64, f64::max) + SORT_STEP;
        let area = Area {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.to_string(),
            sort_order,
            created_at: now,
            updated_at: now,
        };
        let id = area.id.clone();
        self.areas.push(area);
        Ok((id, true))
    }

    /// Rename an area. Refused when the new name is empty or already names
    /// a different area.
    pub fn rename_area(&mut self, id: &str, title: &str, now: DateTime<Utc>) -> Result<(), String> {
        let title = title.trim();
        if title.is_empty() {
            return Err("an area needs a name".to_string());
        }
        if let Some(other) = area_by_name(&self.areas, title).filter(|a| a.id != id) {
            return Err(format!("\u{201c}{}\u{201d} already names another area", other.title));
        }
        let area = self
            .areas
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or_else(|| format!("unknown area id '{id}'"))?;
        area.title = title.to_string();
        area.updated_at = now;
        Ok(())
    }

    /// Move an area one place up (`up`) or down in nav order. Renumbers
    /// every area's `sort_order` to `SORT_STEP` multiples so the order is
    /// unambiguous; returns the ids whose `sort_order` changed (to persist).
    pub fn move_area(&mut self, id: &str, up: bool, now: DateTime<Utc>) -> Vec<String> {
        let mut order: Vec<String> = self.areas_sorted().iter().map(|a| a.id.clone()).collect();
        let Some(i) = order.iter().position(|a| a == id) else { return Vec::new() };
        let j = if up {
            match i.checked_sub(1) {
                Some(j) => j,
                None => return Vec::new(),
            }
        } else if i + 1 < order.len() {
            i + 1
        } else {
            return Vec::new();
        };
        order.swap(i, j);
        let mut changed = Vec::new();
        for (n, aid) in order.iter().enumerate() {
            let want = (n as f64 + 1.0) * SORT_STEP;
            if let Some(a) = self.areas.iter_mut().find(|a| &a.id == aid) {
                if a.sort_order != want {
                    a.sort_order = want;
                    a.updated_at = now;
                    changed.push(aid.clone());
                }
            }
        }
        changed
    }

    /// Delete an area: its projects (and any to-dos filed directly under it)
    /// move to No area. Nothing else is deleted.
    pub fn delete_area(&mut self, id: &str, now: DateTime<Utc>) -> Result<AreaDeleteOutcome, String> {
        let before = self.areas.len();
        self.areas.retain(|a| a.id != id);
        if self.areas.len() == before {
            return Err(format!("unknown area id '{id}'"));
        }
        let mut out = AreaDeleteOutcome::default();
        for p in self.projects.iter_mut().filter(|p| p.area_id.as_deref() == Some(id)) {
            p.area_id = None;
            p.updated_at = now;
            out.projects.push(p.id.clone());
        }
        for t in self.todos.iter_mut().filter(|t| t.area_id.as_deref() == Some(id)) {
            t.area_id = None;
            t.updated_at = now;
            out.todos.push(t.id.clone());
        }
        Ok(out)
    }

    /// File `project_ids` under `area_id` (`None` = No area). Returns the
    /// projects that actually changed.
    pub fn assign_area(
        &mut self,
        project_ids: &[String],
        area_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, String> {
        if let Some(aid) = area_id {
            if !self.areas.iter().any(|a| a.id == aid) {
                return Err(format!("unknown area id '{aid}'"));
            }
        }
        let mut changed = Vec::new();
        for p in self.projects.iter_mut().filter(|p| project_ids.contains(&p.id)) {
            if p.area_id.as_deref() != area_id {
                p.area_id = area_id.map(str::to_string);
                p.updated_at = now;
                changed.push(p.id.clone());
            }
        }
        Ok(changed)
    }

    /// Projects filed under `area_id`.
    pub fn area_project_count(&self, area_id: &str) -> usize {
        self.projects.iter().filter(|p| p.area_id.as_deref() == Some(area_id)).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::model::{Project, Todo, TodoStatus};

    fn project(id: &str, area: Option<&str>) -> Project {
        Project {
            id: id.into(),
            title: id.into(),
            notes: String::new(),
            area_id: area.map(str::to_string),
            status: TodoStatus::Open,
            deadline: None,
            sort_order: 0.0,
            path: None,
            aliases: Vec::new(),
            distinct_from: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn create_matches_an_existing_area_by_normalized_name() {
        let mut s = PlannerState::default();
        let (work, created) = s.create_area("Deep Work", Utc::now()).unwrap();
        assert!(created);
        let (again, created) = s.create_area("  deep-work ", Utc::now()).unwrap();
        assert!(!created);
        assert_eq!(again, work);
        assert_eq!(s.areas.len(), 1);
        assert!(s.create_area("   ", Utc::now()).is_err());
        let (home, _) = s.create_area("Home", Utc::now()).unwrap();
        assert!(s.areas.iter().find(|a| a.id == home).unwrap().sort_order > s.areas[0].sort_order);
    }

    #[test]
    fn rename_refuses_a_name_another_area_has() {
        let mut s = PlannerState::default();
        let (a, _) = s.create_area("Work", Utc::now()).unwrap();
        s.create_area("Home", Utc::now()).unwrap();
        assert!(s.rename_area(&a, "home", Utc::now()).is_err());
        s.rename_area(&a, "Office", Utc::now()).unwrap();
        assert_eq!(s.areas[0].title, "Office");
        // Renaming to its own name in another case is fine.
        s.rename_area(&a, "office", Utc::now()).unwrap();
    }

    #[test]
    fn move_area_reorders_and_reports_changes() {
        let mut s = PlannerState::default();
        let (a, _) = s.create_area("A", Utc::now()).unwrap();
        let (b, _) = s.create_area("B", Utc::now()).unwrap();
        let (c, _) = s.create_area("C", Utc::now()).unwrap();
        let changed = s.move_area(&c, true, Utc::now());
        let order: Vec<&str> = s.areas_sorted().iter().map(|x| x.title.as_str()).collect();
        assert_eq!(order, vec!["A", "C", "B"]);
        assert!(changed.contains(&b) && changed.contains(&c), "{changed:?}");
        assert!(!changed.contains(&a));
        // Edges are no-ops.
        assert!(s.move_area(&a, true, Utc::now()).is_empty());
        assert!(s.move_area(&b, false, Utc::now()).is_empty());
        s.move_area(&a, false, Utc::now());
        let order: Vec<&str> = s.areas_sorted().iter().map(|x| x.title.as_str()).collect();
        assert_eq!(order, vec!["C", "A", "B"]);
    }

    #[test]
    fn delete_area_moves_its_projects_and_todos_to_no_area() {
        let mut s = PlannerState::default();
        let (w, _) = s.create_area("Work", Utc::now()).unwrap();
        let (h, _) = s.create_area("Home", Utc::now()).unwrap();
        s.projects = vec![project("p1", Some(&w)), project("p2", Some(&h)), project("p3", Some(&w))];
        let mut t = Todo::new("t", 0.0);
        t.area_id = Some(w.clone());
        s.todos.push(t);
        let out = s.delete_area(&w, Utc::now()).unwrap();
        assert_eq!(out.projects, vec!["p1", "p3"]);
        assert_eq!(out.todos.len(), 1);
        assert_eq!(s.areas.len(), 1);
        assert!(s.projects.iter().filter(|p| p.id != "p2").all(|p| p.area_id.is_none()));
        assert_eq!(s.projects[1].area_id.as_deref(), Some(h.as_str()));
        assert!(s.todos[0].area_id.is_none());
        assert!(s.delete_area(&w, Utc::now()).is_err());
    }

    #[test]
    fn assign_area_files_and_clears() {
        let mut s = PlannerState::default();
        let (w, _) = s.create_area("Work", Utc::now()).unwrap();
        s.projects = vec![project("p1", None), project("p2", Some(&w)), project("p3", None)];
        let ids = vec!["p1".to_string(), "p2".to_string()];
        let changed = s.assign_area(&ids, Some(&w), Utc::now()).unwrap();
        assert_eq!(changed, vec!["p1"], "p2 was already there");
        assert_eq!(s.area_project_count(&w), 2);
        let changed = s.assign_area(&ids, None, Utc::now()).unwrap();
        assert_eq!(changed, vec!["p1", "p2"]);
        assert_eq!(s.area_project_count(&w), 0);
        assert!(s.assign_area(&ids, Some("nope"), Utc::now()).is_err());
    }
}
