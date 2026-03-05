use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SkillParserError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML parsing error: {0}")]
    Yaml(#[from] serde_yml::Error),
    #[error("Missing frontmatter delimiter")]
    MissingFrontmatter,
    #[error("Invalid frontmatter format")]
    InvalidFrontmatter,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")] // Claude Skills use kebab-case (e.g., allowed-tools)
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub disable_model_invocation: bool,
    #[serde(default = "default_true")] // Defaults to true if missing
    pub user_invocable: bool,
    #[serde(alias = "allowed_tools")]
    pub allowed_tools: Option<Vec<String>>,
    // We can add context/agent fields later as we implement them
    // pub context: Option<String>,
    // pub agent: Option<String>,
    pub argument_hint: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Skill {
    pub metadata: SkillMetadata,
    pub instructions: String,
    // Path to the SKILL.md file
    #[serde(skip)]
    pub path: PathBuf,
    // Root directory of the skill
    #[serde(skip)]
    pub root_path: PathBuf,
    // List of script filenames relative to root/scripts/
    #[serde(skip)]
    pub scripts: Vec<String>,
    // List of resource filenames relative to root/resources/
    #[serde(skip)]
    pub resources: Vec<String>,
}

impl Skill {
    pub fn from_file(path: &Path) -> Result<Self, SkillParserError> {
        let content = fs::read_to_string(path)?;
        // Harden path resolution: Ensure we use the absolute, canonical path
        let canonical_path = if let Ok(abs_path) = path.canonicalize() {
            abs_path
        } else {
            // Fallback for non-existent paths (e.g. tests)
            path.to_path_buf()
        };

        Self::parse(&content, canonical_path)
    }

    pub fn parse(content: &str, path: PathBuf) -> Result<Self, SkillParserError> {
        if !content.starts_with("---") {
            return Err(SkillParserError::MissingFrontmatter);
        }

        let parts: Vec<&str> = content.splitn(3, "---").collect();

        if parts.len() < 3 {
            return Err(SkillParserError::InvalidFrontmatter);
        }

        let frontmatter_str = parts[1];
        let instructions = parts[2].trim().to_string();

        let metadata: SkillMetadata = serde_yml::from_str(frontmatter_str)?;

        // Determine root path
        let root_path = path.parent().unwrap_or(&path).to_path_buf();

        // Discover scripts
        let mut scripts = Vec::new();
        let scripts_dir = root_path.join("scripts");
        if scripts_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(scripts_dir) {
                for entry in entries.flatten() {
                    if let Ok(file_type) = entry.file_type() {
                        if file_type.is_file() {
                            if let Ok(name) = entry.file_name().into_string() {
                                if !name.starts_with('.') {
                                    scripts.push(name);
                                }
                            }
                        }
                    }
                }
            }
        }
        scripts.sort();

        // Discover resources
        let mut resources = Vec::new();
        let resources_dir = root_path.join("resources");
        if resources_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(resources_dir) {
                for entry in entries.flatten() {
                    if let Ok(file_type) = entry.file_type() {
                        if file_type.is_file() {
                            if let Ok(name) = entry.file_name().into_string() {
                                if !name.starts_with('.') {
                                    resources.push(name);
                                }
                            }
                        }
                    }
                }
            }
        }
        resources.sort();

        Ok(Skill {
            metadata,
            instructions,
            path,
            root_path,
            scripts,
            resources,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_skill() {
        let content = r#"---
name: test-skill
description: A test skill
disable-model-invocation: true
allowed-tools: [Read, Grep]
---
These are instructions.
"#;
        let skill = Skill::parse(content, PathBuf::from("test.md")).unwrap();

        assert_eq!(skill.metadata.name, "test-skill");
        assert_eq!(skill.metadata.description, "A test skill");
        assert!(skill.metadata.disable_model_invocation);
        assert!(skill.metadata.user_invocable); // Default
        assert_eq!(
            skill.metadata.allowed_tools,
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
        assert_eq!(skill.instructions, "These are instructions.");
    }

    #[test]
    fn test_missing_frontmatter() {
        let content = "Just text";
        assert!(matches!(
            Skill::parse(content, PathBuf::from("test.md")),
            Err(SkillParserError::MissingFrontmatter)
        ));
    }
}
