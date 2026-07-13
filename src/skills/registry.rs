use super::parser::Skill;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Lightweight skill descriptor for prompt injection (model invocation).
#[derive(Debug, Clone, PartialEq)]
pub struct AvailableSkill {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub disable_model_invocation: bool,
}

/// Process-wide mirror of the loaded skills, readable from non-UI code (the
/// prompt builder) without threading a Signal through every call site. Updated
/// whenever `reload_into_signal` refreshes the registry — the same OnceLock
/// pattern used by the shared Gemini cache store.
static AVAILABLE_SKILLS: std::sync::OnceLock<RwLock<Vec<AvailableSkill>>> =
    std::sync::OnceLock::new();

fn available_skills_cell() -> &'static RwLock<Vec<AvailableSkill>> {
    AVAILABLE_SKILLS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Snapshot of all loaded skills' metadata (name, description, hint).
pub fn available_skills_snapshot() -> Vec<AvailableSkill> {
    available_skills_cell()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

#[cfg(test)]
pub(crate) fn set_available_skills_for_test(skills: Vec<AvailableSkill>) {
    *available_skills_cell()
        .write()
        .unwrap_or_else(|p| p.into_inner()) = skills;
}

/// A skill directory that was found on disk but failed to parse (bad YAML,
/// missing frontmatter, unreadable file). Surfaced in the Settings UI instead
/// of being silently dropped.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillLoadError {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    // Map of skill name to Skill
    pub skills: Arc<RwLock<HashMap<String, Skill>>>,
    pub loaded_paths: Arc<RwLock<Vec<PathBuf>>>,
    pub load_errors: Arc<RwLock<Vec<SkillLoadError>>>,
}

