//! The in-process backend.
//!
//! The tools themselves are no longer implemented here. They live in their own
//! crates — `common-mcp` for text, filesystem, hex and eval, plus `ssh-mcp`,
//! `resx-mcp` and `everything-search-mcp` — and are embedded as library calls.
//!
//! That is the point of the split: those crates are usable standalone, and a
//! host that wants several of them pays for one process rather than a
//! subprocess per group. omni-mcp keeps what is genuinely its own — routing,
//! proxies, supervised sidecars and diagnostics.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mcp_toolkit::{Composite, Member, ToolGroup};
use serde_json::Value;

use super::{Backend, BackendKind, BackendStatus};
use crate::config::{Config, SshServerConfig, ToolPolicy};
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, Content, Tool};

pub struct NativeBackend {
    group: Arc<Composite>,
    descriptors: Vec<Tool>,
}

impl NativeBackend {
    pub fn new(group: Composite) -> Self {
        let descriptors = group.tools().iter().map(to_protocol_tool).collect();
        Self { group: Arc::new(group), descriptors }
    }

    /// Assembles every embedded tool crate from omni-mcp's configuration.
    pub fn from_config(config: &Config) -> Self {
        Self::new(assemble(&config.tools, config.ssh.clone()))
    }

    /// The tools available with a bare policy and no SSH servers.
    pub fn with_defaults(policy: &ToolPolicy) -> Self {
        Self::new(assemble(policy, Vec::new()))
    }

    pub fn handles(&self, tool: &str) -> bool {
        self.descriptors.iter().any(|t| t.name == tool)
    }

    /// Tool names claimed by more than one crate. Only the first is reachable,
    /// so this is surfaced rather than left to be found by a failing call.
    pub fn collisions(&self) -> &[String] {
        self.group.collisions()
    }
}

/// Builds the composite from omni-mcp's policy.
fn assemble(policy: &ToolPolicy, ssh: Vec<SshServerConfig>) -> Composite {
    let roots: Vec<PathBuf> = policy.allowed_roots.clone();

    let common = common_mcp::policy::Policy::new(
        policy.allow_file_mutation,
        policy.allow_code_execution,
        &roots,
        &[],
        policy.max_file_bytes,
    );
    let resx =
        resx_mcp::policy::Policy::new(policy.allow_file_mutation, &roots, policy.max_file_bytes);

    let mut members = vec![
        Member::new(Arc::new(common_mcp::text::TextTools::new(common.clone()))),
        Member::new(Arc::new(common_mcp::fs::FsTools::new(common.clone()))),
        Member::new(Arc::new(common_mcp::hex::HexTools::new(common.clone()))),
        Member::new(Arc::new(common_mcp::eval::EvalTools::new(common))),
        Member::new(Arc::new(resx_mcp::tools::ResxTools::new(resx))),
        Member::new(Arc::new(everything_search_mcp::tools::SearchTools::new(
            everything_search_mcp::tools::Endpoint {
                host: "127.0.0.1".into(),
                port: 14680,
                username: None,
                password: None,
            },
        ))),
    ];

    // Only offer the SSH tools when servers are configured; advertising them
    // with nothing to connect to just invites failing calls.
    if !ssh.is_empty() {
        let ssh_policy = ssh_mcp::policy::Policy::new(
            policy.allow_file_mutation,
            policy.allow_host_key_override,
            policy.allow_host_key_learning,
            &roots,
        );
        members.push(Member::new(Arc::new(ssh_mcp::tools::SshTools::new(ssh, ssh_policy))));
    }

    Composite::new(members)
}

fn to_protocol_tool(def: &mcp_toolkit::ToolDef) -> Tool {
    Tool {
        name: def.name.clone(),
        description: Some(def.description.clone()),
        input_schema: def.schema.clone(),
        extra: serde_json::Map::new(),
    }
}

fn to_protocol_result(output: mcp_toolkit::ToolOutput) -> CallToolResult {
    CallToolResult {
        content: vec![Content::text(output.text)],
        is_error: output.is_error.then_some(true),
        structured_content: output.structured,
    }
}

/// Maps a tool crate's failure onto omni-mcp's error type, preserving the
/// distinction between "the caller got it wrong" and "it ran and failed".
fn to_tool_error(failure: mcp_toolkit::ToolFailure) -> ToolError {
    use mcp_toolkit::ToolFailure as F;
    match failure {
        F::InvalidArguments(msg) => ToolError::InvalidArguments(msg),
        F::NotFound(name) => ToolError::NotFound(name),
        F::Denied(msg) => ToolError::Denied(msg),
        F::Timeout { tool, seconds } => ToolError::Timeout { tool, seconds },
        F::Failed(msg) => ToolError::Failed(msg),
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
        if !self.handles(tool) {
            return Err(ToolError::NotFound(tool.to_string()));
        }
        let execution = self.group.call(tool, args);

        match tokio::time::timeout(timeout, execution).await {
            Ok(Ok(output)) => Ok(to_protocol_result(output)),
            Ok(Err(failure)) => Err(to_tool_error(failure)),
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
mod tests;
