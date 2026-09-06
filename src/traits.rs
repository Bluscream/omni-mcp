use async_trait::async_trait;
use serde_json::Value;
use crate::types::{CallToolResult, Tool};

/// Trait implemented by native Rust MCP modules.
/// To add a new native tool to omni-mcp, implement this trait and register it in `registry.rs`.
#[async_trait]
pub trait McpModule: Send + Sync {
    /// Unique identifier for this module
    fn name(&self) -> &'static str;

    /// List of tools exposed by this module
    fn tools(&self) -> Vec<Tool>;

    /// Invokes a tool by name with arguments
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult, String>;
}
