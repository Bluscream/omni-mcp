//! Supervised child-process MCP servers speaking JSON-RPC over stdio.
//!
//! Three defects in the previous implementation are addressed here.
//!
//! 1. **Stream desynchronisation.** Every request was sent with `"id": 1` and
//!    answered by reading exactly one line. A sidecar that emitted a
//!    notification, a log line, or answered out of order permanently offset the
//!    stream, so every later call returned some other call's result. Requests
//!    now carry monotonic ids and a dedicated reader task demultiplexes
//!    responses to the waiting caller, discarding anything unmatched.
//!
//! 2. **Head-of-line blocking on the registry lock.** Spawning held a mutex
//!    over the whole sidecar map across an un-timed `initialize` round trip, so
//!    one hanging sidecar froze every other backend — the exact failure mode
//!    that motivated this project. Spawning now happens per-sidecar, under a
//!    deadline, and gated by a small global semaphore so N sidecars can never
//!    launch simultaneously.
//!
//! 3. **Leaked processes.** Children were never killed, and stderr went to
//!    `/dev/null`, making failures undiagnosable. Children are now killed on
//!    drop and their stderr is surfaced through `tracing`.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, Semaphore, oneshot};
use tracing::{debug, info, warn};

use super::{Backend, BackendKind, BackendStatus, snippet};
use crate::config::SidecarConfig;
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, Response, Tool, version};

/// A live connection to one child process.
struct Connection {
    stdin: Mutex<ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Response>>>,
    next_id: AtomicU64,
    alive: AtomicBool,
    /// Held so the child is killed when the connection is dropped
    /// (`kill_on_drop` is set on the command).
    _child: Child,
}

impl Connection {
    fn fail_all(&self) {
        self.alive.store(false, Ordering::SeqCst);
        // Dropping the senders wakes every waiter with a receive error.
        if let Ok(mut pending) = self.pending.try_lock() {
            pending.clear();
        }
    }
}

pub struct SidecarBackend {
    config: SidecarConfig,
    connection: Mutex<Option<Arc<Connection>>>,
    /// Shared across all sidecars: bounds simultaneous process creation.
    spawn_permits: Arc<Semaphore>,
    last_error: Mutex<Option<String>>,
}

impl SidecarBackend {
    pub fn new(config: SidecarConfig, spawn_permits: Arc<Semaphore>) -> Self {
        Self { config, connection: Mutex::new(None), spawn_permits, last_error: Mutex::new(None) }
    }

    pub fn config(&self) -> &SidecarConfig {
        &self.config
    }

    /// Returns a live connection, spawning or respawning as needed.
    async fn connect(&self) -> ToolResult<Arc<Connection>> {
        let mut slot = self.connection.lock().await;

        if let Some(existing) = slot.as_ref() {
            if existing.alive.load(Ordering::SeqCst) {
                return Ok(Arc::clone(existing));
            }
            if !self.config.restart_on_failure {
                return Err(self.unavailable("process exited and restart_on_failure is false"));
            }
            info!(sidecar = %self.config.name, "restarting exited sidecar");
            *slot = None;
        }

        let permit = Arc::clone(&self.spawn_permits)
            .acquire_owned()
            .await
            .map_err(|_| self.unavailable("spawn semaphore closed"))?;

        let started = tokio::time::timeout(self.config.startup_timeout.get(), self.spawn()).await;
        drop(permit);

        let connection = match started {
            Ok(Ok(connection)) => connection,
            Ok(Err(err)) => {
                *self.last_error.lock().await = Some(err.to_string());
                return Err(err);
            }
            Err(_) => {
                let reason = format!(
                    "did not complete MCP initialize within {}s",
                    self.config.startup_timeout.get().as_secs()
                );
                *self.last_error.lock().await = Some(reason.clone());
                return Err(self.unavailable(&reason));
            }
        };

        *self.last_error.lock().await = None;
        *slot = Some(Arc::clone(&connection));
        Ok(connection)
    }

    async fn spawn(&self) -> ToolResult<Arc<Connection>> {
        let cfg = &self.config;
        info!(sidecar = %cfg.name, command = %cfg.command, "spawning sidecar");

        let mut command = Command::new(&cfg.command);
        command
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &cfg.cwd {
            command.current_dir(cwd);
        }

        let mut child =
            command.spawn().map_err(|e| self.unavailable(&format!("spawn failed: {e}")))?;
        let stdin =
            child.stdin.take().ok_or_else(|| self.unavailable("child stdin unavailable"))?;
        let stdout =
            child.stdout.take().ok_or_else(|| self.unavailable("child stdout unavailable"))?;
        let stderr = child.stderr.take();

        let connection = Arc::new(Connection {
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            _child: child,
        });

        tokio::spawn(read_responses(Arc::downgrade(&connection), stdout, cfg.name.clone()));
        if let Some(stderr) = stderr {
            tokio::spawn(forward_stderr(stderr, cfg.name.clone()));
        }

        self.initialize(&connection).await?;
        Ok(connection)
    }

