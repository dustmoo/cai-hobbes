# Composio API Endpoints Reference

This document catalogs all Composio API calls made by the `ComposioClient` in Hobbes, organized by API surface (REST vs MCP) and their purpose.

## Architecture Overview

```mermaid
flowchart TB
    subgraph UI["UI Layer"]
        MP[MCP Marketplace]
        SP[Settings Panel]
    end
    
    subgraph CC["ComposioClient"]
        REST["REST API Calls\n(Admin/Registry)"]
        MCP["MCP Proxy Calls\n(Operational)"]
    end
    
    subgraph Backend["Composio Backend"]
        RESTAPI["Registry API\nhttps://backend.composio.dev/api/v3/..."]
        MCPAPI["MCP Proxy\nhttps://backend.composio.dev/v3/mcp/{server_id}/mcp"]
    end
    
    MP --> CC
    SP --> CC
    
    REST -->|"x-api-key header"| RESTAPI
    MCP -->|"?user_id=... param"| MCPAPI
    
    RESTAPI -->|"Marketplace catalog\nAuth configs\nServer patching"| REST
    MCPAPI -->|"Tool discovery\nTool execution"| MCP
```

---

## API Surface Summary

| Surface | Base URL Pattern | Auth Method | Purpose |
|---------|-----------------|-------------|---------|
| **REST API** | `https://backend.composio.dev/api/v3/...` | `x-api-key` header | Admin tasks: catalog browsing, auth config, server management |
| **MCP Proxy** | `https://backend.composio.dev/v3/mcp/{server_id}/mcp?user_id=...` | `user_id` query param | Operational tasks: tool listing, tool execution |

---

## REST API Calls (Registry/Admin)

These calls use the `x-api-key` header for authentication and are used for administrative/setup tasks.

| Function | HTTP | Endpoint | Purpose |
|----------|------|----------|---------|
| `validate_composio_api_key()` | GET | `/api/v1/cli/auth/user_info` | Validate API key on profile setup |
| `list_connected_accounts()` | GET | `/api/v3/connected_accounts?user_id=&user_uuid=` | Cache hydration for account IDs (used by `execute_tool`) |
| `create_auth_config()` | POST | `/api/v3/auth_configs` | Create OAuth/API key auth configuration for a toolkit |
| `get_auth_config_id()` | GET | `/api/v3/auth_configs?toolkitSlug=` | Retrieve existing auth config ID |
| `list_all_toolkits()` | GET | `/api/v3/toolkits?...` | Fetch marketplace catalog (paginated) |
| `list_toolkit_categories()` | GET | `/api/v3/toolkits/categories` | Fetch category list for marketplace filtering |
| `get_toolkit_tools()` | GET | `/api/v3/tools/enum` | Get tool names for a specific toolkit |
| `get_toolkit_tools_detailed()` | GET | `/api/v3/tools/enum` | Get tool names + descriptions for smart selection |
| `add_toolkit_to_server()` | PATCH | `/api/v3/mcp/{server_id}` | Add toolkit to MCP server config, set allowed tools |
| `initiate_connection()` | POST | `/api/v3/connected_accounts/link` | Generate OAuth link URL for user authentication |

---

## MCP Proxy Calls (Operational)

These calls use the `user_id` query parameter for routing and **do not** include `x-api-key`. They follow JSON-RPC 2.0 protocol.

| Function | Method | JSON-RPC Method | Purpose |
|----------|--------|-----------------|---------|
| `list_tools()` | POST | `tools/list` | Get all tools available for the user's connected toolkits |
| `get_connected_toolkit_slugs()` | POST | `tools/list` | Derive connected toolkit slugs from tool names |
| `list_connected_toolkits()` | POST | `tools/list` | Group tools by toolkit for UI display |
| `list_tools_filtered()` | POST | `tools/list` | Get tools filtered by toolkit slug |
| `search_tools()` | POST | `tools/list` | Search tools by query string |
| `list_tools_for_session()` | POST | `tools/list` | Get tools for force-loaded toolkits in a chat session |
| `execute_tool()` | POST | `tools/call` | Execute a tool with arguments |

---

## URL Construction

### REST API Base URL
Derived from the MCP endpoint URL stored in the profile:
```
Input:  https://backend.composio.dev/v3/mcp/0a4474b3-d8e6-4417-a848-0d0c867b20f4/mcp
Output: https://backend.composio.dev/api/v3
```

### MCP Proxy URL
Built using the stored base URL with `user_id` appended:
```
Base:   https://backend.composio.dev/v3/mcp/0a4474b3-d8e6-4417-a848-0d0c867b20f4/mcp
Output: https://backend.composio.dev/v3/mcp/0a4474b3-d8e6-4417-a848-0d0c867b20f4/mcp?user_id=bb98696d-d833-4953-8857-091cb87cc041
```

---

## Critical Mandates

1. **MCP-First for Status**: Connected toolkit status in the Marketplace MUST be derived from `tools/list` MCP response, NOT from `list_connected_accounts` REST call.

2. **Pure MCP Payload**: Do NOT inject `user_id` or `connected_account_id` into the JSON-RPC body. These are routing parameters, not payload data.

3. **No API Key on Proxy**: MCP Proxy calls MUST NOT include the `x-api-key` header. Auth is implicit via `user_id`.

4. **Accept Header**: MCP Proxy calls MUST include `Accept: application/json, text/event-stream` header for SSE responses.
