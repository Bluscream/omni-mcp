//! Backends: the three places a tool call can be served from.
//!
//! * [`native`]  — Rust code in this binary, microsecond latency.
//! * [`sidecar`] — a supervised child process speaking MCP over stdio.
//! * [`proxy`]   — a remote MCP server over HTTP.
//!
//! All three implement [`Backend`] so the router can treat them uniformly and
//! route by an explicit tool-name table instead of the old "try every backend
//! in turn and keep the first non-error" scattergun, which broadcast every
//! unknown tool call to every configured endpoint.

pub mod native;
pub mod proxy;
pub mod sidecar;

use std::time::Duration;

use async_trait::async_trait;

use crate::error::ToolResult;
use crate::protocol::{CallToolResult, Tool};

#[async_trait]
pub trait Backend: Send + Sync {
    /// Stable identifier, used in diagnostics and the routing table.
    fn name(&self) -> &str;

    /// What kind of backend this is, for `omni_status`.
    fn kind(&self) -> BackendKind;

    /// Prefix applied to this backend's tool names to keep them unique.
    fn prefix(&self) -> Option<&str> {
        None
    }

    /// Enumerates the tools this backend serves.
    async fn list_tools(&self, timeout: Duration) -> ToolResult<Vec<Tool>>;

    /// Invokes `tool` (the *unprefixed*, backend-local name).
    async fn call(
        &self,
        tool: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> ToolResult<CallToolResult>;

    /// Cheap liveness description for `omni_status`. Must not spawn anything.
    async fn status(&self) -> BackendStatus;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Native,
    Sidecar,
    Proxy,
}

impl BackendKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Sidecar => "sidecar",
            Self::Proxy => "proxy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendStatus {
    /// Ready to serve calls.
    Ready,
    /// Configured but not yet contacted; will start on first use.
    Idle,
    /// Last interaction failed.
    Failed(String),
    /// Turned off in configuration.
    Disabled,
}

impl BackendStatus {
    pub fn label(&self) -> &str {
        match self {
            Self::Ready => "ready",
            Self::Idle => "idle (lazy)",
            Self::Failed(reason) => reason,
            Self::Disabled => "disabled",
        }
    }
}

/// Shortens untrusted backend output for log lines and error messages, without
/// splitting a multi-byte character.
pub fn snippet(text: &str) -> String {
    const LIMIT: usize = 400;
    let text = text.trim();
    if text.len() <= LIMIT {
        return text.to_string();
    }
    let cut = (0..=LIMIT).rev().find(|i| text.is_char_boundary(*i)).unwrap_or(0);
    format!("{}…", &text[..cut])
}

/// Applies a backend's namespace prefix to a tool name.
pub fn apply_prefix(prefix: Option<&str>, name: &str) -> String {
    match prefix {
        Some(p) if !p.is_empty() && !name.starts_with(p) => format!("{p}{name}"),
        _ => name.to_string(),
    }
}

/// Removes a backend's namespace prefix to recover the backend-local name.
pub fn strip_prefix<'a>(prefix: Option<&str>, name: &'a str) -> &'a str {
    match prefix {
        Some(p) if !p.is_empty() => name.strip_prefix(p).unwrap_or(name),
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_round_trips() {
        let p = Some("ha_");
        assert_eq!(apply_prefix(p, "turn_on"), "ha_turn_on");
        assert_eq!(strip_prefix(p, "ha_turn_on"), "turn_on");
    }

    #[test]
    fn absent_or_empty_prefix_is_a_no_op() {
        assert_eq!(apply_prefix(None, "x"), "x");
        assert_eq!(apply_prefix(Some(""), "x"), "x");
        assert_eq!(strip_prefix(None, "x"), "x");
        assert_eq!(strip_prefix(Some(""), "x"), "x");
    }

    #[test]
    fn an_already_prefixed_name_is_not_double_prefixed() {
        assert_eq!(apply_prefix(Some("ha_"), "ha_turn_on"), "ha_turn_on");
    }

    #[test]
    fn stripping_a_prefix_that_is_absent_leaves_the_name_alone() {
        assert_eq!(strip_prefix(Some("ha_"), "other_tool"), "other_tool");
    }
}
