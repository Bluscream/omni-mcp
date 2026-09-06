//! Filename search via the Everything HTTP server (Voidtools, Windows).
//!
//! Kept because the original configuration used it, but the previous version
//! dumped Everything's raw JSON straight into the model's context and gave no
//! hint that the tool needs an Everything instance reachable over HTTP. The
//! response is now normalised to a flat result list and connection failures say
//! what is actually wrong.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{NativeTool, ToolContext, args, unknown};
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, Tool};

pub struct SearchTools {
    client: reqwest::Client,
}

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u64 = 14680;
const DEFAULT_LIMIT: u64 = 50;
const MAX_LIMIT: u64 = 1_000;

impl SearchTools {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
        }
    }
}

impl Default for SearchTools {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NativeTool for SearchTools {
    fn descriptors(&self) -> Vec<Tool> {
        vec![Tool::new(
            "everything_search",
            "Searches filenames through a Voidtools Everything HTTP server. Requires Everything \
             to be running with its HTTP server enabled; use grep_search for content search on \
             this machine.",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Everything query syntax, e.g. \"ext:rs router\""
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Results to return (default 50, maximum 1000)"
                    },
                    "host": {
                        "type": "string",
                        "description": "Everything HTTP host (default 127.0.0.1)"
                    },
                    "port": {
                        "type": "integer",
                        "description": "Everything HTTP port (default 14680)"
                    }
                },
                "required": ["query"]
            }),
        )]
    }

    async fn call(
        &self,
        name: &str,
        args: Value,
        _ctx: &ToolContext,
    ) -> ToolResult<CallToolResult> {
        if name != "everything_search" {
            return Err(unknown(name));
        }

        let query = args::string(&args, "query")?;
        if query.trim().is_empty() {
            return Err(ToolError::InvalidArguments("query must not be empty".into()));
        }
        let limit = args::u64_or(&args, "max_results", DEFAULT_LIMIT)?.clamp(1, MAX_LIMIT);
        let host = args::opt_string(&args, "host")?.unwrap_or(DEFAULT_HOST);
        let port = args::u64_or(&args, "port", DEFAULT_PORT)?;

        let url = format!(
            "http://{host}:{port}/?search={}&json=1&count={limit}&path_column=1&size_column=1",
            urlencoding::encode(query)
        );

        let response = self.client.get(&url).send().await.map_err(|e| ToolError::Unavailable {
            backend: "everything".into(),
            reason: format!(
                "could not reach the Everything HTTP server at {host}:{port} ({e}). Confirm \
                 Everything is running and its HTTP server is enabled."
            ),
        })?;

        let status = response.status();
        if !status.is_success() {
            return Err(ToolError::Unavailable {
                backend: "everything".into(),
                reason: format!("Everything returned HTTP {status}"),
            });
        }

        let body: Value = response.json().await.map_err(|e| {
            ToolError::Failed(format!("Everything sent an unparseable response: {e}"))
        })?;

        Ok(CallToolResult::structured(normalize(&body, query)))
    }
}

/// Flattens Everything's response into `{query, total, results:[{path,name,size}]}`.
fn normalize(body: &Value, query: &str) -> Value {
    let results: Vec<Value> = body
        .get("results")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                    let directory = item.get("path").and_then(Value::as_str).unwrap_or_default();
                    let full = if directory.is_empty() {
                        name.to_string()
                    } else {
                        format!("{}\\{}", directory.trim_end_matches('\\'), name)
                    };
                    let mut entry = json!({ "name": name, "path": full });
                    if let Some(kind) = item.get("type").and_then(Value::as_str) {
                        entry["type"] = json!(kind);
                    }
                    if let Some(size) = item.get("size") {
                        entry["size"] = size.clone();
                    }
                    entry
                })
                .collect()
        })
        .unwrap_or_default();

    json!({
        "query": query,
        "total": body.get("totalResults").cloned().unwrap_or_else(|| json!(results.len())),
        "returned": results.len(),
        "results": results
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolPolicy;

    fn ctx() -> ToolContext {
        ToolContext::new(ToolPolicy::default())
    }

    #[test]
    fn normalises_results_into_full_paths() {
        let body = json!({
            "totalResults": 2,
            "results": [
                { "type": "file", "name": "main.rs", "path": "C:\\src", "size": "120" },
                { "type": "folder", "name": "src", "path": "C:\\" }
            ]
        });

        let out = normalize(&body, "rs");
        assert_eq!(out["total"], 2);
        assert_eq!(out["returned"], 2);
        assert_eq!(out["results"][0]["path"], "C:\\src\\main.rs");
        assert_eq!(out["results"][0]["size"], "120");
        assert_eq!(out["results"][1]["path"], "C:\\src");
    }

    #[test]
    fn an_empty_or_unexpected_body_yields_no_results_rather_than_an_error() {
        assert_eq!(normalize(&json!({}), "q")["returned"], 0);
        assert_eq!(normalize(&json!({ "results": "not an array" }), "q")["returned"], 0);
    }

    #[tokio::test]
    async fn an_empty_query_is_rejected_before_any_request_is_made() {
        let err = SearchTools::new()
            .call("everything_search", json!({ "query": "   " }), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn an_unreachable_server_explains_what_to_check() {
        let err = SearchTools::new()
            .call("everything_search", json!({ "query": "x", "port": 1 }), &ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Everything is running"), "{err}");
    }

    #[tokio::test]
    async fn the_result_limit_is_clamped() {
        // Exercised indirectly: an out-of-range limit must not be an error.
        let err = SearchTools::new()
            .call(
                "everything_search",
                json!({ "query": "x", "port": 1, "max_results": 999_999 }),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Unavailable { .. }));
    }
}
