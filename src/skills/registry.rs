use super::parser::Skill;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::fs;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    // Map of skill name to Skill
    pub skills: Arc<RwLock<HashMap<String, Skill>>>,
    pub loaded_paths: Arc<RwLock<Vec<PathBuf>>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: Arc::new(RwLock::new(HashMap::new())),
            loaded_paths: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn load_from_directory(&self, dir: &Path) -> Result<Vec<String>, String> {
        if !dir.exists() {
            return Err(format!("Skills directory not found: {:?}", dir));
        }

        let mut loaded_names = Vec::new();
        let mut skills_lock = self.skills.write().unwrap();
        
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
                            skills_lock.insert(name.clone(), skill);
                            loaded_names.push(name);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to parse skill at {:?}: {}", skill_file, e);
                        }
                    }
                }
            }
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
    use crate::skills::parser::{SkillMetadata, Skill};
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
        registry.skills.write().unwrap().insert("test-skill".to_string(), skill.clone());
        
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

    #[test]
    fn test_load_multiple_skills() {
        let temp_dir = TempDir::new().unwrap();
        
        // Create skill 1
        let skill1_dir = temp_dir.path().join("skill-one");
        fs::create_dir(&skill1_dir).unwrap();
        fs::write(skill1_dir.join("SKILL.md"), "---\nname: skill-one\ndescription: First\n---\nInstructions").unwrap();
        
        // Create skill 2  
        let skill2_dir = temp_dir.path().join("skill-two");
        fs::create_dir(&skill2_dir).unwrap();
        fs::write(skill2_dir.join("SKILL.md"), "---\nname: skill-two\ndescription: Second\n---\nInstructions").unwrap();
        
        let registry = SkillRegistry::new();
        let result = registry.load_from_directory(temp_dir.path());
        
        assert!(result.is_ok());
        let loaded = result.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(registry.list_skills().len(), 2);
    }
}
