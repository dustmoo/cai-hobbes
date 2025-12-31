# Task: Implement Native Composio Client (Rust)

## Context
We are replacing the generic SSE-based MCP connection for Composio with a native Rust client (`ComposioClient`) that interacts directly with the Composio REST API. This mirrors the logic of the Composio Python SDK, providing better control over tool listing, execution, and authentication.

## Objectives
1.  **Create `src/mcp/composio_client.rs`:** A Rust module handling HTTP communication with the Composio API.
2.  **Define Data Models:** Rust structs representing Composio's Tool, Toolkit, and Execution objects, derived from the Python SDK analysis.
3.  **Implement Core Methods:** `list_tools` and `execute_tool`.
4.  **Integrate into `McpManager`:** Update the manager to use `ComposioClient` for the "composio" source, adapting its output to standard MCP `Tool` objects.

## Technical Details (Derived from Python SDK Analysis)

### 1. Base URL & Auth
*   **Base URL:** Default to `https://backend.composio.dev/api/v1` (verify this if possible, but this is the standard v1 endpoint).
*   **Auth:** Header `x-api-key: <YOUR_API_KEY>`.

### 2. Data Models (`src/mcp/composio_client.rs`)
You will need to define structs that match the expected JSON response from Composio. Based on `tools.py` and standard API patterns:

```rust
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ComposioTool {
    pub name: String, // e.g., "GMAIL_SEND_EMAIL"
    pub description: Option<String>,
    pub parameters: serde_json::Value, // JSON Schema
    pub toolkit: Option<ComposioToolkit>,
    // Add other fields as necessary from the API response
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ComposioToolkit {
    pub slug: String,
    // ...
}
```

### 3. Client Implementation
Use `reqwest::Client`.

*   **`new(api_key: String)`**: Constructor.
*   **`list_tools(&self)`**:
    *   Endpoint: `GET /tools` (or `GET /actions` - check strict endpoint if possible, otherwise assume standard REST resource mapping from `self._client.tools.list`).
    *   **Note:** The Python SDK uses `self._client.tools.list`. This implies a resource-oriented URL.
    *   Parameters: `toolkit_slug` (optional), `limit`, `search`.
*   **`execute_tool(&self, slug: &str, args: serde_json::Value)`**:
    *   Endpoint: `POST /tools/{slug}/execute` (deduced from `self._client.tools.execute`).
    *   Body: `{ "arguments": args, "connected_account_id": ... }` (Handle connected account resolution if needed, or rely on implicit/default for now).

### 4. Integration in `src/mcp/manager.rs`
*   In `launch_servers` or a new `launch_composio_client` method:
    *   Instantiate `ComposioClient`.
    *   Call `list_tools`.
    *   **Adapter:** Convert `ComposioTool` -> `rmcp::model::Tool`.
        *   `name`: `tool.name`
        *   `description`: `tool.description`
        *   `inputSchema`: `tool.parameters`
    *   Store in `ActiveMcpClient` (you might need to wrap `ComposioClient` in a struct that implements `McpService` trait or handling logic, similar to how `RunningService` works, or adapt `ActiveMcpClient` to hold an enum `ClientType { Sse(RunningService), Native(ComposioClient) }`).

## Step-by-Step Instructions
1.  **Create File:** Create `src/mcp/composio_client.rs`.
2.  **Define Structs:** scaffolding the request/response shapes.
3.  **Implement Client:** Write the `impl ComposioClient` with `reqwest`.
4.  **Update Manager:** Modify `src/mcp/manager.rs`:
    *   Add `mod composio_client;`
    *   Update `ActiveMcpClient` struct to support the new client type (or create a unified trait).
    *   Update `launch_servers` to detect Composio config and use the new client.
    *   Update `use_mcp_tool` to route execution to `ComposioClient::execute_tool`.

## Crucial Note on "Virtual" MCP
We are *not* running a separate process. We are embedding the client directly into Hobbes. `McpManager` will treat it *as if* it were an MCP server, but internally it just makes HTTP calls.