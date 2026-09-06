use async_trait::async_trait;
use serde_json::{json, Value};
use crate::traits::McpModule;
use crate::types::{CallToolResult, Tool};

pub struct EverythingModule {
    client: reqwest::Client,
}

impl EverythingModule {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl McpModule for EverythingModule {
    fn name(&self) -> &'static str {
        "everything"
    }

    fn tools(&self) -> Vec<Tool> {
        vec![Tool {
            name: "everything_search".to_string(),
            description: Some("Fast file search using Voidtools Everything HTTP server".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query pattern" },
                    "max_results": { "type": "integer", "description": "Maximum results to return (default: 50)" },
                    "host": { "type": "string", "description": "Everything HTTP host (default: 127.0.0.1)" },
                    "port": { "type": "integer", "description": "Everything HTTP port (default: 14680)" }
                },
                "required": ["query"]
            }),
        }]
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult, String> {
        if name != "everything_search" {
            return Err(format!("Unknown tool: {}", name));
        }

        let query = arguments.get("query").and_then(|v| v.as_str()).ok_or("Missing query parameter")?;
        let max_results = arguments.get("max_results").and_then(|v| v.as_u64()).unwrap_or(50);
        let host = arguments.get("host").and_then(|v| v.as_str()).unwrap_or("127.0.0.1");
        let port = arguments.get("port").and_then(|v| v.as_u64()).unwrap_or(14680);

        let url = format!("http://{}:{}/?search={}&json=1&count={}", host, port, urlencoding::encode(query), max_results);

        let resp = self.client.get(&url).send().await.map_err(|e| format!("Everything API request failed: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("Everything HTTP returned status {}", resp.status()));
        }

        let json_body: Value = resp.json().await.map_err(|e| format!("Failed to parse Everything response: {}", e))?;

        Ok(CallToolResult::text(serde_json::to_string_pretty(&json_body).unwrap_or_default()))
    }
}
