//! Client for the public Glama MCP registry API.
//!
//! Base URL: `https://glama.ai/api/mcp/v1` — no authentication required.
//! The registry indexes open-source MCP servers with hosting attributes
//! (local-only / remote-capable / hybrid), a repository link and an env-var
//! JSON schema, but it does NOT provide a run command or a hosted endpoint
//! URL; local install commands are derived from the repository manifest
//! (see [`derive_run_command`]) and remote endpoints are pasted by the user.

use serde::{Deserialize, Serialize};

pub const GLAMA_API_BASE: &str = "https://glama.ai/api/mcp/v1";

/// Hosting capability advertised by the registry via `attributes`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlamaHosting {
    LocalOnly,
    RemoteCapable,
    Hybrid,
    Unknown,
}

impl GlamaHosting {
    /// The registry attribute lookup key.
    ///
    /// Currently unused: the Glama registry API ignores attribute filter
    /// params, so hosting/official filtering is done client-side in the
    /// marketplace. Retained so server-side filtering can be wired up if the
    /// API starts honoring `attributes`.
    #[allow(dead_code)]
    pub fn attribute_key(&self) -> Option<&'static str> {
        match self {
            GlamaHosting::LocalOnly => Some("hosting:local-only"),
            GlamaHosting::RemoteCapable => Some("hosting:remote-capable"),
            GlamaHosting::Hybrid => Some("hosting:hybrid"),
            GlamaHosting::Unknown => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            GlamaHosting::LocalOnly => "Local",
            GlamaHosting::RemoteCapable => "Remote",
            GlamaHosting::Hybrid => "Hybrid",
            GlamaHosting::Unknown => "Unknown",
        }
    }
}

pub const OFFICIAL_ATTRIBUTE: &str = "author:official";

/// One environment variable extracted from a server's env-var JSON schema.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct GlamaEnvVar {
    pub name: String,
    pub description: Option<String>,
    pub default: Option<String>,
    pub required: bool,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GlamaRepository {
    pub url: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GlamaLicense {
    pub name: String,
    pub url: Option<String>,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GlamaTool {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GlamaServer {
    pub id: String,
    pub name: String,
    pub namespace: String,
    pub slug: String,
    pub description: Option<String>,
    /// The server's page on glama.ai.
    pub url: String,
    pub repository: Option<GlamaRepository>,
    pub spdx_license: Option<GlamaLicense>,
    pub attributes: Vec<String>,
    pub environment_variables_json_schema: Option<serde_json::Value>,
    pub tools: Vec<GlamaTool>,
}

impl GlamaServer {
    pub fn hosting(&self) -> GlamaHosting {
        for attr in &self.attributes {
            match attr.as_str() {
                "hosting:local-only" => return GlamaHosting::LocalOnly,
                "hosting:remote-capable" => return GlamaHosting::RemoteCapable,
                "hosting:hybrid" => return GlamaHosting::Hybrid,
                _ => {}
            }
        }
        GlamaHosting::Unknown
    }

    pub fn is_official(&self) -> bool {
        self.attributes.iter().any(|a| a == OFFICIAL_ATTRIBUTE)
    }

    /// Qualified name used as a stable identifier, e.g. `owner/repo-slug`.
    pub fn qualified_name(&self) -> String {
        format!("{}/{}", self.namespace, self.slug)
    }

    pub fn repository_url(&self) -> Option<&str> {
        self.repository.as_ref().map(|r| r.url.as_str())
    }

    /// Parse the env-var JSON schema into a flat list of variables.
    pub fn env_vars(&self) -> Vec<GlamaEnvVar> {
        let Some(schema) = &self.environment_variables_json_schema else {
            return Vec::new();
        };
        let required: Vec<&str> = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
            return Vec::new();
        };
        props
            .iter()
            .map(|(name, spec)| GlamaEnvVar {
                name: name.clone(),
                description: spec
                    .get("description")
                    .and_then(|d| d.as_str())
                    .map(str::to_string),
                default: spec.get("default").and_then(|d| d.as_str()).map(str::to_string),
                required: required.contains(&name.as_str()),
            })
            .collect()
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GlamaPageInfo {
    pub end_cursor: Option<String>,
    pub has_next_page: bool,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GlamaServerList {
    pub servers: Vec<GlamaServer>,
    pub page_info: GlamaPageInfo,
}

#[derive(Clone, Debug)]
pub struct GlamaClient {
    http: reqwest::Client,
    base_url: String,
}

impl Default for GlamaClient {
    fn default() -> Self {
        Self::new()
    }
}

impl GlamaClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: GLAMA_API_BASE.to_string(),
        }
    }

    #[cfg(test)]
    pub fn with_base_url(base_url: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
        }
    }

    /// Search/browse the registry with cursor pagination.
    ///
    /// `attributes` are lookup keys such as `hosting:local-only` or
    /// `author:official`; multiple keys are comma-joined.
    pub async fn list_servers(
        &self,
        query: Option<&str>,
        attributes: &[&str],
        first: u32,
        after: Option<&str>,
    ) -> Result<GlamaServerList, String> {
        let mut params: Vec<(&str, String)> = vec![("first", first.to_string())];
        if let Some(q) = query {
            if !q.trim().is_empty() {
                params.push(("query", q.trim().to_string()));
            }
        }
        if !attributes.is_empty() {
            params.push(("attributes", attributes.join(",")));
        }
        if let Some(cursor) = after {
            params.push(("after", cursor.to_string()));
        }

        let url = format!("{}/servers", self.base_url);
        let response = self
            .http
            .get(&url)
            .query(&params)
            .send()
            .await
            .map_err(|e| format!("Glama request failed: {}", e))?;

        if !response.status().is_success() {
            return Err(format!(
                "Glama registry returned HTTP {}",
                response.status()
            ));
        }

        response
            .json::<GlamaServerList>()
            .await
            .map_err(|e| format!("Failed to parse Glama response: {}", e))
    }

    /// Fetch a single server by its registry id, or by `namespace/slug`.
    /// Not yet called from the UI — kept for upcoming server-detail views.
    #[allow(dead_code)]
    pub async fn get_server(&self, id_or_path: &str) -> Result<GlamaServer, String> {
        let url = format!("{}/servers/{}", self.base_url, id_or_path);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("Glama request failed: {}", e))?;

        if !response.status().is_success() {
            return Err(format!(
                "Glama registry returned HTTP {}",
                response.status()
            ));
        }

        response
            .json::<GlamaServer>()
            .await
            .map_err(|e| format!("Failed to parse Glama server: {}", e))
    }
}

