use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tracing::info;

use crate::config::{Config, SidecarConfig};
use crate::traits::McpModule;
use crate::types::{CallToolResult, JsonRpcRequest, JsonRpcResponse, Tool};

#[allow(dead_code)]
pub struct SidecarWorker {
    pub name: String,
    pub child: Mutex<Child>,
    pub stdin: Mutex<ChildStdin>,
    pub reader: Mutex<BufReader<ChildStdout>>,
}

impl SidecarWorker {
    pub async fn spawn(cfg: &SidecarConfig) -> Result<Self, String> {
        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn {}: {}", cfg.command, e))?;
        let stdin = child.stdin.take().ok_or("Failed to open stdin")?;
        let stdout = child.stdout.take().ok_or("Failed to open stdout")?;

        let worker = Self {
            name: cfg.name.clone(),
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            reader: Mutex::new(BufReader::new(stdout)),
        };

        let init_params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "omni-mcp", "version": env!("CARGO_PKG_VERSION") }
        });
        let _ = worker.request("initialize", Some(init_params)).await;

        let notif_str = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
        let mut stdin_guard = worker.stdin.lock().await;
        let _ = stdin_guard.write_all(notif_str.as_bytes()).await;
        let _ = stdin_guard.flush().await;
        drop(stdin_guard);

        Ok(worker)
    }

    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, String> {
        let req_json = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params
        });

        let mut req_str = serde_json::to_string(&req_json).map_err(|e| e.to_string())?;
        req_str.push('\n');

        let mut stdin = self.stdin.lock().await;
        stdin.write_all(req_str.as_bytes()).await.map_err(|e| format!("Stdin write error: {}", e))?;
        stdin.flush().await.map_err(|e| format!("Stdin flush error: {}", e))?;

        let mut reader = self.reader.lock().await;
        let mut line = String::new();
        reader.read_line(&mut line).await.map_err(|e| format!("Stdout read error: {}", e))?;

        if line.trim().is_empty() {
            return Err("Empty response from sidecar".to_string());
        }

        let resp: JsonRpcResponse = serde_json::from_str(&line).map_err(|e| format!("Invalid JSON from sidecar: {}", e))?;
        if let Some(res) = resp.result {
            Ok(res)
        } else if let Some(err) = resp.error {
            Err(err.message)
        } else {
            Err("Unknown sidecar response".to_string())
        }
    }
}