    /// Performs the MCP handshake: `initialize` then `notifications/initialized`.
    async fn initialize(&self, connection: &Connection) -> ToolResult<()> {
        let params = json!({
            "protocolVersion": version::LATEST,
            "capabilities": { "roots": { "listChanged": false } },
            "clientInfo": { "name": "omni-mcp", "version": env!("CARGO_PKG_VERSION") }
        });
        request(connection, "initialize", Some(params))
            .await
            .map_err(|e| self.unavailable(&format!("initialize failed: {e}")))?;
        notify(connection, "notifications/initialized")
            .await
            .map_err(|e| self.unavailable(&format!("initialized notification failed: {e}")))?;
        Ok(())
    }

    fn unavailable(&self, reason: &str) -> ToolError {
        ToolError::Unavailable { backend: self.config.name.clone(), reason: reason.to_string() }
    }
}

/// Sends a request and awaits the response with the matching id.
async fn request(
    connection: &Connection,
    method: &str,
    params: Option<Value>,
) -> ToolResult<Value> {
    let id = connection.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    connection.pending.lock().await.insert(id, tx);

    let envelope = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    if let Err(err) = write_line(connection, &envelope).await {
        connection.pending.lock().await.remove(&id);
        connection.fail_all();
        return Err(err);
    }

    if let Ok(response) = rx.await {
        response.into_result().map_err(|e| ToolError::Failed(e.message))
    } else {
        connection.pending.lock().await.remove(&id);
        Err(ToolError::Failed("sidecar closed the connection before responding".into()))
    }
}

/// Sends a notification, which by definition receives no reply.
async fn notify(connection: &Connection, method: &str) -> ToolResult<()> {
    write_line(connection, &json!({ "jsonrpc": "2.0", "method": method })).await
}

async fn write_line(connection: &Connection, envelope: &Value) -> ToolResult<()> {
    let mut line = serde_json::to_string(envelope)
        .map_err(|e| ToolError::Failed(format!("could not encode request: {e}")))?;
    line.push('\n');

    let mut stdin = connection.stdin.lock().await;
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|e| ToolError::Failed(format!("write to sidecar failed: {e}")))?;
    stdin.flush().await.map_err(|e| ToolError::Failed(format!("flush to sidecar failed: {e}")))?;
    Ok(())
}

/// Demultiplexes the child's stdout into the waiting callers.
async fn read_responses<R>(connection: Weak<Connection>, stdout: R, name: String)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(err) => {
                warn!(sidecar = %name, %err, "sidecar stdout read failed");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let Some(connection) = connection.upgrade() else { return };

        // Anything that is not a well-formed response with a numeric id is
        // diagnostic noise (log lines, notifications, server-initiated
        // requests). Skipping it keeps the stream in sync instead of
        // permanently offsetting every later reply.
        let Ok(response) = serde_json::from_str::<Response>(&line) else {
            debug!(sidecar = %name, line = %snippet(&line), "ignoring non-response output");
            continue;
        };
        let Some(id) = response.id.as_ref().and_then(Value::as_u64) else {
            debug!(sidecar = %name, "ignoring message without a numeric id");
            continue;
        };

        if let Some(waiter) = connection.pending.lock().await.remove(&id) {
            let _ = waiter.send(response);
        } else {
            debug!(sidecar = %name, id, "response for unknown id discarded");
        }
    }

    if let Some(connection) = connection.upgrade() {
        info!(sidecar = %name, "sidecar closed its output stream");
        connection.fail_all();
    }
}

async fn forward_stderr<R>(stderr: R, name: String)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        debug!(sidecar = %name, "{}", snippet(&line));
    }
}

#[async_trait]
impl Backend for SidecarBackend {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn kind(&self) -> BackendKind {
        BackendKind::Sidecar
    }

    fn prefix(&self) -> Option<&str> {
        self.config.prefix.as_deref()
    }

    async fn list_tools(&self, timeout: Duration) -> ToolResult<Vec<Tool>> {
        let connection = self.connect().await?;
        let value = with_deadline(
            &self.config.name,
            "tools/list",
            timeout,
            request(&connection, "tools/list", None),
        )
        .await?;

        let tools = value.get("tools").cloned().unwrap_or_else(|| json!([]));
        serde_json::from_value(tools)
            .map_err(|e| ToolError::Failed(format!("sidecar sent a malformed tool list: {e}")))
    }

    async fn call(&self, tool: &str, args: Value, timeout: Duration) -> ToolResult<CallToolResult> {
        let connection = self.connect().await?;
        let params = json!({ "name": tool, "arguments": args });
        let value = with_deadline(
            &self.config.name,
            tool,
            timeout,
            request(&connection, "tools/call", Some(params)),
        )
        .await?;

        serde_json::from_value(value)
            .map_err(|e| ToolError::Failed(format!("sidecar sent a malformed tool result: {e}")))
    }

