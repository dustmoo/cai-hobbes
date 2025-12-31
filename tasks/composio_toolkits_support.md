# Task: Implement Composio Client Integration via SDK Logic

## Context
The previous approach of treating Composio as a generic MCP server was oversimplified. We need to implement a dedicated Composio client logic within `src/mcp` that mirrors the behavior of their Python SDK, rather than just connecting to a pre-existing MCP URL.

The user states: "The SDK approach shows that it expects OUR TOOL to use the the API to list tools, the composio-mcp is obsolete and we shouldn't be using it. What we need is a composio client similar to what we did for smithery but it needs to use the core SDK (python_ as a source... Our API uses OLD information, the new system uses the SDK for better token use."

## Objectives
1.  **Replicate SDK Logic:** We need to port the logic from `temp_composio_sdk/python/composio/core/models/tools.py` and `mcp.py` into Rust.
    *   This involves fetching tools via the Composio API (HTTP) first.
    *   Filtering/resolving tools based on "toolkits" and "auth configs".
    *   *Then* potentially executing them or setting up the connection.
2.  **Move away from "Composio MCP Server":** Instead of connecting to a single monolithic MCP server URL provided by Composio, we should likely be treating Composio as a *source* of tools (like Smithery) where we dynamically fetch tool definitions and then execute them (possibly via their API proxy or a specific execution endpoint).
3.  **Implement `ComposioClient`:** Create a struct similar to `SmitheryClient` that handles:
    *   Authentication (API Key).
    *   Listing Toolkits/Tools.
    *   Resolving tool schemas.
    *   Executing tools (proxying requests).

## Analysis of Python SDK (`tools.py` & `mcp.py`)
*   **`Tools.get_raw_composio_tools`:** Fetches tool schemas from `self._client.tools.list`.
    *   It supports filtering by `toolkits`, `tools` (slugs), `search`.
    *   It handles "custom tools" vs "API tools".
*   **`Tools.execute`:**
    *   First retrieves the tool schema.
    *   Applies "modifiers" (before/after hooks).
    *   Calls `self._client.tools.execute`.
*   **`MCP` class (`mcp.py`):**
    *   `create`: Creates a new "MCP server configuration" on Composio's side.
    *   `generate`: Generates a connection URL for a specific user and config.
    *   *Crucially:* The SDK seems to wrap the API. If we are building a *client* for Composio in Rust, we should hit their REST API directly to list tools and execute them, effectively acting as the SDK itself.

## Architecture Change
Instead of:
`Hobbes -> Generic MCP Client -> Composio MCP Server (SSE)`

We want:
`Hobbes -> Composio Manager (Rust) -> Composio REST API`

Or, if we still want to expose it as an MCP server *internally* to Hobbes's `McpManager`:
`Hobbes -> McpManager -> "Virtual" Composio MCP Client (Rust) -> Composio REST API`

This "Virtual" client would:
1.  `list_tools`: Call `https://api.composio.dev/v1/tools` (or equivalent).
2.  `call_tool`: Call `https://api.composio.dev/v1/tools/execute`.

## Step-by-Step Instructions for Jr. Coder
1.  **API Inspection:** We need to know the exact HTTP endpoints. The Python SDK uses a generated client (`composio.client`). We don't have that source code in the repo (it's likely installed).
    *   *Action:* Use `curl` or `playwright` to find the API base URL and endpoints from their docs or by inspecting network traffic if possible. (Likely `https://backend.composio.dev/api/v1/...`).
    *   *Alternative:* Look at `temp_composio_sdk/python/composio/client/__init__.py` - it mentions `_base_client`.
2.  **Create `src/mcp/composio_client.rs`:**
    *   Define structs for `Toolkit`, `Tool`, `ExecuteRequest`.
    *   Implement `ComposioClient` with methods `list_tools` and `execute_tool`.
3.  **Integrate into `McpManager`:**
    *   Modify `src/mcp/manager.rs` to have a special case for "composio" source (like `smithery`).
    *   Instead of launching an SSE client, it should instantiate the `ComposioClient`.
    *   The `ActiveMcpClient` struct might need to be generic or have an enum for `service` to support both `RunningService` (standard MCP) and `ComposioClient` (REST-based adapter).
4.  **Token Handling:** Ensure `COMPOSIO_API_KEY` is used for these REST calls.

## Refined Plan
1.  **Confirm Endpoints:** We need to verify the REST API endpoints.
2.  **Create Client:** Scaffold `src/components/composio_client.rs` (similar to `smithery_client.rs`).
3.  **Virtual MCP Adapter:** Implement a wrapper that makes `ComposioClient` look like an MCP server (implementing `list_tools` and `call_tool` traits/interfaces if possible, or just adapting it in `McpManager`).
