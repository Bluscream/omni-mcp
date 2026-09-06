//! The tool routing table.
//!
//! Discovery asks every backend what it serves and records, per advertised tool
//! name, exactly which backend owns it. The previous implementation had no such
//! table: an unrecognised `tools/call` was sent to every sidecar (spawning each
//! one) and then to every configured proxy in turn, keeping whichever answered
//! first. That leaked the caller's arguments to unrelated third-party
//! endpoints and made an unknown-tool typo expensive.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::backend::{Backend, apply_prefix};
use crate::error::ToolError;
use crate::protocol::Tool;

/// A resolved snapshot of what every backend serves.
pub struct Routes {
    built_at: Instant,
    tools: Vec<Tool>,
    owners: HashMap<String, usize>,
    /// Tools whose schema we augmented with a `timeout` property, and from
    /// which that argument must therefore be stripped before dispatch.
    augmented: HashSet<String>,
    /// Backends that failed discovery, with the reason, for `omni_status`.
    failures: Vec<(String, String)>,
}

impl Routes {
    pub fn is_stale(&self, ttl: Duration) -> bool {
        self.built_at.elapsed() >= ttl
    }

    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// The backend index serving `name`, if any.
    pub fn owner(&self, name: &str) -> Option<usize> {
        self.owners.get(name).copied()
    }

    pub fn was_augmented(&self, name: &str) -> bool {
        self.augmented.contains(name)
    }

    pub fn failures(&self) -> &[(String, String)] {
        &self.failures
    }

    /// Queries every backend concurrently and assembles the table.
    ///
    /// A backend that fails or times out is recorded and skipped; it never
    /// prevents the others from being listed, because a client that gets no
    /// tool list treats the whole gateway as broken.
    pub async fn discover(backends: &[Arc<dyn Backend>], timeout: Duration) -> Self {
        let queries = backends.iter().enumerate().map(|(index, backend)| async move {
            // Enforce the deadline here rather than trusting each backend to
            // honour it internally. A sidecar's connect phase is governed by its
            // own startup_timeout, so without this outer bound a handful of slow
            // sidecars could stall `tools/list` for minutes.
            let outcome = match tokio::time::timeout(timeout, backend.list_tools(timeout)).await {
                Ok(outcome) => outcome,
                Err(_) => Err(ToolError::Timeout {
                    tool: format!("{}:tools/list", backend.name()),
                    seconds: timeout.as_secs(),
                }),
            };
            (index, Arc::clone(backend), outcome)
        });
        let results = futures::future::join_all(queries).await;

        let mut table = Self {
            built_at: Instant::now(),
            tools: Vec::new(),
            owners: HashMap::new(),
            augmented: HashSet::new(),
            failures: Vec::new(),
        };

        for (index, backend, outcome) in results {
            match outcome {
                Ok(tools) => table.absorb(index, backend.as_ref(), tools),
                Err(err) => {
                    warn!(backend = backend.name(), %err, "tool discovery failed");
                    table.failures.push((backend.name().to_string(), err.to_string()));
                }
            }
        }

        table
    }

    fn absorb(&mut self, index: usize, backend: &dyn Backend, tools: Vec<Tool>) {
        for tool in tools {
            let advertised = apply_prefix(backend.prefix(), &tool.name);

            if let Some(&existing) = self.owners.get(&advertised) {
                if existing != index {
                    warn!(
                        tool = %advertised,
                        backend = backend.name(),
                        "tool name already claimed by an earlier backend; ignoring the duplicate. \
                         Set a `prefix` on one of them to expose both."
                    );
                }
                continue;
            }

            let (tool, was_augmented) = ensure_timeout_property(tool.with_name(advertised.clone()));
            if was_augmented {
                self.augmented.insert(advertised.clone());
            }
            self.owners.insert(advertised, index);
            self.tools.push(tool);
        }
        debug!(backend = backend.name(), total = self.tools.len(), "absorbed tools");
    }
}

