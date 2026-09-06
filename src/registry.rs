use std::collections::HashMap;
use std::sync::Arc;
use serde_json::{json, Value};
use tracing::info;

use crate::config::Config;
use crate::traits::McpModule;
use crate::types::{CallToolResult, JsonRpcRequest, JsonRpcResponse, Tool};

pub struct Registry {
    modules: HashMap<String, Arc<dyn McpModule>>,
    config: Config,
    client: reqwest::Client,
}

impl Registry {
    pub fn new(config: Config) -> Self {
        Self {
            modules: HashMap::new(),
            config,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Register a native Rust MCP module
    pub fn register<M: McpModule + 'static>(&mut self, module: M) {
        let name = module.name().to_string();
        info!("Registering native Rust MCP module: {}", name);
        self.modules.insert(name, Arc::new(module));
    }

    /// Process an incoming JSON-RPC request
    pub async fn handle_request(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        match req.method.as_str() {
            "initialize" => {
                let result = json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "omni-mcp",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                });
                JsonRpcResponse::success(req.id, result)
            }
            "ping" => JsonRpcResponse::success(req.id, json!({})),
            "tools/list" => {
                let mut all_tools = Vec::new();

                // 0. Add omni-mcp system diagnostic tool
                all_tools.push(Tool {
                    name: "omni_status".to_string(),
                    description: Some("Provides real-time health diagnostic status of all native tools, proxies, and sidecars in omni-mcp".to_string()),
                    input_schema: json!({ "type": "object", "properties": {} }),
                });

                // 1. Collect native Rust module tools
                for module in self.modules.values() {
                    all_tools.extend(module.tools());
                }

                // 2. Query tools from HTTP/SSE proxies gracefully
                for proxy in &self.config.proxies {
                    match self.fetch_proxy_tools(proxy).await {
                        Ok(proxy_tools) => {
                            all_tools.extend(proxy_tools);
                        }
                        Err(err) => {
                            info!("Proxy '{}' unavailable: {}", proxy.name, err);
                        }
                    }
                }

                let result = json!({ "tools": all_tools });
                JsonRpcResponse::success(req.id, result)
            }
            "tools/call" => {
                let params = match req.params {
                    Some(p) => p,
                    None => return JsonRpcResponse::error(req.id, -32602, "Missing params".to_string()),
                };

                let tool_name = match params.get("name").and_then(|n| n.as_str()) {
                    Some(n) => n,
                    None => return JsonRpcResponse::error(req.id, -32602, "Missing tool name".to_string()),
                };

                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

                // Handle system status diagnostic tool
                if tool_name == "omni_status" {
                    let status_report = self.get_status_report().await;
                    return JsonRpcResponse::success(req.id, serde_json::to_value(CallToolResult::text(status_report)).unwrap());
                }

                // 1. Check native Rust modules
                for module in self.modules.values() {
                    if module.tools().iter().any(|t| t.name == tool_name) {
                        match module.call_tool(tool_name, arguments.clone()).await {
                            Ok(res) => return JsonRpcResponse::success(req.id, serde_json::to_value(res).unwrap()),
                            Err(e) => {
                                let err_res = CallToolResult::error(format!("[omni-mcp Error] Tool '{}' failed: {}", tool_name, e));
                                return JsonRpcResponse::success(req.id, serde_json::to_value(err_res).unwrap());
                            }
                        }
                    }
                }

                // 2. Check proxy targets gracefully
                for proxy in &self.config.proxies {
                    match self.call_proxy_tool(proxy, tool_name, arguments.clone()).await {
                        Ok(res) => return JsonRpcResponse::success(req.id, res),
                        Err(e) => {
                            info!("Call to tool '{}' via proxy '{}' failed: {}", tool_name, proxy.name, e);
                        }
                    }
                }

                // Return explicit error feedback to agent
                let not_found_msg = format!("[omni-mcp Feedback] Tool '{}' is currently unavailable or failed to respond. Call tool 'omni_status' to view active modules and diagnostics.", tool_name);
                JsonRpcResponse::success(req.id, serde_json::to_value(CallToolResult::error(not_found_msg)).unwrap())
            }
            _ => JsonRpcResponse::error(req.id, -32601, format!("Method not found: {}", req.method)),
        }
    }

    async fn get_status_report(&self) -> String {
        let mut report = String::from("# omni-mcp System Health & Diagnostic Report\n\n");

        report.push_str("## 1. Native Rust Modules (0ms Overhead, Instant Execution)\n");
        for name in self.modules.keys() {
            report.push_str(&format!("  - **{}**: ACTIVE (Loaded natively)\n", name));
        }

        report.push_str("\n## 2. Configured Proxies & Remote Endpoints\n");
        if self.config.proxies.is_empty() {
            report.push_str("  (No proxy endpoints configured)\n");
        } else {
            for proxy in &self.config.proxies {
                match self.fetch_proxy_tools(proxy).await {
                    Ok(tools) => {
                        report.push_str(&format!("  - **{}** ({}): ONLINE ({} tools registered)\n", proxy.name, proxy.url, tools.len()));
                    }
                    Err(err) => {
                        report.push_str(&format!("  - **{}** ({}): OFFLINE / UNREACHABLE\n    - *Reason*: {}\n", proxy.name, proxy.url, err));
                    }
                }
            }
        }

        report.push_str("\n## 3. Configured Local Sidecars\n");
        if self.config.sidecars.is_empty() {
            report.push_str("  (No sidecar executables configured)\n");
        } else {
            for sidecar in &self.config.sidecars {
                report.push_str(&format!("  - **{}**: Configured command: `{}` {}\n", sidecar.name, sidecar.command, sidecar.args.join(" ")));
            }
        }

        report
    }

    async fn fetch_proxy_tools(&self, proxy: &crate::config::ProxyConfig) -> Result<Vec<Tool>, String> {
        let mut req_builder = self.client.post(&proxy.url).json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        }));

        if let Some(token) = &proxy.token {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", token));
        } else if let Some(bearer) = &proxy.bearer {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", bearer));
        }

        for (k, v) in &proxy.headers {
            req_builder = req_builder.header(k, v);
        }

        let resp = req_builder.send().await.map_err(|e| format!("Connection failed: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP status {}", resp.status()));
        }

        let rpc_res: JsonRpcResponse = resp.json().await.map_err(|e| format!("Invalid JSON response: {}", e))?;

        if let Some(result) = rpc_res.result {
            if let Some(tools_arr) = result.get("tools") {
                let tools: Vec<Tool> = serde_json::from_value(tools_arr.clone()).map_err(|e| format!("Schema error: {}", e))?;
                return Ok(tools);
            }
        }

        Err("Response missing 'tools' array".to_string())
    }

    async fn call_proxy_tool(&self, proxy: &crate::config::ProxyConfig, name: &str, args: Value) -> Result<Value, String> {
        let mut req_builder = self.client.post(&proxy.url).json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": args
            }
        }));

        if let Some(token) = &proxy.token {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", token));
        } else if let Some(bearer) = &proxy.bearer {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", bearer));
        }

        for (k, v) in &proxy.headers {
            req_builder = req_builder.header(k, v);
        }

        let resp = req_builder.send().await.map_err(|e| format!("Proxy request failed: {}", e))?;
        let rpc_res: JsonRpcResponse = resp.json().await.map_err(|e| format!("Invalid JSON response: {}", e))?;

        if let Some(res) = rpc_res.result {
            Ok(res)
        } else if let Some(err) = rpc_res.error {
            Err(err.message)
        } else {
            Err("Empty response from proxy".to_string())
        }
    }
}
