// Smithery API Client
// This module will handle all interactions with the Smithery Registry API.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SmitheryServer {
    pub qualified_name: String,
    pub display_name: String,
    pub description: String,
    #[serde(default)]
    pub icon_url: String,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub use_count: u32,
    #[serde(default)]
    pub remote: bool,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub homepage: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Pagination {
    pub current_page: u32,
    pub page_size: u32,
    pub total_pages: u32,
    pub total_count: u32,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct SmitheryResponse {
    pub servers: Vec<SmitheryServer>,
    pub pagination: Pagination,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct SmitheryServerDetail {
    #[serde(rename = "qualifiedName")]
    pub qualified_name: String,
    #[serde(default)]
    pub configs: Option<Vec<SmitheryConfig>>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct SmitheryConfig {
    pub platform: String,
    pub command: String,
    pub args: Vec<String>,
}

pub struct SmitheryClient {
    api_key: String,
    client: reqwest::Client,
}

impl SmitheryClient {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }

    pub async fn fetch_servers(
        &self,
        query: Option<&str>,
        page: Option<u32>,
        sort: Option<&str>,
    ) -> Result<SmitheryResponse, String> {
        let url = "https://registry.smithery.ai/servers";
        let platform = get_platform();
        let search_query = query.unwrap_or("is:verified");
        let page = page.unwrap_or(1).to_string();
        let sort_param = sort.unwrap_or("relevance");

        let response = self
            .client
            .get(url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .query(&[
                ("q", search_query),
                ("platform", &platform),
                ("page", &page),
                ("sort", sort_param),
            ])
            .send()
            .await
            .map_err(|e| format!("Failed to fetch from Smithery: {}", e))?;

        if !response.status().is_success() {
            return Err(format!(
                "Smithery API returned status: {}",
                response.status()
            ));
        }

        let smithery_response: SmitheryResponse = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse Smithery response: {}", e))?;

        Ok(smithery_response)
    }

    #[allow(dead_code)]
    pub async fn fetch_server_details(
        &self,
        server_id: &str,
    ) -> Result<SmitheryServerDetail, String> {
        let url = format!("https://registry.smithery.ai/servers/{}", server_id);

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .map_err(|e| format!("Failed to fetch from Smithery: {}", e))?;

        if !response.status().is_success() {
            return Err(format!(
                "Smithery API returned status: {}",
                response.status()
            ));
        }

        response
            .json()
            .await
            .map_err(|e| format!("Failed to parse Smithery response: {}", e))
    }
}

pub fn get_platform() -> String {
    match std::env::consts::OS {
        "macos" => "macos".to_string(),
        "linux" => "linux".to_string(),
        "windows" => "windows".to_string(),
        other => other.to_string(),
    }
}
