//! Request dispatch: MCP methods in, JSON-RPC responses out.

pub mod routes;
pub mod status;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tracing::{debug, warn};

use crate::backend::{
    Backend, native::NativeBackend, proxy::ProxyBackend, sidecar::SidecarBackend, strip_prefix,
};
use crate::config::Config;
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, ProtocolVersion, Request, Response, Tool, code};
use routes::Routes;

pub struct Router {
    backends: Vec<Arc<dyn Backend>>,
    routes: RwLock<Option<Routes>>,
    config: Config,
    /// Bounds how many tool calls run at once, so a client that fires twenty
    /// parallel calls cannot spawn twenty subprocess trees.
    call_permits: Semaphore,
    /// Serialises discovery. Without it, every caller that arrives after the
    /// TTL expires starts its own full sweep of every backend — a stampede of
    /// redundant round trips against exactly the endpoints we are trying not
    /// to overload.
    discovery_lock: Mutex<()>,
    /// How many discovery sweeps have run. Reported by `omni_status`, and the
    /// signal that coalescing actually works.
    discoveries: AtomicU64,
}

impl Router {
    /// Builds every configured backend. Native tools come first so they always
    /// win a name collision against a remote server.
    pub fn build(config: Config) -> Result<Self, ToolError> {
        let mut backends: Vec<Arc<dyn Backend>> = Vec::new();

        backends.push(Arc::new(NativeBackend::from_config(&config)));

        let spawn_permits = Arc::new(Semaphore::new(config.limits.max_concurrent_spawns));
        for sidecar in config.enabled_sidecars() {
            backends
                .push(Arc::new(SidecarBackend::new(sidecar.clone(), Arc::clone(&spawn_permits))));
        }

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("omni-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ToolError::Failed(format!("could not build the HTTP client: {e}")))?;
        for proxy in config.enabled_proxies() {
            backends.push(ProxyBackend::new(proxy.clone(), client.clone())?);
        }

        let call_permits = Semaphore::new(config.limits.max_concurrent_calls);
        Ok(Self {
            backends,
            routes: RwLock::new(None),
            config,
            call_permits,
            discovery_lock: Mutex::new(()),
            discoveries: AtomicU64::new(0),
        })
    }

    /// Eagerly starts non-lazy sidecars and warms the routing table.
    pub async fn warm_up(&self) {
        let eager: Vec<&str> =
            self.config.enabled_sidecars().filter(|s| !s.lazy).map(|s| s.name.as_str()).collect();
        if !eager.is_empty() {
            debug!(?eager, "starting non-lazy sidecars");
        }
        self.refresh().await;
    }

    /// Answers one JSON-RPC message. Returns `None` for notifications, which
    /// must not be answered.
    pub async fn handle(&self, request: Request) -> Option<Response> {
        if request.is_notification() {
            debug!(method = %request.method, "notification");
            return None;
        }
        let id = request.id.clone();

        let response = match request.method.as_str() {
            "initialize" => Response::success(id, initialize(&request.params_or_empty())),
            "tools/list" => Response::success(id, json!({ "tools": self.list_tools().await })),
            "tools/call" => self.tools_call(id, request.params).await,
            "prompts/list" => Response::success(id, json!({ "prompts": [] })),
            "resources/list" => Response::success(id, json!({ "resources": [] })),
            "resources/templates/list" => Response::success(id, json!({ "resourceTemplates": [] })),
            "ping" | "logging/setLevel" => Response::success(id, json!({})),
            other => Response::error(
                id,
                code::METHOD_NOT_FOUND,
                format!("method {other:?} is not supported by omni-mcp"),
            ),
        };
        Some(response)
    }

    pub async fn list_tools(&self) -> Vec<Tool> {
        self.ensure_fresh().await;
        let guard = self.routes.read().await;
        let mut tools = vec![status::descriptor()];
        if let Some(routes) = guard.as_ref() {
            tools.extend(routes.tools().iter().cloned());
        }
        tools
    }

    async fn tools_call(&self, id: Option<Value>, params: Option<Value>) -> Response {
        let Some(params) = params else {
            return Response::error(id, code::INVALID_PARAMS, "tools/call requires params");
        };
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Response::error(id, code::INVALID_PARAMS, "tools/call requires a tool name");
        };
        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let timeout = self.deadline(&arguments);

        match self.invoke(name, arguments, timeout).await {
            Ok(result) => Response::success(id, result.into_value()),
            // A tool that fails is a *result*, not a transport error: the model
            // needs to read the message and try something else.
            Err(
                err @ (ToolError::Failed(_)
                | ToolError::Unavailable { .. }
                | ToolError::Denied(_)
                | ToolError::Timeout { .. }),
            ) => {
                warn!(tool = %name, %err, "tool call failed");
                Response::success(id, CallToolResult::error(err.to_string()).into_value())
            }
            Err(err) => Response::error(id, err.rpc_code(), err.to_string()),
        }
    }

