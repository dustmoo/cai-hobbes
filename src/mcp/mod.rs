// This module will contain all MCP-related logic.
pub mod authenticated_sse;
pub mod composio_client;
#[cfg(test)]
mod composio_client_test;
pub mod core_client;
pub mod manager;
pub mod planner_client;
pub mod oauth_flow;
pub mod smithery_client;
pub mod tool_selection;
pub mod image_client;
pub mod terminal_client;
