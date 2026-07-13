use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SkillParserError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML parsing error: {0}")]
    Yaml(#[from] serde_norway::Error),
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
    #[serde(default, skip_serializing_if = "is_false")]
    pub disable_model_invocation: bool,
    #[serde(default = "default_true", skip_serializing_if = "is_true")] // Defaults to true if missing
    pub user_invocable: bool,
    #[serde(alias = "allowed_tools", skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    // We can add context/agent fields later as we implement them
    // pub context: Option<String>,
    // pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
}

fn default_true() -> bool {
    true
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn is_true(v: &bool) -> bool {
    *v
}

impl SkillMetadata {
    /// Validate metadata before writing to disk. The name must be a single
    /// kebab-case token so it works as a `/name` autocomplete command and a
    /// directory name; the description feeds autocomplete and prompt context.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if self.name.is_empty() {
            errors.push("Name is required".to_string());
        } else {
            let mut chars = self.name.chars();
            let first_ok = chars
                .next()
                .map(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                .unwrap_or(false);
            let rest_ok = self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
            if !first_ok || !rest_ok {
                errors.push(
                    "Name must be kebab-case: lowercase letters, digits, '-' or '_', starting with a letter or digit (e.g. my-skill)"
                        .to_string(),
                );
            }
        }

        if self.description.trim().is_empty() {
            errors.push("Description is required".to_string());
        } else if self.description.contains("---") {
            // The parser splits on the literal `---` fence, so a `---` inside
            // frontmatter values would corrupt the file on round-trip.
            errors.push("Description cannot contain '---'".to_string());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
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
    /// Serialize back to SKILL.md format: YAML frontmatter in `---` fences
    /// followed by the markdown instructions. Round-trips through `parse`.
    pub fn to_markdown(&self) -> Result<String, SkillParserError> {
        let frontmatter = serde_norway::to_string(&self.metadata)?;
        Ok(format!(
            "---\n{}---\n\n{}\n",
            frontmatter,
            self.instructions.trim_end()
        ))
    }

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

        let metadata: SkillMetadata = serde_norway::from_str(frontmatter_str)?;

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

    fn full_metadata_skill() -> Skill {
        Skill {
            metadata: SkillMetadata {
                name: "round-trip".to_string(),
                description: "A skill with every field set — naïve UTF-8 ✓".to_string(),
                disable_model_invocation: true,
                user_invocable: false,
                allowed_tools: Some(vec!["Read".to_string(), "composio-search".to_string()]),
                argument_hint: Some("<topic>".to_string()),
            },
            instructions: "# Heading\n\nDo the thing.\n\n---\n\nText after a horizontal rule.".to_string(),
            path: PathBuf::new(),
            root_path: PathBuf::new(),
            scripts: Vec::new(),
            resources: Vec::new(),
        }
    }

    #[test]
    fn test_to_markdown_round_trip_all_fields() {
        let skill = full_metadata_skill();
        let markdown = skill.to_markdown().unwrap();
        let parsed = Skill::parse(&markdown, PathBuf::from("SKILL.md")).unwrap();
        assert_eq!(parsed.metadata, skill.metadata);
        assert_eq!(parsed.instructions, skill.instructions);
    }

    #[test]
    fn test_to_markdown_uses_kebab_case_keys_and_skips_defaults() {
        let mut skill = full_metadata_skill();
        let markdown = skill.to_markdown().unwrap();
        assert!(markdown.contains("disable-model-invocation: true"));
        assert!(markdown.contains("user-invocable: false"));
        assert!(markdown.contains("allowed-tools:"));
        assert!(markdown.contains("argument-hint:"));

        // Defaults and Nones are omitted for clean frontmatter
        skill.metadata.disable_model_invocation = false;
        skill.metadata.user_invocable = true;
        skill.metadata.allowed_tools = None;
        skill.metadata.argument_hint = None;
        let markdown = skill.to_markdown().unwrap();
        assert!(!markdown.contains("disable-model-invocation"));
        assert!(!markdown.contains("user-invocable"));
        assert!(!markdown.contains("allowed-tools"));
        assert!(!markdown.contains("argument-hint"));
        // Still round-trips to the same (default) values
        let parsed = Skill::parse(&markdown, PathBuf::from("SKILL.md")).unwrap();
        assert_eq!(parsed.metadata, skill.metadata);
    }

    #[test]
    fn test_round_trip_body_with_hr_fence() {
        let skill = full_metadata_skill();
        let markdown = skill.to_markdown().unwrap();
        // The `---` inside the body must survive the fence split
        let parsed = Skill::parse(&markdown, PathBuf::from("SKILL.md")).unwrap();
        assert!(parsed.instructions.contains("---"));
        assert!(parsed.instructions.contains("Text after a horizontal rule."));
    }

    #[test]
    fn test_validate_accepts_kebab_case() {
        for name in ["my-skill", "skill2", "2fa-helper", "a", "snake_case"] {
            let mut skill = full_metadata_skill();
            skill.metadata.name = name.to_string();
            assert!(skill.metadata.validate().is_ok(), "expected '{}' valid", name);
        }
    }

    #[test]
    fn test_validate_rejects_bad_names() {
        for name in ["", "My Skill", "a/b", "-leading", "UPPER", "with space", "naïve"] {
            let mut skill = full_metadata_skill();
            skill.metadata.name = name.to_string();
            assert!(
                skill.metadata.validate().is_err(),
                "expected '{}' invalid",
                name
            );
        }
    }

    #[test]
    fn test_validate_rejects_empty_or_fenced_description() {
        let mut skill = full_metadata_skill();
        skill.metadata.description = "  ".to_string();
        assert!(skill.metadata.validate().is_err());
        skill.metadata.description = "contains --- fence".to_string();
        assert!(skill.metadata.validate().is_err());
    }
}