pub struct Registry {
    modules: HashMap<String, Arc<dyn McpModule>>,
    config: Config,
    client: reqwest::Client,
    sidecars: Mutex<HashMap<String, Arc<SidecarWorker>>>,
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
            sidecars: Mutex::new(HashMap::new()),
        }
    }

    pub fn register<M: McpModule + 'static>(&mut self, module: M) {
        let name = module.name().to_string();
        info!("Registering native Rust MCP module: {}", name);
        self.modules.insert(name, Arc::new(module));
    }

    async fn get_or_spawn_sidecar(&self, name: &str) -> Result<Arc<SidecarWorker>, String> {
        let mut sidecars = self.sidecars.lock().await;
        if let Some(worker) = sidecars.get(name) {
            return Ok(worker.clone());
        }

        let sidecar_cfg = self.config.sidecars.iter().find(|s| s.name == name)
            .ok_or_else(|| format!("Sidecar config not found: {}", name))?;

        info!("Spawning sidecar worker: {}", name);
        let worker = Arc::new(SidecarWorker::spawn(sidecar_cfg).await?);
        sidecars.insert(name.to_string(), worker.clone());
        Ok(worker)
    }

    pub async fn handle_request(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        match req.method.as_str() {
            "initialize" => {
                let result = json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {},
                        "prompts": {},
                        "resources": {},
                        "logging": {}
                    },
                    "serverInfo": { "name": "omni-mcp", "version": env!("CARGO_PKG_VERSION") }
                });
                JsonRpcResponse::success(req.id, result)
            }
            "notifications/initialized" | "initialized" | "cancelled" | "$/cancelRequest" | "logging/setLevel" => {
                JsonRpcResponse::success(req.id, json!({}))
            }
            "ping" => JsonRpcResponse::success(req.id, json!({})),
            "prompts/list" => JsonRpcResponse::success(req.id, json!({ "prompts": [] })),
            "resources/list" => JsonRpcResponse::success(req.id, json!({ "resources": [] })),
            "resources/templates/list" => JsonRpcResponse::success(req.id, json!({ "resourceTemplates": [] })),
            "tools/list" => {
                let tools = self.handle_tools_list().await;
                JsonRpcResponse::success(req.id, json!({ "tools": tools }))
            }
            "tools/call" => self.handle_tools_call(req.id, req.params).await,
            _ => JsonRpcResponse::error(req.id, -32601, format!("Method not found: {}", req.method)),
        }
    }

    async fn handle_tools_list(&self) -> Vec<Tool> {
        let mut all_tools = Vec::new();

        all_tools.push(Tool {
            name: "omni_status".to_string(),
            description: Some("Provides real-time health diagnostic status of all native tools, proxies, and sidecars in omni-mcp".to_string()),
            input_schema: json!({ "type": "object", "properties": {} }),
        });

        for module in self.modules.values() {
            all_tools.extend(module.tools());
        }

        for proxy in &self.config.proxies {
            let fetch = self.fetch_proxy_tools(proxy);
            if let Ok(Ok(proxy_tools)) = tokio::time::timeout(std::time::Duration::from_millis(1500), fetch).await {
                all_tools.extend(proxy_tools);
            }
        }

        for sidecar in &self.config.sidecars {
            if let Ok(Ok(worker)) = tokio::time::timeout(std::time::Duration::from_millis(1500), self.get_or_spawn_sidecar(&sidecar.name)).await {
                if let Ok(Ok(res)) = tokio::time::timeout(std::time::Duration::from_millis(1500), worker.request("tools/list", None)).await {
                    if let Some(tools_arr) = res.get("tools") {
                        if let Ok(tools) = serde_json::from_value::<Vec<Tool>>(tools_arr.clone()) {
                            all_tools.extend(tools);
                        }
                    }
                }
            }
        }

        all_tools.into_iter().map(|t| t.ensure_timeout_param()).collect()
    }

    fn extract_timeout(arguments: &Value) -> std::time::Duration {
        let secs = arguments.get("timeout")
            .and_then(|v| v.as_u64())
            .or_else(|| arguments.get("timeout_ms").and_then(|v| v.as_u64().map(|ms| ms.div_ceil(1000))))
            .unwrap_or(30)
            .clamp(1, 300);
        std::time::Duration::from_secs(secs)
    }

    async fn handle_tools_call(&self, id: Option<Value>, params: Option<Value>) -> JsonRpcResponse {
        let params = match params {
            Some(p) => p,
            None => return JsonRpcResponse::error(id, -32602, "Missing params".to_string()),
        };

        let tool_name = match params.get("name").and_then(|n| n.as_str()) {
            Some(n) => n,
            None => return JsonRpcResponse::error(id, -32602, "Missing tool name".to_string()),
        };

        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        let timeout_dur = Self::extract_timeout(&arguments);

        let execution = self.execute_tool(tool_name, arguments);
        match tokio::time::timeout(timeout_dur, execution).await {
            Ok(resp_val) => JsonRpcResponse::success(id, resp_val),
            Err(_) => {
                let err_msg = format!("[omni-mcp Timeout] Tool '{}' exceeded hard-cap timeout of {}s", tool_name, timeout_dur.as_secs());
                JsonRpcResponse::success(id, serde_json::to_value(CallToolResult::error(err_msg)).unwrap())
            }
        }
    }

    async fn execute_tool(&self, tool_name: &str, arguments: Value) -> Value {
        if tool_name == "omni_status" {
            let status_report = self.get_status_report().await;
            return serde_json::to_value(CallToolResult::text(status_report)).unwrap();
        }

        // 1. Check native Rust modules
        for module in self.modules.values() {
            if module.tools().iter().any(|t| t.name == tool_name) {
                return match module.call_tool(tool_name, arguments).await {
                    Ok(res) => serde_json::to_value(res).unwrap(),
                    Err(e) => serde_json::to_value(CallToolResult::error(format!("[omni-mcp Error] Tool '{}' failed: {}", tool_name, e))).unwrap(),
                };
            }
        }

        // 2. Check sidecars
        for sidecar in &self.config.sidecars {
            if let Ok(worker) = self.get_or_spawn_sidecar(&sidecar.name).await {
                if let Ok(res) = worker.request("tools/call", Some(json!({ "name": tool_name, "arguments": arguments }))).await {
                    return res;
                }
            }
        }

        // 3. Check proxy targets
        for proxy in &self.config.proxies {
            if let Ok(res) = self.call_proxy_tool(proxy, tool_name, arguments.clone()).await {
                return res;
            }
        }

        let not_found_msg = format!("[omni-mcp Feedback] Tool '{}' is currently unavailable. Call 'omni_status' to view active modules.", tool_name);
        serde_json::to_value(CallToolResult::error(not_found_msg)).unwrap()
    }

    async fn get_status_report(&self) -> String {
        let mut report = String::from("# omni-mcp System Health & Diagnostic Report\n\n");

        report.push_str("## 1. Native Rust Modules (0ms Overhead, Instant Execution)\n");
        for name in self.modules.keys() {
            report.push_str(&format!("  - **{}**: ACTIVE (Loaded natively)\n", name));
        }

        report.push_str("\n## 2. Managed Sidecars (Python, Node, Binaries)\n");
        if self.config.sidecars.is_empty() {
            report.push_str("  (No sidecars configured)\n");
        } else {
            for sidecar in &self.config.sidecars {
                match self.get_or_spawn_sidecar(&sidecar.name).await {
                    Ok(_) => report.push_str(&format!("  - **{}**: RUNNING (`{}`)\n", sidecar.name, sidecar.command)),
                    Err(e) => report.push_str(&format!("  - **{}**: STOPPED / ERROR ({})\n", sidecar.name, e)),
                }
            }
        }

        report.push_str("\n## 3. Configured Proxies & Remote Endpoints\n");
        if self.config.proxies.is_empty() {
            report.push_str("  (No proxy endpoints configured)\n");
        } else {
            for proxy in &self.config.proxies {
                match self.fetch_proxy_tools(proxy).await {
                    Ok(tools) => report.push_str(&format!("  - **{}** ({}): ONLINE ({} tools registered)\n", proxy.name, proxy.url, tools.len())),
                    Err(err) => report.push_str(&format!("  - **{}** ({}): OFFLINE / UNREACHABLE ({})\n", proxy.name, proxy.url, err)),
                }
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
            "params": { "name": name, "arguments": args }
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