    async fn status(&self) -> BackendStatus {
        if let Some(err) = self.last_error.lock().await.clone() {
            return BackendStatus::Failed(err);
        }
        match self.connection.lock().await.as_ref() {
            Some(c) if c.alive.load(Ordering::SeqCst) => BackendStatus::Ready,
            Some(_) => BackendStatus::Failed("process exited".into()),
            None => BackendStatus::Idle,
        }
    }
}

async fn with_deadline<F>(
    backend: &str,
    tool: &str,
    timeout: Duration,
    future: F,
) -> ToolResult<Value>
where
    F: Future<Output = ToolResult<Value>>,
{
    if let Ok(result) = tokio::time::timeout(timeout, future).await {
        result
    } else {
        warn!(%backend, %tool, "sidecar call timed out");
        Err(ToolError::Timeout { tool: tool.to_string(), seconds: timeout.as_secs() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(command: &str, args: &[&str]) -> SidecarConfig {
        SidecarConfig {
            name: "test".into(),
            command: command.into(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
            lazy: true,
            prefix: None,
            startup_timeout: crate::config::duration::HumanDuration::secs(5),
            restart_on_failure: true,
            enabled: true,
        }
    }

    fn backend(config: SidecarConfig) -> SidecarBackend {
        SidecarBackend::new(config, Arc::new(Semaphore::new(1)))
    }

    /// A minimal MCP server in `sh`: answers `initialize` and `tools/list`, and
    /// deliberately interleaves a log line and a notification to prove the
    /// reader stays in sync.
    fn noisy_stub() -> SidecarConfig {
        config(
            "sh",
            &[
                "-c",
                r#"
                echo "starting up, this is not JSON" >&2
                while IFS= read -r line; do
                  case "$line" in
                    *'"initialize"'*)
                      echo '{"jsonrpc":"2.0","method":"notifications/message","params":{}}'
                      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
                      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{}}" ;;
                    *'"tools/list"'*)
                      echo 'plain log line on stdout'
                      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
                      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"ping\",\"inputSchema\":{\"type\":\"object\"}}]}}" ;;
                    *'"tools/call"'*)
                      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
                      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"pong\"}]}}" ;;
                  esac
                done
                "#,
            ],
        )
    }

    #[tokio::test]
    async fn lists_tools_despite_interleaved_noise() {
        let backend = backend(noisy_stub());
        let tools = backend.list_tools(Duration::from_secs(10)).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ping");
    }

    #[tokio::test]
    async fn stays_in_sync_across_many_sequential_calls() {
        // The old one-line-per-request reader drifted permanently after the
        // first stray line; this asserts the demultiplexer does not.
        let backend = backend(noisy_stub());
        for _ in 0..5 {
            let result = backend.call("ping", json!({}), Duration::from_secs(10)).await.unwrap();
            assert_eq!(result.content, vec![crate::protocol::Content::text("pong")]);
        }
    }

    #[tokio::test]
    async fn concurrent_calls_each_receive_their_own_response() {
        let backend = Arc::new(backend(noisy_stub()));
        let calls = (0..8).map(|_| {
            let backend = Arc::clone(&backend);
            tokio::spawn(
                async move { backend.call("ping", json!({}), Duration::from_secs(10)).await },
            )
        });
        for handle in calls {
            assert!(handle.await.unwrap().is_ok());
        }
    }

    #[tokio::test]
    async fn a_command_that_does_not_exist_is_unavailable_not_a_panic() {
        let backend = backend(config("omni-mcp-no-such-binary", &[]));
        let err = backend.list_tools(Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, ToolError::Unavailable { .. }), "got {err:?}");
        assert!(matches!(backend.status().await, BackendStatus::Failed(_)));
    }

    #[tokio::test]
    async fn a_sidecar_that_never_answers_initialize_times_out() {
        let mut cfg = config("sleep", &["60"]);
        cfg.startup_timeout = crate::config::duration::HumanDuration::millis(300);
        let backend = backend(cfg);

        let err = backend.list_tools(Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, ToolError::Unavailable { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn a_sidecar_that_exits_is_reported_and_then_retried() {
        let backend = backend(config("true", &[]));
        assert!(backend.list_tools(Duration::from_secs(5)).await.is_err());
        // A second attempt must not hang or reuse the dead connection.
        assert!(backend.list_tools(Duration::from_secs(5)).await.is_err());
    }

    #[tokio::test]
    async fn status_is_idle_before_first_use() {
        assert_eq!(backend(noisy_stub()).status().await, BackendStatus::Idle);
        // Crucially, asking for status must not have spawned anything.
    }
}
