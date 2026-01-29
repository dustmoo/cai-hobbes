pub mod parser;
pub mod registry;
mod executor;

// Re-export common types
pub use parser::Skill;
pub use registry::SkillRegistry;
// Re-export from executor (the new consolidated module)
pub use executor::execute_skill;