/// Adds an optional `timeout` argument so a caller can bound a slow tool.
/// Returns whether the schema was actually modified.
fn ensure_timeout_property(mut tool: Tool) -> (Tool, bool) {
    let Some(schema) = tool.input_schema.as_object_mut() else { return (tool, false) };

    let properties = schema.entry("properties").or_insert_with(|| json!({}));
    let Some(properties) = properties.as_object_mut() else { return (tool, false) };

    if properties.contains_key("timeout") || properties.contains_key("timeout_ms") {
        return (tool, false);
    }

    properties.insert(
        "timeout".to_string(),
        json!({
            "type": "integer",
            "description": "Optional. Abort this call after this many seconds."
        }),
    );
    (tool, true)
}

/// Removes the injected `timeout` argument so it is never forwarded to a
/// backend whose schema validation would reject an unknown property.
pub fn strip_injected_timeout(mut args: Value) -> Value {
    if let Some(object) = args.as_object_mut() {
        object.remove("timeout");
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendKind, BackendStatus};
    use crate::error::{ToolError, ToolResult};
    use crate::protocol::CallToolResult;
    use async_trait::async_trait;

    struct Stub {
        name: &'static str,
        prefix: Option<&'static str>,
        tools: Vec<&'static str>,
        fail: bool,
    }

    impl Stub {
        fn serving(name: &'static str, tools: Vec<&'static str>) -> Arc<dyn Backend> {
            Arc::new(Self { name, prefix: None, tools, fail: false })
        }

        fn prefixed(
            name: &'static str,
            prefix: &'static str,
            tools: Vec<&'static str>,
        ) -> Arc<dyn Backend> {
            Arc::new(Self { name, prefix: Some(prefix), tools, fail: false })
        }

        fn broken(name: &'static str) -> Arc<dyn Backend> {
            Arc::new(Self { name, prefix: None, tools: vec![], fail: true })
        }
    }

    #[async_trait]
    impl Backend for Stub {
        fn name(&self) -> &str {
            self.name
        }
        fn kind(&self) -> BackendKind {
            BackendKind::Native
        }
        fn prefix(&self) -> Option<&str> {
            self.prefix
        }
        async fn list_tools(&self, _t: Duration) -> ToolResult<Vec<Tool>> {
            if self.fail {
                return Err(ToolError::Unavailable {
                    backend: self.name.into(),
                    reason: "down".into(),
                });
            }
            Ok(self
                .tools
                .iter()
                .map(|n| Tool::new(*n, "desc", json!({ "type": "object", "properties": {} })))
                .collect())
        }
        async fn call(&self, _n: &str, _a: Value, _t: Duration) -> ToolResult<CallToolResult> {
            Ok(CallToolResult::text("ok"))
        }
        async fn status(&self) -> BackendStatus {
            BackendStatus::Ready
        }
    }

    async fn table(backends: Vec<Arc<dyn Backend>>) -> Routes {
        Routes::discover(&backends, Duration::from_secs(5)).await
    }

    #[tokio::test]
    async fn each_tool_maps_to_the_backend_that_serves_it() {
        let routes =
            table(vec![Stub::serving("a", vec!["one"]), Stub::serving("b", vec!["two"])]).await;
        assert_eq!(routes.owner("one"), Some(0));
        assert_eq!(routes.owner("two"), Some(1));
        assert_eq!(routes.owner("three"), None);
        assert_eq!(routes.tools().len(), 2);
    }

    #[tokio::test]
    async fn a_failing_backend_does_not_suppress_the_others() {
        let routes = table(vec![Stub::broken("dead"), Stub::serving("live", vec!["works"])]).await;
        assert_eq!(routes.owner("works"), Some(1));
        assert_eq!(routes.failures().len(), 1);
        assert_eq!(routes.failures()[0].0, "dead");
    }

    #[tokio::test]
    async fn the_first_backend_to_claim_a_name_keeps_it() {
        let routes = table(vec![
            Stub::serving("first", vec!["shared"]),
            Stub::serving("second", vec!["shared"]),
        ])
        .await;
        assert_eq!(routes.owner("shared"), Some(0));
        assert_eq!(routes.tools().len(), 1, "the duplicate must not be advertised twice");
    }

    #[tokio::test]
    async fn a_prefix_lets_two_backends_expose_the_same_tool_name() {
        let routes = table(vec![
            Stub::serving("first", vec!["search"]),
            Stub::prefixed("second", "ha_", vec!["search"]),
        ])
        .await;

        assert_eq!(routes.owner("search"), Some(0));
        assert_eq!(routes.owner("ha_search"), Some(1));
        assert_eq!(routes.tools().len(), 2);
    }

    #[tokio::test]
    async fn a_timeout_property_is_injected_and_tracked() {
        let routes = table(vec![Stub::serving("a", vec!["one"])]).await;
        let tool = &routes.tools()[0];
        assert!(tool.input_schema["properties"]["timeout"].is_object());
        assert!(routes.was_augmented("one"));
    }

    #[test]
    fn a_tool_declaring_its_own_timeout_is_left_alone() {
        let tool = Tool::new(
            "t",
            "d",
            json!({ "type": "object", "properties": { "timeout": { "type": "string" } } }),
        );
        let (tool, augmented) = ensure_timeout_property(tool);
        assert!(!augmented);
        assert_eq!(tool.input_schema["properties"]["timeout"]["type"], "string");

        let tool = Tool::new(
            "t",
            "d",
            json!({ "type": "object", "properties": { "timeout_ms": { "type": "integer" } } }),
        );
        assert!(!ensure_timeout_property(tool).1);
    }

    #[test]
    fn the_injected_timeout_is_stripped_before_dispatch() {
        let args = json!({ "path": "/tmp", "timeout": 5 });
        let stripped = strip_injected_timeout(args);
        assert_eq!(stripped, json!({ "path": "/tmp" }));
    }

    #[test]
    fn stripping_tolerates_non_object_arguments() {
        assert_eq!(strip_injected_timeout(json!([1, 2])), json!([1, 2]));
        assert_eq!(strip_injected_timeout(json!(null)), json!(null));
    }

    /// A backend that never answers, to prove discovery is bounded.
    struct Hangs;

    #[async_trait]
    impl Backend for Hangs {
        fn name(&self) -> &'static str {
            "hangs"
        }
        fn kind(&self) -> BackendKind {
            BackendKind::Sidecar
        }
        async fn list_tools(&self, _t: Duration) -> ToolResult<Vec<Tool>> {
            // Ignores the deadline it was handed, exactly like a sidecar stuck
            // in its connect phase.
            std::future::pending::<()>().await;
            unreachable!()
        }
        async fn call(&self, _n: &str, _a: Value, _t: Duration) -> ToolResult<CallToolResult> {
            Ok(CallToolResult::text("x"))
        }
        async fn status(&self) -> BackendStatus {
            BackendStatus::Idle
        }
    }

    #[tokio::test]
    async fn a_backend_that_ignores_its_deadline_cannot_stall_discovery() {
        let backends: Vec<Arc<dyn Backend>> =
            vec![Arc::new(Hangs), Stub::serving("live", vec!["works"])];

        let started = std::time::Instant::now();
        let routes = Routes::discover(&backends, Duration::from_millis(200)).await;

        assert!(started.elapsed() < Duration::from_secs(5), "discovery was not bounded");
        assert_eq!(routes.owner("works"), Some(1), "the healthy backend must still be listed");
        assert_eq!(routes.failures().len(), 1);
        assert_eq!(routes.failures()[0].0, "hangs");
    }

    #[tokio::test]
    async fn staleness_is_measured_against_the_ttl() {
        let routes = table(vec![Stub::serving("a", vec!["one"])]).await;
        assert!(!routes.is_stale(Duration::from_secs(60)));
        assert!(routes.is_stale(Duration::from_nanos(1)));
    }
}
