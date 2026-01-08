// This module will contain all MCP-related logic.
pub mod manager;
pub mod authenticated_sse;
pub mod oauth_flow;
pub mod smithery_client;
pub mod composio_client;
pub mod tool_selection;
#[cfg(test)]
mod composio_client_test;