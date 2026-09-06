//! Remote MCP servers reached over HTTP.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::warn;

use super::{Backend, BackendKind, BackendStatus};
use crate::config::ProxyConfig;
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, Request, Response, Tool};

pub struct ProxyBackend {
    config: ProxyConfig,
    client: reqwest::Client,
    headers: HeaderMap,
    last_error: Mutex<Option<String>>,
    contacted: Mutex<bool>,
    next_id: std::sync::atomic::AtomicU64,
}

impl ProxyBackend {
    /// Builds a proxy client. Malformed header names or values are reported
    /// rather than silently dropped, which is how the old code behaved.
    pub fn new(config: ProxyConfig, client: reqwest::Client) -> Result<Arc<Self>, ToolError> {
        let mut headers = HeaderMap::new();

        // MCP's Streamable HTTP transport requires the client to accept both
        // encodings; servers reject the request outright otherwise. Home
        // Assistant answers 400 and omniroute 406 without this.
        headers.insert(ACCEPT, HeaderValue::from_static("application/json, text/event-stream"));

        if let Some(bearer) = config.bearer.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
            let mut value = HeaderValue::from_str(&format!("Bearer {bearer}")).map_err(|_| {
                ToolError::InvalidArguments(format!(
                    "proxy {:?} has a bearer token containing characters that are not valid in an HTTP header",
                    config.name
                ))
            })?;
            // Keeps the credential out of `{:?}` output and error messages.
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }

        for (key, raw) in &config.headers {
            let name = HeaderName::from_bytes(key.as_bytes()).map_err(|_| {
                ToolError::InvalidArguments(format!(
                    "proxy {:?} has an invalid header name {key:?}",
                    config.name
                ))
            })?;
            let value = HeaderValue::from_str(raw).map_err(|_| {
                ToolError::InvalidArguments(format!(
                    "proxy {:?} has an invalid value for header {key:?}",
                    config.name
                ))
            })?;
            headers.insert(name, value);
        }

        Ok(Arc::new(Self {
            config,
            client,
            headers,
            last_error: Mutex::new(None),
            contacted: Mutex::new(false),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }))
    }

    async fn rpc(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> ToolResult<Value> {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let envelope = Request {
            jsonrpc: Some("2.0".into()),
            id: Some(json!(id)),
            method: method.to_string(),
            params,
        };

        let outcome = self
            .client
            .post(&self.config.url)
            .headers(self.headers.clone())
            .timeout(timeout)
            .json(&envelope)
            .send()
            .await;

        let result = self.interpret(outcome).await;
        self.record(&result).await;
        result
    }

    async fn interpret(
        &self,
        outcome: Result<reqwest::Response, reqwest::Error>,
    ) -> ToolResult<Value> {
        let response = outcome.map_err(|e| {
            let reason = if e.is_timeout() {
                "request timed out".to_string()
            } else if e.is_connect() {
                "connection refused or host unreachable".to_string()
            } else {
                e.to_string()
            };
            ToolError::Unavailable { backend: self.config.name.clone(), reason }
        })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ToolError::Unavailable {
                backend: self.config.name.clone(),
                reason: format!("HTTP {status}: {}", super::snippet(&body)),
            });
        }

        // Some MCP servers answer JSON-RPC over an SSE-framed body. Accept both.
        let body = response
            .text()
            .await
            .map_err(|e| ToolError::Failed(format!("could not read proxy body: {e}")))?;
        let envelope: Response = parse_body(&body).ok_or_else(|| {
            ToolError::Failed(format!(
                "proxy returned a body that is not JSON-RPC: {}",
                super::snippet(&body)
            ))
        })?;

        envelope.into_result().map_err(|e| ToolError::Failed(e.message))
    }

    async fn record(&self, result: &ToolResult<Value>) {
        *self.contacted.lock().await = true;
        *self.last_error.lock().await = match result {
            Ok(_) => None,
            Err(err) => Some(err.to_string()),
        };
    }
}

/// Accepts either a bare JSON-RPC body or one wrapped in SSE `data:` frames.
fn parse_body(body: &str) -> Option<Response> {
    if let Ok(parsed) = serde_json::from_str::<Response>(body) {
        return Some(parsed);
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|payload| serde_json::from_str::<Response>(payload.trim()).ok())
        .next_back()
}

#[async_trait]
impl Backend for ProxyBackend {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn kind(&self) -> BackendKind {
        BackendKind::Proxy
    }

    fn prefix(&self) -> Option<&str> {
        self.config.prefix.as_deref()
    }

    async fn list_tools(&self, timeout: Duration) -> ToolResult<Vec<Tool>> {
        let value = self.rpc("tools/list", None, timeout.min(self.config.timeout.get())).await?;
        let tools = value.get("tools").cloned().unwrap_or_else(|| json!([]));
        serde_json::from_value(tools).map_err(|e| {
            warn!(proxy = %self.config.name, %e, "malformed tool list");
            ToolError::Failed(format!("proxy sent a malformed tool list: {e}"))
        })
    }

