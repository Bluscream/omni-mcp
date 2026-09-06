//! MCP wire protocol: JSON-RPC 2.0 envelopes, MCP payload types, and version negotiation.

pub mod jsonrpc;
pub mod mcp;
pub mod version;

pub use jsonrpc::{Request, Response, RpcError, code};
pub use mcp::{CallToolResult, Content, Tool};
pub use version::ProtocolVersion;
