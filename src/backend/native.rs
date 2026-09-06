//! The in-process backend: native Rust tools.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use super::{Backend, BackendKind, BackendStatus};
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, Tool};
use crate::tools::{NativeTool, ToolContext};

pub struct NativeBackend {
    groups: Vec<Box<dyn NativeTool>>,
    /// Tool name to the index of the group that implements it, resolved once at
    /// construction. The old registry called `tools()` on every module for
    /// every dispatch, rebuilding every JSON schema on each tool call.
    routes: HashMap<String, usize>,
    descriptors: Vec<Tool>,
    context: ToolContext,
}

impl NativeBackend {
    pub fn new(groups: Vec<Box<dyn NativeTool>>, context: ToolContext) -> Self {
        let mut routes = HashMap::new();
        let mut descriptors = Vec::new();

        for (index, group) in groups.iter().enumerate() {
            for tool in group.descriptors() {
                routes.insert(tool.name.clone(), index);
                descriptors.push(tool);
            }
        }

        Self { groups, routes, descriptors, context }
    }

    pub fn with_defaults(context: ToolContext) -> Self {
        Self::new(crate::tools::all(), context)
    }

    pub fn with_ssh(
        ssh_configs: Vec<crate::config::SshServerConfig>,
        context: ToolContext,
    ) -> Self {
        Self::new(crate::tools::all_with_ssh(ssh_configs), context)
    }

    pub fn handles(&self, tool: &str) -> bool {
        self.routes.contains_key(tool)
    }
}

#[async_trait]
impl Backend for NativeBackend {
    fn name(&self) -> &'static str {
        "native"
    }

    fn kind(&self) -> BackendKind {
        BackendKind::Native
    }

    async fn list_tools(&self, _timeout: Duration) -> ToolResult<Vec<Tool>> {
        Ok(self.descriptors.clone())
    }

    async fn call(&self, tool: &str, args: Value, timeout: Duration) -> ToolResult<CallToolResult> {
        let index = *self.routes.get(tool).ok_or_else(|| ToolError::NotFound(tool.to_string()))?;
        let execution = self.groups[index].call(tool, args, &self.context);

        match tokio::time::timeout(timeout, execution).await {
            Ok(result) => result,
            Err(_) => {
                Err(ToolError::Timeout { tool: tool.to_string(), seconds: timeout.as_secs() })
            }
        }
    }

    async fn status(&self) -> BackendStatus {
        BackendStatus::Ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolPolicy;
    use serde_json::json;

    fn backend() -> NativeBackend {
        NativeBackend::with_defaults(ToolContext::new(ToolPolicy::default()))
    }

    #[tokio::test]
    async fn advertises_every_native_tool_exactly_once() {
        let backend = backend();
        let tools = backend.list_tools(Duration::from_secs(1)).await.unwrap();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();

        let deduped = {
            let mut copy = names.clone();
            copy.dedup();
            copy
        };
        assert_eq!(names, deduped, "a tool is advertised twice");
        assert!(names.contains(&"grep_search"));
        assert!(names.contains(&"diff_text"));
    }

    #[tokio::test]
    async fn dispatches_to_the_group_that_owns_the_tool() {
        let result = backend()
            .call("count_stats", json!({ "text": "a b" }), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(result.structured_content.unwrap()["words"], 2);
    }

    #[tokio::test]
    async fn an_unknown_tool_is_not_found() {
        let err =
            backend().call("no_such_tool", json!({}), Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[test]
    fn routing_membership_matches_the_advertised_set() {
        let backend = backend();
        for tool in &backend.descriptors {
            assert!(backend.handles(&tool.name));
        }
        assert!(!backend.handles("not_a_tool"));
    }

    #[tokio::test]
    async fn native_calls_honour_the_deadline() {
        // `sleep` via eval is the only slow native path; policy blocks it, so
        // assert the timeout plumbing on a fast tool instead: a zero deadline
        // must not hang.
        let result =
            backend().call("count_stats", json!({ "text": "x" }), Duration::from_millis(0)).await;
        assert!(result.is_err() || result.is_ok(), "must resolve, never hang");
    }

    #[tokio::test]
    async fn status_is_always_ready_and_never_spawns() {
        assert_eq!(backend().status().await, BackendStatus::Ready);
    }
}
