//! Centralized constants for the Hobbes application.
//!
//! This module consolidates constants that are used across multiple modules
//! to ensure consistency and avoid duplication.

/// The service name used for macOS Keychain operations.
/// Uses a different value for debug builds to prevent credential conflicts
/// between development and release builds.
#[cfg(debug_assertions)]
pub const SERVICE_NAME: &str = "ai.clearmirror.cai-hobbes.dev";

#[cfg(not(debug_assertions))]
pub const SERVICE_NAME: &str = "ai.clearmirror.cai-hobbes";