    /// Resolves `name` to its owning backend and runs it.
    pub async fn invoke(
        &self,
        name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> ToolResult<CallToolResult> {
        if name == status::TOOL_NAME {
            return Ok(self.status_report().await);
        }

        let _permit = self
            .call_permits
            .acquire()
            .await
            .map_err(|_| ToolError::Failed("the gateway is shutting down".into()))?;

        let (index, strip) = if let Some(found) = self.resolve(name).await {
            found
        } else {
            // The tool may have appeared since the last discovery.
            self.refresh().await;
            self.resolve(name).await.ok_or_else(|| ToolError::NotFound(name.to_string()))?
        };

        let backend = Arc::clone(&self.backends[index]);
        let local_name = strip_prefix(backend.prefix(), name).to_string();
        let arguments = if strip { routes::strip_injected_timeout(arguments) } else { arguments };

        backend.call(&local_name, arguments, timeout).await
    }

    /// Returns the owning backend index and whether to strip `timeout`.
    async fn resolve(&self, name: &str) -> Option<(usize, bool)> {
        self.ensure_fresh().await;
        let guard = self.routes.read().await;
        let routes = guard.as_ref()?;
        Some((routes.owner(name)?, routes.was_augmented(name)))
    }

    /// Rediscovers only if the table is missing or past its TTL.
    ///
    /// Callers that pile up behind the lock re-check staleness once they hold
    /// it, so the first one through refreshes and the rest simply use its
    /// result.
    async fn ensure_fresh(&self) {
        if !self.is_stale().await {
            return;
        }
        let _guard = self.discovery_lock.lock().await;
        if !self.is_stale().await {
            return;
        }
        self.discover_now().await;
    }

    async fn is_stale(&self) -> bool {
        self.routes
            .read()
            .await
            .as_ref()
            .is_none_or(|routes| routes.is_stale(self.config.limits.discovery_ttl.get()))
    }

    /// Forces a rediscovery, still serialised against concurrent sweeps.
    pub async fn refresh(&self) {
        let _guard = self.discovery_lock.lock().await;
        self.discover_now().await;
    }

    /// Performs one sweep. The caller must hold `discovery_lock`.
    async fn discover_now(&self) {
        self.discoveries.fetch_add(1, Ordering::Relaxed);
        let discovered =
            Routes::discover(&self.backends, self.config.limits.discovery_timeout.get()).await;
        *self.routes.write().await = Some(discovered);
    }

    /// Number of discovery sweeps performed so far.
    pub fn discovery_count(&self) -> u64 {
        self.discoveries.load(Ordering::Relaxed)
    }

    async fn status_report(&self) -> CallToolResult {
        self.ensure_fresh().await;
        let guard = self.routes.read().await;
        let (tools, failures) = match guard.as_ref() {
            Some(routes) => (routes.tools().to_vec(), routes.failures().to_vec()),
            None => (Vec::new(), Vec::new()),
        };
        drop(guard);

        let mut all = vec![status::descriptor()];
        all.extend(tools);
        status::report(&self.backends, &all, &failures, self.discovery_count()).await
    }

    /// Chooses the deadline for a call: the caller's `timeout`/`timeout_ms`
    /// argument if present, clamped to the configured maximum.
    fn deadline(&self, arguments: &Value) -> Duration {
        let limits = &self.config.limits;
        let seconds = arguments
            .get("timeout")
            .and_then(Value::as_u64)
            .or_else(|| {
                arguments.get("timeout_ms").and_then(Value::as_u64).map(|ms| ms.div_ceil(1000))
            })
            .map_or(limits.default_tool_timeout.get().as_secs(), |s| s);

        Duration::from_secs(seconds.clamp(1, limits.max_tool_timeout.get().as_secs().max(1)))
    }
}

/// Builds the `initialize` result, negotiating the protocol version against
/// what the client proposed.
fn initialize(params: &Value) -> Value {
    let negotiated =
        ProtocolVersion::negotiate(params.get("protocolVersion").and_then(Value::as_str));
    json!({
        "protocolVersion": negotiated.as_str(),
        "capabilities": { "tools": { "listChanged": false }, "logging": {} },
        "serverInfo": { "name": "omni-mcp", "version": env!("CARGO_PKG_VERSION") }
    })
}

#[cfg(test)]
mod tests;
