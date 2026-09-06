//! omni-mcp: one process that serves native Rust tools, supervises local MCP
//! sidecars and proxies remote MCP servers, behind a single stdio or HTTP
//! endpoint.
//!
//! The design goal is containment. Running a dozen MCP servers as independent
//! container-wrapped processes is what produced the kernel lock contention this
//! project was written to eliminate, so every path that can create work is
//! bounded: sidecar spawns are serialised, concurrent tool calls are capped,
//! every call has a deadline, and no operation fans out to a backend that does
//! not own the tool being called.

pub mod backend;
pub mod cli;
pub mod config;
pub mod error;
pub mod protocol;
pub mod router;
pub mod server;
pub mod tools;

pub use config::Config;
pub use error::{StartupError, ToolError};
pub use router::Router;

/// The version reported over `initialize` and `--version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
