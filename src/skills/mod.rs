mod executor;
pub mod invocation;
pub mod parser;
pub mod registry;
pub mod watcher;

// Re-export common types
pub use parser::Skill;
pub use registry::SkillRegistry;
// Re-export from executor (the new consolidated module)
pub use executor::execute_skill;