// ============================================================================
// LOCAL INSTALL COMMAND DERIVATION
// ============================================================================

/// Parse a GitHub repository URL into `(owner, repo)`.
/// Only the root repo is considered (subpaths are ignored per design).
pub fn parse_github_repo(repo_url: &str) -> Option<(String, String)> {
    let rest = repo_url
        .trim()
        .strip_prefix("https://github.com/")
        .or_else(|| repo_url.trim().strip_prefix("http://github.com/"))?;
    let mut parts = rest.split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.trim_end_matches(".git").to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

/// Validate a name against the npm package-name grammar (optionally
/// `@scope/name`). The manifest is attacker-controlled: a "name" like
/// `--registry=https://evil` would be flag-injection into npx, so anything
/// outside the strict grammar is rejected.
fn is_valid_npm_name(name: &str) -> bool {
    fn valid_segment(seg: &str) -> bool {
        let mut chars = seg.chars();
        match chars.next() {
            Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
            _ => return false,
        }
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
    }
    if name.is_empty() || name.len() > 214 {
        return false;
    }
    match name.strip_prefix('@') {
        Some(rest) => {
            let mut parts = rest.splitn(2, '/');
            matches!(
                (parts.next(), parts.next()),
                (Some(scope), Some(pkg)) if valid_segment(scope) && valid_segment(pkg)
            )
        }
        None => valid_segment(name),
    }
}

/// Validate a name against the PEP 508 project-name grammar. Same rationale
/// as [`is_valid_npm_name`] — rejects flag-injection into uvx.
fn is_valid_python_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 214 {
        return false;
    }
    let bytes = name.as_bytes();
    let alnum = |b: u8| b.is_ascii_alphanumeric();
    alnum(bytes[0])
        && alnum(bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Extract the package name from a package.json manifest.
/// Returns None for workspace/monorepo roots without a runnable package
/// (no `name`, or `private: true` with no `bin`), or for names that fail
/// the npm grammar (potential flag injection).
pub fn npm_package_name(package_json: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(package_json).ok()?;
    let name = json.get("name")?.as_str()?.to_string();
    let is_private = json
        .get("private")
        .and_then(|p| p.as_bool())
        .unwrap_or(false);
    let has_bin = json.get("bin").is_some();
    if is_private && !has_bin {
        return None;
    }
    if !is_valid_npm_name(&name) {
        return None;
    }
    Some(name)
}

/// Extract the project name from a pyproject.toml manifest.
/// Names failing the PEP 508 grammar are rejected (potential flag injection).
pub fn python_project_name(pyproject: &str) -> Option<String> {
    let doc: toml::Value = toml::from_str(pyproject).ok()?;
    let name = doc.get("project")?.get("name")?.as_str()?.to_string();
    if is_valid_python_name(&name) {
        Some(name)
    } else {
        None
    }
}

/// Best-effort derivation of a local run command from a server's repository:
/// package.json name → `npx -y <name>`; pyproject.toml name → `uvx <name>`.
/// Returns None when nothing standard is published — the install dialog then
/// asks the user to enter the command manually.
pub async fn derive_run_command(repo_url: &str) -> Option<(String, Vec<String>)> {
    let (owner, repo) = parse_github_repo(repo_url)?;
    let http = reqwest::Client::new();

    // HEAD ref resolves the default branch without an extra API call.
    let pkg_url = format!(
        "https://raw.githubusercontent.com/{}/{}/HEAD/package.json",
        owner, repo
    );
    if let Ok(resp) = http.get(&pkg_url).send().await {
        if resp.status().is_success() {
            if let Ok(body) = resp.text().await {
                if let Some(name) = npm_package_name(&body) {
                    return Some(("npx".to_string(), vec!["-y".to_string(), name]));
                }
            }
        }
    }

    let py_url = format!(
        "https://raw.githubusercontent.com/{}/{}/HEAD/pyproject.toml",
        owner, repo
    );
    if let Ok(resp) = http.get(&py_url).send().await {
        if resp.status().is_success() {
            if let Ok(body) = resp.text().await {
                if let Some(name) = python_project_name(&body) {
                    return Some(("uvx".to_string(), vec![name]));
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a live `GET /servers?first=3&query=filesystem` response
    /// (2026-07-03), trimmed to two entries.
    const FIXTURE: &str = r#"{
        "pageInfo": {
            "endCursor": "eyJjcmVhdGVkQXQiOjE3ODI3NzA2MDcsImlkIjoibTM3bnNjMmxubSJ9",
            "hasNextPage": true,
            "hasPreviousPage": false,
            "startCursor": "eyJjcmVhdGVkQXQiOjE3ODI4OTY4MzAsImlkIjoicTJvMjhjY3ppbiJ9"
        },
        "servers": [
            {
                "attributes": ["hosting:local-only"],
                "description": "A filesystem-backed MCP server with REST API.",
                "environmentVariablesJsonSchema": {
                    "properties": {
                        "PORT": { "description": "REST API port", "type": "string", "default": "5000" },
                        "MCP_MODE": { "description": "Set to 'stdio' for MCP protocol mode", "type": "string" }
                    },
                    "type": "object",
                    "required": []
                },
                "id": "q2o28cczin",
                "name": "Memo MCP Server",
                "namespace": "devmaster-x",
                "repository": { "url": "https://github.com/devmaster-x/mcp" },
                "slug": "mcp",
                "spdxLicense": { "name": "MIT License", "url": "https://spdx.org/licenses/MIT.json" },
                "tools": [],
                "url": "https://glama.ai/mcp/servers/q2o28cczin"
            },
            {
                "attributes": ["hosting:remote-capable", "author:official"],
                "description": "Obsidian vault server.",
                "environmentVariablesJsonSchema": {
                    "properties": {
                        "OBSIDIAN_VAULT": { "description": "Absolute path to the Obsidian vault directory", "type": "string" }
                    },
                    "type": "object",
                    "required": ["OBSIDIAN_VAULT"]
                },
                "id": "mdjzwiv728",
                "name": "mcp-obsidian",
                "namespace": "NeveuGregor",
                "repository": { "url": "https://github.com/NeveuGregor/mcp-obsidian" },
                "slug": "mcp-obsidian",
                "spdxLicense": null,
                "tools": [],
                "url": "https://glama.ai/mcp/servers/mdjzwiv728"
            }
        ]
    }"#;

    #[test]
    fn parses_live_fixture() {
        let list: GlamaServerList = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(list.servers.len(), 2);
        assert!(list.page_info.has_next_page);
        assert!(list.page_info.end_cursor.is_some());

        let first = &list.servers[0];
        assert_eq!(first.id, "q2o28cczin");
        assert_eq!(first.qualified_name(), "devmaster-x/mcp");
        assert_eq!(first.hosting(), GlamaHosting::LocalOnly);
        assert!(!first.is_official());
        assert_eq!(first.spdx_license.as_ref().unwrap().name, "MIT License");
        assert_eq!(
            first.repository_url(),
            Some("https://github.com/devmaster-x/mcp")
        );

        let second = &list.servers[1];
        assert_eq!(second.hosting(), GlamaHosting::RemoteCapable);
        assert!(second.is_official());
        assert!(second.spdx_license.is_none());
    }

    #[test]
    fn env_vars_extracts_schema() {
        let list: GlamaServerList = serde_json::from_str(FIXTURE).unwrap();
        let mut vars = list.servers[0].env_vars();
        vars.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[1].name, "PORT");
        assert_eq!(vars[1].default.as_deref(), Some("5000"));
        assert!(!vars[1].required);

        let required = list.servers[1].env_vars();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].name, "OBSIDIAN_VAULT");
        assert!(required[0].required);
    }

    #[test]
    fn env_vars_tolerates_missing_schema() {
        let server = GlamaServer::default();
        assert!(server.env_vars().is_empty());
        assert_eq!(server.hosting(), GlamaHosting::Unknown);
    }

    #[test]
    fn parses_github_urls() {
        assert_eq!(
            parse_github_repo("https://github.com/owner/repo"),
            Some(("owner".to_string(), "repo".to_string()))
        );
        assert_eq!(
            parse_github_repo("https://github.com/owner/repo.git"),
            Some(("owner".to_string(), "repo".to_string()))
        );
        // Subpaths collapse to the root repo
        assert_eq!(
            parse_github_repo("https://github.com/owner/repo/tree/main/packages/x"),
            Some(("owner".to_string(), "repo".to_string()))
        );
        assert_eq!(parse_github_repo("https://gitlab.com/owner/repo"), None);
        assert_eq!(parse_github_repo("https://github.com/owner"), None);
    }

    #[test]
    fn npm_name_extraction() {
        assert_eq!(
            npm_package_name(r#"{ "name": "@scope/pkg", "version": "1.0.0" }"#),
            Some("@scope/pkg".to_string())
        );
        // Private workspace root with no bin — not runnable
        assert_eq!(
            npm_package_name(r#"{ "name": "monorepo", "private": true, "workspaces": ["a"] }"#),
            None
        );
        // Private but with a bin is still runnable
        assert_eq!(
            npm_package_name(r#"{ "name": "tool", "private": true, "bin": { "tool": "cli.js" } }"#),
            Some("tool".to_string())
        );
        assert_eq!(npm_package_name("not json"), None);
        assert_eq!(npm_package_name("{}"), None);
    }

    #[test]
    fn npm_name_rejects_flag_injection() {
        // Flags, uppercase, spaces, traversal — all outside the npm grammar
        for bad in [
            r#"{ "name": "--registry=https://evil.example" }"#,
            r#"{ "name": "-y" }"#,
            r#"{ "name": "pkg name" }"#,
            r#"{ "name": "../evil" }"#,
            r#"{ "name": ".hidden" }"#,
            r#"{ "name": "@scope" }"#,
            r#"{ "name": "@-scope/pkg" }"#,
            r#"{ "name": "UPPER" }"#,
        ] {
            assert_eq!(npm_package_name(bad), None, "should reject: {}", bad);
        }
        assert_eq!(
            npm_package_name(r#"{ "name": "@modelcontextprotocol/server-filesystem" }"#),
            Some("@modelcontextprotocol/server-filesystem".to_string())
        );
    }

    #[test]
    fn python_name_rejects_flag_injection() {
        for bad in [
            "[project]\nname = \"--index-url=https://evil\"",
            "[project]\nname = \"-e\"",
            "[project]\nname = \"pkg name\"",
            "[project]\nname = \"pkg-\"",
        ] {
            assert_eq!(python_project_name(bad), None, "should reject: {}", bad);
        }
        assert_eq!(
            python_project_name("[project]\nname = \"Mcp_Server.fetch-2\""),
            Some("Mcp_Server.fetch-2".to_string())
        );
    }

    #[test]
    fn python_name_extraction() {
        let pyproject = r#"
[build-system]
requires = ["hatchling"]

[project]
name = "mcp-server-fetch"
version = "1.0.0"
"#;
        assert_eq!(
            python_project_name(pyproject),
            Some("mcp-server-fetch".to_string())
        );
        assert_eq!(python_project_name("[tool.poetry]\nname = 'x'"), None);
        assert_eq!(python_project_name("not toml ==="), None);
    }
}
