//! Native tools implemented directly in this binary.

pub mod args;
pub mod context;
pub mod eval;
pub mod fs;
pub mod hex;
pub mod resx;
pub mod search;
pub mod ssh;
pub mod text;

use async_trait::async_trait;
use serde_json::Value;

use crate::config::SshServerConfig;
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, Tool};
pub use context::ToolContext;

/// A group of related native tools.
#[async_trait]
pub trait NativeTool: Send + Sync {
    /// Tools this group advertises.
    fn descriptors(&self) -> Vec<Tool>;

    /// Runs one of them. `name` is guaranteed to be one of the advertised names.
    async fn call(&self, name: &str, args: Value, ctx: &ToolContext) -> ToolResult<CallToolResult>;
}

/// Every native tool group, in registration order.
pub fn all() -> Vec<Box<dyn NativeTool>> {
    all_with_ssh(Vec::new())
}

/// Every native tool group including configured SSH server profiles.
pub fn all_with_ssh(ssh_configs: Vec<SshServerConfig>) -> Vec<Box<dyn NativeTool>> {
    vec![
        Box::new(text::TextTools),
        Box::new(fs::FsTools),
        Box::new(hex::HexTools),
        Box::new(resx::ResxTools),
        Box::new(search::SearchTools::new()),
        Box::new(eval::EvalTools),
        Box::new(ssh::SshTools::new(ssh_configs)),
    ]
}

/// Used by tool groups to reject a name they do not implement. Reaching this is
/// a routing bug rather than a user error.
pub fn unknown(name: &str) -> ToolError {
    ToolError::NotFound(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn native_tool_names_are_unique() {
        let mut seen = HashSet::new();
        for group in all() {
            for tool in group.descriptors() {
                assert!(seen.insert(tool.name.clone()), "duplicate native tool {}", tool.name);
            }
        }
        assert!(!seen.is_empty());
    }

    #[test]
    fn every_native_tool_is_described_and_has_an_object_schema() {
        for group in all() {
            for tool in group.descriptors() {
                let description = tool.description.unwrap_or_default();
                assert!(
                    description.len() > 20,
                    "{} needs a description the model can act on",
                    tool.name
                );
                assert_eq!(
                    tool.input_schema["type"], "object",
                    "{} must take an object",
                    tool.name
                );
                assert!(
                    tool.input_schema["properties"].is_object(),
                    "{} must declare properties",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn every_required_property_is_actually_declared() {
        for group in all() {
            for tool in group.descriptors() {
                let Some(required) = tool.input_schema["required"].as_array() else { continue };
                for field in required {
                    let name = field.as_str().unwrap();
                    assert!(
                        tool.input_schema["properties"].get(name).is_some(),
                        "{} requires {name:?} but does not declare it",
                        tool.name
                    );
                }
            }
        }
    }
}