/// Returns all canonical skill directories (global + platform-specific).
/// - Global: `~/.hobbes/skills` (cross-platform, user-facing)
/// - Platform: `data_local_dir/com.clearmirror.hobbes/skills` (macOS Application Support, etc.)
pub fn get_skills_directories() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // 1. Global: ~/.hobbes/skills
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".hobbes").join("skills"));
    }

    // 2. Platform-specific: ~/Library/Application Support/com.clearmirror.hobbes/skills (macOS)
    //    or equivalent data_local_dir on other platforms
    if let Some(data_dir) = dirs::data_local_dir() {
        let platform_dir = data_dir.join("com.clearmirror.hobbes").join("skills");
        // Avoid duplicating if they happen to resolve to the same path
        if !dirs.contains(&platform_dir) {
            dirs.push(platform_dir);
        }
    }

    dirs
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: Arc::new(RwLock::new(HashMap::new())),
            loaded_paths: Arc::new(RwLock::new(Vec::new())),
            load_errors: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Directory where in-app-created skills are written: `~/.hobbes/skills`.
    /// Created on demand.
    pub fn default_create_dir() -> Result<PathBuf, String> {
        let dir = get_skills_directories()
            .into_iter()
            .next()
            .ok_or_else(|| "Could not resolve home directory".to_string())?;
        fs::create_dir_all(&dir).map_err(|e| format!("Failed to create {:?}: {}", dir, e))?;
        Ok(dir)
    }

    /// True if `root` is a direct child of one of the managed skills
    /// directories — the only locations this registry will mutate on disk.
    fn is_managed_path(root: &Path, managed_dirs: &[PathBuf]) -> bool {
        root.parent()
            .map(|parent| managed_dirs.iter().any(|dir| dir.as_path() == parent))
            .unwrap_or(false)
    }

    /// Create a new skill on disk under `default_create_dir()/{name}/SKILL.md`
    /// and insert it into the registry. Rejects duplicate names.
    pub fn create_skill(&self, skill: &Skill) -> Result<Skill, String> {
        let base = Self::default_create_dir()?;
        self.create_skill_in(&base, skill)
    }

    pub(crate) fn create_skill_in(&self, base_dir: &Path, skill: &Skill) -> Result<Skill, String> {
        skill.metadata.validate().map_err(|errs| errs.join("; "))?;
        let name = skill.metadata.name.clone();

        if self.get_skill(&name).is_some() {
            return Err(format!("A skill named '{}' already exists", name));
        }

        let root = base_dir.join(&name);
        if root.exists() {
            return Err(format!(
                "A skill directory already exists at {:?}",
                root
            ));
        }

        fs::create_dir_all(&root).map_err(|e| format!("Failed to create {:?}: {}", root, e))?;
        let file = root.join("SKILL.md");
        let markdown = skill.to_markdown().map_err(|e| e.to_string())?;
        fs::write(&file, markdown).map_err(|e| format!("Failed to write {:?}: {}", file, e))?;

        // Re-parse from disk so the registry holds exactly what was written.
        let parsed = Skill::from_file(&file).map_err(|e| e.to_string())?;
        self.skills
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name, parsed.clone());
        Ok(parsed)
    }

    /// Save an existing skill in place. If the name changed, the skill's root
    /// directory is renamed to match (frontmatter `name` is the source of
    /// truth; `scripts/` and `resources/` move with the directory).
    pub fn save_skill(&self, original_name: &str, skill: &Skill) -> Result<Skill, String> {
        self.save_skill_guarded(original_name, skill, &get_skills_directories())
    }

    pub(crate) fn save_skill_guarded(
        &self,
        original_name: &str,
        skill: &Skill,
        managed_dirs: &[PathBuf],
    ) -> Result<Skill, String> {
        skill.metadata.validate().map_err(|errs| errs.join("; "))?;
        let new_name = skill.metadata.name.clone();

        let existing = self
            .get_skill(original_name)
            .ok_or_else(|| format!("Skill '{}' not found in registry", original_name))?;

        let mut root = existing.root_path.clone();
        if !Self::is_managed_path(&root, managed_dirs) {
            return Err(format!(
                "Refusing to modify skill outside managed skills directories: {:?}",
                root
            ));
        }

        if new_name != original_name {
            if self.get_skill(&new_name).is_some() {
                return Err(format!("A skill named '{}' already exists", new_name));
            }
            let new_root = root
                .parent()
                .ok_or_else(|| "Skill directory has no parent".to_string())?
                .join(&new_name);
            if new_root.exists() {
                return Err(format!(
                    "A skill directory already exists at {:?}",
                    new_root
                ));
            }
            fs::rename(&root, &new_root)
                .map_err(|e| format!("Failed to rename {:?} -> {:?}: {}", root, new_root, e))?;
            root = new_root;
        }

        let file = root.join("SKILL.md");
        let markdown = skill.to_markdown().map_err(|e| e.to_string())?;
        fs::write(&file, markdown).map_err(|e| format!("Failed to write {:?}: {}", file, e))?;

        let parsed = Skill::from_file(&file).map_err(|e| e.to_string())?;
        {
            let mut skills = self.skills.write().unwrap_or_else(|p| p.into_inner());
            if new_name != original_name {
                skills.remove(original_name);
            }
            skills.insert(new_name, parsed.clone());
        }
        Ok(parsed)
    }

    /// Delete a skill's directory from disk and remove it from the registry.
    /// Refuses to touch paths outside the canonical skills directories.
    pub fn delete_skill(&self, name: &str) -> Result<(), String> {
        self.delete_skill_guarded(name, &get_skills_directories())
    }

    pub(crate) fn delete_skill_guarded(
        &self,
        name: &str,
        managed_dirs: &[PathBuf],
    ) -> Result<(), String> {
        let skill = self
            .get_skill(name)
            .ok_or_else(|| format!("Skill '{}' not found in registry", name))?;

        let root = &skill.root_path;
        if !Self::is_managed_path(root, managed_dirs) {
            return Err(format!(
                "Refusing to delete skill outside managed skills directories: {:?}",
                root
            ));
        }

        fs::remove_dir_all(root).map_err(|e| format!("Failed to delete {:?}: {}", root, e))?;
        self.skills
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(name);
        Ok(())
    }

    /// Load skills from all canonical directories (global + platform).
    /// Returns the names of all successfully loaded skills. Per-skill parse
    /// failures are recorded in `load_errors` (and logged), not dropped.
    pub fn load_all(&self) -> Vec<String> {
        self.load_errors
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        let mut all_names = Vec::new();
        for dir in get_skills_directories() {
            if dir.exists() {
                match self.load_from_directory(&dir) {
                    Ok(names) => all_names.extend(names),
                    Err(e) => {
                        tracing::warn!("Failed to load skills from {:?}: {}", dir, e);
                        self.load_errors
                            .write()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(SkillLoadError {
                                path: dir.clone(),
                                error: e,
                            });
                    }
                }
            } else {
                tracing::debug!("Skills directory not found, skipping: {:?}", dir);
            }
        }
        all_names
    }

    /// Reload all skills from canonical directories and replace the contents of
    /// a Dioxus Signal. Encapsulates the spawn_blocking + RwLock swap pattern
    /// used at startup, by the Settings "Reload Skills" button, and after CRUD
    /// operations. The fresh load REPLACES the map — an empty result is
    /// legitimate (e.g. the last skill was deleted); only a join error keeps
    /// the old state.
    pub async fn reload_into_signal(mut signal: dioxus::prelude::Signal<SkillRegistry>) {
        use dioxus_signals::Writable;
        let loaded = tokio::task::spawn_blocking(move || {
            let temp_registry = SkillRegistry::new();
            let names = temp_registry.load_all();
            (temp_registry, names)
        })
        .await;

        match loaded {
            Ok((loaded_registry, names)) => {
                let loaded_skills = loaded_registry
                    .skills
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                let loaded_errors = loaded_registry
                    .load_errors
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                // Refresh the process-wide metadata mirror used by the prompt builder
                let mut available: Vec<AvailableSkill> = loaded_skills
                    .values()
                    .map(|s| AvailableSkill {
                        name: s.metadata.name.clone(),
                        description: s.metadata.description.clone(),
                        argument_hint: s.metadata.argument_hint.clone(),
                        disable_model_invocation: s.metadata.disable_model_invocation,
                    })
                    .collect();
                available.sort_by(|a, b| a.name.cmp(&b.name));
                *available_skills_cell()
                    .write()
                    .unwrap_or_else(|p| p.into_inner()) = available;

                let registry = signal.write();
                *registry.skills.write().unwrap_or_else(|p| p.into_inner()) = loaded_skills;
                *registry
                    .load_errors
                    .write()
                    .unwrap_or_else(|p| p.into_inner()) = loaded_errors;
                tracing::info!("Loaded {} skills: {:?}", names.len(), names);
            }
            Err(e) => {
                tracing::error!("Skill reload task failed; keeping previous registry: {}", e);
            }
        }
    }

    pub fn load_from_directory(&self, dir: &Path) -> Result<Vec<String>, String> {
        if !dir.exists() {
            return Err(format!("Skills directory not found: {:?}", dir));
        }

        let mut loaded_names = Vec::new();
        // Atomic swap pattern: load into local map first (no lock held during I/O),
        // then briefly acquire write lock for the merge. This prevents UI thread
        // blocking when chat_input autocomplete reads skills during reload.
        let mut new_skills = std::collections::HashMap::new();

        let _valid_extensions = &["md", "markdown"];

        // Simple recursive scan (can be improved later)
        // For now, we look for SKILL.md or just .md files that look like skills?
        // Claude specs says "directories containing at least a SKILL.md file".
        // We should look for directories with SKILL.md

        // Strategy 1: Iterate entries, if dir check for SKILL.md
        let entries = fs::read_dir(dir).map_err(|e| e.to_string())?;

        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();

            if path.is_dir() {
                let skill_file = path.join("SKILL.md");
                if skill_file.exists() {
                    match Skill::from_file(&skill_file) {
                        Ok(skill) => {
                            let name = skill.metadata.name.clone();
                            new_skills.insert(name.clone(), skill);
                            loaded_names.push(name);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to parse skill at {:?}: {}", skill_file, e);
                            self.load_errors
                                .write()
                                .unwrap_or_else(|p| p.into_inner())
                                .push(SkillLoadError {
                                    path: skill_file.clone(),
                                    error: e.to_string(),
                                });
                        }
                    }
                }
            }
        }

        // Atomic merge: brief write lock only for HashMap insertion
        {
            let mut skills_lock = self.skills.write().unwrap();
            skills_lock.extend(new_skills);
        }

        // Update loaded paths
        let mut paths_lock = self.loaded_paths.write().unwrap();
        if !paths_lock.contains(&dir.to_path_buf()) {
            paths_lock.push(dir.to_path_buf());
        }

        Ok(loaded_names)
    }

    #[allow(dead_code)]
    pub fn get_skill(&self, name: &str) -> Option<Skill> {
        self.skills
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .cloned()
    }

    pub fn list_skills(&self) -> Vec<Skill> {
        self.skills
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::parser::{Skill, SkillMetadata};
    use std::fs;
    use tempfile::TempDir;

    fn create_test_skill(name: &str, description: &str) -> Skill {
        Skill {
            metadata: SkillMetadata {
                name: name.to_string(),
                description: description.to_string(),
                disable_model_invocation: false,
                user_invocable: true,
                allowed_tools: None,
                argument_hint: None,
            },
            instructions: format!("Instructions for {}", name),
            path: PathBuf::new(),
            root_path: PathBuf::new(),
            scripts: Vec::new(),
            resources: Vec::new(),
        }
    }

    #[test]
    fn test_registry_new() {
        let registry = SkillRegistry::new();
        assert!(registry.list_skills().is_empty());
        assert!(registry.loaded_paths.read().unwrap().is_empty());
    }

    #[test]
    fn test_registry_get_skill_not_found() {
        let registry = SkillRegistry::new();
        assert!(registry.get_skill("nonexistent").is_none());
    }

    #[test]
    fn test_registry_insert_and_get() {
        let registry = SkillRegistry::new();
        let skill = create_test_skill("test-skill", "A test skill");

        // Insert directly for testing
        registry
            .skills
            .write()
            .unwrap()
            .insert("test-skill".to_string(), skill.clone());

        let retrieved = registry.get_skill("test-skill");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().metadata.name, "test-skill");
    }

    #[test]
    fn test_registry_list_skills() {
        let registry = SkillRegistry::new();

        let skill1 = create_test_skill("skill-alpha", "First skill");
        let skill2 = create_test_skill("skill-beta", "Second skill");

        {
            let mut skills = registry.skills.write().unwrap();
            skills.insert("skill-alpha".to_string(), skill1);
            skills.insert("skill-beta".to_string(), skill2);
        }

        let all_skills = registry.list_skills();
        assert_eq!(all_skills.len(), 2);

        let names: Vec<String> = all_skills.iter().map(|s| s.metadata.name.clone()).collect();
        assert!(names.contains(&"skill-alpha".to_string()));
        assert!(names.contains(&"skill-beta".to_string()));
    }

    #[test]
    fn test_load_from_nonexistent_directory() {
        let registry = SkillRegistry::new();
        let result = registry.load_from_directory(Path::new("/nonexistent/path/to/skills"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_load_from_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let registry = SkillRegistry::new();

        let result = registry.load_from_directory(temp_dir.path());
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
        assert!(registry.list_skills().is_empty());
    }

    #[test]
    fn test_load_from_directory_with_skill() {
        let temp_dir = TempDir::new().unwrap();

        // Create a skill directory with SKILL.md
        let skill_dir = temp_dir.path().join("my-test-skill");
        fs::create_dir(&skill_dir).unwrap();

        let skill_content = r#"---
name: my-test-skill
description: A skill for testing
---

# Instructions

This is a test skill.
"#;
        fs::write(skill_dir.join("SKILL.md"), skill_content).unwrap();

        let registry = SkillRegistry::new();
        let result = registry.load_from_directory(temp_dir.path());

        assert!(result.is_ok());
        let loaded = result.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], "my-test-skill");

        // Verify skill was loaded
        let skill = registry.get_skill("my-test-skill");
        assert!(skill.is_some());
        assert_eq!(skill.unwrap().metadata.description, "A skill for testing");
    }

    #[test]
    fn test_load_from_directory_tracks_paths() {
        let temp_dir = TempDir::new().unwrap();
        let registry = SkillRegistry::new();

        registry.load_from_directory(temp_dir.path()).unwrap();

        let paths = registry.loaded_paths.read().unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], temp_dir.path());
    }

    #[test]
    fn test_load_ignores_files_without_skill_md() {
        let temp_dir = TempDir::new().unwrap();

        // Create a directory without SKILL.md
        let not_a_skill = temp_dir.path().join("not-a-skill");
        fs::create_dir(&not_a_skill).unwrap();
        fs::write(not_a_skill.join("README.md"), "# Not a skill").unwrap();

        // Create a plain file (not a directory)
        fs::write(temp_dir.path().join("random.txt"), "random content").unwrap();

        let registry = SkillRegistry::new();
        let result = registry.load_from_directory(temp_dir.path());

        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    /// Canonicalized TempDir path (macOS TempDir is a /var -> /private/var
    /// symlink; Skill::from_file canonicalizes, so guards must compare
    /// canonical parents).
    fn canonical_temp() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        (temp, canonical)
    }

    #[test]
    fn test_create_skill_writes_and_registers() {
        let (_temp, base) = canonical_temp();
        let registry = SkillRegistry::new();
        let skill = create_test_skill("new-skill", "Created in-app");

        let created = registry.create_skill_in(&base, &skill).unwrap();
        assert!(base.join("new-skill").join("SKILL.md").exists());
        assert_eq!(created.metadata.description, "Created in-app");
        assert!(registry.get_skill("new-skill").is_some());
        assert_eq!(created.root_path, base.join("new-skill"));
    }

    #[test]
    fn test_create_skill_rejects_duplicates_and_invalid_names() {
        let (_temp, base) = canonical_temp();
        let registry = SkillRegistry::new();
        let skill = create_test_skill("dup", "First");
        registry.create_skill_in(&base, &skill).unwrap();

        // Duplicate name in registry
        assert!(registry.create_skill_in(&base, &skill).is_err());

        // Invalid names
        for bad in ["My Skill", "a/b", ""] {
            let s = create_test_skill(bad, "desc");
            assert!(registry.create_skill_in(&base, &s).is_err(), "'{}' accepted", bad);
        }

        // Directory collision without registry entry
        std::fs::create_dir_all(base.join("orphan-dir")).unwrap();
        let s = create_test_skill("orphan-dir", "desc");
        assert!(registry.create_skill_in(&base, &s).is_err());
    }

    #[test]
    fn test_save_skill_in_place_edit() {
        let (_temp, base) = canonical_temp();
        let registry = SkillRegistry::new();
        registry
            .create_skill_in(&base, &create_test_skill("edit-me", "Before"))
            .unwrap();

        let mut updated = registry.get_skill("edit-me").unwrap();
        updated.metadata.description = "After".to_string();
        updated.instructions = "New instructions".to_string();

        let managed = vec![base.clone()];
        let saved = registry
            .save_skill_guarded("edit-me", &updated, &managed)
            .unwrap();
        assert_eq!(saved.metadata.description, "After");
        assert_eq!(saved.instructions, "New instructions");
        // On-disk content matches
        let reread = Skill::from_file(&base.join("edit-me").join("SKILL.md")).unwrap();
        assert_eq!(reread.metadata.description, "After");
    }

    #[test]
    fn test_save_skill_rename_moves_directory_and_preserves_assets() {
        let (_temp, base) = canonical_temp();
        let registry = SkillRegistry::new();
        registry
            .create_skill_in(&base, &create_test_skill("old-name", "desc"))
            .unwrap();
        // Add a script that must survive the rename
        let scripts_dir = base.join("old-name").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        std::fs::write(scripts_dir.join("run.sh"), "#!/bin/sh").unwrap();

        let mut renamed = registry.get_skill("old-name").unwrap();
        renamed.metadata.name = "new-name".to_string();

        let managed = vec![base.clone()];
        let saved = registry
            .save_skill_guarded("old-name", &renamed, &managed)
            .unwrap();

        assert!(!base.join("old-name").exists());
        assert!(base.join("new-name").join("SKILL.md").exists());
        assert!(base.join("new-name").join("scripts").join("run.sh").exists());
        assert_eq!(saved.scripts, vec!["run.sh".to_string()]);
        assert!(registry.get_skill("old-name").is_none());
        assert!(registry.get_skill("new-name").is_some());
    }

    #[test]
    fn test_save_skill_rename_collision_rejected() {
        let (_temp, base) = canonical_temp();
        let registry = SkillRegistry::new();
        registry
            .create_skill_in(&base, &create_test_skill("skill-a", "A"))
            .unwrap();
        registry
            .create_skill_in(&base, &create_test_skill("skill-b", "B"))
            .unwrap();

        let mut renamed = registry.get_skill("skill-a").unwrap();
        renamed.metadata.name = "skill-b".to_string();
        let managed = vec![base.clone()];
        assert!(registry
            .save_skill_guarded("skill-a", &renamed, &managed)
            .is_err());
        // Nothing moved
        assert!(base.join("skill-a").exists());
        assert!(registry.get_skill("skill-a").is_some());
    }

    #[test]
    fn test_delete_skill_removes_dir_and_entry() {
        let (_temp, base) = canonical_temp();
        let registry = SkillRegistry::new();
        registry
            .create_skill_in(&base, &create_test_skill("doomed", "desc"))
            .unwrap();

        let managed = vec![base.clone()];
        registry.delete_skill_guarded("doomed", &managed).unwrap();
        assert!(!base.join("doomed").exists());
        assert!(registry.get_skill("doomed").is_none());
    }

    #[test]
    fn test_delete_skill_refuses_unmanaged_paths() {
        let (_temp, base) = canonical_temp();
        let (_other_temp, other) = canonical_temp();
        let registry = SkillRegistry::new();
        registry
            .create_skill_in(&base, &create_test_skill("guarded", "desc"))
            .unwrap();

        // Managed dirs list does NOT include `base` — delete must refuse
        let managed = vec![other];
        assert!(registry.delete_skill_guarded("guarded", &managed).is_err());
        assert!(base.join("guarded").exists());
        assert!(registry.get_skill("guarded").is_some());
    }

    #[test]
    fn test_load_records_parse_errors() {
        let temp_dir = TempDir::new().unwrap();
        let bad_dir = temp_dir.path().join("broken-skill");
        fs::create_dir(&bad_dir).unwrap();
        fs::write(bad_dir.join("SKILL.md"), "no frontmatter here").unwrap();

        let registry = SkillRegistry::new();
        let loaded = registry.load_from_directory(temp_dir.path()).unwrap();
        assert!(loaded.is_empty());
        let errors = registry.load_errors.read().unwrap();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].path.ends_with("broken-skill/SKILL.md"));
    }

    #[test]
    fn test_load_multiple_skills() {
        let temp_dir = TempDir::new().unwrap();

        // Create skill 1
        let skill1_dir = temp_dir.path().join("skill-one");
        fs::create_dir(&skill1_dir).unwrap();
        fs::write(
            skill1_dir.join("SKILL.md"),
            "---\nname: skill-one\ndescription: First\n---\nInstructions",
        )
        .unwrap();

        // Create skill 2
        let skill2_dir = temp_dir.path().join("skill-two");
        fs::create_dir(&skill2_dir).unwrap();
        fs::write(
            skill2_dir.join("SKILL.md"),
            "---\nname: skill-two\ndescription: Second\n---\nInstructions",
        )
        .unwrap();

        let registry = SkillRegistry::new();
        let result = registry.load_from_directory(temp_dir.path());

        assert!(result.is_ok());
        let loaded = result.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(registry.list_skills().len(), 2);
    }
}