    async fn call(&self, tool: &str, args: Value, timeout: Duration) -> ToolResult<CallToolResult> {
        let params = json!({ "name": tool, "arguments": args });
        let value =
            self.rpc("tools/call", Some(params), timeout.min(self.config.timeout.get())).await?;
        serde_json::from_value(value)
            .map_err(|e| ToolError::Failed(format!("proxy sent a malformed tool result: {e}")))
    }

    async fn status(&self) -> BackendStatus {
        if let Some(err) = self.last_error.lock().await.clone() {
            return BackendStatus::Failed(err);
        }
        if *self.contacted.lock().await { BackendStatus::Ready } else { BackendStatus::Idle }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::duration::HumanDuration;

    fn config(url: &str) -> ProxyConfig {
        ProxyConfig {
            name: "p".into(),
            url: url.into(),
            bearer: None,
            headers: std::collections::BTreeMap::new(),
            timeout: HumanDuration::secs(2),
            prefix: None,
            enabled: true,
        }
    }

    #[test]
    fn bearer_token_becomes_a_sensitive_authorization_header() {
        let mut cfg = config("http://x.invalid/mcp");
        cfg.bearer = Some("s3cret".into());
        let backend = ProxyBackend::new(cfg, reqwest::Client::new()).unwrap();

        let header = backend.headers.get(AUTHORIZATION).unwrap();
        assert!(header.is_sensitive(), "credential must not leak into debug output");
        assert_eq!(header.to_str().unwrap(), "Bearer s3cret");
        assert!(!format!("{header:?}").contains("s3cret"));
    }

    #[test]
    fn every_request_accepts_both_json_and_sse() {
        // Live servers reject us without this: Home Assistant with 400 and
        // omniroute with an explicit "must accept both" 406.
        let backend =
            ProxyBackend::new(config("http://x.invalid/mcp"), reqwest::Client::new()).unwrap();
        let accept = backend.headers.get(ACCEPT).unwrap().to_str().unwrap();
        assert!(accept.contains("application/json"), "{accept}");
        assert!(accept.contains("text/event-stream"), "{accept}");
    }

    #[test]
    fn a_blank_bearer_does_not_produce_an_empty_authorization_header() {
        let mut cfg = config("http://x.invalid/mcp");
        cfg.bearer = Some("   ".into());
        let backend = ProxyBackend::new(cfg, reqwest::Client::new()).unwrap();
        assert!(backend.headers.get(AUTHORIZATION).is_none());
    }

    #[test]
    fn a_configured_accept_header_overrides_the_default() {
        let mut cfg = config("http://x.invalid/mcp");
        cfg.headers.insert("accept".into(), "application/json".into());
        let backend = ProxyBackend::new(cfg, reqwest::Client::new()).unwrap();
        assert_eq!(backend.headers.get(ACCEPT).unwrap(), "application/json");
    }

    #[test]
    fn invalid_header_configuration_is_reported_not_dropped() {
        let mut cfg = config("http://x.invalid/mcp");
        cfg.headers.insert("bad header name".into(), "v".into());
        assert!(ProxyBackend::new(cfg, reqwest::Client::new()).is_err());

        let mut cfg = config("http://x.invalid/mcp");
        cfg.bearer = Some("tok\nInjected: yes".into());
        assert!(ProxyBackend::new(cfg, reqwest::Client::new()).is_err());
    }

    #[test]
    fn parses_plain_json_rpc_bodies() {
        let parsed = parse_body(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#).unwrap();
        assert!(parsed.result.is_some());
    }

    #[test]
    fn parses_sse_framed_bodies_and_takes_the_last_frame() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"n\":1}}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"n\":2}}\n\n";
        let parsed = parse_body(body).unwrap();
        assert_eq!(parsed.result.unwrap()["n"], json!(2));
    }

    #[test]
    fn rejects_bodies_that_are_not_json_rpc_at_all() {
        assert!(parse_body("<html>gateway error</html>").is_none());
    }

    #[tokio::test]
    async fn an_unreachable_proxy_is_unavailable_rather_than_fatal() {
        // Port 1 on loopback refuses connections immediately.
        let backend =
            ProxyBackend::new(config("http://127.0.0.1:1/mcp"), reqwest::Client::new()).unwrap();
        let err = backend.list_tools(Duration::from_secs(2)).await.unwrap_err();
        assert!(matches!(err, ToolError::Unavailable { .. }), "got {err:?}");
        assert!(matches!(backend.status().await, BackendStatus::Failed(_)));
    }

    #[tokio::test]
    async fn status_is_idle_until_first_contact() {
        let backend =
            ProxyBackend::new(config("http://127.0.0.1:1/mcp"), reqwest::Client::new()).unwrap();
        assert_eq!(backend.status().await, BackendStatus::Idle);
    }
}
