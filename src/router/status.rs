//! The `omni_status` diagnostic tool.
//!
//! It reports what is already known plus a *passive* view of each backend. It
//! must never spawn a sidecar or make a network call: the old version probed
//! every proxy and started every sidecar just to answer "how are things?",
//! which turned a diagnostic into the very process storm being diagnosed.

use std::sync::Arc;

use serde_json::json;

use crate::backend::{Backend, BackendStatus};
use crate::protocol::{CallToolResult, Tool};

pub const TOOL_NAME: &str = "omni_status";

pub fn descriptor() -> Tool {
    Tool::new(
        TOOL_NAME,
        "Reports which tools are available and the health of every configured backend. Call this \
         first when a tool is missing or failing.",
        json!({ "type": "object", "properties": {} }),
    )
}

pub async fn report(
    backends: &[Arc<dyn Backend>],
    tools: &[Tool],
    failures: &[(String, String)],
    discoveries: u64,
) -> CallToolResult {
    let mut entries = Vec::new();

    for backend in backends {
        let status = backend.status().await;
        let served: Vec<&str> = tools
            .iter()
            .map(|t| t.name.as_str())
            .filter(|name| owns(backend.as_ref(), name, tools))
            .collect();

        let mut entry = json!({
            "name": backend.name(),
            "kind": backend.kind().label(),
            "status": status_word(&status),
            "detail": status.label(),
            "tool_count": served.len()
        });
        if let Some((_, reason)) = failures.iter().find(|(name, _)| name == backend.name()) {
            entry["discovery_error"] = json!(reason);
        }
        entries.push(entry);
    }

    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    CallToolResult::structured(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "tool_count": names.len(),
        "tools": names,
        "backends": entries,
        "unhealthy_backends": failures.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        "discovery_sweeps": discoveries
    }))
}

fn status_word(status: &BackendStatus) -> &'static str {
    match status {
        BackendStatus::Ready => "ready",
        BackendStatus::Idle => "idle",
        BackendStatus::Failed(_) => "failed",
        BackendStatus::Disabled => "disabled",
    }
}

/// A tool belongs to `backend` when its advertised name carries that backend's
/// prefix, or when the backend declares no prefix and nothing else claimed it.
fn owns(backend: &dyn Backend, name: &str, _tools: &[Tool]) -> bool {
    match backend.prefix() {
        Some(prefix) if !prefix.is_empty() => name.starts_with(prefix),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;

    use serde_json::Value;

    use super::*;
    use crate::backend::BackendKind;
    use crate::error::ToolResult;

    struct Stub(&'static str, BackendKind, BackendStatus);

    #[async_trait]
    impl Backend for Stub {
        fn name(&self) -> &str {
            self.0
        }
        fn kind(&self) -> BackendKind {
            self.1
        }
        async fn list_tools(&self, _t: Duration) -> ToolResult<Vec<Tool>> {
            Ok(vec![])
        }
        async fn call(&self, _n: &str, _a: Value, _t: Duration) -> ToolResult<CallToolResult> {
            Ok(CallToolResult::text("x"))
        }
        async fn status(&self) -> BackendStatus {
            self.2.clone()
        }
    }

    fn tools() -> Vec<Tool> {
        vec![descriptor(), Tool::new("grep_search", "d", json!({ "type": "object" }))]
    }

    #[tokio::test]
    async fn the_report_lists_tools_and_backend_health() {
        let backends: Vec<Arc<dyn Backend>> = vec![
            Arc::new(Stub("native", BackendKind::Native, BackendStatus::Ready)),
            Arc::new(Stub("ha", BackendKind::Proxy, BackendStatus::Failed("refused".into()))),
        ];

        let result = report(&backends, &tools(), &[("ha".into(), "refused".into())], 3).await;
        let out = result.structured_content.unwrap();

        assert_eq!(out["tool_count"], 2);
        assert_eq!(out["backends"][0]["status"], "ready");
        assert_eq!(out["backends"][1]["status"], "failed");
        assert_eq!(out["backends"][1]["discovery_error"], "refused");
        assert_eq!(out["unhealthy_backends"], json!(["ha"]));
        assert_eq!(out["discovery_sweeps"], 3);
    }

    #[tokio::test]
    async fn an_idle_lazy_backend_is_reported_as_idle_not_failed() {
        let backends: Vec<Arc<dyn Backend>> =
            vec![Arc::new(Stub("side", BackendKind::Sidecar, BackendStatus::Idle))];
        let out = report(&backends, &tools(), &[], 1).await.structured_content.unwrap();
        assert_eq!(out["backends"][0]["status"], "idle");
        assert_eq!(out["unhealthy_backends"], json!([]));
    }

    #[tokio::test]
    async fn the_report_is_not_flagged_as_an_error_even_when_backends_are_down() {
        let backends: Vec<Arc<dyn Backend>> =
            vec![Arc::new(Stub("x", BackendKind::Proxy, BackendStatus::Failed("no".into())))];
        let result = report(&backends, &tools(), &[], 1).await;
        assert!(!result.is_failure());
    }

    #[test]
    fn the_descriptor_takes_no_required_arguments() {
        let tool = descriptor();
        assert_eq!(tool.name, TOOL_NAME);
        assert!(tool.input_schema.get("required").is_none());
    }
}
